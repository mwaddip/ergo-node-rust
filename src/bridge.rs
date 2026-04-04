//! Bridge implementations: connect ergo-sync traits to concrete P2P and chain types.

use std::sync::Arc;

use enr_chain::{ChainError, Header, HeaderChain, SyncInfo};
use enr_p2p::node::P2pNode;
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_sync::{SyncChain, SyncStore, SyncTransport};
use tokio::sync::{mpsc, Mutex};

/// Wraps `P2pNode` + event receiver to implement `SyncTransport`.
pub struct P2pTransport {
    node: Arc<P2pNode>,
    events: mpsc::Receiver<ProtocolEvent>,
}

impl P2pTransport {
    pub fn new(node: Arc<P2pNode>, events: mpsc::Receiver<ProtocolEvent>) -> Self {
        Self { node, events }
    }
}

impl SyncTransport for P2pTransport {
    async fn send_to(
        &self,
        peer: PeerId,
        message: ProtocolMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        self.node
            .send_to(peer, message)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }

    async fn outbound_peers(&self) -> Vec<PeerId> {
        self.node.outbound_peers().await
    }

    async fn next_event(&mut self) -> Option<ProtocolEvent> {
        self.events.recv().await
    }
}

/// Wraps `Arc<Mutex<HeaderChain>>` to implement `SyncChain`.
pub struct SharedChain {
    chain: Arc<Mutex<HeaderChain>>,
}

impl SharedChain {
    pub fn new(chain: Arc<Mutex<HeaderChain>>) -> Self {
        Self { chain }
    }
}

impl SyncChain for SharedChain {
    async fn chain_height(&self) -> u32 {
        self.chain.lock().await.height()
    }

    async fn build_sync_info(&self) -> Vec<u8> {
        let chain = self.chain.lock().await;
        enr_chain::build_sync_info(&chain)
    }

    fn parse_sync_info(&self, body: &[u8]) -> Result<SyncInfo, ChainError> {
        enr_chain::parse_sync_info(body)
    }

    async fn header_at(&self, height: u32) -> Option<Header> {
        self.chain.lock().await.header_at(height).cloned()
    }

    async fn header_state_root(&self, height: u32) -> Option<[u8; 33]> {
        let chain = self.chain.lock().await;
        let header = chain.header_at(height)?;
        Some(header.state_root.0)
    }
}

/// Wraps `Arc<RedbModifierStore>` to implement `SyncStore`.
pub struct SharedStore {
    store: Arc<RedbModifierStore>,
}

impl SharedStore {
    pub fn new(store: Arc<RedbModifierStore>) -> Self {
        Self { store }
    }
}

impl SyncStore for SharedStore {
    async fn has_modifier(&self, type_id: u8, id: &[u8; 32]) -> bool {
        let store = self.store.clone();
        let type_id = type_id;
        let id = *id;
        tokio::task::spawn_blocking(move || {
            match store.contains(type_id, &id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("store.contains failed: {e}");
                    false
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    async fn get_modifier(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>> {
        let store = self.store.clone();
        let type_id = type_id;
        let id = *id;
        tokio::task::spawn_blocking(move || match store.get(type_id, &id) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("store.get failed: {e}");
                None
            }
        })
        .await
        .unwrap_or(None)
    }
}
