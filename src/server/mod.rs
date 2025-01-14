use crate::{
    block::{
        parser::BlockParser, precomputed::PrecomputedBlock, receiver::BlockReceiver,
        store::BlockStore, BlockHash, BlockWithoutHeight,
    },
    state::{
        ledger::{self, genesis::GenesisRoot, public_key::PublicKey, Ledger},
        summary::{SummaryShort, SummaryVerbose},
        IndexerMode, IndexerState,
    },
    store::IndexerStore,
    CANONICAL_UPDATE_THRESHOLD, MAINNET_GENESIS_HASH, MAINNET_TRANSITION_FRONTIER_K,
    PRUNE_INTERVAL_DEFAULT, SOCKET_NAME,
};
use clap::Parser;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use interprocess::local_socket::tokio::{LocalSocketListener, LocalSocketStream};
use log::trace;
use std::{path::PathBuf, process};
use tokio::fs::{self, create_dir_all, metadata};
use tracing::{debug, error, info, instrument, level_filters::LevelFilter};
use tracing_subscriber::prelude::*;
use uuid::Uuid;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct ServerArgs {
    /// Path to the root ledger (if non-genesis, set --non-genesis-ledger and --root-hash)
    #[arg(short, long)]
    ledger: PathBuf,
    /// Use a non-genesis ledger
    #[arg(short, long, default_value_t = false)]
    non_genesis_ledger: bool,
    /// Hash of the base ledger
    #[arg(
        long,
        default_value = MAINNET_GENESIS_HASH
    )]
    root_hash: String,
    /// Path to startup blocks directory
    #[arg(short, long, default_value = concat!(env!("HOME"), "/.mina-indexer/startup-blocks"))]
    startup_dir: PathBuf,
    /// Path to directory to watch for new blocks
    #[arg(short, long, default_value = concat!(env!("HOME"), "/.mina-indexer/watch-blocks"))]
    watch_dir: PathBuf,
    /// Path to directory for rocksdb
    #[arg(short, long, default_value = concat!(env!("HOME"), "/.mina-indexer/database"))]
    database_dir: PathBuf,
    /// Path to directory for logs
    #[arg(long, default_value = concat!(env!("HOME"), "/.mina-indexer/logs"))]
    log_dir: PathBuf,
    /// Only store canonical blocks in the db
    #[arg(short, long, default_value_t = false)]
    keep_non_canonical_blocks: bool,
    /// Max file log level
    #[arg(long, default_value_t = LevelFilter::DEBUG)]
    log_level: LevelFilter,
    /// Max stdout log level
    #[arg(long, default_value_t = LevelFilter::INFO)]
    log_level_stdout: LevelFilter,
    /// Ignore restoring indexer state from an existing db on the path provided by database_dir
    #[arg(short, long, default_value_t = false)]
    ignore_db: bool,
    /// Interval for pruning the root branch
    #[arg(short, long, default_value_t = PRUNE_INTERVAL_DEFAULT)]
    prune_interval: u32,
    /// Threshold for updating the canonical tip/ledger
    #[arg(short, long, default_value_t = CANONICAL_UPDATE_THRESHOLD)]
    canonical_update_threshold: u32,
}

pub struct IndexerConfiguration {
    ledger: GenesisRoot,
    non_genesis_ledger: bool,
    root_hash: BlockHash,
    startup_dir: PathBuf,
    watch_dir: PathBuf,
    database_dir: PathBuf,
    keep_noncanonical_blocks: bool,
    log_file: PathBuf,
    log_level: LevelFilter,
    log_level_stdout: LevelFilter,
    ignore_db: bool,
    prune_interval: u32,
    canonical_update_threshold: u32,
}

#[instrument(skip_all)]
pub async fn handle_command_line_arguments(
    args: ServerArgs,
) -> anyhow::Result<IndexerConfiguration> {
    trace!("Parsing server args");

    let non_genesis_ledger = args.non_genesis_ledger;
    let root_hash = BlockHash(args.root_hash.to_string());
    let startup_dir = args.startup_dir;
    let watch_dir = args.watch_dir;
    let database_dir = args.database_dir;
    let keep_noncanonical_blocks = args.keep_non_canonical_blocks;
    let log_dir = args.log_dir;
    let log_level = args.log_level;
    let log_level_stdout = args.log_level_stdout;
    let ignore_db = args.ignore_db;
    let prune_interval = args.prune_interval;
    let canonical_update_threshold = args.canonical_update_threshold;

    assert!(
        // bad things happen if this condition fails
        canonical_update_threshold < MAINNET_TRANSITION_FRONTIER_K,
        "canonical update threshold must be strictly less than the transition frontier length!"
    );

    create_dir_if_non_existent(watch_dir.to_str().unwrap()).await;
    create_dir_if_non_existent(log_dir.to_str().unwrap()).await;

    info!("Parsing genesis ledger file at {}", args.ledger.display());

    match ledger::genesis::parse_file(&args.ledger).await {
        Err(err) => {
            error!(
                reason = "Unable to parse genesis ledger",
                error = err.to_string(),
                path = &args.ledger.display().to_string()
            );
            process::exit(100)
        }
        Ok(ledger) => {
            info!("Genesis ledger parsed successfully!");

            let mut log_number = 0;
            let mut log_fname = format!("{}/mina-indexer-{}.log", log_dir.display(), log_number);

            while tokio::fs::metadata(&log_fname).await.is_ok() {
                log_number += 1;
                log_fname = format!("{}/mina-indexer-{}.log", log_dir.display(), log_number);
            }

            Ok(IndexerConfiguration {
                ledger,
                non_genesis_ledger,
                root_hash,
                startup_dir,
                watch_dir,
                database_dir,
                keep_noncanonical_blocks,
                log_file: PathBuf::from(&log_fname),
                log_level,
                log_level_stdout,
                ignore_db,
                prune_interval,
                canonical_update_threshold,
            })
        }
    }
}

