// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Bottom up checkpoint manager

use crate::config::subnet::SubnetConfig;
use crate::config::Subnet;
use crate::manager::{BottomUpCheckpointRelayer, BtcSubnetManager, EthSubnetManager};
use crate::observe::CheckpointSubmitted;
use anyhow::{anyhow, Result};
use futures_util::future::try_join_all;
use fvm_shared::address::Address;
use fvm_shared::clock::ChainEpoch;
use ipc_api::checkpoint::{BottomUpCheckpointBundle, QuorumReachedEvent};
use ipc_api::subnet_id::NetworkType;
use ipc_observability::{emit, serde::HexEncodableBlockHash};
use ipc_wallet::{EthKeyAddress, PersistentKeyStore};
use std::cmp::max;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Tracks the config required for bottom up checkpoint submissions
/// parent/child subnet and checkpoint period.
pub struct CheckpointConfig {
    parent: Subnet,
    child: Subnet,
    period: ChainEpoch,
}

/// Manages the submission of bottom up checkpoint. It checks if the submitter has already
/// submitted in the `last_checkpoint_height`, if not, it will submit the checkpoint at that height.
/// Then it will submit at the next submission height for the new checkpoint.
pub struct BottomUpCheckpointManager {
    metadata: CheckpointConfig,
    parent_handler: Arc<Box<dyn BottomUpCheckpointRelayer>>,
    child_handler: Arc<Box<dyn BottomUpCheckpointRelayer>>,
    /// The number of blocks away from the chain head that is considered final
    finalization_blocks: ChainEpoch,
    submission_semaphore: Arc<Semaphore>,
    keystore: Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
}

// impl BottomUpCheckpointManager {
//     pub async fn new(
//         parent: Subnet,
//         child: Subnet,
//         parent_handler: Arc<Box<dyn BottomUpCheckpointRelayer>>,
//         child_handler: Arc<Box<dyn BottomUpCheckpointRelayer>>,
//         max_parallelism: usize,
//     ) -> Result<Self> {
//         let period = parent_handler
//             .checkpoint_period(&child.id)
//             .await
//             .map_err(|e| anyhow!("cannot get bottom up checkpoint period: {e}"))?;
//         Ok(Self {
//             metadata: CheckpointConfig {
//                 parent,
//                 child,
//                 period,
//             },
//             parent_handler,
//             child_handler,
//             finalization_blocks: 0,
//             submission_semaphore: Arc::new(Semaphore::new(max_parallelism)),
//         })
//     }

// pub fn with_finalization_blocks(mut self, finalization_blocks: ChainEpoch) -> Self {
//     self.finalization_blocks = finalization_blocks;
//     self
// }
// }

impl BottomUpCheckpointManager {
    pub async fn new(
        parent: Subnet,
        child: Subnet,
        max_parallelism: usize,
        keystore: Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
    ) -> Result<Self> {
        tracing::info!("parent: {:?}", parent.config);
        tracing::info!("child: {:?}", child.config);
        let parent_handler: Box<dyn BottomUpCheckpointRelayer> = match &parent.config {
            SubnetConfig::Fevm(_) => Box::new(EthSubnetManager::from_subnet_with_wallet_store(
                &parent,
                Some(keystore.clone()),
            )?),
            SubnetConfig::Btc(_) => Box::new(BtcSubnetManager::new(&parent)?),
        };

        let child_handler: Box<dyn BottomUpCheckpointRelayer> = match &child.config {
            SubnetConfig::Fevm(_) => Box::new(EthSubnetManager::from_subnet_with_wallet_store(
                &child,
                Some(keystore.clone()),
            )?),
            SubnetConfig::Btc(_) => Box::new(BtcSubnetManager::new(&child)?),
        };

        let period = parent_handler
            .checkpoint_period(&child.id)
            .await
            .map_err(|e| anyhow!("cannot get bottom up checkpoint period: {e}"))?;

        Ok(Self {
            metadata: CheckpointConfig {
                parent,
                child,
                period,
            },
            parent_handler: Arc::new(parent_handler),
            child_handler: Arc::new(child_handler),
            finalization_blocks: 0,
            submission_semaphore: Arc::new(Semaphore::new(max_parallelism)),
            keystore,
        })
    }

    pub fn with_finalization_blocks(mut self, finalization_blocks: ChainEpoch) -> Self {
        self.finalization_blocks = finalization_blocks;
        self
    }
}

impl Display for BottomUpCheckpointManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bottom-up relayer, parent: {:}, child: {:}",
            self.metadata.parent.id, self.metadata.child.id
        )
    }
}

impl BottomUpCheckpointManager {
    /// Getter for the parent subnet this checkpoint manager is handling
    pub fn parent_subnet(&self) -> &Subnet {
        &self.metadata.parent
    }

    /// Getter for the target subnet this checkpoint manager is handling
    pub fn child_subnet(&self) -> &Subnet {
        &self.metadata.child
    }

    /// The checkpoint period that the current manager is submitting upon
    pub fn checkpoint_period(&self) -> ChainEpoch {
        self.metadata.period
    }

