//! Exponential backoff for the validation sweep retry, and the single
//! emitter of the `validation_stuck` contract event.
//!
//! When the validator's applied tip (`validated_height()`) fails to
//! advance across a full sweep, the block at the frontier is failing
//! *deterministically* — a consensus divergence, a corrupt-but-on-disk
//! block, or (the motivating case) a script the evaluator wrongly
//! rejects. Left ungated, the sweep re-runs every couple of seconds: the
//! deferred-eval-failure path rolls the validator back and pulls
//! `downloaded_height` down, then the section ticker re-advances it over
//! the still-on-disk sections and re-runs the sweep. That pegs a core and
//! floods the journal for as long as the condition holds — which, being
//! deterministic, is until the binary, the data, or the chain changes. A
//! testnet node spun this way ~every 10s for 13h on a since-fixed
//! `tree_version` script bug.
//!
//! The gate derives entirely from authoritative state — the applied tip —
//! not from a parallel failure counter. A sweep that does not move
//! `validated_height()` past `frontier` is a stall, full stop, whether the
//! failure surfaced as an `apply_state` error or a deferred-eval rollback;
//! the stall *detection* is deliberately blind to which. Consecutive stalls
//! at the same frontier ramp an exponential delay ([`BASE_DELAY`], doubling,
//! capped at [`MAX_DELAY`]). The instant the tip advances (real progress)
//! or the frontier changes (reorg / rollback to a different height), the
//! backoff clears — so a transient one-off failure never accrues delay;
//! only a genuinely wedged frontier ramps to the cap. The block keeps
//! being retried, so the node self-heals the moment a fix, restart, or
//! reorg changes things — just no longer at full speed.
//!
//! ## `validation_stuck`
//!
//! The same frontier-stall count that drives the delay also drives the
//! contract `validation_stuck` WARN event (`facts/journal-events.md`
//! § "Validation and sync", broadened in v1.3). It fires once when the
//! consecutive stall count crosses [`STUCK_THRESHOLD`], at most once per
//! frontier, re-arming when the frontier changes or real progress resets
//! the count. This is the Doctor adapter's primary "node stuck" signal.
//!
//! Because detection is mode-agnostic but the event names the mode, the
//! caller — the sweep, which caught the failure — hands in a
//! [`StallDetail`] (`error_kind`, optional `missing_key`). Before v1.3 a
//! retired per-`apply_state` tracker owned this emission and the
//! deferred-eval wedge bypassed it entirely (it never fired during the
//! 13h stall above); folding it onto the mode-agnostic frontier count
//! closes that gap, so eval-failure stalls now alarm too.
//!
//! Backoff state is in-memory and resets on restart; a restart is a
//! legitimate reset (the operator may have swapped the binary or the data
//! underneath us).

use tokio::time::{Duration, Instant};

/// First delay imposed once a frontier is found stalled.
const BASE_DELAY: Duration = Duration::from_secs(1);

/// Ceiling on the exponential delay (~5 min).
const MAX_DELAY: Duration = Duration::from_secs(300);

/// Consecutive same-frontier stalls before `validation_stuck` is emitted.
/// Pinned by the contract (the old `apply_state` tracker's threshold).
const STUCK_THRESHOLD: u32 = 5;

/// Failure label the sweep attaches to a stall, used only to populate the
/// `validation_stuck` event's `error_kind` / `missing_key` fields when the
/// stall count crosses [`STUCK_THRESHOLD`].
///
/// The backoff's stall *detection* keys on the frontier alone and never
/// inspects this — it is purely the mode label the caller, which caught the
/// failure, hands in so the emitted contract event names the mode. See
/// [`crate::apply_state_error::classify_apply_state_error`] for the
/// `apply_state` path that produces `error_kind` / `missing_key`.
#[derive(Debug, Clone)]
pub(crate) struct StallDetail {
    /// `validation_stuck.error_kind`: an `apply_state` error kind
    /// (`"missing_key"` / `"other"`), or `"script_eval"` for a
    /// deferred-eval rejection.
    pub error_kind: &'static str,
    /// `validation_stuck.missing_key` — present only for the `apply_state`
    /// `missing_key` case (a 32-byte AVL key, hex).
    pub missing_key: Option<String>,
}

