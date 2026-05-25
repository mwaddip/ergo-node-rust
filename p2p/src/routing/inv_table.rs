//! Inv table: maps modifier IDs to the peer that announced them.
//!
//! # Contract
//! - `record(id, peer)`: associates a modifier ID with a peer.
//!   Postcondition: `lookup(id) == Some(peer)`.
//! - `lookup(id)`: returns the peer that announced the modifier, or None.
//! - `purge_peer(peer)`: removes all entries for a peer.
//!   Postcondition: no entry maps to the purged peer.
//! - Invariant: the table never contains entries for disconnected peers
//!   (enforced by caller invoking `purge_peer` on disconnect).

use crate::types::{ModifierId, PeerId};
use std::collections::HashMap;

/// Hard cap on inv table entries. At ~40 bytes per entry, 100K ≈ 4MB.
const MAX_ENTRIES: usize = 100_000;

/// Fraction of entries to evict when the cap is hit (1/4 = 25%).
const EVICT_FRACTION: usize = 4;

pub struct InvTable {
    entries: HashMap<ModifierId, PeerId>,
}

impl Default for InvTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InvTable {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn record(&mut self, modifier_id: ModifierId, peer: PeerId) {
        if self.entries.len() >= MAX_ENTRIES {
            self.evict();
        }
        self.entries.insert(modifier_id, peer);
    }

    /// Bulk-evict ~25% of entries. HashMap iteration order is arbitrary,
    /// which is fine — we just need to shed load, not be fair about it.
    fn evict(&mut self) {
        let to_remove = self.entries.len() / EVICT_FRACTION;
        let keys: Vec<ModifierId> = self.entries.keys().take(to_remove).copied().collect();
        for key in keys {
            self.entries.remove(&key);
        }
    }

    pub fn lookup(&self, modifier_id: &ModifierId) -> Option<PeerId> {
        self.entries.get(modifier_id).copied()
    }

    pub fn purge_peer(&mut self, peer: PeerId) {
        self.entries.retain(|_, p| *p != peer);

        #[cfg(debug_assertions)]
        self.check_invariant_no_peer(peer);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(debug_assertions)]
    fn check_invariant_no_peer(&self, peer: PeerId) {
        debug_assert!(
            !self.entries.values().any(|p| *p == peer),
            "Invariant violated: Inv table still contains entries for purged peer {}",
            peer
        );
    }
}
