mod digest;
mod prover_memory;
mod sections;
mod state_changes;
#[cfg(test)]
mod test_support;
mod tx_validation;
mod utxo;
mod voting;

use std::collections::HashMap;

use ergo_chain_types::{ADDigest, Header};

pub use digest::DigestValidator;
pub use prover_memory::ProverMemoryEstimate;
pub use sections::{
    parse_block_transactions, parse_extension, serialize_ad_proofs, serialize_block_transactions,
    serialize_extension, ExtensionField, ParsedAdProofs, ParsedBlockTransactions, ParsedExtension,
};
pub use state_changes::{
    compute_state_changes, transactions_to_summaries, Insertion, StateChanges,
};
pub use tx_validation::{
    build_state_context, build_upcoming_state_context, deserialize_box, evaluate_scripts,
    validate_single_transaction,
};
pub use utxo::{proofs_from_storage, EmissionSource, UtxoValidator};
pub use voting::{pack_parameters, parse_parameters_from_extension};

// Re-export types needed by mempool callers
pub use ergo_lib::chain::ergo_state_context::ErgoStateContext;
pub use ergo_lib::chain::parameters::{Parameter, Parameters};
pub use ergo_lib::chain::transaction::Transaction;
pub use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

/// Outcome of a successful state application.
///
/// Carries nothing about script evaluation, because there is nothing to
/// carry: `apply_state` evaluates the block's scripts itself and an `Ok`
/// already means they passed (facts/validation.md, "Script evaluation").
#[derive(Debug)]
pub struct ApplyStateOutcome {
    /// `Some(parsed)` if this was an epoch-boundary block AND the parsed
    /// parameters from its extension matched `expected_boundary_params`.
    /// The caller MUST pass these to `chain.apply_epoch_boundary_parameters()`
    /// after persisting the block, before validating the next block.
    pub epoch_boundary_params: Option<Parameters>,
    /// `Some(bytes)` if this was an epoch-boundary block. The raw
    /// `ErgoValidationSettingsUpdate` payload from extension key
    /// `[0x00, 124]` (empty Vec if the field is absent). The caller
    /// passes this to `chain.apply_epoch_boundary_parameters` alongside
    /// the parameters so both advance atomically.
    pub epoch_boundary_proposed_update: Option<Vec<u8>>,
}

/// Everything needed to verify a block's transaction spending proofs.
///
/// Built inside `apply_state` and consumed by [`evaluate_scripts`] a few
/// lines later — it never leaves the stack frame that built it. That is why
/// it is a plain bundle of public fields: no constructor to route through, no
/// self-weighing, and no `Send` requirement, all of which existed only to
/// feed a queue that no longer exists.
///
/// **Renamed from `DeferredEval` in v0.8.0.** The old name described a
/// deferral that does not happen.
#[derive(Debug)]
pub struct ScriptEvalInputs {
    /// Block height (for error reporting).
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
/// The validator evaluates the block's scripts itself, before persisting.
/// There is no mode and nothing the caller is left owing — see
/// [`apply_state`](BlockValidator::apply_state).
pub trait BlockValidator {
    /// Apply state transition: parse sections, compute state changes,
    /// apply AVL operations, verify digest, evaluate scripts, persist —
    /// in that order.
    ///
    /// After Ok, state has advanced to this block's height AND the block's
    /// scripts have been evaluated and passed. `Ok` means exactly that;
    /// there is nothing left owed. Heights at or below the validator's
    /// `checkpoint_height` skip evaluation entirely — the state-root check
    /// alone is the guarantee there.
    ///
    /// On Err nothing moved: `validated_height()`, `current_digest()`, and
    /// the underlying prover are exactly as they were on entry, so the
    /// caller may retry the block or move on to another candidate.
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
        expected_proposed_update: Option<&[u8]>,
    ) -> Result<ApplyStateOutcome, ValidationError>;

    /// Height of the last validated block. 0 = genesis state set but no blocks applied.
    fn validated_height(&self) -> u32;

    /// Current state root digest (33 bytes).
    fn current_digest(&self) -> &ADDigest;

    /// Reset to a previous state after a reorg.
    ///
    /// On Err the underlying state rollback FAILED and the validator's
    /// observable state is UNCHANGED: `validated_height()`,
    /// `current_digest()`, and the prover are exactly as before the call.
    /// The caller must not advance its own bookkeeping (watermarks, caches)
    /// onto the un-rolled state — it decides recovery.
    fn reset_to(&mut self, height: u32, digest: ADDigest) -> Result<(), ValidationError>;

