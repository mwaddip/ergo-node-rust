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
pub fn generate_candidate(
    config: &MinerConfig,
    parent: &Header,
    n_bits: u32,
    parent_interlinks: &[BlockId],
    emission_box: &ErgoBox,
    validator_proofs: &dyn Fn(&[Transaction]) -> Option<Result<(Vec<u8>, ADDigest), ValidationError>>,
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
    let reemission_rules = emission::ReemissionRules::mainnet();
    let emission_tx = emission::build_emission_tx(
        emission_box,
        height,
        &config.miner_pk,
        config.reward_delay,
        &reemission_rules,
    )?;

    // 2. Transaction list: [emission_tx] (no mempool txs yet, no fee tx)
    let transactions = vec![emission_tx];

    // 3. Compute state root and AD proofs via validator
    let (ad_proof_bytes, state_root) = validator_proofs(&transactions)
        .ok_or(MiningError::Unavailable("UTXO mode required for mining".into()))?
        .map_err(MiningError::Validation)?;

    // 4. Build extension
    let extension = extension::build_extension(parent, parent_interlinks)?;

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

/// Stateful candidate manager — caches the current candidate and serves
/// it to multiple miner polls. Invalidated when the chain tip changes
/// or the candidate TTL expires.
pub struct CandidateGenerator {
    pub config: MinerConfig,
    cached: RwLock<Option<CachedCandidate>>,
}

impl CandidateGenerator {
    pub fn new(config: MinerConfig) -> Self {
        Self {
            config,
            cached: RwLock::new(None),
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

    /// Store a freshly generated candidate.
    pub fn cache_candidate(
        &self,
        block: CandidateBlock,
        work: WorkMessage,
        tip_height: u32,
    ) {
        let mut guard = self.cached.write().unwrap();
        *guard = Some(CachedCandidate {
            block,
            work,
            tip_height,
            created: Instant::now(),
        });
    }

    /// Get the cached CandidateBlock for solution validation.
    pub fn cached_block(&self) -> Option<CandidateBlock> {
        let guard = self.cached.read().ok()?;
        guard.as_ref().map(|c| c.block.clone())
    }

    /// Invalidate the cached candidate (called when chain tip changes).
    pub fn invalidate(&self) {
        if let Ok(mut guard) = self.cached.write() {
            *guard = None;
        }
    }
}