#[instrument(skip_all)]
pub async fn run(args: ServerArgs) -> Result<(), anyhow::Error> {
    debug!("Checking that a server instance isn't already running");
    LocalSocketStream::connect(SOCKET_NAME)
        .await
        .expect_err("Server is already running... Exiting.");

    info!("Starting mina-indexer server");
    let IndexerConfiguration {
        ledger,
        non_genesis_ledger,
        root_hash,
        startup_dir,
        watch_dir,
        database_dir,
        keep_noncanonical_blocks,
        log_file,
        log_level,
        log_level_stdout,
        ignore_db,
        prune_interval,
        canonical_update_threshold,
    } = handle_command_line_arguments(args).await?;

    // setup tracing
    if let Some(parent) = log_file.parent() {
        create_dir_if_non_existent(parent.to_str().unwrap()).await;
    }

    let log_file = std::fs::File::create(log_file)?;
    let file_layer = tracing_subscriber::fmt::layer().with_writer(log_file);

    let stdout_layer = tracing_subscriber::fmt::layer();
    tracing_subscriber::registry()
        .with(stdout_layer.with_filter(log_level_stdout))
        .with(file_layer.with_filter(log_level))
        .init();

    let mode = if keep_noncanonical_blocks {
        IndexerMode::Full
    } else {
        IndexerMode::Light
    };
    let mut indexer_state = if ignore_db {
        info!(
            "Initializing indexer state from blocks in {}",
            startup_dir.display()
        );
        IndexerState::new(
            mode,
            root_hash.clone(),
            ledger.ledger,
            Some(&database_dir),
            MAINNET_TRANSITION_FRONTIER_K,
            prune_interval,
            canonical_update_threshold,
        )?
    } else {
        // if db exists in database_dir, use it's blocks to restore state before reading blocks from startup_dir (or maybe go right to watching)
        // if no db or it doesn't have blocks, use the startup_dir like usual
        IndexerState::new_from_db(&database_dir)?;
        todo!("Restoring from db in {}", database_dir.display());
    };

    let mut block_parser = BlockParser::new(&startup_dir)?;
    if ignore_db && !non_genesis_ledger {
        indexer_state
            .initialize_with_contiguous_canonical(&mut block_parser)
            .await?;
    } else {
        indexer_state
            .initialize_without_contiguous_canonical(&mut block_parser)
            .await?;
    }

    let mut block_receiver = BlockReceiver::new().await?;
    block_receiver.load_directory(&watch_dir).await?;
    info!("Block receiver set to watch {watch_dir:?}");

    let listener = LocalSocketListener::bind(SOCKET_NAME)?;
    info!("Local socket listener started");

    loop {
        tokio::select! {
            block_fut = block_receiver.recv() => {
                if let Some(block_result) = block_fut {
                    let precomputed_block = block_result?;
                    let block = BlockWithoutHeight::from_precomputed(&precomputed_block);
                    debug!("Receiving block {block:?}");

                    indexer_state.add_block(&precomputed_block)?;
                    info!("Added {block:?}");
                } else {
                    info!("Block receiver shutdown, system exit");
                    return Ok(())
                }
            }

            conn_fut = listener.accept() => {
                let conn = conn_fut?;
                info!("Receiving connection");
                let best_chain = indexer_state.root_branch.longest_chain();

                let primary_path = database_dir.clone();
                let mut secondary_path = primary_path.clone();
                secondary_path.push(Uuid::new_v4().to_string());

                debug!("Spawning secondary readonly RocksDB instance");
                let block_store_readonly = IndexerStore::new_read_only(&primary_path, &secondary_path)?;
                let summary = indexer_state.summary_verbose();
                let ledger = indexer_state.best_ledger()?.unwrap();

                // handle the connection
                tokio::spawn(async move {
                    debug!("Handling connection");
                    if let Err(e) = handle_conn(conn, block_store_readonly, best_chain, ledger, summary).await {
                        error!("Error handling connection: {e}");
                    }

                    debug!("Removing readonly instance at {}", secondary_path.display());
                    tokio::fs::remove_dir_all(&secondary_path).await.ok();
                });
            }
        }
    }
}

