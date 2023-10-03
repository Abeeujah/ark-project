pub mod event_handler;
pub mod managers;
pub mod storage;

use crate::storage::types::BlockIndexingStatus;
use anyhow::Result;
use ark_starknet::client::StarknetClient;
use event_handler::EventHandler;
use log::{info, trace};
use managers::{BlockManager, CollectionManager, EventManager, PendingBlockData, TokenManager};
use starknet::core::types::*;
use std::sync::Arc;
use storage::types::{ContractType, StorageError};
use storage::Storage;
use tokio::sync::RwLock as AsyncRwLock;
use tracing::{span, Level};
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Registry};

pub type IndexerResult<T> = Result<T, IndexerError>;

/// Generic errors for Pontos.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error("Storage error occurred")]
    StorageError(StorageError),
    #[error("An error occurred")]
    Anyhow(String),
}

impl From<StorageError> for IndexerError {
    fn from(e: StorageError) -> Self {
        IndexerError::StorageError(e)
    }
}

impl From<anyhow::Error> for IndexerError {
    fn from(e: anyhow::Error) -> Self {
        IndexerError::Anyhow(e.to_string())
    }
}

pub struct PontosConfig {
    pub indexer_version: String,
    pub indexer_identifier: String,
}

pub struct Pontos<S: Storage, C: StarknetClient, E: EventHandler> {
    client: Arc<C>,
    event_handler: Arc<E>,
    config: PontosConfig,
    block_manager: Arc<BlockManager<S>>,
    event_manager: Arc<EventManager<S>>,
    token_manager: Arc<TokenManager<S, C>>,
    collection_manager: Arc<AsyncRwLock<CollectionManager<S, C>>>,
    pending_cache: Arc<AsyncRwLock<PendingBlockData>>,
}

impl<S: Storage, C: StarknetClient, E: EventHandler + Send + Sync> Pontos<S, C, E> {
    ///
    pub fn new(
        client: Arc<C>,
        storage: Arc<S>,
        event_handler: Arc<E>,
        config: PontosConfig,
    ) -> Self {
        init_tracing();

        Pontos {
            config,
            client: Arc::clone(&client),
            event_handler: Arc::clone(&event_handler),
            block_manager: Arc::new(BlockManager::new(Arc::clone(&storage))),
            event_manager: Arc::new(EventManager::new(Arc::clone(&storage))),
            token_manager: Arc::new(TokenManager::new(Arc::clone(&storage), Arc::clone(&client))),
            // Collection manager has internal cache, so some functions are using `&mut self`.
            // For this reason, we must protect the write operations in order to share
            // the cache with any possible thread using `index_block_range` of this instance.
            collection_manager: Arc::new(AsyncRwLock::new(CollectionManager::new(
                Arc::clone(&storage),
                Arc::clone(&client),
            ))),
            pending_cache: Arc::new(AsyncRwLock::new(PendingBlockData::new())),
        }
    }

    /// Starts a loop to only index the pending block.
    pub async fn index_pending(&self) -> IndexerResult<()> {
        loop {
            let mut cache = self.pending_cache.write().await;

            let (ts, txs) = self
                .client
                .block_txs_hashes(BlockId::Tag(BlockTag::Pending))
                .await?;

            // We define the block number as being the timestamp by default
            // until the actual block number is available.
            let mut block_number = ts;

            if cache.get_timestamp() == 0 {
                cache.set_timestamp(ts);
            }

            log::debug!("Pending block {} with {} txs", ts, txs.len());

            // If the timestamp is different from the previous loop,
            // we must first ensure we've fetched and processed all the transactions
            // of the previous pending block, which is now the "Latest".
            if ts != cache.get_timestamp() {
                log::debug!("ts differ! {} {}", ts, cache.get_timestamp());
                // Get the latest block number, generated by the sequencer, which is
                // expected to be the one we just processed.
                block_number = self.client.block_number().await?;
                let (latest_ts, txs) = self
                    .client
                    .block_txs_hashes(BlockId::Tag(BlockTag::Latest))
                    .await?;

                // The latest block is supposed to be the one we have in cache.
                // If not, we must clean up the last block that may be skipped
                // by the sequencer.
                if latest_ts != cache.get_timestamp() {
                    log::debug!(
                        "Mismatch with latest block timestamp expected {} got {}",
                        cache.get_timestamp(),
                        latest_ts,
                    );

                    self.block_manager
                        .clean_block(cache.get_timestamp())
                        .await?;

                    // Clean up and wait next tick to restart on the last pending block.
                    cache.set_timestamp(0);
                    cache.clear_tx_hashes();
                    continue;
                }

                // Ensures all the txs of the latest block are processed correctly.
                for tx_hash in txs {
                    if cache.is_tx_processed(&tx_hash) {
                        continue;
                    } else {
                        match self.client.events_from_tx_receipt(tx_hash).await {
                            Ok(events) => {
                                self.process_events(events, block_number, latest_ts).await?;
                                cache.add_tx_as_processed(&tx_hash);
                            }
                            Err(e) => {
                                log::error!(
                                    "[latest] error processing tx {:#064x} {:?}",
                                    tx_hash,
                                    e
                                );
                                // TODO: cleanup then?
                                // This should not happen on the latest block, we want
                                // to stop if this happen.
                                return Err(e.into());
                            }
                        }
                    }
                }

                self.block_manager
                    .update_last_pending_block(block_number, latest_ts)
                    .await?;

                log::info!(
                    "Pending block {} is now latest block number #{}",
                    latest_ts,
                    block_number
                );

                // Setup the local variables to directly start the pending block
                // indexation instead of waiting the next tick.
                cache.set_timestamp(ts);
                cache.clear_tx_hashes();
                block_number = ts;
            }

            // Process pending txs, if not already processed.
            for tx_hash in txs {
                if cache.is_tx_processed(&tx_hash) {
                    continue;
                } else {
                    log::debug!("processing tx {:#064x}", tx_hash);
                    match self.client.events_from_tx_receipt(tx_hash).await {
                        Ok(events) => {
                            self.process_events(events, block_number, cache.get_timestamp())
                                .await?;
                            cache.add_tx_as_processed(&tx_hash);
                        }
                        Err(e) => {
                            log::warn!("error processing tx {:#064x} {:?}", tx_hash, e);
                            // Sometimes, the tx hash is not found. To avoid
                            // loosing this tx as it will be available few seconds
                            // later, we skip it and try to parse it at the next
                            // tick.
                            continue;
                        }
                    }
                }
            }

            // TODO: make this configurable?
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        }
    }

