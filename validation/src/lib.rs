mod digest;
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
pub use ergo_lib::chain::parameters::{Parameter, Parameters};
pub use ergo_lib::chain::transaction::Transaction;
pub use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

/// When a validator evaluates a block's ErgoScript spending proofs.
///
/// Fixed at construction, for the validator's life. The caller must be
/// configured to match: `Inline` with a caller that waits for eval results
/// freezes the sync frontier (none ever arrive), `Deferred` with a caller that
/// never evaluates advances it over unverified blocks. Both validators take
/// the mode and behave the same way under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptEvalMode {
    /// `apply_state` returns the block's [`DeferredEval`] and the caller
    /// evaluates it, typically on a background pool. Buys sync throughput at
    /// the cost of a crash-consistency window: the block is persisted before
    /// its scripts are checked, so an unclean shutdown leaves
    /// applied-but-unverified state on disk for `sync/` to reconcile at
    /// startup.
    #[default]
    Deferred,
    /// `apply_state` evaluates the scripts itself — after the state-root
    /// check, before persisting — and returns `deferred_eval = None`. `Ok`
    /// therefore *means* the scripts passed, nothing unverified reaches
    /// storage, and there is no gap to reconcile. Costs the block's script
    /// evaluation on the applying thread.
    Inline,
}

/// Outcome of a successful state application.
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
    /// `Some` if the caller still owes this block a script evaluation —
    /// i.e. the validator is in [`ScriptEvalMode::Deferred`] and the height
    /// is above its checkpoint. Pass it to `evaluate_scripts()`, which
    /// returns the block-accumulated transaction cost.
    ///
    /// `None` means nothing is owed, for either of two reasons: the height
    /// is at or below the checkpoint, or the validator is in
    /// [`ScriptEvalMode::Inline`] and already evaluated them — in which case
    /// this `Ok` *is* the verdict that they passed.
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
    /// Estimated retained heap of this struct, in bytes — the input to the
    /// sync layer's bytes-in-flight bound on the deferred-eval queue
    /// (facts/validation.md, facts/sync.md "Eval backpressure"). Per-item
    /// weight spans three orders of magnitude, so a count cap bounds the
    /// wrong quantity; this is what makes the right one boundable.
    ///
    /// An **estimate**, deliberately: derived at construction from serialized
    /// sizes already on hand, never by measuring. It is a budget input, not a
    /// measurement — nothing may assert on its exact value. It is positive for
    /// every block, empty ones included, because a zero would silently
    /// disable the caller's bound.
    ///
    /// **Private on purpose** (read it via [`DeferredEval::approx_heap_bytes`]).
    /// A public field lets any out-of-crate struct literal set it to zero, and
    /// "never zero" would then be enforced by nothing but the hope that nobody
    /// writes one. Private makes such a literal impossible, so [`Self::new`] is
    /// the only way in and the estimator cannot be bypassed.
    approx_heap_bytes: usize,
}

// ── approx_heap_bytes accounting ────────────────────────────────────────────
//
// One estimator, called from one constructor, used by both validators. If
// digest mode and UTXO mode weighed a block differently the sync layer's
// bound would behave differently per sync mode for the same chain — hence a
// shared path rather than two copies of the arithmetic.
//
// Everything below is derived from serialized lengths the caller already
// holds. Nothing here serializes, hashes, or walks box contents.

/// Inflation applied to the serialized transaction section.
///
/// Parsing a transaction does not merely decode its bytes, it multiplies
/// them. `ergo_lib::Transaction` materializes every output *twice* — once as
/// the `ErgoBoxCandidate` it parsed and again as the `ErgoBox` carrying
/// txId/index — which alone is 304 + 392 bytes of inline struct per output
/// before any payload, against outputs that are often ~100 bytes on the wire.
/// Each copy holds a decoded `ErgoTree` whose constants cost 128 bytes of
/// inline `Constant` apiece however few bytes they occupied serialized, and
/// inputs keep their proof bytes verbatim on top. 6x sits in the middle of
/// the range that shape produces for ordinary transactions.
const TX_SECTION_HEAP_INFLATION: usize = 6;

/// Inflation applied to the serialized proof-box bytes.
///
/// Lower than the transaction side because a proof box is materialized once,
/// not twice. A wire-parsed `ErgoBox` retains its exact serialized image
/// (`ErgoBox::serialized_bytes`, kept so `id` survives non-canonical
/// encodings) — 1x on its own — plus the decoded fields, which for the
/// register- and token-heavy boxes that dominate the byte count run to
/// roughly the same again.
///
/// Known under-estimate direction: an opcode-dense script expands into boxed
/// `Expr` nodes far past 3x. Such boxes are rare and small in serialized
/// terms, and the per-entry term below absorbs part of it; if the field is
/// ever calibrated against live jemalloc numbers, this is the constant that
/// moves.
const PROOF_BOX_HEAP_INFLATION: usize = 3;

