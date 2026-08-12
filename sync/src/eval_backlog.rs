//! Periodic depth report for the deferred script-eval queue.
//!
//! `apply_state` hands each block's script evaluation to the rayon pool and
//! moves on; results come back through a channel and are drained non-blocking
//! between blocks. During catch-up the sweep-end drain does not drain to zero,
//! so a 192-block sweep dispatches up to 192 evals and keeps going. On a host
//! where verification is slower than state application the queue grows across
//! sweeps — and until 2026-08-12 nothing bounded it and nothing exposed it.
//!
//! Which is why the failure mode took a field OOM to surface: a 4-thread
//! 1.8 GHz node on v0.7.11 was killed mid-sweep on 2026-08-12 at **anon-rss
//! 10.62 GiB, file-rss 2.4 MB** — heap, not page cache — after 26h of entirely
//! steady cadence. That anon-rss is the measured quantity and the one to quote.
//! Converting it to a queue depth requires a per-item estimate that has already
//! moved by 8× once (≈27,000 evals became ≈3,000 purely by correcting the
//! per-item figure) and will move again once `eval_bytes_in_flight` has been
//! compared against jemalloc; the diagnosis is the same at either number. On a
//! high-core-count host rayon keeps pace and the queue stays shallow, so the
//! same binary looks fine there.
//!
//! The queue now **is** bounded — `eval_backlog_max_mb` / `eval_backlog_max_blocks`,
//! enforced at dispatch (`../facts/sync.md` § "Eval backpressure"). This module
//! did not become redundant when that landed; it sits alongside the bound
//! rather than in place of one, and it answers questions the bound cannot:
//!
//! - **Is the bound binding, and which half of it?** `evals_in_flight` and
//!   `eval_bytes_in_flight` against their configured limits say whether this
//!   host is pipelining freely or running at the gate all sweep.
//! - **Is the byte accounting real?** `approx_heap_bytes` is theory-grounded
//!   from sigma-rust struct shapes and has never been checked against a running
//!   allocator. `eval_bytes_in_flight` beside `jemalloc_allocated` — which this
//!   record already carried — makes the estimator falsifiable on the next
//!   catch-up run. If the two do not move together, the accounting in
//!   `facts/validation.md` is wrong and the byte bound is calibrated against
//!   fiction. Cheap to emit: the gate has to maintain the sum regardless.
//!
//! See `../facts/sync.md` § "Catch-up progress instrumentation".
//!
//! ## Policy
//!
//! - **Time-based, not per-block.** Through the empty early chain the sweep
//!   clears ~192 blocks/second; a per-block record is unreadable. The gate is
//!   [`REPORT_INTERVAL`] of elapsed wall time.
//! - **Catch-up only.** At tip `sweep_size <= 1` and the sweep drains
//!   synchronously every time, so the queue is empty by construction and the
//!   record would be pure noise. A tip sweep is suppressed *without* consuming
//!   the interval, so the first catch-up sweep after one still reports.
//! - **Does not perturb what it measures.** The heap probe is a closure called
//!   only once the gate has passed — never per block — and the record
//!   allocates nothing beyond itself.

use tokio::time::{Duration, Instant};

/// Elapsed wall time between backlog records during catch-up.
///
/// Matches `SyncConfig::delivery_check_interval`, the cadence at which the
/// rest of the sync machine's periodic work already runs, so an operator
/// reading the journal sees backlog depth on the same grid as delivery
/// activity. Cheap relative to what it sits beside: catch-up emits `block
/// applied` at INFO *per block*, so this record is a rounding error in the
/// journal.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// The quantities one backlog record reports, minus the heap probe.
///
/// `Copy` and stack-only — building one per block costs three register moves
/// whether or not the record is emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BacklogDepth {
    /// Eval tasks dispatched to rayon but not yet drained — the quantity the
    /// OOM hypothesis is about, and the guardrail half of the bound.
    pub evals_in_flight: u32,
    /// Σ `approx_heap_bytes()` of those same tasks — the quantity the primary
    /// bound gates on, and the only thing that ever checks the estimator. See
    /// the module doc.
    pub eval_bytes_in_flight: u64,
    /// Highest height where `apply_state()` returned Ok.
    ///
    /// Read from the validator's own `validated_height()`, NOT from
    /// `HeaderSync::state_applied_height` — that struct field is a cache
    /// reconciled at sweep *end*, so mid-sweep it is frozen at the pre-sweep
    /// tip while `script_verified_height` climbs past it. Reporting the cache
    /// would peg `eval_lag` at 0 (via saturation) for the whole sweep and
    /// measure nothing.
    pub state_applied_height: u32,
    /// Highest height where `evaluate_scripts()` has completed.
    pub script_verified_height: u32,
}

