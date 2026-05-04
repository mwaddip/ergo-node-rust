//! Request tracker and SyncInfo tracker.
//!
//! # RequestTracker Contract
//! - `record(id, peer)`: records that `peer` requested modifier `id`.
//! - `fulfill(id)`: returns and removes the requester for `id`.
//!   Postcondition: entry is removed after fulfill.
//! - `purge_peer(peer)`: removes all entries for a peer.
//!
//! # SyncTracker Contract
//! - `pair(inbound, outbound)`: records that `inbound`'s sync is handled by `outbound`.
//! - `outbound_for(inbound)`: returns the outbound peer handling this inbound peer's sync.
//! - `inbound_for(outbound)`: returns the inbound peer whose sync is handled by this outbound peer.
//! - `purge_peer(peer)`: removes any pairing involving this peer.
//! - Invariant: pairings are bidirectionally consistent — if A→B exists, B→A exists.

use crate::types::{ModifierId, PeerId};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Hard cap on pending requests. At ~40 bytes per entry, 50K ≈ 2MB.
const MAX_PENDING: usize = 50_000;

/// Fraction of entries to evict when the cap is hit (1/4 = 25%).
const EVICT_FRACTION: usize = 4;

pub struct RequestTracker {
    pending: HashMap<ModifierId, (PeerId, Instant)>,
    max_pending: usize,
}

impl Default for RequestTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestTracker {
    pub fn new() -> Self {
        Self { pending: HashMap::new(), max_pending: MAX_PENDING }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { pending: HashMap::new(), max_pending: cap }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn record(&mut self, modifier_id: ModifierId, requester: PeerId) {
        if self.pending.len() >= self.max_pending {
            self.evict();
        }
        self.pending.insert(modifier_id, (requester, Instant::now()));
    }

    /// Bulk-evict ~25% of entries. Evicted requests will never be fulfilled
    /// (the response arrives, nobody to route it to) — acceptable under load
    /// because the alternative is OOM.
    fn evict(&mut self) {
        let to_remove = self.pending.len() / EVICT_FRACTION;
        let keys: Vec<ModifierId> = self.pending.keys().take(to_remove).copied().collect();
        for key in keys {
            self.pending.remove(&key);
        }
    }

    pub fn lookup(&self, modifier_id: &ModifierId) -> Option<PeerId> {
        self.pending.get(modifier_id).map(|(peer, _)| *peer)
    }

    pub fn fulfill(&mut self, modifier_id: &ModifierId) -> Option<PeerId> {
        self.pending.remove(modifier_id).map(|(peer, _)| peer)
    }

    /// Remove entries older than `max_age`. Called periodically by the router.
    pub fn sweep_expired(&mut self, max_age: Duration) {
        let cutoff = Instant::now() - max_age;
        self.pending.retain(|_, (_, ts)| *ts > cutoff);
    }

    pub fn purge_peer(&mut self, peer: PeerId) {
        self.pending.retain(|_, (p, _)| *p != peer);
    }
}

pub struct SyncTracker {
    inbound_to_outbound: HashMap<PeerId, PeerId>,
    outbound_to_inbound: HashMap<PeerId, PeerId>,
}

impl Default for SyncTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncTracker {
    pub fn new() -> Self {
        Self {
            inbound_to_outbound: HashMap::new(),
            outbound_to_inbound: HashMap::new(),
        }
    }

    pub fn pair(&mut self, inbound: PeerId, outbound: PeerId) {
        self.inbound_to_outbound.insert(inbound, outbound);
        self.outbound_to_inbound.insert(outbound, inbound);

        #[cfg(debug_assertions)]
        self.check_invariant();
    }

    pub fn outbound_for(&self, inbound: &PeerId) -> Option<PeerId> {
        self.inbound_to_outbound.get(inbound).copied()
    }

    pub fn inbound_for(&self, outbound: &PeerId) -> Option<PeerId> {
        self.outbound_to_inbound.get(outbound).copied()
    }

    pub fn purge_peer(&mut self, peer: PeerId) {
        if let Some(outbound) = self.inbound_to_outbound.remove(&peer) {
            self.outbound_to_inbound.remove(&outbound);
        }
        if let Some(inbound) = self.outbound_to_inbound.remove(&peer) {
            self.inbound_to_outbound.remove(&inbound);
        }

        #[cfg(debug_assertions)]
        self.check_invariant();
    }

    #[cfg(debug_assertions)]
    fn check_invariant(&self) {
        for (&inb, &outb) in &self.inbound_to_outbound {
            debug_assert_eq!(
                self.outbound_to_inbound.get(&outb),
                Some(&inb),
                "SyncTracker invariant violated: inbound {} → outbound {} but reverse missing",
                inb, outb
            );
        }
        for (&outb, &inb) in &self.outbound_to_inbound {
            debug_assert_eq!(
                self.inbound_to_outbound.get(&inb),
                Some(&outb),
                "SyncTracker invariant violated: outbound {} → inbound {} but reverse missing",
                outb, inb
            );
        }
    }
}
