use std::collections::VecDeque;

/// Fee bucket for histogram display.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeBucket {
    /// Number of transactions in this fee range.
    pub count: usize,
    /// Lower bound of fee range (fee_per_factor).
    pub min_fee: u64,
    /// Upper bound of fee range (fee_per_factor).
    pub max_fee: u64,
    /// Average wait time in ms for txs in this bucket.
    pub avg_wait_ms: u64,
}

pub struct FeeStats {
    /// (fee_per_factor, wait_ms) for recently confirmed transactions.
    history: VecDeque<(u64, u64)>,
    max_history: usize,
}

impl FeeStats {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: VecDeque::new(),
            max_history,
        }
    }

    /// Record a confirmation: the tx had this fee_per_factor and waited this long.
    pub fn record_confirmation(&mut self, fee_per_factor: u64, wait_ms: u64) {
        if self.history.len() >= self.max_history {
            self.history.pop_front();
        }
        self.history.push_back((fee_per_factor, wait_ms));
    }

    /// Fee histogram with N buckets.
    pub fn histogram(&self, bucket_count: usize) -> Vec<FeeBucket> {
        if self.history.is_empty() || bucket_count == 0 {
            return Vec::new();
        }

        let min_fee = self.history.iter().map(|(f, _)| *f).min().unwrap();
        let max_fee = self.history.iter().map(|(f, _)| *f).max().unwrap();

        if min_fee == max_fee {
            let avg_wait = self.history.iter().map(|(_, w)| *w).sum::<u64>()
                / self.history.len() as u64;
            return vec![FeeBucket {
                count: self.history.len(),
                min_fee,
                max_fee: max_fee + 1,
                avg_wait_ms: avg_wait,
            }];
        }

        let range = max_fee - min_fee + 1;
        let bucket_width = (range + bucket_count as u64 - 1) / bucket_count as u64;

        let mut buckets: Vec<(usize, u64)> = vec![(0, 0); bucket_count];

        for &(fee, wait) in &self.history {
            let idx = ((fee - min_fee) / bucket_width) as usize;
            let idx = idx.min(bucket_count - 1);
            buckets[idx].0 += 1;
            buckets[idx].1 += wait;
        }

        buckets.iter().enumerate().filter(|(_, (count, _))| *count > 0).map(|(i, (count, total_wait))| {
            FeeBucket {
                count: *count,
                min_fee: min_fee + i as u64 * bucket_width,
                max_fee: min_fee + (i as u64 + 1) * bucket_width,
                avg_wait_ms: *total_wait / *count as u64,
            }
        }).collect()
    }

    /// Estimated wait time for a transaction with given fee_per_factor.
    pub fn expected_wait(&self, fee_per_factor: u64) -> Option<u64> {
        if self.history.is_empty() {
            return None;
        }

        // Find the closest recorded fee and use its wait time
        let mut closest_fee_diff = u64::MAX;
        let mut closest_wait = 0u64;
        let mut count = 0u64;

        for &(fee, wait) in &self.history {
            let diff = if fee > fee_per_factor { fee - fee_per_factor } else { fee_per_factor - fee };
            if diff < closest_fee_diff {
                closest_fee_diff = diff;
                closest_wait = wait;
                count = 1;
            } else if diff == closest_fee_diff {
                closest_wait += wait;
                count += 1;
            }
        }

        if count == 0 { None } else { Some(closest_wait / count) }
    }

    /// Recommended fee_per_factor to achieve target wait time.
    pub fn recommended_fee(&self, target_wait_ms: u64) -> Option<u64> {
        if self.history.is_empty() {
            return None;
        }

        // Find the lowest fee that achieved wait_ms <= target
        self.history.iter()
            .filter(|(_, wait)| *wait <= target_wait_ms)
            .map(|(fee, _)| *fee)
            .min()
    }
}
