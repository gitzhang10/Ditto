use crate::core::{CoreMessage, RoundNumber};
use crate::crypto::Hash as _;
use crate::crypto::PublicKey;
use crate::error::ConsensusResult;
use crate::messages::{Block, QC};
use crate::network::NetMessage;
use crate::store::Store;
use crate::timer::TimerManager;
use futures::future::FutureExt as _;
use futures::select;
use futures::stream::futures_unordered::FuturesUnordered;
use futures::stream::StreamExt as _;
use log::{debug, error};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::{channel, Receiver, Sender};

#[cfg(test)]
#[path = "tests/synchronizer_tests.rs"]
pub mod synchronizer_tests;

enum SyncMessage {
    SyncParent(Vec<u8>, Block),
    SyncPayload(Vec<u8>, Block, Receiver<()>),
}

pub struct Synchronizer {
    name: PublicKey,
    store: Store,
    inner_channel: Sender<SyncMessage>,
    network_channel: Sender<NetMessage>,
    pending_payloads: HashMap<RoundNumber, Sender<()>>,
}

impl Synchronizer {
    pub async fn new(
        name: PublicKey,
        store: Store,
        network_channel: Sender<NetMessage>,
        core_channel: Sender<CoreMessage>,
        mut timer_manager: TimerManager,
        sync_retry_delay: u64,
    ) -> Self {
        let (tx_inner, mut rx_inner): (_, Receiver<SyncMessage>) = channel(1000);
        let (tx_timer, mut rx_timer) = channel(100);
        timer_manager
            .schedule(sync_retry_delay, "sync".to_string(), tx_timer.clone())
            .await;

        let store_copy = store.clone();
        let network_channel_copy = network_channel.clone();
        tokio::spawn(async move {
            let mut waiting = FuturesUnordered::new();
            let mut pending_parents = HashSet::new();
            loop {
                select! {
                    message = rx_inner.recv().fuse() => {
                        match message {
                            Some(SyncMessage::SyncParent(wait_on, block)) => {
                                if pending_parents.insert(block.digest()) {
                                    let fut = Self::waiter(store_copy.clone(), wait_on, block, None);
                                    waiting.push(fut);
                                }
                            },
                            Some(SyncMessage::SyncPayload(wait_on, block, cancellation_handler)) => {
                                let fut = Self::waiter(store_copy.clone(), wait_on, block, Some(cancellation_handler));
                                waiting.push(fut);
                            },
                            _ => ()
                        }
                    },
                    result = waiting.select_next_some() => {
                        match result {
                            Ok(Some(block)) => {
                                let _ = pending_parents.remove(&block.digest());
                                let message = CoreMessage::LoopBack(block);
                                if let Err(e) = core_channel.send(message).await {
                                    panic!("Failed to send message through core channel: {}", e);
                                }
                            },
                            Ok(None) => (),
                            Err(e) => error!("{}", e)
                        }
                    },
                    notification = rx_timer.recv().fuse() => {
                        if notification.is_some() {
                            // This ensure liveness in case Sync Requests are lost.
                            for digest in &pending_parents {
                                let sync_request = NetMessage::SyncRequest(digest.clone(), name);
                                if let Err(e) = network_channel_copy.send(sync_request).await {
                                    panic!("Failed to send Sync Request to network: {}", e);
                                }
                            }
                            timer_manager
                                .schedule(sync_retry_delay, "sync".to_string(), tx_timer.clone())
                                .await;
                        }
                    }
                }
            }
        });
        Self {
            name,
            store,
            inner_channel: tx_inner,
            network_channel,
            pending_payloads: HashMap::new(),
        }
    }

    async fn waiter(
        mut store: Store,
        wait_on: Vec<u8>,
        deliver: Block,
        cancellation: Option<Receiver<()>>,
    ) -> ConsensusResult<Option<Block>> {
        if let Some(mut cancellation) = cancellation {
            select! {
                result = store.notify_read(wait_on).fuse() => {
                    let _ = result?;
                    Ok(Some(deliver))
                },
                _ = cancellation.recv().fuse() => {
                    Ok(None)
                }
            }
        } else {
            let _ = store.notify_read(wait_on).await?;
            Ok(Some(deliver))
        }
    }

    async fn get_previous_block(&mut self, block: &Block) -> ConsensusResult<Option<Block>> {
        if block.qc == QC::genesis() {
            return Ok(Some(Block::genesis()));
        }
        let parent = block.previous();
        match self.store.read(parent.to_vec()).await? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes)?)),
            None => {
                debug!("Requesting sync for block {:?}", parent);
                let message = NetMessage::SyncRequest(parent.clone(), self.name);
                if let Err(e) = self.network_channel.send(message).await {
                    panic!("Failed to send Sync Request to network: {}", e);
                }
                let message = SyncMessage::SyncParent(parent.to_vec(), block.clone());
                if let Err(e) = self.inner_channel.send(message).await {
                    panic!("Failed to send request to synchronizer: {}", e);
                }
                Ok(None)
            }
        }
    }

    pub async fn get_ancestors(
        &mut self,
        block: &Block,
    ) -> ConsensusResult<Option<(Block, Block, Block)>> {
        let b2 = match self.get_previous_block(block).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        let b1 = self
            .get_previous_block(&b2)
            .await?
            .expect("We should have all ancestors of delivered blocks");
        let b0 = self
            .get_previous_block(&b1)
            .await?
            .expect("We should have all ancestors of delivered blocks");
        Ok(Some((b0, b1, b2)))
    }

    pub async fn register_payload(&mut self, block: &Block) {
        if !self.pending_payloads.contains_key(&block.round) {
            let (tx_cancellation, rx_cancellation) = channel(1);
            let round = block.round;
            self.pending_payloads.insert(round, tx_cancellation);
            let message =
                SyncMessage::SyncPayload(block.payload.clone(), block.clone(), rx_cancellation);
            if let Err(e) = self.inner_channel.send(message).await {
                panic!("Failed to send request to synchronizer: {}", e);
            }
        }
    }

    pub async fn cleanup(&mut self, round: RoundNumber) {
        for (k, v) in &self.pending_payloads {
            if !v.is_closed() && k < &round {
                let _ = v.send(()).await;
            }
        }
        self.pending_payloads.retain(|k, _| k < &round);
    }
}
