//! Periodic heap/height record emitted while the node is catching up.
//!
//! What survives of `eval_backlog.rs`. That module reported the depth of the
//! deferred script-eval queue against the 2026-08-12 field OOM — a 4-thread
//! 1.8 GHz node killed mid-sweep at **anon-rss 10.62 GiB, file-rss 2.4 MB**,
//! heap rather than page cache, after 26h of steady cadence. Four of its six
//! fields (`evals_in_flight`, `eval_bytes_in_flight`, `script_verified_height`,
//! `eval_lag`) described that queue. The queue is gone in v0.8.0 —
//! `apply_state` evaluates before it persists — so those four describe nothing.
//!
//! The two that remain describe the sweep itself and are the pair an operator
//! actually reads together: how far state has been applied, and what the
//! allocator is holding while it gets there. Nothing else emits a heap reading
//! on a fixed grid during catch-up; `block applied` fires per block and carries
//! no heap, and the sweep summary fires twice per sweep.
//!
//! See `../facts/sync.md` § "Catch-up progress instrumentation".
//!
//! ## Policy
//!
//! Unchanged from the record this replaces, because none of it was about the
//! queue:
//!
//! - **Time-based, not per-block.** Through the empty early chain the sweep
//!   clears ~192 blocks/second; a per-block record is unreadable. The gate is
//!   [`REPORT_INTERVAL`] of elapsed wall time.
//! - **Catch-up only.** At tip `sweep_size <= 1` and the record would be one
//!   line per block interval forever. A tip sweep is suppressed *without*
//!   consuming the interval, so the first catch-up sweep after one still
//!   reports.
//! - **Does not perturb what it measures.** The heap probe is a closure called
//!   only once the gate has passed — never per block — and the record
//!   allocates nothing beyond itself.

use tokio::time::{Duration, Instant};

/// Elapsed wall time between records during catch-up.
///
/// Matches `SyncConfig::delivery_check_interval`, the cadence at which the
/// rest of the sync machine's periodic work already runs, so an operator
/// reading the journal sees this on the same grid as delivery activity. Cheap
/// relative to what it sits beside: catch-up emits `block applied` at INFO
/// *per block*, so this record is a rounding error in the journal.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Interval gate for the catch-up progress record.
///
/// `Default` is "never reported yet", which makes the first eligible block of
/// the first catch-up sweep emit immediately — a baseline record at the start
/// of the run rather than 5 seconds into it. In-memory; a restart legitimately
/// re-baselines.
#[derive(Debug, Default, Clone)]
pub(crate) struct CatchUpProgressReporter {
    last_emit: Option<Instant>,
}

impl CatchUpProgressReporter {
    /// Emit a progress record if this is a catch-up sweep and
    /// [`REPORT_INTERVAL`] has elapsed since the last one. Returns whether it
    /// emitted.
    ///
    /// `state_applied_height` MUST be the validator's own `validated_height()`,
    /// not `HeaderSync::state_applied_height` — that struct field is a cache
    /// reconciled at sweep *end*, so mid-sweep it is frozen at the pre-sweep
    /// tip and this record would report a height the node passed some time ago.
    ///
    /// `heap_probe` is called only when the record actually fires — it is
    /// `SyncConfig::flush_probe`, a `mallctl` round-trip into jemalloc, and
    /// running it per block would be exactly the kind of overhead an instrument
    /// has no business adding. Returns `None` when no probe is wired (the build
    /// has no jemalloc feature), in which case the field is absent.
    pub(crate) fn maybe_emit(
        &mut self,
        now: Instant,
        sweep_size: u32,
        state_applied_height: u32,
        heap_probe: impl FnOnce() -> Option<u64>,
    ) -> bool {
        if sweep_size <= 1 {
            // Not catch-up. Returns before touching `last_emit`, so a dip to
            // tip does not consume the interval.
            return false;
        }
        if let Some(last) = self.last_emit {
            // Saturating: a non-monotonic clock reading must not panic here.
            if now.saturating_duration_since(last) < REPORT_INTERVAL {
                return false;
            }
        }
        self.last_emit = Some(now);
        emit(state_applied_height, heap_probe());
        true
    }
}

