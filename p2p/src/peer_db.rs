//! In-memory peer registry that backs `GetPeers` responses, populates
//! candidates for the outbound manager, and feeds `P2pNode::all_peers`.
//!
//! Persistence is owned by a separate [`PeerStorage`] trait so the
//! store crate can back it with redb without `p2p` knowing about disks.
//! All `PeerDb` operations are synchronous; callers wrap it in an
//! `Arc<Mutex<PeerDb>>` at the integration points where the router and
//! the outbound manager both need access.
//!
//! Every entry is either *observed* (we completed a handshake with the
//! address) or *hearsay* (a third party named it in a `Peers` body).
//! `GetPeers` answers come from the observed set only, the outbound
//! dialer draws from both, and eviction takes hearsay first and then
//! the most crowded address group.
//!
//! See `facts/p2p-peerdb.md` for the contract.

use crate::blacklist::Blacklist;
use rand::seq::IndexedRandom;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// Default soft cap on the number of entries kept in memory.
pub const DEFAULT_CAP: usize = 1000;

/// A persisted peer entry. Originates from either our own handshake
/// (observed) or a `Peers` gossip from a third party (hearsay);
/// `last_handshake_ms` keeps the two apart. See `facts/p2p-peerdb.md`
/// § Observed and hearsay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    pub address: SocketAddr,
    /// Unix epoch ms of the most recent *mention* of this address: a
    /// handshake we completed, a `Peers` entry from a third party, a
    /// manual add.
    pub last_seen_ms: u64,
    /// Unix epoch ms of the most recent handshake **we completed with
    /// this address**. `0` means never: the entry is hearsay. Only the
    /// handshake path writes it; gossip cannot raise it.
    pub last_handshake_ms: u64,
    pub agent_name: String,
    pub node_name: String,
    pub version: (u8, u8, u8),
    pub features: Vec<(u8, Vec<u8>)>,
}

impl PeerRecord {
    /// Whether we have ever completed a handshake with this address.
    /// Observed entries are what the node tells others about and what it
    /// keeps under cap pressure; hearsay is only ever dialed.
    pub fn is_observed(&self) -> bool {
        self.last_handshake_ms > 0
    }
}

/// Error type returned by `PeerStorage` implementations. Boxed so the
/// trait does not need a generic associated type — callers that need a
/// concrete error can wrap their own.
pub type PeerStorageError = Box<dyn std::error::Error + Send + Sync>;

/// Persistence backing for [`PeerDb`].
///
/// Implementors:
/// - `load_all` is called once at construction; failure is fatal.
/// - `put` is write-through on every `record()`; expected to be fast
///   (single redb write). Failures are logged by [`PeerDb`] and not
///   propagated — the in-memory state becomes ephemeral until the next
///   successful write.
/// - `delete` is called on `forget()` and on eviction.
pub trait PeerStorage: Send + Sync {
    fn load_all(&self) -> Result<Vec<PeerRecord>, PeerStorageError>;
    fn put(&self, record: &PeerRecord) -> Result<(), PeerStorageError>;
    fn delete(&self, addr: SocketAddr) -> Result<(), PeerStorageError>;
}

/// In-memory peer registry. Wrap in `Arc<Mutex<PeerDb>>` at the
/// integration point — the type itself is not internally synchronised.
pub struct PeerDb {
    entries: HashMap<SocketAddr, PeerRecord>,
    cap: usize,
    blacklist: Arc<Blacklist>,
    storage: Box<dyn PeerStorage>,
    /// Addresses considered "self" — every listener's declared address
    /// at startup. Records with these addresses are dropped at every
    /// entry point (gossip-time `record`, startup `load_all` filter).
    /// Captured by value at construction; not tracked after.
    self_addresses: HashSet<SocketAddr>,
}

impl PeerDb {
    /// Construct a `PeerDb` and repopulate it from `storage`.
    ///
    /// `self_addresses` is the set of our own declared listener
    /// addresses (post-UPnP, post-IPv6-auto-detect). Persisted records
    /// matching one of these addresses are dropped from the in-memory
    /// table but left on disk — a self-address today may legitimately
    /// be a different host tomorrow (e.g. IPv6 prefix change).
    ///
    /// # Errors
    /// Returns the storage error from `load_all`. The caller (main
    /// crate) treats this as fatal — let the node fail to start rather
    /// than run with an empty peer table after losing persistence.
    pub fn new(
        storage: Box<dyn PeerStorage>,
        blacklist: Arc<Blacklist>,
        cap: usize,
        self_addresses: HashSet<SocketAddr>,
    ) -> Result<Self, PeerStorageError> {
        let loaded = storage.load_all()?;
        let mut entries = HashMap::with_capacity(loaded.len());
        for rec in loaded {
            if self_addresses.contains(&rec.address) {
                continue;
            }
            entries.insert(rec.address, rec);
        }
        Ok(Self {
            entries,
            cap,
            blacklist,
            storage,
            self_addresses,
        })
    }

