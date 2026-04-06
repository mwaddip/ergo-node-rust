use std::time::{Duration, Instant};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;

/// Read-only access to the UTXO set for transaction validation.
///
/// Implemented by the main crate, combining the persistent UTXO state
/// with unconfirmed mempool outputs.
pub trait UtxoReader {
    /// Look up a box by its ID. Returns the deserialized ErgoBox.
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ErgoBox>;
}

/// A validated transaction in the mempool with metadata.
pub struct UnconfirmedTx {
    /// The transaction.
    pub tx: Transaction,
    /// Serialized transaction bytes (cached for P2P propagation).
    pub tx_bytes: Vec<u8>,
    /// Transaction fee in nanoERG.
    pub fee: u64,
    /// Validation cost estimate (tx byte size as proxy until sigma-rust adds cost tracking).
    pub cost: u32,
    /// When this transaction entered the pool.
    pub created: Instant,
    /// When this transaction was last revalidated.
    pub last_checked: Instant,
    /// Source peer (None if locally submitted via API).
    pub source: Option<u64>,
}

/// Outcome of processing a transaction.
#[derive(Debug)]
pub enum ProcessingOutcome {
    /// Transaction accepted and added to the pool.
    Accepted { tx_id: [u8; 32] },
    /// Transaction replaced one or more double-spending transactions.
    Replaced { tx_id: [u8; 32], removed: Vec<[u8; 32]> },
    /// Transaction rejected — higher-fee txs already spend the same inputs.
    DoubleSpendLoser { winner_ids: Vec<[u8; 32]> },
    /// Transaction temporarily declined — may succeed later.
    Declined { reason: String },
    /// Transaction permanently invalid — added to invalidation cache.
    Invalidated { reason: String },
    /// Transaction already in pool.
    AlreadyInPool,
}

/// Fee sorting strategy.
#[derive(Debug, Clone, Copy)]
pub enum FeeStrategy {
    /// fee * 1024 / tx_byte_size
    FeePerByte,
    /// fee * 1024 / validation_cost
    FeePerCycle,
}

/// Mempool configuration.
pub struct MempoolConfig {
    /// Maximum number of transactions in the pool (JVM default: 1000).
    pub capacity: usize,
    /// Minimum fee in nanoERG to enter the pool (JVM default: 1,000,000 = 0.001 ERG).
    pub min_fee: u64,
    /// Fee sorting strategy.
    pub fee_strategy: FeeStrategy,
    /// Minimum interval between revalidation of a transaction (JVM: 30s).
    pub cleanup_interval: Duration,
    /// Number of transactions to rebroadcast per cleanup cycle (JVM: 3).
    pub rebroadcast_count: usize,
    /// Time-to-live for invalidated tx IDs.
    pub invalidation_ttl: Duration,
    /// Maximum entries in the invalidation cache.
    pub invalidation_capacity: usize,
    /// Maximum validation cost budget per block interval for remote txs.
    pub cost_per_block: u64,
    /// Maximum validation cost budget per peer per block interval.
    pub cost_per_peer_per_block: u64,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            capacity: 1000,
            min_fee: 1_000_000,
            fee_strategy: FeeStrategy::FeePerByte,
            cleanup_interval: Duration::from_secs(30),
            rebroadcast_count: 3,
            invalidation_ttl: Duration::from_secs(1800),
            invalidation_capacity: 10_000,
            cost_per_block: 12_000_000,
            cost_per_peer_per_block: 10_000_000,
        }
    }
}
