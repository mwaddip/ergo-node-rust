use std::cmp::Ordering;
use std::time::Instant;

use crate::types::FeeStrategy;

/// Ordering key for mempool transactions.
///
/// Sorted by weight descending (highest fee first), then tx_id ascending
/// for deterministic tiebreak. The BTreeMap uses this as its key, so the
/// first entry is the highest-priority transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxWeight {
    /// Effective weight — starts as fee_per_factor, increased by family weighting.
    pub weight: u64,
    /// Base fee per factor (before family adjustments).
    pub fee_per_factor: u64,
    /// Transaction ID — tiebreaker for deterministic ordering.
    pub tx_id: [u8; 32],
    /// Insertion timestamp.
    pub created: Instant,
}

impl Ord for TxWeight {
    fn cmp(&self, other: &Self) -> Ordering {
        other.weight.cmp(&self.weight)
            .then(self.tx_id.cmp(&other.tx_id))
    }
}

impl PartialOrd for TxWeight {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Fake validation cost when real cost tracking is unavailable.
const FAKE_COST: u32 = 1000;

impl TxWeight {
    /// Compute a TxWeight for a new transaction.
    pub fn new(
        tx_id: [u8; 32],
        fee: u64,
        tx_byte_size: usize,
        cost: u32,
        strategy: FeeStrategy,
    ) -> Self {
        let fee_factor = match strategy {
            FeeStrategy::FeePerByte => tx_byte_size as u64,
            FeeStrategy::FeePerCycle => {
                if cost == 0 { FAKE_COST as u64 } else { cost as u64 }
            }
        };
        let fee_per_factor = if fee_factor == 0 { 0 } else { fee * 1024 / fee_factor };
        Self {
            weight: fee_per_factor,
            fee_per_factor,
            tx_id,
            created: Instant::now(),
        }
    }
}
