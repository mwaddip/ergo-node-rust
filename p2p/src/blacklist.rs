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

use std::collections::HashSet;
use std::net::SocketAddr;
use tokio::sync::Mutex;

/// Set of peer addresses that have been permanently penalized this session.
pub struct Blacklist {
    banned: Mutex<HashSet<SocketAddr>>,
}

impl Blacklist {
    pub fn new() -> Self {
        Self { banned: Mutex::new(HashSet::new()) }
    }

    /// Record a permanent ban for `addr`.
    pub async fn record_permanent(&self, addr: SocketAddr) {
        self.banned.lock().await.insert(addr);
    }

    /// Return all currently-banned addresses, in unspecified order.
    pub async fn list(&self) -> Vec<SocketAddr> {
        self.banned.lock().await.iter().copied().collect()
    }

    /// Whether `addr` is in the banned set.
    pub async fn contains(&self, addr: SocketAddr) -> bool {
        self.banned.lock().await.contains(&addr)
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

    #[tokio::test]
    async fn empty_blacklist_lists_nothing() {
        let bl = Blacklist::new();
        assert!(bl.list().await.is_empty());
        assert!(!bl.contains(addr("1.2.3.4:9000")).await);
    }

    #[tokio::test]
    async fn record_and_query() {
        let bl = Blacklist::new();
        bl.record_permanent(addr("1.2.3.4:9000")).await;
        bl.record_permanent(addr("5.6.7.8:9001")).await;

        assert!(bl.contains(addr("1.2.3.4:9000")).await);
        assert!(bl.contains(addr("5.6.7.8:9001")).await);
        assert!(!bl.contains(addr("9.9.9.9:9000")).await);

        let mut list = bl.list().await;
        list.sort();
        assert_eq!(list, vec![addr("1.2.3.4:9000"), addr("5.6.7.8:9001")]);
    }

    #[tokio::test]
    async fn duplicate_record_is_idempotent() {
        let bl = Blacklist::new();
        bl.record_permanent(addr("1.2.3.4:9000")).await;
        bl.record_permanent(addr("1.2.3.4:9000")).await;
        assert_eq!(bl.list().await.len(), 1);
    }
}
