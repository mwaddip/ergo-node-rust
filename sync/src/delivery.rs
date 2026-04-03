//! Delivery tracker for modifier requests.
//!
//! Tracks pending modifier requests with timeouts and retry logic,
//! matching the JVM's `DeliveryTracker` behavior. The lossy Ergo sync
//! protocol (SyncInfo → Inv → Request → Response) silently drops at
//! every step — this tracker makes it robust via 10-second retries.

use std::collections::HashMap;

use enr_p2p::types::PeerId;
use tokio::time::{Duration, Instant};

/// Default timeout before re-requesting a modifier (JVM: `deliveryTimeout`).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default max re-request attempts before abandoning (JVM: `maxDeliveryChecks`).
const DEFAULT_MAX_CHECKS: u32 = 100;

/// Notification from the pipeline to the delivery tracker.
/// Sent via channel — the sync machine receives and dispatches to the tracker.
pub enum DeliveryEvent {
    /// Modifier IDs successfully received by the pipeline.
    Received(Vec<[u8; 32]>),
    /// Modifier IDs evicted from the LRU buffer before chaining.
    Evicted(Vec<[u8; 32]>),
    /// Pipeline needs a specific modifier to complete a reorg.
    /// The sync machine should request it from any available peer.
    NeedModifier { type_id: u8, id: [u8; 32] },
    /// A chain reorg occurred. The sync machine should adjust its section
    /// queue and full_block_height watermark.
    Reorg {
        fork_point: u32,
        old_tip: u32,
        new_tip: u32,
    },
}

/// A pending modifier request.
struct PendingRequest {
    peer: PeerId,
    requested_at: Instant,
    checks: u32,
}

/// A modifier that needs (re-)requesting.
pub struct RetryAction {
    pub id: [u8; 32],
    /// The peer that failed to deliver. Caller should pick a different one.
    pub failed_peer: PeerId,
}

/// Result of a timeout check.
pub struct CheckResult {
    /// Timed-out requests — need re-request from a different peer.
    pub retries: Vec<RetryAction>,
    /// Evicted modifier IDs — need fresh request from any peer.
    pub fresh: Vec<[u8; 32]>,
    /// Abandoned IDs (exceeded max_checks).
    pub abandoned: Vec<[u8; 32]>,
}

/// Tracks pending modifier requests with timeout-based retry.
///
/// State machine per modifier ID:
/// ```text
/// Unknown → Requested → Received
///               ↓
///          (timeout) → re-request (different peer)
///               ↓
///          (max checks) → Abandoned
/// ```
pub struct DeliveryTracker {
    pending: HashMap<[u8; 32], PendingRequest>,
    /// IDs that need requesting (from LRU eviction in the modifier buffer).
    evicted: Vec<[u8; 32]>,
    timeout: Duration,
    max_checks: u32,
}