/// Fixed cost of a `proof_boxes` entry, independent of the box's serialized
/// size: the 32-byte key, the inline `ErgoBox`, and hashbrown's capacity
/// slack (it grows past a 7/8 load factor, so capacity ≈ len × 8/7).
/// `size_of` rather than a literal so it tracks the sigma-rust pin.
const PROOF_BOX_ENTRY_BYTES: usize = (32 + std::mem::size_of::<ErgoBox>()) * 8 / 7;

/// Per-header allowance: the inline `Header` plus its boxed `EcPoint`s,
/// nonce, and `unparsed_bytes`. Headers are near-fixed-size; this term
/// exists so a coinbase-only block — eleven headers and almost nothing
/// else — is not estimated at nearly nothing.
const HEADER_HEAP_BYTES: usize = std::mem::size_of::<Header>() + 256;

/// Allowance for the `Parameters` table (a small enum → i32 map).
const PARAMETERS_HEAP_BYTES: usize = std::mem::size_of::<Parameters>() + 256;

/// Estimate the retained heap of a `DeferredEval` from sizes available at
/// construction. Pure arithmetic — saturating throughout, so a hostile
/// length can inflate the estimate (which only throttles us) but can never
/// wrap it back down to a small number, and never panics.
fn estimate_heap_bytes(
    tx_section_bytes: usize,
    proof_box_bytes: usize,
    tx_count: usize,
    proof_box_count: usize,
    header_count: usize,
) -> usize {
    let payload = tx_section_bytes
        .saturating_mul(TX_SECTION_HEAP_INFLATION)
        .saturating_add(proof_box_bytes.saturating_mul(PROOF_BOX_HEAP_INFLATION));

    let per_item = tx_count
        .saturating_mul(std::mem::size_of::<Transaction>())
        .saturating_add(proof_box_count.saturating_mul(PROOF_BOX_ENTRY_BYTES))
        .saturating_add(header_count.saturating_mul(HEADER_HEAP_BYTES));

    // The base term is why the result is never zero.
    std::mem::size_of::<DeferredEval>()
        .saturating_add(PARAMETERS_HEAP_BYTES)
        .saturating_add(payload)
        .saturating_add(per_item)
}

impl DeferredEval {
    /// Bundle a validated block's script-evaluation inputs and estimate
    /// `approx_heap_bytes` from serialized sizes already on hand.
    ///
    /// The only way to build one — the private estimate field makes a struct
    /// literal impossible outside this crate — so every construction path,
    /// including a future out-of-crate one (the startup re-evaluation that
    /// rebuilds gap blocks from stored sections), weighs a block the same way.
    ///
    /// `block_txs` is the raw BlockTransactions section the transactions were
    /// parsed from; its 32-byte header-id prefix and VLQ counts are counted
    /// too — a few bytes in the over-estimating direction, which is the side
    /// the contract asks us to err on. `proof_box_bytes` is the summed length
    /// of the serialized boxes the AVL layer returned, accumulated in the
    /// loop that deserializes them (no second walk).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        height: u32,
        transactions: Vec<Transaction>,
        proof_boxes: HashMap<[u8; 32], ErgoBox>,
        header: Header,
        preceding_headers: Vec<Header>,
        parameters: Parameters,
        block_txs: &[u8],
        proof_box_bytes: usize,
    ) -> Self {
        let approx_heap_bytes = estimate_heap_bytes(
            block_txs.len(),
            proof_box_bytes,
            transactions.len(),
            proof_boxes.len(),
            // The block's own header, plus the preceding ones it carries.
            1 + preceding_headers.len(),
        );

        Self {
            height,
            transactions,
            proof_boxes,
            header,
            preceding_headers,
            parameters,
            approx_heap_bytes,
        }
    }

    /// Estimated retained heap of this value, in bytes.
    ///
    /// A budget input for the caller's bytes-in-flight bound, never a
    /// measurement — see the field's documentation for what it does and does
    /// not promise. Always positive.
    pub fn approx_heap_bytes(&self) -> usize {
        self.approx_heap_bytes
    }
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
/// Who evaluates the block's scripts is a construction-time choice — see
/// [`ScriptEvalMode`]. In `Deferred` mode the caller receives a
/// `DeferredEval` and runs `evaluate_scripts()` itself; in `Inline` mode
/// `apply_state` runs it before persisting and an `Ok` already means the
/// scripts passed.
pub trait BlockValidator {
    /// Apply state transition: parse sections, compute state changes,
    /// apply AVL operations, verify digest, evaluate scripts (inline mode
    /// only), persist.
    ///
    /// After Ok, state has advanced to this block's height. Whether the
    /// caller still owes the block a script evaluation is answered by
    /// `ApplyStateOutcome::deferred_eval`.
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

    /// Reset to a previous state after reorg or deferred-eval failure.
    ///
    /// On Err the underlying state rollback FAILED and the validator's
    /// observable state is UNCHANGED: `validated_height()`,
    /// `current_digest()`, and the prover are exactly as before the call.
    /// The caller must not advance its own bookkeeping (watermarks, caches)
    /// onto the un-rolled state — it decides recovery.
    fn reset_to(&mut self, height: u32, digest: ADDigest) -> Result<(), ValidationError>;