impl StallDetail {
    /// A deferred script-eval rejection rolled the tip back.
    pub(crate) fn script_eval() -> Self {
        Self { error_kind: "script_eval", missing_key: None }
    }

    /// A non-specific stall: the sweep broke before/around `apply_state`
    /// for a reason without a richer label (a header/section gap, an
    /// epoch-boundary processing error). Reuses the `apply_state`
    /// catch-all kind.
    pub(crate) fn other() -> Self {
        Self { error_kind: "other", missing_key: None }
    }
}

/// Exponential backoff keyed on the sweep's applied-tip frontier.
///
/// `Default` is the idle state (no backoff). See the module docs for the
/// policy.
#[derive(Debug, Default, Clone)]
pub(crate) struct SweepBackoff {
    armed: Option<Armed>,
}

#[derive(Debug, Clone)]
struct Armed {
    /// The applied tip the sweep could not advance past.
    frontier: u32,
    /// Consecutive stalled sweeps observed at `frontier` (>= 1).
    consecutive: u32,
    /// Earliest instant a sweep at `frontier` may run again.
    next_allowed: Instant,
    /// Whether `validation_stuck` has already fired for this frontier
    /// window. Gates the once-per-frontier emission; cleared when the
    /// frontier changes or progress resets the backoff.
    stuck_emitted: bool,
}

/// Reported when a stall arms or escalates the backoff, so the caller can
/// log the engagement. Returned by [`SweepBackoff::record`] on a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stall {
    /// Consecutive stall count at this frontier (1 on the first stall).
    pub attempt: u32,
    /// Delay now imposed before the next retry.
    pub delay: Duration,
    /// True only on the call that crossed [`STUCK_THRESHOLD`] and emitted
    /// `validation_stuck` for this frontier window. The caller does not
    /// act on it (the backoff already emitted); it lets tests assert the
    /// once-per-frontier emission without scraping the log.
    pub stuck_fired: bool,
}

impl SweepBackoff {
    /// Whether a sweep starting at `applied_tip` should be deferred right
    /// now. True only while a backoff armed for *this* frontier has not yet
    /// elapsed. A different frontier (the tip moved) never defers — that is
    /// resolved as a reset on the next [`record`](Self::record).
    pub(crate) fn should_defer(&self, applied_tip: u32, now: Instant) -> bool {
        match &self.armed {
            Some(a) => a.frontier == applied_tip && now < a.next_allowed,
            None => false,
        }
    }

    /// Record the outcome of a sweep whose applied tip moved from `before`
    /// to `after`, with `detail` labelling the failure for the
    /// `validation_stuck` event.
    ///
    /// - `after > before` is real progress → clear the backoff, return
    ///   `None` (nothing to log).
    /// - otherwise the frontier stalled at `before` → arm or escalate the
    ///   exponential delay and return the [`Stall`] to log. A stall at a
    ///   *different* frontier than the one currently armed restarts the run
    ///   at `attempt = 1`. When the consecutive count first reaches
    ///   [`STUCK_THRESHOLD`] for a frontier, the contract `validation_stuck`
    ///   WARN is emitted (once per frontier window) using `detail`.
    pub(crate) fn record(
        &mut self,
        before: u32,
        after: u32,
        now: Instant,
        detail: StallDetail,
    ) -> Option<Stall> {
        if after > before {
            self.armed = None;
            return None;
        }
        let (consecutive, stuck_emitted) = match &self.armed {
            Some(a) if a.frontier == before => (a.consecutive + 1, a.stuck_emitted),
            _ => (1, false),
        };
        let delay = delay_for(consecutive);
        // Cross the threshold exactly once per frontier window.
        let stuck_fired = !stuck_emitted && consecutive >= STUCK_THRESHOLD;
        if stuck_fired {
            emit_validation_stuck(before, consecutive, &detail);
        }
        self.armed = Some(Armed {
            frontier: before,
            consecutive,
            next_allowed: now + delay,
            stuck_emitted: stuck_emitted || stuck_fired,
        });
        Some(Stall { attempt: consecutive, delay, stuck_fired })
    }

    /// Current consecutive stall count (0 when idle).
    #[cfg(test)]
    pub(crate) fn consecutive(&self) -> u32 {
        self.armed.as_ref().map_or(0, |a| a.consecutive)
    }
}

