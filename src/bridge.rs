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
        self.chain.lock().await.header_at(height)
    }

    async fn header_state_root(&self, height: u32) -> Option<[u8; 33]> {
        let chain = self.chain.lock().await;
        let header = chain.header_at(height)?;
        Some(header.state_root.0)
    }

    async fn active_parameters(&self) -> ergo_validation::Parameters {
        self.chain.lock().await.active_parameters().clone()
    }

    async fn is_epoch_boundary(&self, height: u32) -> bool {
        self.chain.lock().await.is_epoch_boundary(height)
    }

    async fn compute_expected_parameters(
        &self,
        epoch_boundary_height: u32,
    ) -> Result<ergo_validation::Parameters, ChainError> {
        self.chain
            .lock()
            .await
            .compute_expected_parameters(epoch_boundary_height)
    }

    async fn apply_epoch_boundary_parameters(&self, params: ergo_validation::Parameters) {
        self.chain
            .lock()
            .await
            .apply_epoch_boundary_parameters(params);
    }

    async fn verify_nipopow_envelope(
        &self,
        envelope_body: &[u8],
    ) -> Result<Vec<Header>, ChainError> {
        // Strip the P2P code-91 envelope (length-prefixed inner bytes + future
        // pad). The wire codec lives in the main crate's `nipopow_serve` so
        // sync stays codec-free.
        let inner = crate::nipopow_serve::parse_nipopow_proof(envelope_body)
            .map_err(|e| ChainError::Nipopow(format!("envelope parse: {e}")))?;
        let result = enr_chain::verify_nipopow_proof_bytes(&inner)?;
        Ok(result.headers)
    }

    async fn is_better_nipopow(
        &self,
        this_envelope: &[u8],
        than_envelope: &[u8],
    ) -> Result<bool, ChainError> {
        let inner_a = crate::nipopow_serve::parse_nipopow_proof(this_envelope)
            .map_err(|e| ChainError::Nipopow(format!("envelope parse (a): {e}")))?;
        let inner_b = crate::nipopow_serve::parse_nipopow_proof(than_envelope)
            .map_err(|e| ChainError::Nipopow(format!("envelope parse (b): {e}")))?;
        enr_chain::compare_nipopow_proof_bytes(&inner_a, &inner_b)
    }

    async fn install_nipopow_suffix(
        &self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
    ) -> Result<(), ChainError> {
        self.chain
            .lock()
            .await
            .install_from_nipopow_proof(suffix_head, suffix_tail)
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

/// Reserved type_id for sync metadata (not a real modifier type).
const SYNC_META_TYPE_ID: u8 = 255;
/// Fixed key for script_verified_height metadata.
const SCRIPT_VERIFIED_HEIGHT_KEY: [u8; 32] = {
    let mut k = [0u8; 32];
    k[0] = b's'; k[1] = b'v'; k[2] = b'h'; // "svh" prefix
    k
};

impl SyncStore for SharedStore {
    async fn has_modifier(&self, type_id: u8, id: &[u8; 32]) -> bool {
        let store = self.store.clone();
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

    async fn script_verified_height(&self) -> Option<u32> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            match store.get(SYNC_META_TYPE_ID, &SCRIPT_VERIFIED_HEIGHT_KEY) {
                Ok(Some(bytes)) if bytes.len() == 4 => {
                    Some(u32::from_le_bytes(bytes[..4].try_into().unwrap()))
                }
                _ => None,
            }
        })
        .await
        .unwrap_or(None)
    }

    async fn set_script_verified_height(&self, height: u32) {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = store.put(
                SYNC_META_TYPE_ID,
                &SCRIPT_VERIFIED_HEIGHT_KEY,
                0, // metadata, no meaningful height
                &height.to_le_bytes(),
            ) {
                tracing::warn!(height, "failed to persist script_verified_height: {e}");
            }
        })
        .await
        .ok();
    }
}