    /// If "Latest" is used for the `to_block`,
    /// this function will only index the latest block
    /// that is not pending.
    /// If you use this on latest, be sure to don't have any
    /// other pontos instance running `index_pending` as you may
    /// deal with overlaps or at least check db registers first.
    pub async fn index_block_range(
        &self,
        from_block: BlockId,
        to_block: BlockId,
        do_force: bool,
    ) -> IndexerResult<()> {
        let mut current_u64 = self.client.block_id_to_u64(&from_block).await?;
        let to_u64 = self.client.block_id_to_u64(&to_block).await?;

        loop {
            trace!("Indexing block range: {} {}", current_u64, to_u64);

            if current_u64 > to_u64 {
                info!("End of indexing block range");
                break;
            }

            if self
                .block_manager
                .should_skip_indexing(current_u64, &self.config.indexer_version, do_force)
                .await?
            {
                current_u64 += 1;
                continue;
            }

            self.event_handler.on_block_processing(current_u64).await;

            // Set block as processing.
            self.block_manager
                .set_block_info(
                    current_u64,
                    &self.config.indexer_version,
                    &self.config.indexer_identifier,
                    BlockIndexingStatus::Processing,
                )
                .await?;

            let block_ts = self.client.block_time(BlockId::Number(current_u64)).await?;

            let blocks_events = self
                .client
                .fetch_events(
                    BlockId::Number(current_u64),
                    BlockId::Number(current_u64),
                    self.event_manager.keys_selector(),
                )
                .await?;

            let total_events_count: usize = blocks_events.values().map(|events| events.len()).sum();
            info!(
                "✨ Processing block {}. Total Events Count: {}",
                current_u64, total_events_count
            );

            for (_, events) in blocks_events {
                self.process_events(events, current_u64, block_ts).await?;
            }

            self.block_manager
                .set_block_info(
                    current_u64,
                    &self.config.indexer_version,
                    &self.config.indexer_identifier,
                    BlockIndexingStatus::Terminated,
                )
                .await?;
            self.event_handler
                .on_terminated(current_u64, (current_u64 as f64 / to_u64 as f64) * 100.0)
                .await;

            current_u64 += 1;
        }

        Ok(())
    }

    /// Inner function to process events.
    async fn process_events(
        &self,
        events: Vec<EmittedEvent>,
        block_number: u64,
        block_timestamp: u64,
    ) -> IndexerResult<()> {
        for e in events {
            let contract_address = e.from_address;

            let contract_type = match self
                .collection_manager
                .write()
                .await
                .identify_contract(contract_address, block_number)
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    log::error!(
                        "Error while identifying contract {}: {:?}",
                        contract_address,
                        e
                    );
                    continue;
                }
            };

            if contract_type == ContractType::Other {
                continue;
            }

            let token_event = match self
                .event_manager
                .format_and_register_event(&e, contract_type, block_timestamp)
                .await
            {
                Ok(te) => te,
                Err(err) => {
                    log::error!("Error while registering event {:?}\n{:?}", err, e);
                    continue;
                }
            };

            match self
                .token_manager
                .format_and_register_token(&token_event)
                .await
            {
                Ok(()) => (),
                Err(err) => {
                    log::error!("Can't format token {:?}\ntevent: {:?}", err, token_event);
                    continue;
                }
            }
        }

        Ok(())
    }
}

fn init_tracing() {
    // Initialize the LogTracer to convert `log` records to `tracing` events
    tracing_log::LogTracer::init().expect("Setting log tracer failed.");

    // Create the layers
    let env_filter = EnvFilter::from_default_env();
    let fmt_layer = fmt::layer();

    // Combine layers and set as global default
    let subscriber = Registry::default().with(env_filter).with(fmt_layer);

    tracing::subscriber::set_global_default(subscriber)
        .expect("Setting default subscriber failed.");

    let main_span = span!(Level::TRACE, "main");
    let _main_guard = main_span.enter();
}
