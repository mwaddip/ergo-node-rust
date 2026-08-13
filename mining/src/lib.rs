pub mod candidate;
pub mod emission;
pub mod extension;
pub mod fee;
pub mod selection;
pub mod solution;
pub mod types;

pub use types::*;

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest, Digest32, Header, Votes};
use ergo_lib::chain::parameters::Parameters;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_validation::{
    build_state_context, validate_single_transaction, ErgoStateContext, ValidationError,
};

use crate::selection::{id_bytes, CostedTx};

/// Errors from mining operations.
#[derive(Debug, thiserror::Error)]
pub enum MiningError {
    #[error("mining not available: {0}")]
    Unavailable(String),

    #[error("no cached candidate")]
    NoCachedCandidate,

    #[error("candidate is stale (tip changed)")]
    StaleCandidate,

    #[error("invalid PoW solution: {0}")]
    InvalidSolution(String),

    #[error("block assembly failed: {0}")]
    AssemblyFailed(String),

    #[error("validation error: {0}")]
    Validation(#[from] ValidationError),

    #[error("emission error: {0}")]
    Emission(String),

    #[error("state root computation failed: {0}")]
    StateRoot(String),
}

/// AD proofs + resulting state digest for a candidate's transactions.
pub type ValidatorProofsResult = Option<Result<(Vec<u8>, ADDigest), ValidationError>>;

/// The product of a candidate generation: the block, the miner-facing work
/// message derived from it, and the mempool bookkeeping it produced.
pub struct GeneratedCandidate {
    /// The assembled candidate — `[emission_tx, selected.., fee_tx?]`.
    pub block: CandidateBlock,
    /// Work message derived from the candidate's header.
    pub work: WorkMessage,
    /// Ids of candidate transactions that failed validation during selection.
    /// Report these to the mempool for eviction (contract Step 3.6). Empty
    /// when no mempool transactions were offered.
    pub invalid_txs: Vec<[u8; 32]>,
}

/// Generate a block candidate from the current chain state.
///
/// Performs contract steps 1–8: emission transaction, mempool selection, fee
/// collection, state root, extension, assembly, work message. **This is the
/// only supported way to produce a `CandidateBlock`** — assembling one
/// field-by-field outside the crate is a contract violation, and it is what
/// left selection and fee collection dead through v0.8.0 development.
///
/// The caller owns the mempool, the chain and the UTXO set; this crate owns
/// the assembly. So the caller passes in:
/// - `candidate_txs` — prioritized mempool transactions with their serialized
///   sizes (`mempool.all_prioritized()`), highest fee-rate first
/// - `parameters` — the ACTIVE protocol parameters, for `max_block_cost` and
///   `max_block_size`. Not `boundary_params`: those are the parameters the
///   epoch boundary will *install*, and the JVM likewise bounds assembly with
///   `stateContext.currentParameters` (`CandidateGenerator.scala:591-598`)
/// - `ancestor_headers` — headers before `parent`, newest first. `parent` is
///   prepended internally to form the ≤10-header window the upcoming block's
///   `ErgoStateContext` needs, so this may be empty near genesis
/// - `utxo_lookup` — resolve a box id against the UTXO set
///
/// `validator_proofs` is a closure that calls
/// `validator.proofs_for_transactions()`. This avoids the mining crate
/// needing to know the validator's concrete type.
#[allow(clippy::too_many_arguments)]
pub fn generate_candidate(
    config: &MinerConfig,
    parent: &Header,
    n_bits: u32,
    parent_interlinks: &[BlockId],
    emission_box: &ErgoBox,
    boundary_params: Option<&Parameters>,
    proposed_update_bytes: &[u8],
    candidate_txs: &[(Transaction, usize)],
    parameters: &Parameters,
    ancestor_headers: &[Header],
    utxo_lookup: &dyn Fn(&[u8; 32]) -> Option<ErgoBox>,
    validator_proofs: &dyn Fn(&[Transaction]) -> ValidatorProofsResult,
) -> Result<GeneratedCandidate, MiningError> {
    let height = parent.height + 1;
    let timestamp = {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::cmp::max(now, parent.timestamp + 1)
    };
    let version = parent.version;

    // 1. Build emission transaction — always first in the block.
    let emission_tx = emission::build_emission_tx(
        emission_box,
        height,
        &config.miner_pk,
        config.reward_delay,
        &config.reemission_rules,
    )?;

    // 2-4. Select mempool transactions and collect their fees. Yields the
    //      full ordered list [emission, selected.., fee?].
    let (transactions, invalid_txs) = if candidate_txs.is_empty() {
        // Empty mempool: emission only. Short-circuited rather than run
        // through selection, so no state context is built and the emission
        // transaction is not re-validated for a budget nothing competes for.
        (vec![emission_tx], Vec::new())
    } else {
        let upcoming = upcoming_header(
            parent, version, n_bits, height, timestamp, config,
        );
        let mut window = Vec::with_capacity(1 + ancestor_headers.len().min(9));
        window.push(parent.clone());
        window.extend(ancestor_headers.iter().take(9).cloned());
        let state_context = build_state_context(&upcoming, &window, parameters);

        select_and_collect_fees(
            config,
            height,
            emission_tx,
            emission_box,
            candidate_txs,
            parameters,
            &state_context,
            utxo_lookup,
        )?
    };

    // 5. Compute state root and AD proofs via validator
    let (ad_proof_bytes, state_root) = validator_proofs(&transactions)
        .ok_or(MiningError::Unavailable("UTXO mode required for mining".into()))?
        .map_err(MiningError::Validation)?;

    // 6. Build extension
    let extension = extension::build_extension(
        parent,
        parent_interlinks,
        boundary_params,
        proposed_update_bytes,
    )?;

    // 7. Assemble candidate
    let mut block = CandidateBlock {
        parent: parent.clone(),
        version,
        n_bits,
        state_root,
        ad_proof_bytes,
        transactions,
        timestamp,
        extension,
        votes: config.votes,
        header_bytes: vec![],
    };

    // 8. Build WorkMessage (also fills header_bytes)
    let (header_bytes, work) = candidate::build_work_message(&block, &config.miner_pk.h)?;
    block.header_bytes = header_bytes;

    Ok(GeneratedCandidate {
        block,
        work,
        invalid_txs,
    })
}

/// The header the candidate's transactions will execute under.
///
/// Only the `PreHeader` fields matter — that is all `build_state_context`
/// takes from it — but the miner public key among them is load-bearing: the
/// fee proposition compares `OUTPUTS(0)`'s script against
/// `expectedMinerOutScriptBytes(delay, MINER_PUBKEY)`, and `MINER_PUBKEY`
/// comes from here. A stub carrying anything but the configured miner key
/// makes every fee transaction fail validation.
fn upcoming_header(
    parent: &Header,
    version: u8,
    n_bits: u32,
    height: u32,
    timestamp: u64,
    config: &MinerConfig,
) -> Header {
    Header {
        version,
        id: BlockId(Digest::from([0u8; 32])),
        parent_id: parent.id,
        ad_proofs_root: Digest32::from([0u8; 32]),
        state_root: parent.state_root,
        transaction_root: Digest32::from([0u8; 32]),
        timestamp,
        n_bits,
        height,
        extension_root: Digest32::from([0u8; 32]),
        autolykos_solution: AutolykosSolution {
            miner_pk: config.miner_pk.h.clone(),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes(config.votes),
        unparsed_bytes: Box::new([]),
    }
}

/// Contract steps 3 and 4: select from the mempool, collect the fees the
/// selection created, and return `[emission, selected.., fee?]`.
///
/// ## How the fee transaction is counted against the block limits
///
/// The fee transaction is a transaction in the block: it has serialized size
/// and it has script cost, and both are charged to `max_block_size` /
/// `max_block_cost` by every validator (ours sums per-transaction costs over
/// the whole block — `validation/src/tx_validation.rs`, `enforce_block_cost`).
/// `select_transactions` accumulates over mempool transactions only and knows
/// nothing about it, so selecting up to `max_block_cost` and *then* appending
/// a fee transaction produces exactly the over-limit block the cost bound
/// exists to prevent. The JVM checks limits over the whole set including the
/// recomputed fee transaction (`correctLimits(blockTxs, ..)`,
/// `CandidateGenerator.scala:901`).
///
/// The arrangement here is measure-then-fit, in two parts:
///
/// 1. **The emission transaction is reserved before selecting.** It is known
///    up front, so it is validated once and passed to `select_transactions`
///    as pre-committed cost and size. Selection then bounds
///    `emission + selected` rather than `selected` alone.
/// 2. **The fee transaction is measured after selecting, and selected
///    transactions are dropped from the tail until the whole set fits.** It
///    cannot be reserved: it does not exist until the selection does, and its
///    inputs *are* the selection's fee outputs. Reserving a guessed allowance
///    would be an estimate; this is a measurement.
///
/// Dropping from the tail is both the cheapest and the safest direction.
/// Candidates arrive highest-fee-rate first, so the tail is the least
/// valuable; and a selected transaction can only ever spend outputs of an
/// *earlier* selected one (`select_transactions` publishes a transaction's
/// outputs only after committing it), so intra-block dependencies point
/// backwards and dropping from the tail can never orphan a transaction that
/// is kept. Each drop strictly shrinks the total — the transaction's own cost
/// and size go, and its fee box leaves the fee transaction — so the loop
/// terminates, at worst at emission-only.
///
/// The common case costs one fee-transaction build and validation. The
/// alternative arrangement — the JVM's, rebuilding and revalidating the fee
/// transaction after every accepted candidate — is exact at every step but
/// pays that per accepted transaction, and candidate assembly now runs every
/// 15 s (TTL regeneration) rather than once a block.
#[allow(clippy::too_many_arguments)]
fn select_and_collect_fees(
    config: &MinerConfig,
    height: u32,
    emission_tx: Transaction,
    emission_box: &ErgoBox,
    candidate_txs: &[(Transaction, usize)],
    parameters: &Parameters,
    state_context: &ErgoStateContext,
    utxo_lookup: &dyn Fn(&[u8; 32]) -> Option<ErgoBox>,
) -> Result<(Vec<Transaction>, Vec<[u8; 32]>), MiningError> {
    // Both limits are i32 in the parameters table. A negative value is
    // unreachable through voting bounds; clamp to 0 rather than sign-extend
    // into "everything fits", matching the validator's own handling.
    let max_block_cost = u64::try_from(parameters.max_block_cost()).unwrap_or(0);
    let max_block_size = usize::try_from(parameters.max_block_size()).unwrap_or(0);

    // Reserve the emission transaction's share of both budgets.
    //
    // If its cost cannot be measured we cannot bound the block, so we add
    // nothing to it: the candidate degrades to emission-only, which is
    // exactly what the node produced before selection was wired in. Failing
    // outright would turn a measurement problem into "mining stops", a
    // failure mode this step must not introduce.
    let emission_cost = match validate_single_transaction(
        &emission_tx,
        vec![emission_box.clone()],
        vec![],
        state_context,
    ) {
        Ok(cost) => cost,
        Err(e) => {
            tracing::warn!(
                height,
                "mining: emission transaction did not validate ({e}); \
                 serving an emission-only candidate"
            );
            return Ok((vec![emission_tx], Vec::new()));
        }
    };
    let emission = CostedTx {
        cost: emission_cost,
        size: serialized_size(&emission_tx)?,
        tx: emission_tx,
    };

    let (mut selected, invalid_txs) = selection::select_transactions(
        candidate_txs,
        std::slice::from_ref(&emission),
        state_context,
        max_block_size,
        max_block_cost,
        utxo_lookup,
    );

    // Bound the ASSEMBLED set — emission + selected + fee — dropping the
    // lowest-priority selected transaction until it fits.
    let fee_tx = loop {
        // The JVM builds the fee transaction over the accumulator including
        // the emission transaction; its outputs never match the fee
        // proposition, so this only matters for staying faithful.
        let mut block_txs: Vec<Transaction> = Vec::with_capacity(1 + selected.len());
        block_txs.push(emission.tx.clone());
        block_txs.extend(selected.iter().map(|c| c.tx.clone()));

        let (fee_tx, fee_cost, fee_size) =
            match measure_fee_tx(&block_txs, height, config, state_context) {
                Ok(Some((tx, cost, size))) => (Some(tx), cost, size),
                Ok(None) => (None, 0, 0),
                Err(reason) => {
                    // Ship the block without collecting fees rather than
                    // without a candidate: the fee boxes simply stay unspent
                    // and the block is still valid. The JVM does the same
                    // ("Fee collecting tx is invalid, not including it").
                    tracing::warn!(height, "mining: fee transaction unusable: {reason}");
                    (None, 0, 0)
                }
            };

        let total_cost = selected
            .iter()
            .fold(emission.cost, |acc, c| acc.saturating_add(c.cost))
            .saturating_add(fee_cost);
        let total_size = selected
            .iter()
            .fold(emission.size, |acc, c| acc.saturating_add(c.size))
            .saturating_add(fee_size);

        if total_cost <= max_block_cost && total_size <= max_block_size {
            break fee_tx;
        }

        if selected.pop().is_none() {
            // Nothing left to shed: the emission transaction alone is over
            // budget. Ship it — the block must carry its coinbase, and this
            // is the same block the node produced before selection existed.
            tracing::warn!(
                height,
                total_cost,
                total_size,
                max_block_cost,
                max_block_size,
                "mining: emission transaction alone exceeds the block limits"
            );
            break None;
        }
    };

    let mut transactions: Vec<Transaction> = Vec::with_capacity(2 + selected.len());
    transactions.push(emission.tx);
    transactions.extend(selected.into_iter().map(|c| c.tx));
    // The fee transaction goes LAST: it spends fee boxes that are outputs of
    // the selected transactions, so it cannot precede them.
    transactions.extend(fee_tx);

    Ok((transactions, invalid_txs))
}

/// A fee transaction with its measured `(cost, size)`, or `None` when there
/// was nothing to collect.
type MeasuredFeeTx = Option<(Transaction, u64, usize)>;

/// Build the fee transaction over `block_txs` and measure what it costs the
/// block. `Ok(None)` means there was nothing to collect — a zero-fee block is
/// a normal outcome, not an error.
///
/// `Err` carries a reason for the caller to log and continue without fees.
fn measure_fee_tx(
    block_txs: &[Transaction],
    height: u32,
    config: &MinerConfig,
    state_context: &ErgoStateContext,
) -> Result<MeasuredFeeTx, String> {
    let Some(fee_tx) = fee::build_fee_tx(block_txs, height, config.reward_delay, &config.miner_pk)
        .map_err(|e| format!("build: {e}"))?
    else {
        return Ok(None);
    };

    // The fee transaction's inputs are fee boxes created by `block_txs`; the
    // JVM resolves them the same way (`newBoxes.find(..)`).
    let mut outputs: HashMap<[u8; 32], ErgoBox> = HashMap::new();
    for tx in block_txs {
        for output in tx.outputs.iter() {
            outputs.insert(id_bytes(output.box_id().as_ref()), output.clone());
        }
    }
    let input_boxes: Option<Vec<ErgoBox>> = fee_tx
        .inputs
        .iter()
        .map(|i| outputs.get(&id_bytes(i.box_id.as_ref())).cloned())
        .collect();
    let input_boxes =
        input_boxes.ok_or_else(|| "fee input box not found among block outputs".to_string())?;

    let cost = validate_single_transaction(&fee_tx, input_boxes, vec![], state_context)
        .map_err(|e| format!("validation: {e}"))?;
    let size = serialized_size(&fee_tx).map_err(|e| format!("{e}"))?;

    Ok(Some((fee_tx, cost, size)))
}

/// Serialized size of a transaction — the unit both the JVM generator and the
/// block-transactions size limit count in.
fn serialized_size(tx: &Transaction) -> Result<usize, MiningError> {
    tx.sigma_serialize_bytes()
        .map(|bytes| bytes.len())
        .map_err(|e| MiningError::AssemblyFailed(format!("transaction serialize: {e}")))
}

/// Stateful candidate manager — two candidate slots plus a solved-block
/// latch, mirroring the JVM CandidateGenerator actor state
/// (`cachedCandidate` / `cachedPreviousCandidate` / `solvedBlock`).
///
/// Keeps one previous candidate so that a GPU miner solving the old
/// candidate while a regeneration occurred can still submit its solution.
/// The solved latch suppresses further solution acceptance between a
/// solution being accepted and the resulting block being applied;
/// `on_block_applied` is the only place candidates are dropped and the
/// latch is cleared (see `../facts/mining.md`, Lifecycle API).
pub struct CandidateGenerator {
    pub config: MinerConfig,
    /// Current candidate. None until generated, or after invalidate().
    cached: RwLock<Option<CachedCandidate>>,
    /// The immediately superseded candidate. A miner still hashing the old
    /// work message can submit its solution against this slot.
    previous: RwLock<Option<CachedCandidate>>,
    /// Solved-block latch: set when a solution is accepted and the block is
    /// handed to the (async) submitter; suppresses further solution
    /// acceptance until block application clears it. JVM: `solvedBlock`.
    solved: RwLock<Option<SolvedLatch>>,
}

/// Identity of the block assembled from an accepted solution, pending
/// application.
struct SolvedLatch {
    /// Header id of the block assembled from the accepted solution.
    header_id: BlockId,
    /// Height of that block.
    height: u32,
}

impl CandidateGenerator {
    pub fn new(config: MinerConfig) -> Self {
        Self {
            config,
            cached: RwLock::new(None),
            previous: RwLock::new(None),
            solved: RwLock::new(None),
        }
    }

    /// Get the cached WorkMessage if still valid, or None if stale/missing.
    pub fn cached_work(&self, current_tip_height: u32) -> Option<WorkMessage> {
        let guard = self.cached.read().ok()?;
        let cached = guard.as_ref()?;
        if cached.tip_height == current_tip_height
            && cached.created.elapsed() < self.config.candidate_ttl
        {
            Some(cached.work.clone())
        } else {
            None
        }
    }

    /// Store a freshly generated candidate. The old candidate (if any)
    /// is preserved as `previous` so stale solutions can still be accepted.
    pub fn cache_candidate(
        &self,
        block: CandidateBlock,
        work: WorkMessage,
        tip_height: u32,
    ) {
        let mut guard = self.cached.write().unwrap();
        // Move current → previous before overwriting
        if let Some(old) = guard.take() {
            if let Ok(mut prev) = self.previous.write() {
                *prev = Some(old);
            }
        }
        *guard = Some(CachedCandidate {
            block,
            work,
            tip_height,
            created: Instant::now(),
        });
    }

    /// Get the current cached CandidateBlock for solution validation.
    pub fn cached_block(&self) -> Option<CandidateBlock> {
        let guard = self.cached.read().ok()?;
        guard.as_ref().map(|c| c.block.clone())
    }

    /// Get the previous CandidateBlock (if any).
    pub fn previous_block(&self) -> Option<CandidateBlock> {
        let guard = self.previous.read().ok()?;
        guard.as_ref().map(|c| c.block.clone())
    }

    /// Mempool-change push invalidation. The current candidate moves to
    /// `previous` so in-flight solutions remain valid. Tip changes are NOT
    /// routed here — they go through `on_block_applied`.
    pub fn invalidate(&self) {
        if let Ok(mut guard) = self.cached.write() {
            if let Some(old) = guard.take() {
                if let Ok(mut prev) = self.previous.write() {
                    *prev = Some(old);
                }
            }
        }
    }

    /// Atomically claim the solved latch IFF it is unset; `false` means a
    /// concurrent solution already claimed it and the caller must reject
    /// (400). This is the authoritative anti-self-competition gate, taken
    /// BEFORE the block is handed to the submitter — API handlers run
    /// concurrently (the JVM actor serializes; axum does not), so a
    /// check-then-set spanning the handler would let two simultaneously
    /// valid solutions both pass. A poisoned lock refuses the claim —
    /// conservative: suppresses a solution rather than risking two.
    pub fn try_mark_solved(&self, header_id: BlockId, height: u32) -> bool {
        self.solved.write().is_ok_and(|mut latch| {
            if latch.is_some() {
                false
            } else {
                *latch = Some(SolvedLatch { header_id, height });
                true
            }
        })
    }

    /// Release a claimed latch. ONLY for the submit-failure path (500/503):
    /// the block never left the node, so keeping the latch would wedge
    /// solution acceptance until the next applied block. Success-path
    /// clearing belongs to `on_block_applied` exclusively.
    pub fn clear_solved(&self) {
        if let Ok(mut latch) = self.solved.write() {
            *latch = None;
        }
    }

    /// True while a solved block awaits application. Cheap early rejection
    /// for handlers; `try_mark_solved` is the gate that counts.
    pub fn solved_pending(&self) -> bool {
        self.solved.read().is_ok_and(|l| l.is_some())
    }

    /// Block-application hook — the lifecycle counterpart of the JVM's
    /// `FullBlockApplied` handler. Must be called for EVERY applied block,
    /// own or peer. Drops each candidate slot that no longer builds on the
    /// new tip (`parent.id != applied_id`) and clears the solved latch once
    /// the chain reaches the latched height — either our block landed or a
    /// competitor superseded it; the latch is stale either way.
    pub fn on_block_applied(&self, applied_id: &BlockId, applied_height: u32) {
        for slot in [&self.cached, &self.previous] {
            if let Ok(mut guard) = slot.write() {
                if guard
                    .as_ref()
                    .is_some_and(|c| c.block.parent.id != *applied_id)
                {
                    *guard = None;
                }
            }
        }
        if let Ok(mut latch) = self.solved.write() {
            if let Some(l) = latch.as_ref() {
                if applied_height >= l.height {
                    tracing::debug!(
                        height = l.height,
                        own = (l.header_id == *applied_id),
                        "mining: solved latch cleared"
                    );
                    *latch = None;
                }
            }
        }
    }
}
