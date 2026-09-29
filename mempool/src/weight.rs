use std::cmp::Ordering;

use crate::types::FeeStrategy;

/// Ordering key for mempool transactions.
///
/// Sorted by weight descending (highest fee first), then tx_id ascending
/// for deterministic tiebreak. The BTreeMap uses this as its key, so the
/// first entry is the highest-priority transaction. A transaction's times
/// live on its `UnconfirmedTx`, not here.
#[derive(Clone, Debug)]
pub struct TxWeight {
    /// Effective weight — starts as fee_per_factor, increased by family weighting.
    pub weight: u64,
    /// Base fee per factor (before family adjustments).
    pub fee_per_factor: u64,
    /// Transaction ID — tiebreaker for deterministic ordering.
    pub tx_id: [u8; 32],
}

impl Ord for TxWeight {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .weight
            .cmp(&self.weight)
            .then(self.tx_id.cmp(&other.tx_id))
    }
}

impl PartialOrd for TxWeight {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Equal exactly when `cmp` says `Equal`, on `weight` and `tx_id`, as `Ord`
/// requires.
impl PartialEq for TxWeight {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for TxWeight {}

/// Fallback cost when the real script cost is zero (division-by-zero guard).
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
                if cost == 0 {
                    FAKE_COST as u64
                } else {
                    cost as u64
                }
            }
        };
        // `fee * 1024` passes `u64::MAX` for a fee above about 1.8 × 10^16
        // nanoERG, so the product is wider; a quotient that still doesn't
        // fit saturates.
        let fee_per_factor = if fee_factor == 0 {
            0
        } else {
            u64::try_from(u128::from(fee) * 1024 / u128::from(fee_factor)).unwrap_or(u64::MAX)
        };
        Self {
            weight: fee_per_factor,
            fee_per_factor,
            tx_id,
        }
    }
}