impl BacklogDepth {
    /// Blocks whose state is applied but whose scripts are not yet verified —
    /// the backlog expressed in heights rather than queue entries. Derived
    /// here rather than left to whoever reads the journal: it is the number
    /// the backlog hypothesis predicts will climb, and two readers computing
    /// it by hand from two other fields will not do it the same way.
    ///
    /// Saturating. The contract invariant is
    /// `script_verified_height <= state_applied_height`, but an
    /// instrumentation record must never be the thing that panics a syncing
    /// node.
    pub(crate) fn eval_lag(self) -> u32 {
        self.state_applied_height
            .saturating_sub(self.script_verified_height)
    }
}

/// Interval gate for the backlog record.
///
/// `Default` is "never reported yet", which makes the first eligible block of
/// the first catch-up sweep emit immediately — a baseline record at the start
/// of the run rather than 5 seconds into it. In-memory; a restart legitimately
/// re-baselines.
#[derive(Debug, Default, Clone)]
pub(crate) struct EvalBacklogReporter {
    last_emit: Option<Instant>,
}

impl EvalBacklogReporter {
    /// Emit a backlog record if this is a catch-up sweep and
    /// [`REPORT_INTERVAL`] has elapsed since the last one. Returns whether it
    /// emitted.
    ///
    /// `heap_probe` is called only when the record is actually emitted — it is
    /// `SyncConfig::flush_probe`, a `mallctl` round-trip into jemalloc, and
    /// running it per block would be exactly the kind of overhead an
    /// instrument has no business adding. Returns `None` when no probe is
    /// wired (the build has no jemalloc feature), in which case the field is
    /// absent from the record.
    pub(crate) fn maybe_emit(
        &mut self,
        now: Instant,
        sweep_size: u32,
        depth: BacklogDepth,
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
        emit_eval_backlog(depth, heap_probe());
        true
    }
}