    /// Does this validator own persistent state? `Some` hands out its storage
    /// lifecycle; `None` means it owns none (digest mode).
    ///
    /// REQUIRED — no default body. `sync/` is generic over
    /// `V: BlockValidator` (`HeaderSync<T, C, S, V>`) and never names main's
    /// `Validator`, so this is the only route by which a generic caller can
    /// reach [`StatePersistence`]. [`MiningState`] needs no such accessor
    /// because its only consumer is `main`, which does name the type.
    ///
    /// This method answers a capability question; it does NOT perform work.
    /// That is what separates it from the defaulted no-ops it replaces — a
    /// `None` return is a truthful answer, whereas `Ok(())` from a defaulted
    /// `flush` was a claim that work happened.
    fn state_persistence(&self) -> Option<&dyn StatePersistence>;
}

/// Storage lifecycle. Implemented by [`UtxoValidator`] only — a validator that
/// owns no persistent state does not implement it, and the caller handles that
/// case explicitly rather than being handed a successful no-op.
///
/// ⚠ **The absence of an impl is the mode signal.** [`DigestValidator`] must
/// never gain a no-op impl of this trait: a defaulted `resize_cache` on
/// `BlockValidator` is exactly what let the enum wrapper in `src/main.rs`
/// silently drop the at-tip cache resize for the life of the feature, while
/// logging success (facts/validation.md).
pub trait StatePersistence {
    /// Force a durable commit (fsync) of all outstanding storage writes.
    /// Called at sweep flush points (bounds crash data loss) and on graceful
    /// shutdown.
    ///
    /// Postconditions on Ok: every write issued before this call is durable.
    /// Postconditions on Err: durability is UNKNOWN. The caller must not
    /// advance any watermark that assumes persistence — see facts/sync.md
    /// § "Flush ordering".
    fn flush(&self) -> Result<(), ValidationError>;

    /// Resize the storage read cache at runtime (e.g. on reaching the tip).
    ///
    /// ⚠ Read cache only. `stateCacheBytes` covers read + write, so a 64 MB
    /// resize gives roughly a 128 MB envelope, not 64.
    fn resize_cache(&self, cache_bytes: usize) -> Result<(), ValidationError>;

    /// Resident bytes held by the AVL prover, best-effort.
    /// `None` when a figure cannot be computed — never a fabricated zero.
    ///
    /// It belongs on this trait because the prover *is* the state this trait
    /// already governs, and because [`DigestValidator`] does not implement the
    /// trait at all: digest mode therefore reports absent, which is correct —
    /// it has no prover.
    ///
    /// REQUIRED — no default body, matching the cache accessors. A default
    /// would let an implementor silently return a wrong value, which is
    /// structurally how `AVG_HEADER_BYTES` survived four months of reporting
    /// 1.48 GB for a `Vec` that no longer existed (`facts/api.md`).
    ///
    /// ⚠ **O(resident nodes).** The figure is walked, not multiplied, so the
    /// call costs a full traversal of the materialised tree — on a synced
    /// mainnet node, the same order as applying a block. Sample it on a flush
    /// cadence or every N blocks; not per block, and never from an HTTP
    /// handler.
    fn prover_memory_estimate(&self) -> Option<ProverMemoryEstimate>;
}

/// Mining support. Implemented by [`UtxoValidator`] only — candidate assembly
/// requires a live UTXO set, so digest mode does not implement it and the
/// caller skips mining rather than receiving a `None` that means "wrong mode".
pub trait MiningState {
    /// Compute AD proofs and the resulting state root for a set of
    /// transactions WITHOUT modifying persistent state.
    ///
    /// No outer `Option`: "wrong mode" is expressed by not implementing the
    /// trait, so every layer of this return type now carries exactly one
    /// meaning. An `Err` is a real failure to compute the proof.
    fn proofs_for_transactions(
        &self,
        txs: &[Transaction],
    ) -> Result<(Vec<u8>, ADDigest), ValidationError>;

    /// Current emission box ID in the UTXO set, updated after each block.
    ///
    /// `None` means **all ERG has been emitted** — nothing else. It no longer
    /// doubles as the digest-mode signal.
    ///
    /// ⚠ **Valid immediately after construction, not only after the first
    /// applied block.** [`UtxoValidator::new`] recovers it from a required
    /// [`EmissionSource`]; see facts/validation.md, "Recovering
    /// `emission_box_id` on resume".
    fn emission_box_id(&self) -> Option<[u8; 32]>;
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

    #[error("block cost {cost} exceeds maxBlockCost {max_cost}")]
    BlockCostExceeded { cost: u64, max_cost: u64 },

    #[error("block version mismatch: parameters blockVersion {expected} != header.version {got}")]
    BlockVersionMismatch { expected: i32, got: u8 },

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

    #[error("epoch-boundary proposed-update mismatch at height {height}")]
    ProposedUpdateMismatch {
        height: u32,
        expected: Vec<u8>,
        actual: Vec<u8>,
    },
}
