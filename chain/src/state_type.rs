/// Node state management mode — determines which block sections are required
/// and how state transitions are validated.
///
/// Mirrors JVM's `StateType` enum (`utxo` vs `digest`). The `Light` variant
/// has no direct JVM analog — JVM expresses light-client mode as the
/// orthogonal `nipopowBootstrap` flag layered on top of `Digest`. We collapse
/// the two-flag combination into a single state-type variant because the
/// shape of work in light mode (no block bodies, no validator) is sufficiently
/// different that gating it with a boolean on `Digest` would require parallel
/// "is light?" checks throughout sync, validator wiring, and section download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateType {
    /// Maintain the full UTXO set. Validate transactions by looking up inputs
    /// directly. Does not need AD proofs — state transitions are verified
    /// against the UTXO set itself.
    Utxo,
    /// Maintain only the AVL+ tree root hash (state digest). Validate state
    /// transitions using authenticated dictionary proofs (AD proofs) provided
    /// in each block. Requires downloading AD proofs from peers.
    Digest,
    /// NiPoPoW light-client mode. Downloads NO block bodies. Bootstraps the
    /// header chain from a verified NiPoPoW proof's suffix and follows the
    /// tip thereafter. No transaction validation runs in this mode.
    Light,
}

impl StateType {
    /// Whether this mode requires AD proofs for state validation.
    ///
    /// Mirrors JVM's `stateType.requireProofs`:
    /// - UTXO mode validates against the UTXO set directly → no proofs needed
    /// - Digest mode validates against the state root hash → proofs required
    /// - Light mode downloads no block bodies at all — proofs are moot.
    pub fn requires_proofs(&self) -> bool {
        matches!(self, StateType::Digest)
    }

    /// Whether this mode downloads block bodies (transactions, AD proofs,
    /// extension) at all.
    ///
    /// Returns `true` for `Utxo` and `Digest`, `false` for `Light`. Used by
    /// sync to gate the entire block-section download phase. Light mode skips
    /// section queue construction, the watermark scanner, and the block
    /// validator wiring.
    pub fn downloads_block_bodies(&self) -> bool {
        !matches!(self, StateType::Light)
    }
}