/// Emit the contract `validation_stuck` WARN. Marker + fields per
/// `facts/journal-events.md` § "Validation and sync".
fn emit_validation_stuck(frontier: u32, attempts: u32, detail: &StallDetail) {
    match &detail.missing_key {
        Some(hex) => tracing::warn!(
            height = frontier as u64,
            attempts = attempts as u64,
            error_kind = detail.error_kind,
            missing_key = %hex,
            "validation stuck"
        ),
        None => tracing::warn!(
            height = frontier as u64,
            attempts = attempts as u64,
            error_kind = detail.error_kind,
            "validation stuck"
        ),
    }
}

/// `BASE_DELAY * 2^(consecutive - 1)`, saturating, capped at `MAX_DELAY`.
fn delay_for(consecutive: u32) -> Duration {
    let shift = consecutive.saturating_sub(1).min(32);
    let base_ms = BASE_DELAY.as_millis() as u64;
    let ms = base_ms.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_millis(ms).min(MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::from_std(std::time::Instant::now())
    }

    /// Capture the default tracing fmt output produced by `f`, so a test
    /// can assert the rendered `validation_stuck` line.
    fn capture<F: FnOnce()>(f: F) -> String {
        use std::io;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct W(Arc<Mutex<Vec<u8>>>);
        impl io::Write for W {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for W {
            type Writer = W;
            fn make_writer(&'a self) -> W {
                self.clone()
            }
        }

        let w = W::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(w.clone())
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = w.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn idle_never_defers() {
        let b = SweepBackoff::default();
        assert!(!b.should_defer(100, now()));
        assert_eq!(b.consecutive(), 0);
    }

    #[test]
    fn repeated_stall_at_same_frontier_grows_and_caps() {
        // 1s, 2s, 4s, ... doubling, then pinned at the 300s ceiling.
        let mut b = SweepBackoff::default();
        let now = now();
        let expected_secs = [1u64, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300];
        for (i, exp) in expected_secs.iter().enumerate() {
            let stall = b
                .record(100, 100, now, StallDetail::other())
                .expect("a non-advancing sweep arms a stall");
            assert_eq!(stall.attempt, (i + 1) as u32, "attempt counts consecutive stalls");
            assert_eq!(
                stall.delay,
                Duration::from_secs(*exp),
                "delay at attempt {}",
                i + 1
            );
            assert_eq!(b.consecutive(), (i + 1) as u32);
        }
    }

    #[test]
    fn progress_clears_to_zero() {
        let mut b = SweepBackoff::default();
        let now = now();
        for _ in 0..5 {
            b.record(100, 100, now, StallDetail::other());
        }
        assert_eq!(b.consecutive(), 5);
        assert!(
            b.record(100, 105, now, StallDetail::other()).is_none(),
            "an advancing tip clears the backoff and logs nothing"
        );
        assert_eq!(b.consecutive(), 0);
        assert!(!b.should_defer(105, now));
    }

    #[test]
    fn frontier_change_restarts_the_run() {
        // A different failing height is a new situation — the exponent
        // restarts at 1 rather than inheriting the old frontier's ramp.
        let mut b = SweepBackoff::default();
        let now = now();
        for _ in 0..5 {
            b.record(100, 100, now, StallDetail::other());
        }
        let s = b
            .record(200, 200, now, StallDetail::other())
            .expect("new frontier still stalls");
        assert_eq!(s.attempt, 1, "frontier change resets the consecutive count");
        assert_eq!(s.delay, Duration::from_secs(1));
    }

    #[test]
    fn defers_only_until_the_delay_elapses() {
        let mut b = SweepBackoff::default();
        let now = now();
        b.record(100, 100, now, StallDetail::other()); // attempt 1 → 1s window
        assert!(b.should_defer(100, now), "armed, nothing elapsed");
        assert!(
            b.should_defer(100, now + Duration::from_millis(999)),
            "still inside the 1s window"
        );
        assert!(
            !b.should_defer(100, now + Duration::from_secs(1)),
            "window elapsed → retry allowed"
        );
        assert!(
            !b.should_defer(101, now),
            "a different frontier is never deferred by this armed backoff"
        );
    }

    // ---- validation_stuck emission (contract, journal-events.md v1.3) ----

    #[test]
    fn validation_stuck_fires_on_fifth_apply_state_stall() {
        // The frontier wedges on the same apply_state error each sweep.
        // Fires once, on the 5th, carrying the apply_state error_kind and
        // (for the missing_key case) the hex key.
        let key_hex = "ab".repeat(32); // 32 bytes
        let detail = || StallDetail {
            error_kind: "missing_key",
            missing_key: Some(key_hex.clone()),
        };
        let mut b = SweepBackoff::default();
        let now = now();
        let mut fired = Vec::new();
        let output = capture(|| {
            for _ in 0..5 {
                let s = b.record(100, 100, now, detail()).expect("stall");
                fired.push(s.stuck_fired);
            }
        });
        assert_eq!(
            fired,
            vec![false, false, false, false, true],
            "validation_stuck fires only on the 5th consecutive stall"
        );
        assert_eq!(output.matches("validation stuck").count(), 1, "{output}");
        assert!(output.contains("height=100"), "{output}");
        assert!(output.contains("attempts=5"), "{output}");
        assert!(output.contains("error_kind=\"missing_key\""), "{output}");
        assert!(output.contains(&format!("missing_key={key_hex}")), "{output}");
    }

    #[test]
    fn validation_stuck_fires_on_fifth_eval_failure_stall() {
        // The path the retired apply_state tracker missed: a deferred-eval
        // rollback pins the frontier. error_kind is `script_eval`, no key.
        let mut b = SweepBackoff::default();
        let now = now();
        let mut fired = Vec::new();
        let output = capture(|| {
            for _ in 0..5 {
                let s = b
                    .record(100, 100, now, StallDetail::script_eval())
                    .expect("stall");
                fired.push(s.stuck_fired);
            }
        });
        assert_eq!(fired, vec![false, false, false, false, true]);
        assert_eq!(output.matches("validation stuck").count(), 1, "{output}");
        assert!(output.contains("error_kind=\"script_eval\""), "{output}");
        assert!(
            !output.contains("missing_key"),
            "eval stalls carry no key: {output}"
        );
    }

    #[test]
    fn validation_stuck_emits_at_most_once_per_frontier() {
        let mut b = SweepBackoff::default();
        let now = now();
        let mut fires = 0;
        let output = capture(|| {
            for _ in 0..10 {
                if b
                    .record(100, 100, now, StallDetail::script_eval())
                    .unwrap()
                    .stuck_fired
                {
                    fires += 1;
                }
            }
        });
        assert_eq!(fires, 1, "exactly one fire across 10 stalls at one frontier");
        assert_eq!(output.matches("validation stuck").count(), 1, "{output}");
    }

    #[test]
    fn validation_stuck_re_arms_after_progress() {
        // 5 stalls at 100 → fire. The tip then advances (a fix landed):
        // progress resets the backoff. A fresh wedge at a new frontier
        // fires again.
        let mut b = SweepBackoff::default();
        let now = now();
        let output = capture(|| {
            for _ in 0..5 {
                b.record(100, 100, now, StallDetail::script_eval());
            }
            assert!(
                b.record(100, 150, now, StallDetail::script_eval()).is_none(),
                "advancing tip clears the backoff"
            );
            assert_eq!(b.consecutive(), 0);
            for _ in 0..5 {
                b.record(150, 150, now, StallDetail::script_eval());
            }
        });
        assert_eq!(output.matches("validation stuck").count(), 2, "{output}");
        assert!(output.contains("height=100"), "{output}");
        assert!(output.contains("height=150"), "{output}");
    }

    #[test]
    fn validation_stuck_re_arms_on_frontier_change() {
        // 5 stalls at 100 (fire), then the frontier moves to 200 with no
        // intervening progress (reorg to a different height). The run
        // restarts at attempt 1 and fires again at 200.
        let mut b = SweepBackoff::default();
        let now = now();
        let output = capture(|| {
            for _ in 0..5 {
                b.record(100, 100, now, StallDetail::other());
            }
            for _ in 0..5 {
                b.record(200, 200, now, StallDetail::other());
            }
        });
        assert_eq!(output.matches("validation stuck").count(), 2, "{output}");
        assert!(output.contains("height=100"), "{output}");
        assert!(output.contains("height=200"), "{output}");
    }
}