    /// Insert or update a record.
    ///
    /// Drops blacklisted addresses and self-loop candidates silently.
    /// Merges `last_seen_ms` and `last_handshake_ms` with any prior
    /// value, each by max and independently, so a gossip record
    /// (handshake `0`) never lowers a handshake stamp; overwrites the
    /// rest of the fields. Evicts one entry (see [`Self::evict_oldest`])
    /// when a new address would exceed the cap.
    pub fn record(&mut self, mut record: PeerRecord) {
        if self.blacklist.contains(record.address) {
            return;
        }
        if self.self_addresses.contains(&record.address) {
            return;
        }
        if let Some(prior) = self.entries.get(&record.address) {
            record.last_seen_ms = record.last_seen_ms.max(prior.last_seen_ms);
            record.last_handshake_ms = record.last_handshake_ms.max(prior.last_handshake_ms);
        } else if self.entries.len() >= self.cap {
            self.evict_oldest();
        }
        if let Err(e) = self.storage.put(&record) {
            tracing::warn!(addr = %record.address, error = %e, "PeerStorage::put failed");
        }
        self.entries.insert(record.address, record);
    }

    /// Drop a peer entry.
    pub fn forget(&mut self, addr: SocketAddr) {
        if self.entries.remove(&addr).is_some() {
            if let Err(e) = self.storage.delete(addr) {
                tracing::warn!(%addr, error = %e, "PeerStorage::delete failed");
            }
        }
    }

    pub fn get(&self, addr: SocketAddr) -> Option<&PeerRecord> {
        self.entries.get(&addr)
    }

    /// Up to `limit` observed entries (`last_handshake_ms > 0`), most
    /// recent handshake first, excluding addresses in `exclude_addrs`
    /// and any blacklisted address. Hearsay is never returned: this
    /// feeds `Peers` responses, and an address we have only heard of is
    /// never vouched for to a third party (JVM `PeerManager.SeenPeers`
    /// propagates only peers with a completed handshake).
    pub fn observed(&self, limit: usize, exclude_addrs: &HashSet<SocketAddr>) -> Vec<PeerRecord> {
        if limit == 0 {
            return Vec::new();
        }
        let mut candidates: Vec<&PeerRecord> = self
            .entries
            .values()
            .filter(|r| r.is_observed())
            .filter(|r| !exclude_addrs.contains(&r.address))
            .filter(|r| !self.blacklist.contains(r.address))
            .collect();
        candidates.sort_by(|a, b| b.last_handshake_ms.cmp(&a.last_handshake_ms));
        candidates.truncate(limit);
        candidates.into_iter().cloned().collect()
    }

    /// One entry chosen uniformly at random, hearsay included, from those
    /// not in `exclude_addrs`, not blacklisted, and accepted by
    /// `eligible` (the caller's bogus-address filter), preferring entries
    /// whose address group (see [`AddressGroup`]) matches no address in
    /// `connected`; when every eligible entry shares a group with a
    /// connected peer, from all eligible entries. `None` if nothing is
    /// eligible.
    ///
    /// Feeds the outbound-fill dialer; dialing is how hearsay becomes
    /// observation. JVM `PeerManager.RandomPeerExcluding`: the random
    /// choice with group diversity is what makes eclipse-by-gossip
    /// expensive. An attacker controls which addresses we hear about,
    /// not which one we pick, nor how many of our slots its group holds.
    pub fn dial_candidate(
        &self,
        exclude_addrs: &HashSet<SocketAddr>,
        connected: &[SocketAddr],
        eligible: impl Fn(&PeerRecord) -> bool,
    ) -> Option<PeerRecord> {
        let candidates: Vec<&PeerRecord> = self
            .entries
            .values()
            .filter(|r| !exclude_addrs.contains(&r.address))
            .filter(|r| !self.blacklist.contains(r.address))
            .filter(|r| eligible(r))
            .collect();
        let connected_groups: HashSet<AddressGroup> = connected.iter().map(address_group).collect();
        let preferred: Vec<&PeerRecord> = candidates
            .iter()
            .copied()
            .filter(|r| !connected_groups.contains(&address_group(&r.address)))
            .collect();
        let pool = if preferred.is_empty() {
            &candidates
        } else {
            &preferred
        };
        pool.choose(&mut rand::rng()).map(|r| (*r).clone())
    }

