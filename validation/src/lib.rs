mod digest;
mod sections;
mod state_changes;
mod tx_validation;
mod utxo;

use ergo_chain_types::{ADDigest, Header};

pub use digest::DigestValidator;
pub use sections::{ParsedAdProofs, ParsedBlockTransactions, ParsedExtension};
pub use state_changes::StateChanges;
pub use tx_validation::{
    build_state_context, deserialize_box, validate_single_transaction,
};
pub use utxo::UtxoValidator;

// Re-export types needed by mempool callers
pub use ergo_lib::chain::ergo_state_context::ErgoStateContext;
pub use ergo_lib::chain::parameters::Parameters;
pub use ergo_lib::chain::transaction::Transaction;
pub use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

/// Validates block sections against the current UTXO state.
///
/// Two implementations: DigestValidator (AD proof based, no persistent UTXO set)
/// and UtxoValidator (persistent AVL+ tree, future Phase 4b).
pub trait BlockValidator {
    /// Validate a block's sections against the current state.
    ///
    /// `header.height` must equal `self.validated_height() + 1`.
    /// `ad_proofs` is required for digest mode, None for UTXO mode.
    /// `preceding_headers` contains up to 10 headers before this block (newest first).
    fn validate_block(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
    ) -> Result<(), ValidationError>;

    /// Height of the last validated block. 0 = genesis state set but no blocks applied.
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state after reorg.
    fn reset_to(&mut self, height: u32, digest: ADDigest);
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("section parse failed (type {section_type}): {reason}")]
    SectionParse { section_type: u8, reason: String },

    #[error("header ID mismatch in section type {section_type}")]
    HeaderIdMismatch {
        section_type: u8,
        expected: [u8; 32],
        got: [u8; 32],
    },

    #[error("AD proofs digest mismatch")]
    ProofDigestMismatch { expected: [u8; 32], got: [u8; 32] },

    #[error("state root mismatch after AD proof verification")]
    StateRootMismatch { expected: Vec<u8>, got: Vec<u8> },

    #[error("AD proof verification failed: {0}")]
    ProofVerificationFailed(String),

    #[error("intra-block double spend: box {0}")]
    IntraBlockDoubleSpend(String),

    #[error("transaction {index} invalid: {reason}")]
    TransactionInvalid { index: usize, reason: String },

    #[error("AD proofs required but not provided")]
    MissingProof,

    #[error("unexpected block height: expected {expected}, got {got}")]
    HeightMismatch { expected: u32, got: u32 },

    #[error("UTXO state operation failed: {0}")]
    StateOperationFailed(String),
}
