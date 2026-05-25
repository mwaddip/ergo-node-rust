//! In-memory record of peers that have been permanently penalized during
//! this process lifetime.
//!
//! This is a node-local view that complements (rather than replaces) the
//! on-disk PENALTY journal that fail2ban consumes. The journal is the
//! authoritative ban list — iptables enforces those bans across restarts.
//! This in-memory store exists so the REST API can answer
//! `GET /peers/blacklisted` with the current session's view, and so the
//! manual-connect path can reject obviously-banned addresses without
//! shelling out.
//!
//! Only permanent bans are recorded. Transient rate-limiting and
//! misbehavior counters are not surfaced here.
//!
//! # `peer_penalised` kind vocabulary (`facts/journal-events.md`)
//!
//! Every PENALTY emission in this crate uses one of these `kind` values.
//! New emit sites must reuse an existing kind rather than invent a new
//! one; if a new category is genuinely needed, update both
//! `facts/journal-events.md` and `deploy/fail2ban/ergo-proxy.conf`
//! before adding it here.
//!
//! Permanent (fail2ban-actioned, [`Blacklist::record_permanent`]):
//! - `bad_magic`               — frame magic bytes do not match the network
//! - `oversized_frame`         — declared frame body exceeds the 2 MB cap
//! - `bad_checksum`            — blake2b256 checksum disagrees with the body
//! - `handshake_failed`        — peer's handshake refused validation
//! - `address_sanity`          — peer gossiped a bogus address (loopback, private, etc.)
//! - `malformed_peers`         — peer sent a `Peers` body that could not be parsed
//!
//! Misbehavior (logged only, no ban):
//! - `message_parse_failed`    — a single frame's body could not be parsed
//! - `connection_limit_exceeded` — inbound socket dropped at accept time

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Mutex;

/// Set of peer addresses that have been permanently penalized this session.
//
// Backed by a `std::sync::Mutex` so callers in both sync and async
// contexts (e.g. `PeerDb::record`, which runs inside a sync mutex) can
// query the blacklist without ceremony. The critical sections are
// HashSet ops — bounded in time, no awaits.
pub struct Blacklist {
    banned: Mutex<HashSet<SocketAddr>>,
}

impl Blacklist {
    pub fn new() -> Self {
        Self {
            banned: Mutex::new(HashSet::new()),
        }
    }

    /// Record a permanent ban for `addr`.
    pub fn record_permanent(&self, addr: SocketAddr) {
        self.banned
            .lock()
            .expect("blacklist mutex poisoned")
            .insert(addr);
    }

    /// Return all currently-banned addresses, in unspecified order.
    pub fn list(&self) -> Vec<SocketAddr> {
        self.banned
            .lock()
            .expect("blacklist mutex poisoned")
            .iter()
            .copied()
            .collect()
    }

    /// Whether `addr` is in the banned set.
    pub fn contains(&self, addr: SocketAddr) -> bool {
        self.banned
            .lock()
            .expect("blacklist mutex poisoned")
            .contains(&addr)
    }
}

impl Default for Blacklist {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_blacklist_lists_nothing() {
        let bl = Blacklist::new();
        assert!(bl.list().is_empty());
        assert!(!bl.contains(addr("1.2.3.4:9000")));
    }

    #[test]
    fn record_and_query() {
        let bl = Blacklist::new();
        bl.record_permanent(addr("1.2.3.4:9000"));
        bl.record_permanent(addr("5.6.7.8:9001"));

        assert!(bl.contains(addr("1.2.3.4:9000")));
        assert!(bl.contains(addr("5.6.7.8:9001")));
        assert!(!bl.contains(addr("9.9.9.9:9000")));

        let mut list = bl.list();
        list.sort();
        assert_eq!(list, vec![addr("1.2.3.4:9000"), addr("5.6.7.8:9001")]);
    }

    #[test]
    fn duplicate_record_is_idempotent() {
        let bl = Blacklist::new();
        bl.record_permanent(addr("1.2.3.4:9000"));
        bl.record_permanent(addr("1.2.3.4:9000"));
        assert_eq!(bl.list().len(), 1);
    }
}