    /// Every entry. Used by `P2pNode::all_peers` / `GET /peers/all`.
    pub fn all(&self) -> Vec<PeerRecord> {
        self.entries.values().cloned().collect()
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Make room for one new address. Hearsay goes first: the entry with
    /// the smallest `last_seen_ms`, so gossip can fill the table but never
    /// pushes out a peer we have actually talked to while any hearsay
    /// remains. Among observed entries crowding pays before age: the
    /// oldest handshake inside the most crowded address group (see
    /// [`Self::crowded_observed`]), and only when every group holds a
    /// single entry the oldest handshake overall.
    fn evict_oldest(&mut self) {
        let victim = self
            .entries
            .values()
            .filter(|r| !r.is_observed())
            .min_by_key(|r| r.last_seen_ms)
            .or_else(|| self.crowded_observed())
            .or_else(|| self.entries.values().min_by_key(|r| r.last_handshake_ms))
            .map(|r| r.address);
        if let Some(addr) = victim {
            self.entries.remove(&addr);
            if let Err(e) = self.storage.delete(addr) {
                tracing::warn!(%addr, error = %e, "PeerStorage::delete (eviction) failed");
            }
        }
    }

    /// The observed entry crowding pays with: the oldest handshake among
    /// the entries of the address group (or groups, on a tie) holding the
    /// most observed entries, provided that count exceeds one. `None`
    /// when every group holds at most one observed entry. A host or site
    /// that mints observed entries by handshaking under many declared
    /// ports only ever evicts its own.
    fn crowded_observed(&self) -> Option<&PeerRecord> {
        let mut per_group: HashMap<AddressGroup, usize> = HashMap::new();
        for r in self.entries.values().filter(|r| r.is_observed()) {
            *per_group.entry(address_group(&r.address)).or_default() += 1;
        }
        let most = per_group.values().copied().max()?;
        if most <= 1 {
            return None;
        }
        self.entries
            .values()
            .filter(|r| r.is_observed())
            .filter(|r| per_group.get(&address_group(&r.address)).copied() == Some(most))
            .min_by_key(|r| r.last_handshake_ms)
    }
}

/// The unit both dial diversity and eviction crowding reason in: a /16
/// for IPv4 and a /64 for IPv6, one customer allocation. JVM
/// `PeerManager.getIpGroup` takes the first two bytes of either family;
/// for IPv6 that is a /16 and groups most of a continent, which would
/// make diversity meaningless and crowding unbounded on the family this
/// node is built for. The divergence is deliberate (`facts/p2p-peerdb.md`
/// § Address group).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AddressGroup {
    V4([u8; 2]),
    V6([u8; 8]),
}

fn address_group(addr: &SocketAddr) -> AddressGroup {
    match addr.ip() {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            AddressGroup::V4([o[0], o[1]])
        }
        IpAddr::V6(ip) => {
            let o = ip.octets();
            let mut prefix = [0u8; 8];
            prefix.copy_from_slice(&o[..8]);
            AddressGroup::V6(prefix)
        }
    }
}

/// In-memory `PeerStorage` for tests and for callers that do not yet
/// have a persistent backend wired up (e.g. legacy `Router::new()`).
///
/// Records every call into a shared log so tests can assert on the
/// write-through behaviour.
pub struct MemoryPeerStorage {
    inner: std::sync::Mutex<MemoryPeerStorageInner>,
}

struct MemoryPeerStorageInner {
    entries: HashMap<SocketAddr, PeerRecord>,
    pub_log: Vec<MemoryStorageOp>,
}

/// One recorded storage operation. Tests inspect a `MemoryPeerStorage`
/// via [`MemoryPeerStorage::ops`] to assert that write-through fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryStorageOp {
    Put(SocketAddr),
    Delete(SocketAddr),
}

impl MemoryPeerStorage {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(MemoryPeerStorageInner {
                entries: HashMap::new(),
                pub_log: Vec::new(),
            }),
        }
    }

    /// Snapshot of the operations recorded so far.
    pub fn ops(&self) -> Vec<MemoryStorageOp> {
        self.inner.lock().expect("poisoned").pub_log.clone()
    }

    /// Preload a record (test helper — bypasses the op log).
    pub fn preload(&self, record: PeerRecord) {
        let mut inner = self.inner.lock().expect("poisoned");
        inner.entries.insert(record.address, record);
    }
}

impl Default for MemoryPeerStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerStorage for MemoryPeerStorage {
    fn load_all(&self) -> Result<Vec<PeerRecord>, PeerStorageError> {
        Ok(self
            .inner
            .lock()
            .expect("poisoned")
            .entries
            .values()
            .cloned()
            .collect())
    }

    fn put(&self, record: &PeerRecord) -> Result<(), PeerStorageError> {
        let mut inner = self.inner.lock().expect("poisoned");
        inner.entries.insert(record.address, record.clone());
        inner.pub_log.push(MemoryStorageOp::Put(record.address));
        Ok(())
    }

