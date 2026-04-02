use enr_chain::{Header, SyncInfo};
use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;

/// How the sync machine queries persistent storage.
///
/// Implemented by the main crate wrapping `RedbModifierStore`.
pub trait SyncStore {
    /// Check whether a modifier exists in the store.
    fn has_modifier(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> impl std::future::Future<Output = bool> + Send;

    /// Register the height for a modifier ID before its data arrives.
    /// The pipeline stores section bytes without knowing the height;
    /// the sync machine knows the height at queue time.
    fn put_height(
        &self,
        type_id: u8,
        id: &[u8; 32],
        height: u32,
    ) -> impl std::future::Future<Output = ()> + Send;
}

/// How the sync machine sends messages and observes the network.
///
/// Implemented by the main crate wrapping `P2pNode`.
pub trait SyncTransport {
    /// Send a protocol message to a specific peer.
    fn send_to(
        &self,
        peer: PeerId,
        message: ProtocolMessage,
    ) -> impl std::future::Future<Output = Result<(), Box<dyn std::error::Error + Send>>> + Send;

    /// Currently connected outbound peer IDs.
    fn outbound_peers(&self) -> impl std::future::Future<Output = Vec<PeerId>> + Send;

    /// Receive the next protocol event. Returns None if the event stream ends.
    fn next_event(
        &mut self,
    ) -> impl std::future::Future<Output = Option<ProtocolEvent>> + Send;
}

/// How the sync machine queries and updates chain state.
///
/// Implemented by the main crate wrapping `Arc<Mutex<HeaderChain>>`.
pub trait SyncChain {
    /// Height of the validated chain tip, or 0 if empty.
    fn chain_height(&self) -> impl std::future::Future<Output = u32> + Send;

    /// Build a V2 SyncInfo body from current chain state.
    fn build_sync_info(&self) -> impl std::future::Future<Output = Vec<u8>> + Send;

    /// Return the header at a given height, if it exists in the chain.
    fn header_at(&self, height: u32) -> impl std::future::Future<Output = Option<Header>> + Send;

    /// Parse an incoming SyncInfo message body.
    fn parse_sync_info(&self, body: &[u8]) -> Result<SyncInfo, enr_chain::ChainError>;

    /// Peek at the tip headers to determine peer's chain status.
    /// Returns the heights of headers in the sync info, if V2.
    fn sync_info_heights(info: &SyncInfo) -> Vec<u32> {
        match info {
            SyncInfo::V2 { headers } => headers.iter().map(|h| h.height).collect(),
            SyncInfo::V1 { .. } => Vec::new(),
        }
    }
}
