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

    /// Retrieve raw modifier bytes from the store.
    fn get_modifier(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send;

    /// Read the persisted script_verified_height. Returns None if not set.
    fn script_verified_height(&self) -> impl std::future::Future<Output = Option<u32>> + Send;

    /// Persist the script_verified_height.
    fn set_script_verified_height(
        &self,
        height: u32,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// Persist the validator (state_applied) height for fast startup.
    fn set_validator_height(
        &self,
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

    /// Return the state_root (33 bytes: root_hash[32] + tree_height[1]) at a given height.
    /// Returns None if the header doesn't exist.
    fn header_state_root(
        &self,
        height: u32,
    ) -> impl std::future::Future<Output = Option<[u8; 33]>> + Send;

    /// Parse an incoming SyncInfo message body.
    fn parse_sync_info(&self, body: &[u8]) -> Result<SyncInfo, enr_chain::ChainError>;

    /// Current blockchain parameters at the chain tip.
    /// Used by the validator to bound transaction costs on every block.
    fn active_parameters(
        &self,
    ) -> impl std::future::Future<Output = ergo_validation::Parameters> + Send;

    /// `true` iff `height` is the start of a new voting epoch.
    fn is_epoch_boundary(
        &self,
        height: u32,
    ) -> impl std::future::Future<Output = bool> + Send;

    /// Compute the parameters that the block at `epoch_boundary_height` MUST emit
    /// in its extension. Used by the validator at epoch-boundary blocks to verify
    /// the block's parameters match expected. Errors if the height is not a valid
    /// epoch boundary or required headers are missing.
    fn compute_expected_parameters(
        &self,
        epoch_boundary_height: u32,
    ) -> impl std::future::Future<Output = Result<ergo_validation::Parameters, enr_chain::ChainError>>
    + Send;

    /// Apply parameters from a successfully validated epoch-boundary block.
    /// Called by the validator's caller AFTER `validate_block` returns Ok with
    /// `epoch_boundary_params: Some(params)`. Mutates the chain's active parameters.
    fn apply_epoch_boundary_parameters(
        &self,
        params: ergo_validation::Parameters,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// Strip a `NipopowProof` (P2P code 91) message envelope and verify the
    /// inner proof bytes via [`enr_chain::verify_nipopow_proof_bytes`].
    /// Returns the extracted header chain in strictly-increasing height order
    /// on success.
    ///
    /// Used by the light-client bootstrap state machine. The bridge wraps
    /// `nipopow_serve::parse_nipopow_proof` (envelope strip) +
    /// `enr_chain::verify_nipopow_proof_bytes` (verify).
    fn verify_nipopow_envelope(
        &self,
        envelope_body: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<Header>, enr_chain::ChainError>> + Send;

    /// Compare two NiPoPoW proofs (raw P2P code-91 envelope bodies) and
    /// return `true` if `this` is better than `that` per KMZ17 §4.3.
    ///
    /// Used by the light-client bootstrap to pick the best proof from
    /// multiple peers. Both envelopes must be parseable (they should have
    /// already passed [`Self::verify_nipopow_envelope`]).
    fn is_better_nipopow(
        &self,
        this_envelope: &[u8],
        than_envelope: &[u8],
    ) -> impl std::future::Future<Output = Result<bool, enr_chain::ChainError>> + Send;

    /// Install a verified NiPoPoW proof's suffix as the chain's starting
    /// point for light-client mode. Wraps
    /// [`enr_chain::HeaderChain::install_from_nipopow_proof`].
    ///
    /// Precondition: chain must be empty. The headers MUST already be
    /// verified via [`Self::verify_nipopow_envelope`] — this function does
    /// NOT re-verify.
    fn install_nipopow_suffix(
        &self,
        suffix_head: Header,
        suffix_tail: Vec<Header>,
    ) -> impl std::future::Future<Output = Result<(), enr_chain::ChainError>> + Send;

    /// Peek at the tip headers to determine peer's chain status.
    /// Returns the heights of headers in the sync info, if V2.
    fn sync_info_heights(info: &SyncInfo) -> Vec<u32> {
        match info {
            SyncInfo::V2 { headers } => headers.iter().map(|h| h.height).collect(),
            SyncInfo::V1 { .. } => Vec::new(),
        }
    }
}