#[instrument(skip_all)]
async fn handle_conn(
    conn: LocalSocketStream,
    db: IndexerStore,
    best_chain: Vec<BlockHash>,
    ledger: Ledger,
    summary: SummaryVerbose,
) -> Result<(), anyhow::Error> {
    let (reader, mut writer) = conn.into_split();
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::with_capacity(128);
    let _read = reader.read_until(0, &mut buffer).await?;

    if let Some(bytes) = handle_commands(buffer, db, best_chain, ledger, summary)? {
        writer.write_all(&bytes).await?
    }

    Ok(())
}

fn handle_commands<T: BlockStore>(
    buffer: Vec<u8>,
    db: T,
    best_chain: Vec<BlockHash>,
    ledger: Ledger,
    summary: SummaryVerbose,
) -> Result<Option<Vec<u8>>, anyhow::Error> {
    let mut buffers = buffer.split(|byte| *byte == 32);
    let command = buffers.next().unwrap();
    let command_string = String::from_utf8(command.to_vec())?;

    match command_string.as_str() {
        "account" => {
            let data_buffer = buffers.next().unwrap();
            let public_key = PublicKey::from_address(&String::from_utf8(
                data_buffer[..data_buffer.len() - 1].to_vec(),
            )?)?;
            info!("Received account command for {public_key:?}");
            debug!("Using ledger {ledger:?}");
            let account = ledger.accounts.get(&public_key);
            if let Some(account) = account {
                debug!("Writing account {account:?} to client");
                let bytes = bcs::to_bytes(account)?;
                Ok(Some(bytes))
            } else {
                Ok(None)
            }
        }
        "best_chain" => {
            info!("Received best_chain command");
            let data_buffer = buffers.next().unwrap();
            let num = String::from_utf8(data_buffer[..data_buffer.len() - 1].to_vec())?
                .parse::<usize>()?;
            let best_chain: Vec<PrecomputedBlock> = best_chain[..best_chain.len() - 1]
                .iter()
                .take(num)
                .cloned()
                .map(|state_hash| db.get_block(&state_hash).unwrap().unwrap())
                .collect();
            let bytes = bcs::to_bytes(&best_chain)?;
            Ok(Some(bytes))
        }
        "best_ledger" => {
            info!("Received best_ledger command");
            let data_buffer = buffers.next().unwrap();
            let path = &String::from_utf8(data_buffer[..data_buffer.len() - 1].to_vec())?
                .parse::<PathBuf>()?;
            debug!("Writing ledger to {}", path.display());
            //fs::write(path, format!("{ledger:?}"))?;
            let bytes = bcs::to_bytes(&format!("Ledger written to {}", path.display()))?;
            Ok(Some(bytes))
        }
        "summary" => {
            info!("Received summary command");
            let data_buffer = buffers.next().unwrap();
            let verbose = String::from_utf8(data_buffer[..data_buffer.len() - 1].to_vec())?
                .parse::<bool>()?;
            if verbose {
                let bytes = bcs::to_bytes(&summary)?;
                Ok(Some(bytes))
            } else {
                let summary: SummaryShort = summary.into();
                let bytes = bcs::to_bytes(&summary)?;
                Ok(Some(bytes))
            }
        }
        bad_request => {
            let err_msg = format!("Malformed request: {bad_request}");
            error!("{err_msg}");
            Err(anyhow::Error::msg(err_msg))
        }
    }
}

async fn create_dir_if_non_existent(path: &str) {
    if metadata(path).await.is_err() {
        debug!("Creating directory {path}");
        create_dir_all(path).await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use time::{Date, Instant, Month, PrimitiveDateTime, Time};
    use crate::server::handle_commands;
    use crate::block::store::MockBlockStore;
    use crate::state::ledger::Ledger;
    use crate::state::summary::{SummaryVerbose, WitnessTreeSummaryVerbose};

    fn test_handle_commands() {
        let command_strings = vec![
            "summarytrue",
            "summaryfalse",
        ];
        let results: Vec<Result<Option<Vec<u8>>, anyhow::Error>> = vec![
            Ok(None)
        ];
        let ledger: Ledger = Ledger::new();
        let summary  = SummaryVerbose {
            uptime: Default::default(),
            date_time: PrimitiveDateTime::new(Date::from_calendar_date(2023, Month::July, 5).unwrap(), Time::MIDNIGHT),
            blocks_processed: 0,
            witness_tree: WitnessTreeSummaryVerbose {
                best_tip_length: 0,
                best_tip_hash: String::new(),
                canonical_tip_length: 0,
                canonical_tip_hash: String::new(),
                root_hash: String::new(),
                root_height: 0,
                root_length: 0,
                num_leaves: 0,
                num_dangling: 0,
                max_dangling_height: 0,
                max_dangling_length: 0,
                witness_tree: String::new(),
            },
            db_stats: None,
        };
        for (command_string, result) in command_strings.iter().zip(results) {
            let db = MockBlockStore::new();
            let buffer = command_string.bytes().collect::<Vec<_>>();
            assert_eq!(None, handle_commands(buffer, db, vec![], ledger.clone(), summary.clone()).unwrap());
        }
    }
}
