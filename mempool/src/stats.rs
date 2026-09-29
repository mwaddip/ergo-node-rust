//! Statistics over the pool transactions that blocks confirmed: a port of the
//! JVM's `MemPoolStatistics` (v6.0.6). The fee queries in `lib.rs` are the
//! only readers. See `facts/mempool.md` § `FeeStats` and § Fee queries.

use std::time::{Duration, Instant};

/// Bins in the wait histogram: one per whole minute, 0 through 59.
pub const HISTOGRAM_BINS: usize = 60;
/// How often the throughput window may move (the JVM's `measurementIntervalMsec`).
pub const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

/// A count of transactions and the sum of their `fee_per_factor`.
/// JSON `{"nTxns": …, "totalFee": …}`, the JVM's `FeeHistogramBin`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistogramBin {
    pub n_txns: u64,
    /// Saturating.
    pub total_fee: u64,
}

impl FeeHistogramBin {
    /// Count one more transaction, of `fee_per_factor`.
    pub(crate) fn add(&mut self, fee_per_factor: u64) {
        self.n_txns += 1;
        self.total_fee = self.total_fee.saturating_add(fee_per_factor);
    }
}

pub struct FeeStats {
    /// Bin `m`: confirmed transactions that spent at least `m` and less than
    /// `m + 1` whole minutes in the pool.
    histogram: [FeeHistogramBin; HISTOGRAM_BINS],
    /// Throughput window: `taken` pool transactions confirmed since `window_start`.
    window_start: Instant,
    taken: u64,
    /// When the window last moved or declined to, and `taken` at that moment.
    snap_time: Instant,
    snap_taken: u64,
}

impl FeeStats {
    /// Empty histogram, `taken` and `snap_taken` 0, both instants `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            histogram: [FeeHistogramBin::default(); HISTOGRAM_BINS],
            window_start: now,
            taken: 0,
            snap_time: now,
            snap_taken: 0,
        }
    }

    /// A pool transaction that entered the pool at `created`, with base
    /// weight `fee_per_factor`, was confirmed by a block at `now`.
    pub fn record_confirmation(&mut self, now: Instant, created: Instant, fee_per_factor: u64) {
        self.taken += 1;

        if now.saturating_duration_since(self.snap_time) > MEASUREMENT_INTERVAL {
            if self.snap_taken != 0 {
                debug_assert!(
                    self.taken > self.snap_taken,
                    "every confirmation since the snapshot counts, so the window never empties"
                );
                self.taken -= self.snap_taken;
                self.window_start = self.snap_time;
            }
            // The snapshot follows the subtraction. The JVM's precedes it, and
            // its count sinks to zero within a few windows.
            self.snap_taken = self.taken;
            self.snap_time = now;
        }

        // A longer wait is left out of the histogram, but was counted above.
        let minutes = now.saturating_duration_since(created).as_secs() / 60;
        if minutes < HISTOGRAM_BINS as u64 {
            self.histogram[minutes as usize].add(fee_per_factor);
        }
    }

    /// `recommended_fee` before its `min_fee` floor: from the first non-empty
    /// bin among 0 through `min(wait_minutes, 59)`, `(total_fee / n_txns) *
    /// tx_size / 1024`, in that order. `None` when every bin in the range is
    /// empty.
    pub(crate) fn fee_for_wait(&self, wait_minutes: u32, tx_size: u32) -> Option<u64> {
        let last = wait_minutes.min(HISTOGRAM_BINS as u32 - 1) as usize;
        let bin = self.histogram[..=last].iter().find(|bin| bin.n_txns != 0)?;
        let average = bin.total_fee / bin.n_txns;
        Some(saturating_u64(
            u128::from(average) * u128::from(tx_size) / 1024,
        ))
    }

    /// `expected_wait_ms` for a transaction with `position` pool transactions
    /// ahead of it: `elapsed * position / taken`, where `elapsed` is `now −
    /// window_start` capped at `MEASUREMENT_INTERVAL`, in milliseconds. 0
    /// while `taken` is 0.
    pub(crate) fn wait_for_position(&self, now: Instant, position: u64) -> u64 {
        if self.taken == 0 {
            return 0;
        }
        let elapsed = now
            .saturating_duration_since(self.window_start)
            .min(MEASUREMENT_INTERVAL);
        saturating_u64(elapsed.as_millis() * u128::from(position) / u128::from(self.taken))
    }
}