    /// Force a durable commit (fsync) of all outstanding storage writes.
    /// Call periodically during long sweeps (bounds crash data loss) and
    /// on graceful shutdown. Digest-mode validators may make this a no-op.
    fn flush(&self) -> Result<(), ValidationError> {
        Ok(())
    }

    /// Resize the storage read cache at runtime (e.g. after initial sync).
    /// Digest-mode validators may make this a no-op.
    fn resize_cache(&self, _cache_bytes: usize) -> Result<(), ValidationError> {
        Ok(())
    }

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

#[cfg(test)]
mod approx_heap_bytes_tests {
    //! Properties of the `approx_heap_bytes` estimate. It is an estimate, so
    //! no test asserts an exact figure — only ordering, positivity,
    //! monotonicity, and the arithmetic staying sane under hostile lengths.

    use super::*;
    use ergo_chain_types::{
        ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes,
    };

    /// (tx_section_bytes, proof_box_bytes, tx_count, proof_box_count, header_count)
    const COINBASE_ONLY: (usize, usize, usize, usize, usize) = (300, 200, 1, 1, 11);
    const DENSE_BLOCK: (usize, usize, usize, usize, usize) = (400_000, 300_000, 300, 900, 11);

    fn estimate(args: (usize, usize, usize, usize, usize)) -> usize {
        estimate_heap_bytes(args.0, args.1, args.2, args.3, args.4)
    }

    fn test_header() -> Header {
        Header {
            version: 3,
            id: BlockId(Digest32::zero()),
            parent_id: BlockId(Digest32::zero()),
            ad_proofs_root: Digest32::zero(),
            state_root: ADDigest::zero(),
            transaction_root: Digest32::zero(),
            timestamp: 1_000_000,
            n_bits: 100_000,
            height: 7,
            extension_root: Digest32::zero(),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: vec![0; 8],
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    #[test]
    fn never_zero_even_for_an_empty_block() {
        // A zero silently disables the caller's bound — the one failure mode
        // that makes this field a liability. The floor holds with every input
        // at zero, so no block shape can reach it.
        assert!(estimate((0, 0, 0, 0, 0)) > 0);
        assert!(estimate(COINBASE_ONLY) > 0);
    }

    #[test]
    fn monotone_in_every_input() {
        let base = COINBASE_ONLY;
        let bumped = [
            (base.0 + 1_000, base.1, base.2, base.3, base.4),
            (base.0, base.1 + 1_000, base.2, base.3, base.4),
            (base.0, base.1, base.2 + 1, base.3, base.4),
            (base.0, base.1, base.2, base.3 + 1, base.4),
            (base.0, base.1, base.2, base.3, base.4 + 1),
        ];
        for (i, args) in bumped.iter().enumerate() {
            assert!(
                estimate(*args) > estimate(base),
                "input {i} grew but the estimate did not"
            );
        }
    }

    #[test]
    fn dense_block_outweighs_coinbase_block() {
        // The ordering is the whole point: the bound has to distinguish the
        // block shapes whose weights differ by three orders of magnitude.
        assert!(estimate(DENSE_BLOCK) > estimate(COINBASE_ONLY) * 100);
    }

    #[test]
    fn transaction_bytes_never_weigh_less_than_box_bytes() {
        // Guards the two byte arguments against being swapped at the call
        // site: the transaction side inflates harder (outputs are
        // materialized twice), so a swap would silently under-count.
        let as_tx = estimate((10_000, 0, 0, 0, 0));
        let as_boxes = estimate((0, 10_000, 0, 0, 0));
        assert!(as_tx >= as_boxes);
    }

    #[test]
    fn saturates_instead_of_wrapping() {
        // Section lengths come off the wire. Wrapping here would hand the
        // caller a tiny number for an enormous block — the bound would open
        // exactly when it must not.
        let huge = estimate((usize::MAX, usize::MAX, usize::MAX, usize::MAX, usize::MAX));
        assert_eq!(huge, usize::MAX);
    }

    #[test]
    fn new_populates_the_field_from_its_inputs() {
        // Wiring check: the constructor derives the estimate rather than
        // defaulting it, and reflects both byte inputs. Read through the
        // accessor — the field is private precisely so that is the only read
        // path a caller has.
        let block_txs = vec![0u8; 4_096];
        let eval = DeferredEval::new(
            7,
            Vec::new(),
            HashMap::new(),
            test_header(),
            vec![test_header(); 10],
            Parameters::default(),
            &block_txs,
            2_048,
        );
        assert_eq!(
            eval.approx_heap_bytes(),
            estimate_heap_bytes(block_txs.len(), 2_048, 0, 0, 11)
        );

        let smaller = DeferredEval::new(
            7,
            Vec::new(),
            HashMap::new(),
            test_header(),
            vec![test_header(); 10],
            Parameters::default(),
            &block_txs[..1_024],
            512,
        );
        assert!(smaller.approx_heap_bytes() < eval.approx_heap_bytes());
    }
}