impl DeliveryTracker {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            evicted: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_checks: DEFAULT_MAX_CHECKS,
        }
    }

    pub fn with_config(timeout: Duration, max_checks: u32) -> Self {
        Self {
            pending: HashMap::new(),
            evicted: Vec::new(),
            timeout,
            max_checks,
        }
    }

    /// Mark modifier IDs as requested from a peer.
    pub fn mark_requested(&mut self, ids: &[[u8; 32]], peer: PeerId) {
        let now = Instant::now();
        for id in ids {
            self.pending.insert(*id, PendingRequest {
                peer,
                requested_at: now,
                checks: 0,
            });
        }
    }

    /// Mark a modifier as received. Returns true if it was pending.
    pub fn mark_received(&mut self, id: &[u8; 32]) -> bool {
        self.pending.remove(id).is_some()
    }

    /// Schedule modifier IDs for re-request (called when the LRU buffer evicts).
    /// These were already received once but evicted before chaining.
    pub fn schedule_rerequest(&mut self, ids: &[[u8; 32]]) {
        // Remove from pending if present (they were already received)
        for id in ids {
            self.pending.remove(id);
        }
        self.evicted.extend_from_slice(ids);
    }

    /// Check for timed-out requests and queued re-requests.
    /// Caller handles the actual network sends.
    pub fn check_timeouts(&mut self) -> CheckResult {
        let mut retries = Vec::new();
        let mut abandoned = Vec::new();

        let now = Instant::now();
        let mut to_remove = Vec::new();

        for (id, req) in &mut self.pending {
            if now.duration_since(req.requested_at) >= self.timeout {
                req.checks += 1;
                if req.checks >= self.max_checks {
                    abandoned.push(*id);
                    to_remove.push(*id);
                } else {
                    retries.push(RetryAction {
                        id: *id,
                        failed_peer: req.peer,
                    });
                    req.requested_at = now;
                }
            }
        }

        for id in &to_remove {
            self.pending.remove(id);
        }

        let fresh = std::mem::take(&mut self.evicted);

        CheckResult { retries, fresh, abandoned }
    }

    /// Remove all pending requests for a peer. Returns their IDs for re-request.
    pub fn purge_peer(&mut self, peer: PeerId) -> Vec<[u8; 32]> {
        let orphaned: Vec<[u8; 32]> = self.pending.iter()
            .filter(|(_, req)| req.peer == peer)
            .map(|(id, _)| *id)
            .collect();

        for id in &orphaned {
            self.pending.remove(id);
        }

        orphaned
    }

    /// Number of in-flight requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Number of evicted IDs queued for re-request.
    pub fn evicted_count(&self) -> usize {
        self.evicted.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> PeerId {
        PeerId(n as u64)
    }

    fn id(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn mark_requested_and_received() {
        let mut tracker = DeliveryTracker::new();
        tracker.mark_requested(&[id(1), id(2), id(3)], peer(1));
        assert_eq!(tracker.pending_count(), 3);

        assert!(tracker.mark_received(&id(2)));
        assert_eq!(tracker.pending_count(), 2);

        // Receiving unknown ID returns false
        assert!(!tracker.mark_received(&id(99)));
    }

    #[test]
    fn check_timeouts_before_expiry() {
        let mut tracker = DeliveryTracker::new();
        tracker.mark_requested(&[id(1)], peer(1));

        let result = tracker.check_timeouts();
        assert!(result.retries.is_empty());
        assert!(result.fresh.is_empty());
        assert!(result.abandoned.is_empty());
        assert_eq!(tracker.pending_count(), 1);
    }

    #[tokio::test]
    async fn check_timeouts_after_expiry() {
        let mut tracker = DeliveryTracker::new();
        tracker.timeout = Duration::from_millis(10);
        tracker.mark_requested(&[id(1), id(2)], peer(1));

        tokio::time::sleep(Duration::from_millis(20)).await;

        let result = tracker.check_timeouts();
        assert_eq!(result.retries.len(), 2);
        assert!(result.abandoned.is_empty());
        // Still pending — just retried
        assert_eq!(tracker.pending_count(), 2);

        // Verify failed_peer is reported
        for retry in &result.retries {
            assert_eq!(retry.failed_peer, peer(1));
        }
    }

    #[tokio::test]
    async fn max_checks_abandons() {
        let mut tracker = DeliveryTracker::new();
        tracker.timeout = Duration::from_millis(1);
        tracker.max_checks = 2;
        tracker.mark_requested(&[id(1)], peer(1));

        tokio::time::sleep(Duration::from_millis(5)).await;
        let r1 = tracker.check_timeouts();
        assert_eq!(r1.retries.len(), 1); // check 1

        tokio::time::sleep(Duration::from_millis(5)).await;
        let r2 = tracker.check_timeouts();
        assert_eq!(r2.abandoned.len(), 1); // check 2 = max_checks
        assert!(r2.retries.is_empty());
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn purge_peer_returns_orphaned() {
        let mut tracker = DeliveryTracker::new();
        tracker.mark_requested(&[id(1), id(2)], peer(1));
        tracker.mark_requested(&[id(3)], peer(2));

        let orphaned = tracker.purge_peer(peer(1));
        assert_eq!(orphaned.len(), 2);
        assert_eq!(tracker.pending_count(), 1); // peer(2) still there
    }

    #[test]
    fn schedule_rerequest_queues_for_check() {
        let mut tracker = DeliveryTracker::new();

        // Simulate: modifier was received, then evicted from buffer
        tracker.mark_requested(&[id(1)], peer(1));
        tracker.mark_received(&id(1));
        tracker.schedule_rerequest(&[id(1)]);

        assert_eq!(tracker.evicted_count(), 1);
        assert_eq!(tracker.pending_count(), 0);

        let result = tracker.check_timeouts();
        assert_eq!(result.fresh.len(), 1);
        assert_eq!(result.fresh[0], id(1));
        assert_eq!(tracker.evicted_count(), 0); // drained
    }

    #[test]
    fn schedule_rerequest_clears_pending() {
        let mut tracker = DeliveryTracker::new();

        // Modifier is still in pending (not yet received) but buffer evicts it
        // This shouldn't happen normally, but defensive: clear from pending
        tracker.mark_requested(&[id(1)], peer(1));
        tracker.schedule_rerequest(&[id(1)]);

        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.evicted_count(), 1);
    }
}