    fn delete(&self, addr: SocketAddr) -> Result<(), PeerStorageError> {
        let mut inner = self.inner.lock().expect("poisoned");
        inner.entries.remove(&addr);
        inner.pub_log.push(MemoryStorageOp::Delete(addr));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// A gossip-sourced record: `last_handshake_ms = 0`.
    fn hearsay_rec(s: &str, last_seen: u64) -> PeerRecord {
        PeerRecord {
            address: addr(s),
            last_seen_ms: last_seen,
            last_handshake_ms: 0,
            agent_name: "test".into(),
            node_name: "node".into(),
            version: (5, 0, 25),
            features: vec![],
        }
    }

    /// A handshake-sourced record: both stamps set to `handshake`.
    fn observed_rec(s: &str, handshake: u64) -> PeerRecord {
        PeerRecord {
            last_handshake_ms: handshake,
            ..hearsay_rec(s, handshake)
        }
    }

    fn db_with_cap(cap: usize) -> (PeerDb, Arc<Blacklist>) {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let db = PeerDb::new(storage, bl.clone(), cap, HashSet::new()).expect("new ok");
        (db, bl)
    }

    /// Draw `n` dial candidates and return the distinct addresses seen.
    /// Panics if any draw comes back empty.
    fn draw_addrs(
        db: &PeerDb,
        n: usize,
        exclude: &HashSet<SocketAddr>,
        connected: &[SocketAddr],
        eligible: impl Fn(&PeerRecord) -> bool,
    ) -> HashSet<SocketAddr> {
        (0..n)
            .map(|_| {
                db.dial_candidate(exclude, connected, &eligible)
                    .expect("a candidate exists")
                    .address
            })
            .collect()
    }

    fn set(addrs: &[&str]) -> HashSet<SocketAddr> {
        addrs.iter().map(|s| addr(s)).collect()
    }

    #[test]
    fn record_inserts_and_get_returns_entry() {
        let (mut db, _) = db_with_cap(10);
        let r = hearsay_rec("1.2.3.4:9030", 1000);
        db.record(r.clone());
        assert_eq!(db.get(addr("1.2.3.4:9030")), Some(&r));
        assert_eq!(db.count(), 1);
    }

    #[test]
    fn record_merges_last_seen_with_max() {
        let (mut db, _) = db_with_cap(10);
        db.record(hearsay_rec("1.2.3.4:9030", 1000));
        db.record(hearsay_rec("1.2.3.4:9030", 500));
        assert_eq!(db.get(addr("1.2.3.4:9030")).unwrap().last_seen_ms, 1000);
        db.record(hearsay_rec("1.2.3.4:9030", 2000));
        assert_eq!(db.get(addr("1.2.3.4:9030")).unwrap().last_seen_ms, 2000);
    }

    #[test]
    fn record_merges_handshake_and_seen_independently() {
        let (mut db, _) = db_with_cap(10);
        let a = addr("1.2.3.4:9030");
        db.record(observed_rec("1.2.3.4:9030", 1000));

        // Later gossip raises last_seen and leaves the handshake alone.
        let mut gossip = hearsay_rec("1.2.3.4:9030", 2000);
        gossip.agent_name = "gossiped".into();
        db.record(gossip);
        let rec = db.get(a).unwrap();
        assert_eq!(rec.last_seen_ms, 2000);
        assert_eq!(
            rec.last_handshake_ms, 1000,
            "gossip cannot touch the handshake stamp"
        );
        assert_eq!(
            rec.agent_name, "gossiped",
            "non-timestamp fields come from the latest record"
        );

        // A fresh handshake raises the handshake stamp but cannot lower
        // last_seen below the gossip that came in between.
        db.record(observed_rec("1.2.3.4:9030", 1500));
        let rec = db.get(a).unwrap();
        assert_eq!(rec.last_seen_ms, 2000);
        assert_eq!(rec.last_handshake_ms, 1500);
    }

    #[test]
    fn forget_removes_entry() {
        let (mut db, _) = db_with_cap(10);
        db.record(hearsay_rec("1.2.3.4:9030", 1000));
        db.forget(addr("1.2.3.4:9030"));
        assert!(db.get(addr("1.2.3.4:9030")).is_none());
        assert_eq!(db.count(), 0);
    }

    #[test]
    fn blacklist_drops_silently_in_record() {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(storage, bl.clone(), 10, HashSet::new()).unwrap();
        bl.record_permanent(addr("1.2.3.4:9030"));
        db.record(hearsay_rec("1.2.3.4:9030", 1000));
        assert_eq!(db.count(), 0);
    }

    // ---- observed() ----

    #[test]
    fn observed_returns_most_recent_handshake_first() {
        let (mut db, _) = db_with_cap(10);
        db.record(observed_rec("1.0.0.1:9030", 100));
        db.record(observed_rec("1.0.0.2:9030", 300));
        db.record(observed_rec("1.0.0.3:9030", 200));
        // Hearsay mentioned after every handshake must not outrank them.
        db.record(hearsay_rec("1.0.0.4:9030", 1000));
        let observed = db.observed(2, &HashSet::new());
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].address, addr("1.0.0.2:9030"));
        assert_eq!(observed[1].address, addr("1.0.0.3:9030"));
    }

    #[test]
    fn observed_orders_by_handshake_not_last_seen() {
        // An older handshake that was gossiped about recently still ranks
        // below a newer handshake: last_seen_ms is not the sort key.
        let (mut db, _) = db_with_cap(10);
        db.record(observed_rec("1.0.0.1:9030", 100));
        db.record(hearsay_rec("1.0.0.1:9030", 5000));
        db.record(observed_rec("1.0.0.2:9030", 300));
        let observed = db.observed(5, &HashSet::new());
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].address, addr("1.0.0.2:9030"));
        assert_eq!(observed[1].address, addr("1.0.0.1:9030"));
    }

    #[test]
    fn observed_excludes_hearsay() {
        let (mut db, _) = db_with_cap(10);
        db.record(hearsay_rec("1.0.0.1:9030", 9000));
        db.record(hearsay_rec("1.0.0.2:9030", 9500));
        db.record(observed_rec("1.0.0.3:9030", 100));
        let observed = db.observed(10, &HashSet::new());
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].address, addr("1.0.0.3:9030"));
        assert!(observed.iter().all(PeerRecord::is_observed));
    }

    #[test]
    fn observed_filters_exclude() {
        let (mut db, _) = db_with_cap(10);
        db.record(observed_rec("1.0.0.1:9030", 100));
        db.record(observed_rec("1.0.0.2:9030", 300));
        let exclude = set(&["1.0.0.2:9030"]);
        let observed = db.observed(5, &exclude);
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].address, addr("1.0.0.1:9030"));
    }

    #[test]
    fn observed_filters_blacklisted() {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(storage, bl.clone(), 10, HashSet::new()).unwrap();
        db.record(observed_rec("1.0.0.1:9030", 100));
        db.record(observed_rec("1.0.0.2:9030", 300));
        bl.record_permanent(addr("1.0.0.2:9030"));
        let observed = db.observed(5, &HashSet::new());
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].address, addr("1.0.0.1:9030"));
    }

    #[test]
    fn observed_zero_limit_is_empty() {
        let (mut db, _) = db_with_cap(10);
        db.record(observed_rec("1.0.0.1:9030", 100));
        assert!(db.observed(0, &HashSet::new()).is_empty());
    }

    // ---- eviction ----

    #[test]
    fn cap_evicts_oldest_hearsay_entry() {
        let (mut db, _) = db_with_cap(3);
        db.record(hearsay_rec("1.0.0.1:9030", 100));
        db.record(hearsay_rec("1.0.0.2:9030", 200));
        db.record(hearsay_rec("1.0.0.3:9030", 300));
        // Inserting a fourth should evict 1.0.0.1 (oldest mention).
        db.record(hearsay_rec("1.0.0.4:9030", 400));
        assert_eq!(db.count(), 3);
        assert!(db.get(addr("1.0.0.1:9030")).is_none());
        assert!(db.get(addr("1.0.0.4:9030")).is_some());
    }

    #[test]
    fn gossip_never_evicts_observed() {
        let (mut db, _) = db_with_cap(3);
        db.record(hearsay_rec("1.0.0.1:9030", 100));
        db.record(hearsay_rec("1.0.0.2:9030", 200));
        db.record(hearsay_rec("1.0.0.3:9030", 300));
        // One peer we actually talked to, with the OLDEST last_seen of
        // all: a last_seen-keyed policy would evict it first.
        db.record(observed_rec("9.9.9.9:9030", 50));
        assert_eq!(db.count(), 3);
        assert!(
            db.get(addr("1.0.0.1:9030")).is_none(),
            "oldest hearsay evicted to admit it"
        );
        assert!(db.get(addr("9.9.9.9:9030")).is_some());

        // Flood with ever-newer hearsay: every insertion evicts hearsay,
        // never the observed entry.
        for i in 0..50u64 {
            db.record(hearsay_rec(&format!("2.0.{i}.1:9030"), 1000 + i));
            assert!(
                db.get(addr("9.9.9.9:9030")).is_some(),
                "observed entry survived flood step {i}"
            );
            assert_eq!(db.count(), 3);
        }
        // The survivors beside the observed entry are the two newest.
        assert!(db.get(addr("2.0.49.1:9030")).is_some());
        assert!(db.get(addr("2.0.48.1:9030")).is_some());
    }

    #[test]
    fn eviction_without_crowding_takes_oldest_handshake() {
        // Every group holds one observed entry: age decides.
        let (mut db, _) = db_with_cap(2);
        db.record(observed_rec("1.0.0.1:9030", 100));
        db.record(observed_rec("2.0.0.1:9030", 200));
        // Gossip bumps last_seen of the older handshake; eviction still
        // keys on the handshake stamp, so it is the one to go.
        db.record(hearsay_rec("1.0.0.1:9030", 9000));
        db.record(observed_rec("3.0.0.1:9030", 300));
        assert_eq!(db.count(), 2);
        assert!(db.get(addr("1.0.0.1:9030")).is_none());
        assert!(db.get(addr("2.0.0.1:9030")).is_some());
        assert!(db.get(addr("3.0.0.1:9030")).is_some());
    }

    #[test]
    fn eviction_crowded_ipv4_group_pays_first() {
        // Three observed entries in one /16 and one, older than all of
        // them, in another. A fifth from the crowded /16 evicts that
        // group's oldest; the lone entry survives despite its age.
        let (mut db, _) = db_with_cap(4);
        db.record(observed_rec("78.46.1.1:9030", 100));
        db.record(observed_rec("78.46.2.2:9030", 200));
        db.record(observed_rec("78.46.3.3:9030", 300));
        db.record(observed_rec("91.10.1.1:9030", 50));
        db.record(observed_rec("78.46.4.4:9030", 400));
        assert_eq!(db.count(), 4);
        assert!(
            db.get(addr("78.46.1.1:9030")).is_none(),
            "oldest of the crowded /16"
        );
        assert!(
            db.get(addr("91.10.1.1:9030")).is_some(),
            "lone group survives"
        );
        // And again: the crowd keeps paying.
        db.record(observed_rec("78.46.5.5:9030", 500));
        assert!(db.get(addr("78.46.2.2:9030")).is_none());
        assert!(db.get(addr("91.10.1.1:9030")).is_some());
    }

    #[test]
    fn eviction_crowded_ipv6_group_pays_first() {
        // Same shape across a /64: three in 2001:db8:1:2::/64 and one,
        // older, in the neighbouring 2001:db8:1:3::/64 (same /48, same
        // first two bytes; a /16 grouping would lump them all together).
        let (mut db, _) = db_with_cap(4);
        db.record(observed_rec("[2001:db8:1:2::1]:9030", 100));
        db.record(observed_rec("[2001:db8:1:2::2]:9030", 200));
        db.record(observed_rec("[2001:db8:1:2::3]:9030", 300));
        db.record(observed_rec("[2001:db8:1:3::1]:9030", 50));
        db.record(observed_rec("[2001:db8:1:2::4]:9030", 400));
        assert_eq!(db.count(), 4);
        assert!(
            db.get(addr("[2001:db8:1:2::1]:9030")).is_none(),
            "oldest of the crowded /64"
        );
        assert!(
            db.get(addr("[2001:db8:1:3::1]:9030")).is_some(),
            "neighbouring /64 survives"
        );
    }

    #[test]
    fn eviction_crowding_tie_takes_oldest_among_crowded_groups() {
        // Two groups hold two observed entries each and a third holds one
        // that is older than everything: the victim is the oldest entry
        // of the crowded groups, never the singleton.
        let (mut db, _) = db_with_cap(5);
        db.record(observed_rec("78.46.1.1:9030", 100));
        db.record(observed_rec("78.46.2.2:9030", 200));
        db.record(observed_rec("91.10.1.1:9030", 150));
        db.record(observed_rec("91.10.2.2:9030", 250));
        db.record(observed_rec("5.5.5.5:9030", 10));
        db.record(observed_rec("6.6.6.6:9030", 600));
        assert_eq!(db.count(), 5);
        assert!(db.get(addr("78.46.1.1:9030")).is_none());
        assert!(db.get(addr("5.5.5.5:9030")).is_some());
    }

    #[test]
    fn hearsay_is_evicted_before_any_crowded_observed_entry() {
        // Even a heavily crowded observed group is untouched while
        // hearsay remains.
        let (mut db, _) = db_with_cap(4);
        db.record(observed_rec("78.46.1.1:9030", 100));
        db.record(observed_rec("78.46.2.2:9030", 200));
        db.record(observed_rec("78.46.3.3:9030", 300));
        db.record(hearsay_rec("91.10.1.1:9030", 9999));
        db.record(observed_rec("78.46.4.4:9030", 400));
        assert_eq!(db.count(), 4);
        assert!(
            db.get(addr("91.10.1.1:9030")).is_none(),
            "hearsay goes first"
        );
        assert!(db.get(addr("78.46.1.1:9030")).is_some());
    }

    #[test]
    fn update_existing_does_not_trigger_eviction() {
        let (mut db, _) = db_with_cap(2);
        db.record(hearsay_rec("1.0.0.1:9030", 100));
        db.record(hearsay_rec("1.0.0.2:9030", 200));
        // Re-record an existing entry: count must stay 2.
        db.record(hearsay_rec("1.0.0.1:9030", 500));
        assert_eq!(db.count(), 2);
        assert_eq!(db.get(addr("1.0.0.1:9030")).unwrap().last_seen_ms, 500);
    }

    // ---- storage write-through ----

    #[test]
    fn storage_put_is_called_on_record() {
        let bl = Arc::new(Blacklist::new());
        let storage = Arc::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            bl,
            10,
            HashSet::new(),
        )
        .unwrap();
        db.record(hearsay_rec("1.2.3.4:9030", 1000));
        let ops = storage.ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], MemoryStorageOp::Put(_)));
    }

    #[test]
    fn storage_delete_is_called_on_forget() {
        let bl = Arc::new(Blacklist::new());
        let storage = Arc::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            bl,
            10,
            HashSet::new(),
        )
        .unwrap();
        db.record(hearsay_rec("1.2.3.4:9030", 1000));
        db.forget(addr("1.2.3.4:9030"));
        let ops = storage.ops();
        assert!(ops
            .iter()
            .any(|op| matches!(op, MemoryStorageOp::Delete(_))));
    }

    #[test]
    fn storage_delete_is_called_on_eviction() {
        let bl = Arc::new(Blacklist::new());
        let storage = Arc::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            bl,
            2,
            HashSet::new(),
        )
        .unwrap();
        db.record(hearsay_rec("1.0.0.1:9030", 100));
        db.record(hearsay_rec("1.0.0.2:9030", 200));
        db.record(hearsay_rec("1.0.0.3:9030", 300)); // evicts 1.0.0.1
        let ops = storage.ops();
        let deletes: Vec<SocketAddr> = ops
            .iter()
            .filter_map(|op| match op {
                MemoryStorageOp::Delete(a) => Some(*a),
                MemoryStorageOp::Put(_) => None,
            })
            .collect();
        assert_eq!(
            deletes,
            vec![addr("1.0.0.1:9030")],
            "exactly one delete, for the evicted entry"
        );
    }

    #[test]
    fn load_all_repopulates_on_construction() {
        let storage = Arc::new(MemoryPeerStorage::new());
        storage.preload(hearsay_rec("1.0.0.1:9030", 100));
        storage.preload(hearsay_rec("1.0.0.2:9030", 200));
        let db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage)),
            Arc::new(Blacklist::new()),
            10,
            HashSet::new(),
        )
        .unwrap();
        assert_eq!(db.count(), 2);
    }

    #[test]
    fn load_all_keeps_handshake_stamp() {
        // Records come back as the storage hands them over: an observed
        // row stays observed, a hearsay row stays hearsay.
        let storage = Arc::new(MemoryPeerStorage::new());
        storage.preload(observed_rec("1.0.0.1:9030", 100));
        storage.preload(hearsay_rec("1.0.0.2:9030", 200));
        let db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage)),
            Arc::new(Blacklist::new()),
            10,
            HashSet::new(),
        )
        .unwrap();
        assert!(db.get(addr("1.0.0.1:9030")).unwrap().is_observed());
        assert!(!db.get(addr("1.0.0.2:9030")).unwrap().is_observed());
        let observed = db.observed(10, &HashSet::new());
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].address, addr("1.0.0.1:9030"));
    }

    // ---- self addresses ----

    #[test]
    fn record_drops_self_address() {
        let bl = Arc::new(Blacklist::new());
        let storage = Arc::new(MemoryPeerStorage::new());
        let mut self_addresses = HashSet::new();
        self_addresses.insert(addr("1.2.3.4:9030"));
        let mut db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            bl,
            10,
            self_addresses,
        )
        .unwrap();
        db.record(hearsay_rec("1.2.3.4:9030", 1000));
        assert_eq!(db.count(), 0);
        let ops = storage.ops();
        assert!(
            !ops.iter().any(|op| matches!(op, MemoryStorageOp::Put(_))),
            "no storage write should fire for a self-address record"
        );
    }

    #[test]
    fn load_all_filters_self_addresses() {
        let storage = Arc::new(MemoryPeerStorage::new());
        storage.preload(hearsay_rec("1.2.3.4:9030", 100));
        storage.preload(hearsay_rec("5.6.7.8:9030", 200));
        let mut self_addresses = HashSet::new();
        self_addresses.insert(addr("1.2.3.4:9030"));
        let db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            Arc::new(Blacklist::new()),
            10,
            self_addresses,
        )
        .unwrap();
        assert_eq!(db.count(), 1);
        assert!(db.get(addr("1.2.3.4:9030")).is_none());
        assert!(db.get(addr("5.6.7.8:9030")).is_some());
        // The self-record must remain on disk: re-reading the storage
        // directly must still return both rows. PeerDb::new never calls
        // storage.delete on filtered self-addresses.
        let persisted = storage.load_all().expect("infallible");
        let persisted_addrs: HashSet<SocketAddr> = persisted.iter().map(|r| r.address).collect();
        assert!(persisted_addrs.contains(&addr("1.2.3.4:9030")));
        assert!(persisted_addrs.contains(&addr("5.6.7.8:9030")));
        assert!(
            !storage
                .ops()
                .iter()
                .any(|op| matches!(op, MemoryStorageOp::Delete(_))),
            "PeerDb::new must not delete filtered self-addresses from disk"
        );
    }

    // ---- dial_candidate() ----

    #[test]
    fn dial_candidate_prefers_unconnected_ip_group() {
        let (mut db, _) = db_with_cap(10);
        // Two candidates share the connected peer's /16; two do not.
        db.record(hearsay_rec("78.46.5.5:9030", 100));
        db.record(observed_rec("78.46.9.9:9030", 100));
        db.record(hearsay_rec("91.10.1.1:9030", 100));
        db.record(hearsay_rec("91.20.1.1:9030", 100));
        let connected = [addr("78.46.1.1:9030")];
        let seen = draw_addrs(&db, 200, &HashSet::new(), &connected, |_| true);
        assert_eq!(
            seen,
            set(&["91.10.1.1:9030", "91.20.1.1:9030"]),
            "only groups with no connected peer are drawn, and each of them is"
        );
    }

    #[test]
    fn dial_candidate_falls_back_when_all_groups_connected() {
        let (mut db, _) = db_with_cap(10);
        db.record(hearsay_rec("78.46.5.5:9030", 100));
        db.record(hearsay_rec("78.46.9.9:9030", 100));
        let connected = [addr("78.46.1.1:9030"), addr("78.46.2.2:9030")];
        let seen = draw_addrs(&db, 200, &HashSet::new(), &connected, |_| true);
        assert_eq!(seen, set(&["78.46.5.5:9030", "78.46.9.9:9030"]));
    }

    #[test]
    fn dial_candidate_distinguishes_ipv6_slash64s() {
        let (mut db, _) = db_with_cap(10);
        // One candidate shares the connected peer's /64; two sit in other
        // /64s of the same /48 (and the same first two bytes).
        db.record(hearsay_rec("[2001:db8:1:2::9]:9030", 100));
        db.record(hearsay_rec("[2001:db8:1:3::1]:9030", 100));
        db.record(hearsay_rec("[2001:db8:9:9::1]:9030", 100));
        let connected = [addr("[2001:db8:1:2::1]:9030")];
        let seen = draw_addrs(&db, 200, &HashSet::new(), &connected, |_| true);
        assert_eq!(
            seen,
            set(&["[2001:db8:1:3::1]:9030", "[2001:db8:9:9::1]:9030"]),
            "the connected /64 is avoided; the other /64s are each drawn"
        );
    }

    #[test]
    fn dial_candidate_honours_eligible_predicate() {
        let (mut db, _) = db_with_cap(10);
        db.record(hearsay_rec("10.0.0.1:9030", 100));
        db.record(hearsay_rec("78.46.1.1:9030", 100));
        db.record(hearsay_rec("91.10.1.1:9030", 100));
        let not_private = |r: &PeerRecord| match r.address.ip() {
            IpAddr::V4(v4) => !v4.is_private(),
            IpAddr::V6(_) => true,
        };
        let seen = draw_addrs(&db, 200, &HashSet::new(), &[], not_private);
        assert_eq!(seen, set(&["78.46.1.1:9030", "91.10.1.1:9030"]));
        assert!(
            db.dial_candidate(&HashSet::new(), &[], |_| false).is_none(),
            "nothing eligible → None"
        );
    }

    #[test]
    fn dial_candidate_draws_hearsay() {
        let (mut db, _) = db_with_cap(10);
        db.record(hearsay_rec("78.46.1.1:9030", 100));
        let pick = db
            .dial_candidate(&HashSet::new(), &[], |_| true)
            .expect("hearsay is dialable");
        assert_eq!(pick.address, addr("78.46.1.1:9030"));
        assert!(!pick.is_observed());
    }

    #[test]
    fn dial_candidate_skips_excluded_and_blacklisted() {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(storage, bl.clone(), 10, HashSet::new()).unwrap();
        db.record(hearsay_rec("78.46.1.1:9030", 100));
        db.record(hearsay_rec("78.46.2.2:9030", 100));
        db.record(hearsay_rec("78.46.3.3:9030", 100));
        let mut exclude = set(&["78.46.1.1:9030"]);
        bl.record_permanent(addr("78.46.2.2:9030"));
        let seen = draw_addrs(&db, 50, &exclude, &[], |_| true);
        assert_eq!(seen, set(&["78.46.3.3:9030"]));
        exclude.insert(addr("78.46.3.3:9030"));
        assert!(db.dial_candidate(&exclude, &[], |_| true).is_none());
    }

    #[test]
    fn dial_candidate_is_uniform_over_the_pool() {
        let (mut db, _) = db_with_cap(10);
        let pool: Vec<SocketAddr> = (1..=5)
            .map(|i| addr(&format!("78.46.{i}.1:9030")))
            .collect();
        for a in &pool {
            db.record(hearsay_rec(&a.to_string(), 100));
        }
        let mut counts: HashMap<SocketAddr, usize> = HashMap::new();
        for _ in 0..500 {
            let pick = db.dial_candidate(&HashSet::new(), &[], |_| true).unwrap();
            *counts.entry(pick.address).or_default() += 1;
        }
        for a in &pool {
            let n = counts.get(a).copied().unwrap_or(0);
            // Expected 100 ± ~9. A top-N-by-timestamp policy would hand
            // one address all 500 draws and the rest none.
            assert!(n > 40 && n < 200, "{a} drawn {n} times of 500");
        }
    }

    #[test]
    fn address_group_is_v4_slash16_and_v6_slash64() {
        let v4 = address_group(&addr("78.46.10.1:9030"));
        assert_eq!(v4, AddressGroup::V4([78, 46]));
        assert_eq!(address_group(&addr("78.46.200.9:9030")), v4);
        assert_ne!(address_group(&addr("78.47.10.1:9030")), v4);

        let v6 = address_group(&addr("[2001:db8:1:2::1]:9030"));
        assert_eq!(v6, AddressGroup::V6([0x20, 0x01, 0x0d, 0xb8, 0, 1, 0, 2]));
        assert_eq!(address_group(&addr("[2001:db8:1:2:ffff::1]:9030")), v6);
        assert_ne!(address_group(&addr("[2001:db8:1:3::1]:9030")), v6);

        // The families never share a group, whatever their leading bytes.
        assert_ne!(address_group(&addr("32.1.13.184:9030")), v6);
    }

    /// Wraps an `Arc<MemoryPeerStorage>` so a single backing store can
    /// be inspected by the test after handing ownership to `PeerDb`.
    struct MemoryPeerStorageHandle(Arc<MemoryPeerStorage>);

    impl PeerStorage for MemoryPeerStorageHandle {
        fn load_all(&self) -> Result<Vec<PeerRecord>, PeerStorageError> {
            self.0.load_all()
        }
        fn put(&self, record: &PeerRecord) -> Result<(), PeerStorageError> {
            self.0.put(record)
        }
        fn delete(&self, addr: SocketAddr) -> Result<(), PeerStorageError> {
            self.0.delete(addr)
        }
    }
}
