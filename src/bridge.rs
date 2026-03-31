//! Bridge implementations: connect ergo-sync traits to concrete P2P and chain types.

use std::sync::Arc;

use enr_chain::{ChainError, HeaderChain, SyncInfo};
use enr_p2p::node::P2pNode;
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use ergo_sync::{SyncChain, SyncTransport};
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
}
