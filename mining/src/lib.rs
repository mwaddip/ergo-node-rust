pub mod candidate;
pub mod emission;
pub mod extension;
pub mod fee;
pub mod selection;
pub mod solution;
pub mod types;

pub use types::*;

use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ergo_chain_types::{ADDigest, BlockId, Header};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_validation::ValidationError;

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

/// Generate a block candidate from the current chain state.
///
/// Builds the emission transaction, computes the state root via the validator,
/// and assembles the header + WorkMessage for miners. Currently produces
/// empty-mempool candidates (emission tx only). Transaction selection and
/// fee collection are added in later tasks.
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
    boundary_params: Option<&ergo_lib::chain::parameters::Parameters>,
    proposed_update_bytes: &[u8],
    validator_proofs: &dyn Fn(&[Transaction]) -> ValidatorProofsResult,
) -> Result<(CandidateBlock, WorkMessage), MiningError> {
    let height = parent.height + 1;
    let timestamp = {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::cmp::max(now, parent.timestamp + 1)
    };

    // 1. Build emission transaction
    let emission_tx = emission::build_emission_tx(
        emission_box,
        height,
        &config.miner_pk,
        config.reward_delay,
        &config.reemission_rules,
    )?;

    // 2. Transaction list: [emission_tx] (no mempool txs yet, no fee tx)
    let transactions = vec![emission_tx];

    // 3. Compute state root and AD proofs via validator
    let (ad_proof_bytes, state_root) = validator_proofs(&transactions)
        .ok_or(MiningError::Unavailable("UTXO mode required for mining".into()))?
        .map_err(MiningError::Validation)?;

    // 4. Build extension
    let extension = extension::build_extension(
        parent,
        parent_interlinks,
        boundary_params,
        proposed_update_bytes,
    )?;

    // 5. Assemble candidate
    let mut block = CandidateBlock {
        parent: parent.clone(),
        version: parent.version,
        n_bits,
        state_root,
        ad_proof_bytes,
        transactions,
        timestamp,
        extension,
        votes: config.votes,
        header_bytes: vec![],
    };

    // 6. Build WorkMessage (also fills header_bytes)
    let (header_bytes, work) = candidate::build_work_message(&block, &config.miner_pk.h)?;
    block.header_bytes = header_bytes;

    Ok((block, work))
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
