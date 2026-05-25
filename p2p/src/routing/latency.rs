//! Per-peer latency tracking and RRD integration.
//!
//! # Contract
//! - `record_request(modifier_id, target_peer)`: timestamps a forwarded request.
//! - `record_response(modifier_id)`: computes RTT from the stored timestamp.
//!   Postcondition: updates the target peer's latency stats. Removes the timing entry.
//! - `peer_latency(peer)`: returns the exponential moving average RTT for a peer.
//! - `purge_peer(peer)`: removes all timing entries and stats for a peer.
//! - `stats()`: returns (min, avg, max) across all peers with data.

use crate::types::{ModifierId, PeerId};
use std::collections::HashMap;
use std::time::Instant;

const EMA_ALPHA: f64 = 0.3; // Weight for new samples in exponential moving average

/// Timing entry for an in-flight request.
struct PendingTiming {
    target_peer: PeerId,
    sent_at: Instant,
}

/// Per-peer latency statistics.
struct PeerLatency {
    ema_ms: f64,
    sample_count: u64,
}

pub struct LatencyTracker {
    /// In-flight requests: modifier_id → timing info
    pending: HashMap<ModifierId, PendingTiming>,
    /// Per-peer latency stats
    peers: HashMap<PeerId, PeerLatency>,
}

/// Aggregate latency stats across all tracked peers.
pub struct LatencyStats {
    pub min_ms: f64,
    pub avg_ms: f64,
    pub max_ms: f64,
    pub peer_count: usize,
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            peers: HashMap::new(),
        }
    }

    /// Record that we forwarded a request to a target peer.
    pub fn record_request(&mut self, modifier_id: ModifierId, target_peer: PeerId) {
        self.pending.insert(
            modifier_id,
            PendingTiming {
                target_peer,
                sent_at: Instant::now(),
            },
        );
    }

    /// Record that a response arrived. Returns the RTT in milliseconds if timing was tracked.
    pub fn record_response(&mut self, modifier_id: &ModifierId) -> Option<f64> {
        let timing = self.pending.remove(modifier_id)?;
        let rtt_ms = timing.sent_at.elapsed().as_secs_f64() * 1000.0;

        let entry = self.peers.entry(timing.target_peer).or_insert(PeerLatency {
            ema_ms: rtt_ms,
            sample_count: 0,
        });

        if entry.sample_count == 0 {
            entry.ema_ms = rtt_ms;
        } else {
            entry.ema_ms = EMA_ALPHA * rtt_ms + (1.0 - EMA_ALPHA) * entry.ema_ms;
        }
        entry.sample_count += 1;

        Some(rtt_ms)
    }

    /// Get the EMA latency for a peer in milliseconds, if available.
    pub fn peer_latency(&self, peer: &PeerId) -> Option<f64> {
        self.peers.get(peer).map(|p| p.ema_ms)
    }

    /// Remove all data for a disconnected peer.
    pub fn purge_peer(&mut self, peer: PeerId) {
        self.pending.retain(|_, t| t.target_peer != peer);
        self.peers.remove(&peer);
    }

    /// Compute aggregate stats across all tracked peers.
    /// Returns None if no peers have latency data.
    pub fn stats(&self) -> Option<LatencyStats> {
        if self.peers.is_empty() {
            return None;
        }

        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut sum = 0.0;
        let mut count = 0usize;

        for entry in self.peers.values() {
            if entry.sample_count > 0 {
                min = min.min(entry.ema_ms);
                max = max.max(entry.ema_ms);
                sum += entry.ema_ms;
                count += 1;
            }
        }

        if count == 0 {
            return None;
        }

        Some(LatencyStats {
            min_ms: min,
            avg_ms: sum / count as f64,
            max_ms: max,
            peer_count: count,
        })
    }

    /// Pick the fastest peer from a set of candidates.
    pub fn fastest_peer(&self, candidates: &[PeerId]) -> Option<PeerId> {
        candidates
            .iter()
            .filter_map(|p| self.peer_latency(p).map(|lat| (*p, lat)))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(p, _)| p)
    }
}