    /// Run the bottom up checkpoint submission daemon in the foreground
    pub async fn run(self, submitter: Option<Address>, submission_interval: Duration) {
        tracing::info!("launching {self} for {submitter:?}");

        loop {
            if let Err(e) = self.submit_next_epoch(submitter).await {
                tracing::error!("cannot submit checkpoint due to: {e}");
            }
            tokio::time::sleep(submission_interval).await;
        }
    }

    /// Checks if the relayer has already submitted at the next submission epoch, if not it submits it.
    async fn submit_next_epoch(&self, submitter: Option<Address>) -> Result<()> {
        let last_checkpoint_epoch = self
            .parent_handler
            .last_bottom_up_checkpoint_height(&self.metadata.child.id)
            .await
            .map_err(|e| {
                anyhow!("cannot obtain the last bottom up checkpoint height due to: {e:}")
            })?;
        tracing::info!("last submission height: {last_checkpoint_epoch}");

        let current_height = self.child_handler.current_epoch().await?;
        let finalized_height = max(1, current_height - self.finalization_blocks);
        tracing::info!("last submission height: {last_checkpoint_epoch}, current height: {current_height}, finalized_height: {finalized_height}");

        if finalized_height <= last_checkpoint_epoch {
            return Ok(());
        }

        let start = last_checkpoint_epoch + 1;
        tracing::debug!(
            "start querying quorum reached events from : {start} to {finalized_height}"
        );

        let mut count = 0;
        let mut all_submit_tasks = vec![];

        for h in start..=finalized_height {
            let events = self.child_handler.quorum_reached_events(h).await?;
            if events.is_empty() {
                tracing::debug!("no reached events at height : {h}");
                continue;
            }

            tracing::debug!("found reached events at height : {h}");

            for event in events {
                // Note that the event will be emitted later than the checkpoint height.
                // For example, if the checkpoint height is 400 but it's actually created
                // in fendermint at height 403. This means the event.height == 400 which is
                // already committed.
                if event.height <= last_checkpoint_epoch {
                    tracing::debug!("event height already committed: {}", event.height);
                    continue;
                }

                let bundle = self
                    .child_handler
                    .checkpoint_bundle_at(event.height)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "expected checkpoint at height {} but none found",
                            event.height
                        )
                    })?;

                // TODO(themis): get PSBT for this checkpoint -> in the bundle
                // TODO(themis): get PSBT signatures -> in the bundle
                // TODO(themis): potentially update bundle to contain BTC signatures -> done

                log::trace!("bottom up bundle: {bundle:?}");

                // We support parallel checkpoint submission using FIFO order with a limited parallelism (controlled by
                // the size of submission_semaphore).
                // We need to acquire a permit (from a limited permit pool) before submitting a checkpoint.
                // We may wait here until a permit is available.
                let parent_handler_clone = Arc::clone(&self.parent_handler);
                let submission_permit = self
                    .submission_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .unwrap();

                let keystore = self.keystore.clone();
                all_submit_tasks.push(tokio::task::spawn(async move {
                    let height = event.height;
                    let hash = bundle.checkpoint.block_hash.clone();

                    let result = Self::submit_checkpoint(
                        keystore,
                        parent_handler_clone,
                        submitter,
                        bundle,
                        event,
                    )
                    .await
                    .inspect(|_| {
                        emit(CheckpointSubmitted {
                            height,
                            hash: HexEncodableBlockHash(hash),
                        });
                    })
                    .inspect_err(|err| {
                        tracing::error!("Fail to submit checkpoint at height {height}: {err}");
                    });

                    drop(submission_permit);
                    result
                }));

                count += 1;
                tracing::debug!("This round has asynchronously submitted {count} checkpoints",);
            }
        }

        tracing::debug!("Waiting for all submissions to finish");
        // Return error if any of the submit task failed.
        try_join_all(all_submit_tasks).await?;

        Ok(())
    }

    async fn submit_checkpoint(
        keystore: Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
        parent_handler: Arc<Box<dyn BottomUpCheckpointRelayer>>,
        submitter: Option<Address>,
        bundle: BottomUpCheckpointBundle,
        event: QuorumReachedEvent,
    ) -> Result<(), anyhow::Error> {
        // sanity checks and debug logs
        if bundle.checkpoint.subnet_id.parent_network_type() == Some(NetworkType::Btc) {
            if bundle.bitcoin_signatures == None {
                tracing::debug!(
                    "parent subnet is bitcoin but bitcoin signatures were not found for checkpoint at height {}",
                    event.height
                );
            }
        } else {
            if bundle.bitcoin_signatures != None {
                tracing::debug!(
                    "parent subnet is evm but bitcoin signatures were found for checkpoint at height {}",
                    event.height
                );
            }
        };

        let epoch = parent_handler
            .submit_checkpoint(
                keystore,
                &submitter,
                bundle.checkpoint,
                bundle.signatures,
                bundle.signatories,
                bundle.bitcoin_signatures,
            )
            .await
            .map_err(|e| {
                anyhow!(
                    "cannot submit bottom up checkpoint at height {} due to: {e}",
                    event.height
                )
            })?;

        tracing::info!(
            "submitted bottom up checkpoint({}) in parent at height {}",
            event.height,
            epoch
        );
        Ok(())
    }
}
