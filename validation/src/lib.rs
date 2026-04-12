mod digest;
mod sections;
mod state_changes;
mod tx_validation;
mod utxo;
mod voting;

use std::collections::HashMap;

use ergo_chain_types::{ADDigest, Header};

pub use digest::DigestValidator;
pub use sections::{
    ExtensionField, ParsedAdProofs, ParsedBlockTransactions, ParsedExtension, parse_block_transactions,
    parse_extension, serialize_ad_proofs, serialize_block_transactions, serialize_extension,
};
pub use state_changes::{StateChanges, compute_state_changes, transactions_to_summaries};
pub use tx_validation::{
    build_state_context, deserialize_box, evaluate_scripts, validate_single_transaction,
};
pub use utxo::UtxoValidator;
pub use voting::{pack_parameters, parse_parameters_from_extension};

// Re-export types needed by mempool callers
pub use ergo_lib::chain::ergo_state_context::ErgoStateContext;
pub use ergo_lib::chain::parameters::Parameters;
pub use ergo_lib::chain::transaction::Transaction;
pub use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

/// Outcome of a successful state application.
#[derive(Debug)]
pub struct ApplyStateOutcome {
    /// `Some(parsed)` if this was an epoch-boundary block AND the parsed
    /// parameters from its extension matched `expected_boundary_params`.
    /// The caller MUST pass these to `chain.apply_epoch_boundary_parameters()`
    /// after persisting the block, before validating the next block.
    pub epoch_boundary_params: Option<Parameters>,
    /// `Some` if script evaluation is needed (height > checkpoint).
    /// The caller should pass this to `evaluate_scripts()` — either
    /// inline or on a background thread.
    pub deferred_eval: Option<DeferredEval>,
}

/// Everything needed to verify transaction spending proofs.
/// Owned, `Send` — can move to any thread for background evaluation.
#[derive(Debug)]
pub struct DeferredEval {
    /// Block height (for error reporting and result tracking).
    pub height: u32,
    /// Parsed transactions from the block.
    pub transactions: Vec<Transaction>,
    /// Input/data-input boxes extracted from state (keyed by box ID).
    pub proof_boxes: HashMap<[u8; 32], ErgoBox>,
    /// Block header.
    pub header: Header,
    /// Up to 10 preceding headers (newest first).
    pub preceding_headers: Vec<Header>,
    /// Active chain parameters.
    pub parameters: Parameters,
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
/// returned `ApplyStateOutcome::epoch_boundary_params` carries the parsed
/// parameters for the caller to apply via `chain.apply_epoch_boundary_parameters`.
///
/// Script evaluation is NOT performed by `apply_state`. Instead, the caller
/// receives a `DeferredEval` and runs `evaluate_scripts()` — either inline
/// or on a background thread.
pub trait BlockValidator {
    /// Apply state transition: parse sections, compute state changes,
    /// apply AVL operations, verify digest, persist.
    ///
    /// After Ok, state has advanced to this block's height.
    /// Script evaluation is deferred — see `ApplyStateOutcome::deferred_eval`.
    #[allow(clippy::too_many_arguments)]
    fn apply_state(
        &mut self,
        header: &Header,
        block_txs: &[u8],
        ad_proofs: Option<&[u8]>,
        extension: &[u8],
        preceding_headers: &[Header],
        active_params: &Parameters,
        expected_boundary_params: Option<&Parameters>,
    ) -> Result<ApplyStateOutcome, ValidationError>;

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