/// Emit the backlog record. `jemalloc_allocated` is omitted entirely when no
/// probe is wired, matching how `validation_stuck` handles its optional
/// `missing_key` — an absent field rather than a rendered `None`.
fn emit_eval_backlog(depth: BacklogDepth, jemalloc_allocated: Option<u64>) {
    match jemalloc_allocated {
        Some(bytes) => tracing::info!(
            evals_in_flight = depth.evals_in_flight as u64,
            eval_bytes_in_flight = depth.eval_bytes_in_flight,
            state_applied_height = depth.state_applied_height as u64,
            script_verified_height = depth.script_verified_height as u64,
            eval_lag = depth.eval_lag() as u64,
            jemalloc_allocated = bytes,
            "deferred eval backlog"
        ),
        None => tracing::info!(
            evals_in_flight = depth.evals_in_flight as u64,
            eval_bytes_in_flight = depth.eval_bytes_in_flight,
            state_applied_height = depth.state_applied_height as u64,
            script_verified_height = depth.script_verified_height as u64,
            eval_lag = depth.eval_lag() as u64,
            "deferred eval backlog"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Captures the rendered `deferred_eval_backlog` record, which is INFO.
    use crate::test_support::capture;
    use std::cell::Cell;

    fn now() -> Instant {
        Instant::from_std(std::time::Instant::now())
    }

    /// A catch-up sweep: anything above the tip's single block.
    const CATCH_UP: u32 = 192;

    /// `approx_heap_bytes` of an ordinary (~20-tx) block, per
    /// `facts/validation.md`. Lets `depth` derive a plausible byte figure from
    /// the count so the tests that do not care about bytes stay unchanged.
    const ORDINARY_BLOCK_BYTES: u64 = 195 * 1024;

    fn depth(in_flight: u32, applied: u32, verified: u32) -> BacklogDepth {
        depth_bytes(
            in_flight,
            in_flight as u64 * ORDINARY_BLOCK_BYTES,
            applied,
            verified,
        )
    }

    fn depth_bytes(in_flight: u32, bytes: u64, applied: u32, verified: u32) -> BacklogDepth {
        BacklogDepth {
            evals_in_flight: in_flight,
            eval_bytes_in_flight: bytes,
            state_applied_height: applied,
            script_verified_height: verified,
        }
    }

    // ---- the interval gate ----

    #[test]
    fn first_catch_up_report_emits_immediately() {
        // A baseline record at the start of the run, not one interval into it.
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            assert!(
                r.maybe_emit(now(), CATCH_UP, depth(64, 1_000, 900), || None),
                "a fresh reporter has nothing to wait for"
            );
        });
        assert!(
            output.contains("deferred eval backlog"),
            "missing marker: {output}"
        );
    }

    #[test]
    fn suppressed_until_the_interval_elapses() {
        let mut r = EvalBacklogReporter::default();
        let t0 = now();
        let output = capture(|| {
            assert!(r.maybe_emit(t0, CATCH_UP, depth(1, 1_000, 999), || None));
            assert!(
                !r.maybe_emit(t0, CATCH_UP, depth(2, 1_001, 999), || None),
                "same instant — inside the window"
            );
            assert!(
                !r.maybe_emit(
                    t0 + REPORT_INTERVAL - Duration::from_millis(1),
                    CATCH_UP,
                    depth(3, 1_002, 999),
                    || None
                ),
                "one millisecond short of the window"
            );
            assert!(
                r.maybe_emit(t0 + REPORT_INTERVAL, CATCH_UP, depth(4, 1_003, 999), || None),
                "window elapsed → report"
            );
        });
        assert_eq!(
            output.matches("deferred eval backlog").count(),
            2,
            "exactly the first and the post-interval call emitted: {output}"
        );
    }

    #[test]
    fn interval_is_measured_from_the_last_emit_not_the_last_call() {
        // Two intervals of steady sweeping produce two records, not one per
        // block and not one per sweep.
        let mut r = EvalBacklogReporter::default();
        let t0 = now();
        let output = capture(|| {
            for tick in 0..=20u32 {
                // A block every second of sweeping.
                let at = t0 + Duration::from_secs(tick as u64);
                r.maybe_emit(at, CATCH_UP, depth(tick, 1_000 + tick, 1_000), || None);
            }
        });
        // t=0, 5, 10, 15, 20.
        assert_eq!(
            output.matches("deferred eval backlog").count(),
            5,
            "one record per elapsed interval: {output}"
        );
    }

    // ---- catch-up only ----

    #[test]
    fn tip_sweep_never_reports() {
        // At tip the drain is synchronous every sweep — the queue is empty by
        // construction and the record is noise.
        let mut r = EvalBacklogReporter::default();
        let t0 = now();
        let output = capture(|| {
            assert!(!r.maybe_emit(t0, 1, depth(0, 1_785_000, 1_785_000), || None));
            assert!(
                !r.maybe_emit(
                    t0 + Duration::from_secs(3600),
                    1,
                    depth(0, 1_785_001, 1_785_001),
                    || None
                ),
                "no amount of elapsed time makes a tip sweep report"
            );
            assert!(
                !r.maybe_emit(t0, 0, depth(0, 1_785_000, 1_785_000), || None),
                "a zero-block sweep is not catch-up either"
            );
        });
        assert!(
            !output.contains("deferred eval backlog"),
            "tip sweeps emitted: {output}"
        );
    }

    #[test]
    fn a_tip_sweep_does_not_consume_the_interval() {
        // Suppression is not an emission. A node that dips to tip and falls
        // behind again reports on its first catch-up sweep.
        let mut r = EvalBacklogReporter::default();
        let t0 = now();
        let output = capture(|| {
            assert!(!r.maybe_emit(t0, 1, depth(0, 1_000, 1_000), || None));
            assert!(
                r.maybe_emit(t0, CATCH_UP, depth(7, 1_000, 993), || None),
                "the suppressed tip sweep must not have armed the gate"
            );
        });
        assert_eq!(output.matches("deferred eval backlog").count(), 1, "{output}");
    }

    // ---- eval_lag ----

    #[test]
    fn eval_lag_is_the_watermark_difference() {
        assert_eq!(depth(0, 1_000, 700).eval_lag(), 300);
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, depth(300, 1_000, 700), || None);
        });
        assert!(output.contains("eval_lag=300"), "missing eval_lag: {output}");
        assert!(
            output.contains("state_applied_height=1000"),
            "missing state_applied_height: {output}"
        );
        assert!(
            output.contains("script_verified_height=700"),
            "missing script_verified_height: {output}"
        );
        assert!(
            output.contains("evals_in_flight=300"),
            "missing evals_in_flight: {output}"
        );
    }

    #[test]
    fn eval_lag_is_zero_when_the_watermarks_are_level() {
        assert_eq!(depth(0, 1_785_000, 1_785_000).eval_lag(), 0);
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, depth(0, 1_785_000, 1_785_000), || None);
        });
        assert!(output.contains("eval_lag=0"), "missing eval_lag: {output}");
    }

    #[test]
    fn eval_lag_saturates_when_script_verified_leads() {
        // The contract invariant says this cannot happen. If it ever does,
        // the instrumentation reports 0 — it does not underflow and it does
        // not panic a syncing node.
        assert_eq!(depth(0, 900, 1_000).eval_lag(), 0);
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, depth(0, 900, 1_000), || None);
        });
        assert!(output.contains("eval_lag=0"), "missing eval_lag: {output}");
    }

    // ---- the heap probe ----

    #[test]
    fn record_carries_eval_bytes_beside_jemalloc_allocated() {
        // The pairing is the point: `approx_heap_bytes` has never been checked
        // against a running allocator, and these two fields side by side in one
        // record are what makes it falsifiable. A record carrying only one of
        // them settles nothing.
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(
                now(),
                CATCH_UP,
                depth_bytes(70, 268_435_456, 1_779_387, 1_779_317),
                || Some(11_402_000_000),
            );
        });
        assert!(
            output.contains("eval_bytes_in_flight=268435456"),
            "missing byte accumulator: {output}"
        );
        assert!(
            output.contains("jemalloc_allocated=11402000000"),
            "missing heap field: {output}"
        );
    }

    #[test]
    fn record_carries_eval_bytes_without_a_probe() {
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, depth_bytes(3, 10_752, 1_000, 997), || None);
        });
        assert!(
            output.contains("eval_bytes_in_flight=10752"),
            "the byte accumulator does not depend on the probe: {output}"
        );
    }

    #[test]
    fn record_carries_jemalloc_allocated_when_a_probe_is_wired() {
        // The 2026-08-12 field OOM: killed at validation height 1,779,387 with
        // anon-rss 10.62 GiB. The depth is ≈3,000 evals, NOT the ≈27,000 an
        // earlier revision quoted — that figure came from a per-item estimate
        // 6–8× too low. anon-rss is the measured quantity; the queue depth is
        // derived, and reconciling the two is precisely what this record's
        // `eval_bytes_in_flight`/`jemalloc_allocated` pairing exists to do.
        // See the module doc.
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, depth(3_000, 1_779_387, 1_776_387), || {
                Some(11_402_000_000)
            });
        });
        assert!(
            output.contains("jemalloc_allocated=11402000000"),
            "missing heap field: {output}"
        );
    }

    #[test]
    fn record_omits_jemalloc_allocated_without_a_probe() {
        let mut r = EvalBacklogReporter::default();
        let output = capture(|| {
            r.maybe_emit(now(), CATCH_UP, depth(64, 1_000, 936), || None);
        });
        assert!(
            output.contains("deferred eval backlog"),
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
        let mut r = EvalBacklogReporter::default();
        let t0 = now();
        capture(|| {
            r.maybe_emit(t0, CATCH_UP, depth(1, 1_000, 999), probe);
            assert_eq!(calls.get(), 1, "the emitted record reads the probe once");
            for tick in 1..5u32 {
                r.maybe_emit(
                    t0 + Duration::from_millis(tick as u64 * 100),
                    CATCH_UP,
                    depth(tick, 1_000 + tick, 999),
                    probe,
                );
            }
            assert_eq!(calls.get(), 1, "gated calls must not touch the probe");
            r.maybe_emit(t0, 1, depth(0, 1_000, 1_000), probe);
            assert_eq!(calls.get(), 1, "a tip sweep must not touch the probe either");
        });
    }
}