/// `value`, or `u64::MAX` if it doesn't fit.
pub(crate) fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The window and the bins are private, hence unit tests. The fee queries
/// that read them are tested through `Mempool` in `tests/stats_test.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn mins(m: u64) -> Duration {
        Duration::from_secs(60 * m)
    }

    fn ms(m: u64) -> Duration {
        Duration::from_millis(m)
    }

    /// Confirmations at t0 + 1, 61, 62, 123, 124 and 185 s, and the state
    /// after each, worked by hand from `record_confirmation`'s steps.
    #[test]
    fn the_window_moves_across_intervals_and_never_empties() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);

        // (confirmed at, taken, window_start, snap_time, snap_taken), in
        // seconds after t0.
        let steps = [
            // 1 s since the snapshot at t0: no move.
            (1, 1, 0, 0, 0),
            // 61 s: the window moves, but `snap_taken` is 0, so nothing is
            // subtracted and it still starts at t0.
            (61, 2, 0, 61, 2),
            (62, 3, 0, 61, 2),
            // 62 s: 4 − 2, from t0 + 61. The snapshot takes the 2 left; the
            // JVM's takes the 4 before the subtraction.
            (123, 2, 61, 123, 2),
            (124, 3, 61, 123, 2),
            // 62 s: 4 − 2, from t0 + 123. The JVM would be at 4 − 4 = 0.
            (185, 2, 123, 185, 2),
        ];
        for (at, taken, window_start, snap_time, snap_taken) in steps {
            let now = t0 + secs(at);
            stats.record_confirmation(now, now, 1);
            assert_eq!(
                (
                    stats.taken,
                    stats.window_start,
                    stats.snap_time,
                    stats.snap_taken
                ),
                (
                    taken,
                    t0 + secs(window_start),
                    t0 + secs(snap_time),
                    snap_taken
                ),
                "after the confirmation at t0 + {at} s"
            );
        }
    }

    /// The window may move once more than `MEASUREMENT_INTERVAL` has passed
    /// since the snapshot: 60 s exactly is not enough.
    #[test]
    fn the_window_moves_only_after_more_than_an_interval() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);

        stats.record_confirmation(t0 + secs(60), t0, 1);
        assert_eq!((stats.snap_time, stats.snap_taken), (t0, 0), "at 60 s");

        let later = t0 + secs(60) + ms(1);
        stats.record_confirmation(later, t0, 1);
        assert_eq!(
            (stats.snap_time, stats.snap_taken),
            (later, 2),
            "at 60.001 s"
        );
    }

    /// 59 minutes and 59 seconds in the pool lands in bin 59; 60 minutes
    /// lands in no bin. `taken` counts both.
    #[test]
    fn an_hour_in_the_pool_is_counted_but_not_binned() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);

        stats.record_confirmation(t0 + mins(59) + secs(59), t0, 700);
        stats.record_confirmation(t0 + mins(60), t0, 900);

        let mut expected = [FeeHistogramBin::default(); HISTOGRAM_BINS];
        expected[59] = FeeHistogramBin {
            n_txns: 1,
            total_fee: 700,
        };
        assert_eq!(stats.histogram, expected);
        assert_eq!(stats.taken, 2);
    }

    /// Bin `m` holds waits of at least `m` and less than `m + 1` whole
    /// minutes.
    #[test]
    fn waits_are_binned_by_whole_minutes() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);

        stats.record_confirmation(t0 + ms(59_999), t0, 1);
        stats.record_confirmation(t0 + secs(60), t0, 2);
        stats.record_confirmation(t0 + ms(119_999), t0, 4);
        stats.record_confirmation(t0 + secs(120), t0, 8);

        let mut expected = [FeeHistogramBin::default(); HISTOGRAM_BINS];
        expected[0] = FeeHistogramBin {
            n_txns: 1,
            total_fee: 1,
        };
        expected[1] = FeeHistogramBin {
            n_txns: 2,
            total_fee: 2 + 4,
        };
        expected[2] = FeeHistogramBin {
            n_txns: 1,
            total_fee: 8,
        };
        assert_eq!(stats.histogram, expected);
    }

    #[test]
    fn a_bins_total_fee_saturates() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);

        stats.record_confirmation(t0, t0, u64::MAX - 1);
        stats.record_confirmation(t0, t0, 5);

        assert_eq!(
            stats.histogram[0],
            FeeHistogramBin {
                n_txns: 2,
                total_fee: u64::MAX
            }
        );
    }

    /// One confirmation of `u64::MAX` in bin 0: for 2048 bytes that is
    /// `u64::MAX × 2`, which saturates; for 512 bytes it halves.
    #[test]
    fn fee_for_wait_is_none_when_empty_and_saturates_when_too_large() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);
        assert_eq!(stats.fee_for_wait(59, 1024), None);

        stats.record_confirmation(t0, t0, u64::MAX);
        assert_eq!(stats.fee_for_wait(0, 2048), Some(u64::MAX));
        assert_eq!(stats.fee_for_wait(0, 512), Some(u64::MAX / 2));
    }

    /// Two confirmations from a window opened at t0, neither moving it.
    /// 30 s in, 5 transactions ahead: 30,000 × 5 / 2 = 75,000 ms. 90 s in,
    /// `elapsed` is capped at 60,000: 60,000 × 5 / 2 = 150,000 ms.
    #[test]
    fn wait_for_position_scales_elapsed_by_position_over_taken() {
        let t0 = Instant::now();
        let mut stats = FeeStats::new(t0);
        assert_eq!(stats.wait_for_position(t0 + secs(30), 5), 0, "taken 0");

        stats.record_confirmation(t0 + secs(10), t0, 1);
        stats.record_confirmation(t0 + secs(20), t0, 1);

        assert_eq!(stats.wait_for_position(t0 + secs(30), 5), 75_000);
        assert_eq!(stats.wait_for_position(t0 + secs(90), 5), 150_000);
        assert_eq!(stats.wait_for_position(t0 + secs(30), 0), 0, "none ahead");
    }
}