/// Emit the record. `jemalloc_allocated` is omitted entirely when no probe is
/// wired, matching how `validation_stuck` handles its optional `missing_key` —
/// an absent field rather than a rendered `None`.
fn emit(state_applied_height: u32, jemalloc_allocated: Option<u64>) {
    match jemalloc_allocated {
        Some(bytes) => tracing::info!(
            state_applied_height = state_applied_height as u64,
            jemalloc_allocated = bytes,
            "catch-up progress"
        ),
        None => tracing::info!(
            state_applied_height = state_applied_height as u64,
            "catch-up progress"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;
    use std::cell::Cell;

    fn now() -> Instant {
        Instant::from_std(std::time::Instant::now())
    }

    /// A catch-up sweep: anything above the tip's single block.
    const CATCH_UP: u32 = 192;

    // ---- the interval gate ----

    #[test]
    fn first_catch_up_report_emits_immediately() {
        // A baseline record at the start of the run, not one interval into it.
        let mut r = CatchUpProgressReporter::default();
        let output = capture(|| {
            assert!(
                r.maybe_emit(now(), CATCH_UP, 1_000, || None),
                "a fresh reporter has nothing to wait for"
            );
        });
        assert!(output.contains("catch-up progress"), "missing marker: {output}");
    }

    #[test]
    fn suppressed_until_the_interval_elapses() {
        let mut r = CatchUpProgressReporter::default();
        let t0 = now();
        let output = capture(|| {
            assert!(r.maybe_emit(t0, CATCH_UP, 1_000, || None));
            assert!(
                !r.maybe_emit(t0, CATCH_UP, 1_001, || None),
                "same instant — inside the window"
            );
            assert!(
                !r.maybe_emit(
                    t0 + REPORT_INTERVAL - Duration::from_millis(1),
                    CATCH_UP,
                    1_002,
                    || None
                ),
                "one millisecond short of the window"
            );
            assert!(
                r.maybe_emit(t0 + REPORT_INTERVAL, CATCH_UP, 1_003, || None),
                "window elapsed → report"
            );
        });
        assert_eq!(
            output.matches("catch-up progress").count(),
            2,
            "exactly the first and the post-interval call emitted: {output}"
        );
    }

    #[test]
    fn interval_is_measured_from_the_last_emit_not_the_last_call() {
        // Two intervals of steady sweeping produce records on the grid, not one
        // per block and not one per sweep.
        let mut r = CatchUpProgressReporter::default();
        let t0 = now();
        let output = capture(|| {
            for tick in 0..=20u32 {
                // A block every second of sweeping.
                let at = t0 + Duration::from_secs(tick as u64);
                r.maybe_emit(at, CATCH_UP, 1_000 + tick, || None);
            }
        });
        // t=0, 5, 10, 15, 20.
        assert_eq!(
            output.matches("catch-up progress").count(),
            5,
            "one record per elapsed interval: {output}"
        );
    }

    // ---- catch-up only ----

    #[test]
    fn tip_sweep_never_reports() {
        let mut r = CatchUpProgressReporter::default();
        let t0 = now();
        let output = capture(|| {
            assert!(!r.maybe_emit(t0, 1, 1_785_000, || None));
            assert!(
                !r.maybe_emit(t0 + Duration::from_secs(3600), 1, 1_785_001, || None),
                "no amount of elapsed time makes a tip sweep report"
            );
            assert!(
                !r.maybe_emit(t0, 0, 1_785_000, || None),
                "a zero-block sweep is not catch-up either"
            );
        });
        assert!(!output.contains("catch-up progress"), "tip sweeps emitted: {output}");
    }

    #[test]
    fn a_tip_sweep_does_not_consume_the_interval() {
        // Suppression is not an emission. A node that dips to tip and falls
        // behind again reports on its first catch-up sweep.
        let mut r = CatchUpProgressReporter::default();
        let t0 = now();
        let output = capture(|| {
            assert!(!r.maybe_emit(t0, 1, 1_000, || None));
            assert!(
                r.maybe_emit(t0, CATCH_UP, 1_000, || None),
                "the suppressed tip sweep must not have armed the gate"
            );
        });
        assert_eq!(output.matches("catch-up progress").count(), 1, "{output}");
    }

    // ---- the fields ----

    #[test]
    fn record_carries_the_applied_height() {
        let mut r = CatchUpProgressReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, 1_779_387, || None);
        });
        assert!(
            output.contains("state_applied_height=1779387"),
            "missing applied height: {output}"
        );
    }

    #[test]
    fn record_carries_jemalloc_allocated_when_a_probe_is_wired() {
        // The 2026-08-12 field OOM: killed at validation height 1,779,387 with
        // anon-rss 10.62 GiB. The queue that caused it is gone, but the pairing
        // of applied height against allocator heap is what made the shape of
        // that run legible in the first place, and it is what would make the
        // next one legible too.
        let mut r = CatchUpProgressReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, 1_779_387, || Some(11_402_000_000));
        });
        assert!(
            output.contains("jemalloc_allocated=11402000000"),
            "missing heap field: {output}"
        );
    }

    #[test]
    fn record_omits_jemalloc_allocated_without_a_probe() {
        let mut r = CatchUpProgressReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, 1_000, || None);
        });
        assert!(
            output.contains("catch-up progress"),
            "record should still emit: {output}"
        );
        assert!(
            !output.contains("jemalloc_allocated"),
            "field should be absent, not rendered as None: {output}"
        );
    }

    #[test]
    fn heap_probe_is_not_called_while_suppressed() {
        // Do not perturb what you are measuring. The probe is a mallctl
        // round-trip; per-block it would be overhead the instrument invented.
        let calls = Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            Some(1_024u64)
        };
        let mut r = CatchUpProgressReporter::default();
        let t0 = now();
        capture(|| {
            r.maybe_emit(t0, CATCH_UP, 1_000, probe);
            assert_eq!(calls.get(), 1, "the emitted record reads the probe once");
            for tick in 1..5u32 {
                r.maybe_emit(
                    t0 + Duration::from_millis(tick as u64 * 100),
                    CATCH_UP,
                    1_000 + tick,
                    probe,
                );
            }
            assert_eq!(calls.get(), 1, "gated calls must not touch the probe");
            r.maybe_emit(t0, 1, 1_000, probe);
            assert_eq!(calls.get(), 1, "a tip sweep must not touch the probe either");
        });
    }
}
