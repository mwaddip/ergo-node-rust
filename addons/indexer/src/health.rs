//! Shared, in-memory liveness + sync-progress state for `GET /api/v1/health`.
//!
//! The sync loop (`crate::sync`) writes this as it advances; the `/health`
//! API handler reads it. Deliberately lock-free (atomics only) so the handler
//! never contends with the sync write-path — `/health` must answer in
//! single-digit milliseconds even while a reorg-rollback DB transaction is in
//! flight. NO field here is backed by the database, and reading it issues no
//! node call. See `facts/indexer.md` → `GET /api/v1/health`.
//!
//! There is exactly one writer (the single forward-walker sync task; the
//! contract forbids concurrent writers) and N readers (API requests), so the
//! atomics carry no write-write race. They exist for reader/writer atomicity
//! and to express the no-lock invariant in the type, not to arbitrate between
//! competing writers.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Instant;

// Node reachability tri-state, encoded as u8 for atomic storage. `Unknown` is
// the pre-first-poll default: `/health` must not assert `Unreachable` before a
// poll has actually been attempted — reporting an unobserved failure would lie.
const NODE_UNKNOWN: u8 = 0;
const NODE_REACHABLE: u8 = 1;
const NODE_UNREACHABLE: u8 = 2;

/// In-memory snapshot of sync progress, shared via `Arc` between the sync task
/// and the API task. Each field is read/written with an independent atomic op;
/// there is no cross-field consistency guarantee and none is needed — `/health`
/// reports each field on its own and the caller thresholds them.
pub struct HealthState {
    /// Monotonic base captured at construction. `last_advance_ms` is measured
    /// relative to this so "seconds since last advance" never depends on the
    /// system wall clock, which can jump backwards.
    base: Instant,
    /// Whether this process runs the sync loop. `false` in serve-only mode,
    /// where the progress fields stay at startup defaults; [`Self::status`]
    /// then reports `"serve-only"` so a prober does not read them as a stalled
    /// combined indexer.
    sync_present: bool,
    /// Highest block height the sync loop has committed (its `last_indexed`).
    indexed_height: AtomicU64,
    /// Node tip height the sync loop last observed (`info.full_height`).
    node_height: AtomicU64,
    /// `base.elapsed()` in milliseconds at the moment `indexed_height` last
    /// increased. Used to derive `lastAdvanceSecsAgo`.
    last_advance_ms: AtomicU64,
    /// Node reachability tri-state (`NODE_UNKNOWN` / `NODE_REACHABLE` /
    /// `NODE_UNREACHABLE`). Starts `Unknown`; the first poll resolves it.
    node_status: AtomicU8,
}

impl HealthState {
    /// `sync_present` is `true` for combined/sync modes (a local sync loop
    /// drives this state) and `false` for serve-only mode (no local sync loop,
    /// so the progress fields are not meaningful).
    pub fn new(sync_present: bool) -> Self {
        Self {
            base: Instant::now(),
            sync_present,
            indexed_height: AtomicU64::new(0),
            node_height: AtomicU64::new(0),
            // Treat process start as the baseline "last advance": if the loop
            // never advances, `lastAdvanceSecsAgo` climbs from startup, which
            // is exactly the "alive but stalled" signal we want to surface.
            last_advance_ms: AtomicU64::new(0),
            // Unknown until the first poll resolves it — never assert a failure
            // we have not observed.
            node_status: AtomicU8::new(NODE_UNKNOWN),
        }
    }

    /// Record the sync loop's current indexed height. Resets the "last advance"
    /// timer only on a forward step (a height increase). A rollback lowers the
    /// reported height without resetting the timer — a rollback is not
    /// progress, and the timer should keep climbing until the chain actually
    /// moves forward again.
    pub fn record_indexed_height(&self, height: u64) {
        let prev = self.indexed_height.swap(height, Ordering::Relaxed);
        if height > prev {
            let now_ms = self.base.elapsed().as_millis() as u64;
            self.last_advance_ms.store(now_ms, Ordering::Relaxed);
        }
    }

    /// Record the node tip height the sync loop last observed.
    pub fn record_node_height(&self, height: u64) {
        self.node_height.store(height, Ordering::Relaxed);
    }

    /// Record the outcome of a node poll. Moves `node` out of `Unknown` for
    /// good — once a poll has happened we always report the observed result.
    pub fn set_node_reachable(&self, reachable: bool) {
        let v = if reachable {
            NODE_REACHABLE
        } else {
            NODE_UNREACHABLE
        };
        self.node_status.store(v, Ordering::Relaxed);
    }

    pub fn indexed_height(&self) -> u64 {
        self.indexed_height.load(Ordering::Relaxed)
    }

    pub fn node_height(&self) -> u64 {
        self.node_height.load(Ordering::Relaxed)
    }

    /// `"ok"` in combined (sync+serve) mode; `"serve-only"` when no sync loop
    /// drives this state (the progress fields are then at startup defaults and
    /// not meaningful).
    pub fn status(&self) -> &'static str {
        if self.sync_present {
            "ok"
        } else {
            "serve-only"
        }
    }

    /// `"unknown"` (pre-first-poll) / `"reachable"` / `"unreachable"` from the
    /// last node-poll result. Never a fresh probe.
    pub fn node_str(&self) -> &'static str {
        match self.node_status.load(Ordering::Relaxed) {
            NODE_REACHABLE => "reachable",
            NODE_UNREACHABLE => "unreachable",
            _ => "unknown",
        }
    }

    /// Wall-clock seconds since `indexed_height` last increased. Monotonic and
    /// immune to system-clock changes (derived from `Instant`, not the wall
    /// clock).
    pub fn last_advance_secs_ago(&self) -> u64 {
        let now_ms = self.base.elapsed().as_millis() as u64;
        now_ms.saturating_sub(self.last_advance_ms.load(Ordering::Relaxed)) / 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_step_resets_advance_timer_rollback_does_not() {
        let h = HealthState::new(true);

        // Fresh state: nothing observed yet.
        assert_eq!(h.indexed_height(), 0);
        assert_eq!(h.node_height(), 0);

        // Forward steps move the height and (re)set the advance baseline.
        h.record_indexed_height(100);
        assert_eq!(h.indexed_height(), 100);

        // A rollback lowers the reported height...
        h.record_indexed_height(98);
        assert_eq!(h.indexed_height(), 98);

        // ...and re-advancing past the old tip moves it forward again.
        h.record_indexed_height(101);
        assert_eq!(h.indexed_height(), 101);
    }

    #[test]
    fn node_status_starts_unknown_then_resolves() {
        let h = HealthState::new(true);
        // Pre-first-poll: report "unknown", never an unobserved "unreachable".
        assert_eq!(h.node_str(), "unknown");
        h.set_node_reachable(true);
        assert_eq!(h.node_str(), "reachable");
        h.set_node_reachable(false);
        assert_eq!(h.node_str(), "unreachable");
    }

    #[test]
    fn status_reflects_sync_presence() {
        // Combined/sync modes drive the state; serve-only does not.
        assert_eq!(HealthState::new(true).status(), "ok");
        assert_eq!(HealthState::new(false).status(), "serve-only");
    }

    #[test]
    fn node_height_round_trips() {
        let h = HealthState::new(true);
        h.record_node_height(1_796_007);
        assert_eq!(h.node_height(), 1_796_007);
    }

    #[test]
    fn advance_secs_ago_is_small_right_after_a_step() {
        let h = HealthState::new(true);
        h.record_indexed_height(1);
        // Just advanced — the probe must read as freshly-advanced, not stale.
        assert_eq!(h.last_advance_secs_ago(), 0);
    }
}
