mod digest;
mod sections;
mod state_changes;
mod tx_validation;
mod utxo;
mod voting;

use ergo_chain_types::{ADDigest, Header};

pub use digest::DigestValidator;
pub use sections::{
    ExtensionField, ParsedAdProofs, ParsedBlockTransactions, ParsedExtension, parse_block_transactions,
    parse_extension, serialize_ad_proofs, serialize_block_transactions, serialize_extension,
};
pub use state_changes::{StateChanges, compute_state_changes, transactions_to_summaries};
pub use tx_validation::{
    build_state_context, deserialize_box, validate_single_transaction,
};
pub use utxo::UtxoValidator;
pub use voting::{pack_parameters, parse_parameters_from_extension};

// Re-export types needed by mempool callers
pub use ergo_lib::chain::ergo_state_context::ErgoStateContext;
pub use ergo_lib::chain::parameters::Parameters;
pub use ergo_lib::chain::transaction::Transaction;
pub use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

/// Outcome of a successful block validation.
///
/// Carries information the caller needs to advance chain state after the
/// validator returns Ok. Currently only used for epoch-boundary parameter
/// propagation; future fields can extend this struct without breaking the
/// trait signature.
#[derive(Debug, Clone, Default)]
pub struct ValidationOutcome {
    /// `Some(parsed)` if this was an epoch-boundary block AND the parsed
    /// parameters from its extension matched `expected_boundary_params`.
    /// The caller MUST pass these to `chain.apply_epoch_boundary_parameters()`
    /// after persisting the block, before validating the next block.
    pub epoch_boundary_params: Option<Parameters>,
}

/// Validates block sections against the current UTXO state.
///
/// Two implementations: DigestValidator (AD proof based, no persistent UTXO set)
/// and UtxoValidator (persistent AVL+ tree, current Phase 4b).
///
/// Validators are stateless w.r.t. blockchain parameters — the caller passes
/// `active_params` (from `chain.active_parameters()`) on every call. At epoch
/// boundaries, the caller also passes `expected_boundary_params` (from
/// `chain.compute_expected_parameters(height)`) which the validator compares
/// against the parameters parsed from the block's extension. On match, the
/// returned `ValidationOutcome::epoch_boundary_params` carries the parsed
/// parameters for the caller to apply via `chain.apply_epoch_boundary_parameters`.
pub trait BlockValidator {
    /// Validate a block's sections against the current state.
    ///
    /// `header.height` must equal `self.validated_height() + 1`.
    /// `ad_proofs` is required for digest mode, None for UTXO mode.
    /// `preceding_headers` contains up to 10 headers before this block (newest first).
    /// `active_params` is the current chain parameters used for transaction cost bounds.
    /// `expected_boundary_params` is `Some` iff `header.height` is an epoch boundary;
    /// when present, the validator parses the extension's parameters and verifies
    /// they match. On mismatch, returns `Err(ValidationError::ParameterMismatch)`.
    #[allow(clippy::too_many_arguments)]
    fn validate_block(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
        active_params: &Parameters,
        expected_boundary_params: Option<&Parameters>,
    ) -> Result<ValidationOutcome, ValidationError>;

    /// Height of the last validated block. 0 = genesis state set but no blocks applied.
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state after reorg.
    fn reset_to(&mut self, height: u32, digest: ADDigest);

    /// Compute AD proofs and new state root for a set of transactions
    /// without modifying persistent state. Returns None for digest-mode
    /// validators (mining requires UTXO mode).
    fn proofs_for_transactions(
        &self,
        txs: &[Transaction],
    ) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>> {
        let _ = txs;
        None
    }

    /// Current emission box ID in the UTXO set. None if digest mode or
    /// all ERG emitted. Updated after each block validation.
    fn emission_box_id(&self) -> Option<[u8; 32]> {
        None
    }
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

    #[error("epoch-boundary parameter mismatch at height {height}")]
    ParameterMismatch {
        height: u32,
        expected: Box<Parameters>,
        actual: Box<Parameters>,
    },
}
