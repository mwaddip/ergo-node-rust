use std::time::{Duration, Instant};

use ergo_chain_types::{ADDigest, Header};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use serde::Serialize;

/// Miner configuration loaded from node config.
pub struct MinerConfig {
    /// Miner's public key (required for mining).
    pub miner_pk: ProveDlog,
    /// Miner reward maturity delay in blocks (720 on mainnet/testnet).
    pub reward_delay: i32,
    /// Voting preferences: 3 bytes [soft_fork, param_1, param_2].
    pub votes: [u8; 3],
    /// Maximum candidate lifetime before forced regeneration.
    pub candidate_ttl: Duration,
}

/// Extension section key-value pairs for a new block.
pub struct ExtensionCandidate {
    /// Fields as (2-byte key, variable-length value).
    pub fields: Vec<([u8; 2], Vec<u8>)>,
}

/// All components needed to assemble a full block once a PoW solution arrives.
pub struct CandidateBlock {
    /// Parent block header.
    pub parent: Header,
    /// Block version.
    pub version: u8,
    /// Encoded difficulty target (compact bits).
    pub n_bits: u32,
    /// New state root after applying selected transactions.
    pub state_root: ADDigest,
    /// Serialized AD proofs for the state transition.
    pub ad_proof_bytes: Vec<u8>,
    /// Ordered transactions: [emission_tx, mempool_txs..., fee_tx].
    pub transactions: Vec<Transaction>,
    /// Block timestamp: max(now_ms, parent.timestamp + 1).
    pub timestamp: u64,
    /// Extension section (interlinks + voting).
    pub extension: ExtensionCandidate,
    /// Voting bytes (3 bytes).
    pub votes: [u8; 3],
    /// Serialized header-without-PoW bytes (cached for WorkMessage).
    pub header_bytes: Vec<u8>,
}

/// Data sent to the miner. The miner finds nonce n such that pow_hit(msg, n, h) < b.
#[derive(Clone, Serialize)]
pub struct WorkMessage {
    /// Blake2b256(serialized HeaderWithoutPow) — hex-encoded.
    pub msg: String,
    /// Target value from nBits — decimal string.
    pub b: String,
    /// Block height.
    pub h: u32,
    /// Miner public key — hex-encoded compressed point.
    pub pk: String,
    /// Header pre-image for miner verification.
    pub proof: ProofOfUpcomingTransactions,
}

#[derive(Clone, Serialize)]
pub struct ProofOfUpcomingTransactions {
    /// Serialized header-without-PoW — hex-encoded.
    #[serde(rename = "msgPreimage")]
    pub msg_preimage: String,
    /// Merkle proofs for mandatory txs (empty for first release).
    #[serde(rename = "txProofs")]
    pub tx_proofs: Vec<()>,
}

/// Cached candidate with metadata for invalidation.
pub struct CachedCandidate {
    pub block: CandidateBlock,
    pub work: WorkMessage,
    pub tip_height: u32,
    pub created: Instant,
}
