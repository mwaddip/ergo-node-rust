//! In-memory peer registry that backs `GetPeers` responses, populates
//! candidates for the outbound manager, and feeds `P2pNode::all_peers`.
//!
//! Persistence is owned by a separate [`PeerStorage`] trait so the
//! store crate can back it with redb without `p2p` knowing about disks.
//! All `PeerDb` operations are synchronous; callers wrap it in an
//! `Arc<Mutex<PeerDb>>` at the integration points where the router and
//! the outbound manager both need access.
//!
//! See `facts/p2p-peerdb.md` for the contract.

use crate::blacklist::Blacklist;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

/// Default soft cap on the number of entries kept in memory.
pub const DEFAULT_CAP: usize = 1000;

/// A persisted peer entry. Originates from either our own handshake
/// (authoritative) or a `Peers` gossip from a third party (hearsay) —
/// the DB does not distinguish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    pub address: SocketAddr,
    pub last_seen_ms: u64,
    pub agent_name: String,
    pub node_name: String,
    pub version: (u8, u8, u8),
    pub features: Vec<(u8, Vec<u8>)>,
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
}

impl PeerDb {
    /// Construct a `PeerDb` and repopulate it from `storage`.
    ///
    /// # Errors
    /// Returns the storage error from `load_all`. The caller (main
    /// crate) treats this as fatal — let the node fail to start rather
    /// than run with an empty peer table after losing persistence.
    pub fn new(
        storage: Box<dyn PeerStorage>,
        blacklist: Arc<Blacklist>,
        cap: usize,
    ) -> Result<Self, PeerStorageError> {
        let loaded = storage.load_all()?;
        let mut entries = HashMap::with_capacity(loaded.len());
        for rec in loaded {
            entries.insert(rec.address, rec);
        }
        Ok(Self { entries, cap, blacklist, storage })
    }

    /// Insert or update a record.
    ///
    /// Drops blacklisted addresses silently. Merges `last_seen_ms` with
    /// any prior value (max wins) and overwrites the rest of the fields.
    /// Evicts the least-recently-seen entry when the cap is hit.
    pub fn record(&mut self, mut record: PeerRecord) {
        if self.blacklist.contains(record.address) {
            return;
        }
        if let Some(prior) = self.entries.get(&record.address) {
            record.last_seen_ms = record.last_seen_ms.max(prior.last_seen_ms);
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

    /// Top `limit` most-recently-seen peers, excluding addresses in
    /// `exclude_addrs` and any blacklisted address.
    pub fn recent(
        &self,
        limit: usize,
        exclude_addrs: &HashSet<SocketAddr>,
    ) -> Vec<PeerRecord> {
        if limit == 0 {
            return Vec::new();
        }
        let mut candidates: Vec<&PeerRecord> = self.entries
            .values()
            .filter(|r| !exclude_addrs.contains(&r.address))
            .filter(|r| !self.blacklist.contains(r.address))
            .collect();
        candidates.sort_by(|a, b| b.last_seen_ms.cmp(&a.last_seen_ms));
        candidates.truncate(limit);
        candidates.into_iter().cloned().collect()
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

    fn evict_oldest(&mut self) {
        if let Some(addr) = self.entries
            .values()
            .min_by_key(|r| r.last_seen_ms)
            .map(|r| r.address)
        {
            self.entries.remove(&addr);
            if let Err(e) = self.storage.delete(addr) {
                tracing::warn!(%addr, error = %e, "PeerStorage::delete (eviction) failed");
            }
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
        Ok(self.inner.lock().expect("poisoned").entries.values().cloned().collect())
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

    fn record(s: &str, last_seen: u64) -> PeerRecord {
        PeerRecord {
            address: addr(s),
            last_seen_ms: last_seen,
            agent_name: "test".into(),
            node_name: "node".into(),
            version: (5, 0, 25),
            features: vec![],
        }
    }

    fn db_with_cap(cap: usize) -> (PeerDb, Arc<Blacklist>) {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let db = PeerDb::new(storage, bl.clone(), cap).expect("new ok");
        (db, bl)
    }

    #[test]
    fn record_inserts_and_get_returns_entry() {
        let (mut db, _) = db_with_cap(10);
        let r = record("1.2.3.4:9030", 1000);
        db.record(r.clone());
        assert_eq!(db.get(addr("1.2.3.4:9030")), Some(&r));
        assert_eq!(db.count(), 1);
    }

    #[test]
    fn record_merges_last_seen_with_max() {
        let (mut db, _) = db_with_cap(10);
        db.record(record("1.2.3.4:9030", 1000));
        db.record(record("1.2.3.4:9030", 500));
        assert_eq!(db.get(addr("1.2.3.4:9030")).unwrap().last_seen_ms, 1000);
        db.record(record("1.2.3.4:9030", 2000));
        assert_eq!(db.get(addr("1.2.3.4:9030")).unwrap().last_seen_ms, 2000);
    }

    #[test]
    fn forget_removes_entry() {
        let (mut db, _) = db_with_cap(10);
        db.record(record("1.2.3.4:9030", 1000));
        db.forget(addr("1.2.3.4:9030"));
        assert!(db.get(addr("1.2.3.4:9030")).is_none());
        assert_eq!(db.count(), 0);
    }

    #[test]
    fn blacklist_drops_silently_in_record() {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(storage, bl.clone(), 10).unwrap();
        bl.record_permanent(addr("1.2.3.4:9030"));
        db.record(record("1.2.3.4:9030", 1000));
        assert_eq!(db.count(), 0);
    }

    #[test]
    fn recent_returns_most_recent_first() {
        let (mut db, _) = db_with_cap(10);
        db.record(record("1.0.0.1:9030", 100));
        db.record(record("1.0.0.2:9030", 300));
        db.record(record("1.0.0.3:9030", 200));
        let exclude = HashSet::new();
        let recent = db.recent(2, &exclude);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].address, addr("1.0.0.2:9030"));
        assert_eq!(recent[1].address, addr("1.0.0.3:9030"));
    }

    #[test]
    fn recent_filters_exclude() {
        let (mut db, _) = db_with_cap(10);
        db.record(record("1.0.0.1:9030", 100));
        db.record(record("1.0.0.2:9030", 300));
        let mut exclude = HashSet::new();
        exclude.insert(addr("1.0.0.2:9030"));
        let recent = db.recent(5, &exclude);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].address, addr("1.0.0.1:9030"));
    }

    #[test]
    fn recent_filters_blacklisted() {
        let bl = Arc::new(Blacklist::new());
        let storage: Box<dyn PeerStorage> = Box::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(storage, bl.clone(), 10).unwrap();
        db.record(record("1.0.0.1:9030", 100));
        db.record(record("1.0.0.2:9030", 300));
        bl.record_permanent(addr("1.0.0.2:9030"));
        let recent = db.recent(5, &HashSet::new());
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].address, addr("1.0.0.1:9030"));
    }

    #[test]
    fn recent_zero_limit_is_empty() {
        let (mut db, _) = db_with_cap(10);
        db.record(record("1.0.0.1:9030", 100));
        assert!(db.recent(0, &HashSet::new()).is_empty());
    }

    #[test]
    fn cap_evicts_oldest_entry() {
        let (mut db, _) = db_with_cap(3);
        db.record(record("1.0.0.1:9030", 100));
        db.record(record("1.0.0.2:9030", 200));
        db.record(record("1.0.0.3:9030", 300));
        // Inserting a fourth should evict 1.0.0.1 (oldest).
        db.record(record("1.0.0.4:9030", 400));
        assert_eq!(db.count(), 3);
        assert!(db.get(addr("1.0.0.1:9030")).is_none());
        assert!(db.get(addr("1.0.0.4:9030")).is_some());
    }

    #[test]
    fn update_existing_does_not_trigger_eviction() {
        let (mut db, _) = db_with_cap(2);
        db.record(record("1.0.0.1:9030", 100));
        db.record(record("1.0.0.2:9030", 200));
        // Re-record an existing entry — count must stay 2.
        db.record(record("1.0.0.1:9030", 500));
        assert_eq!(db.count(), 2);
        assert_eq!(db.get(addr("1.0.0.1:9030")).unwrap().last_seen_ms, 500);
    }

    #[test]
    fn storage_put_is_called_on_record() {
        let bl = Arc::new(Blacklist::new());
        let storage = Arc::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            bl,
            10,
        ).unwrap();
        db.record(record("1.2.3.4:9030", 1000));
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
        ).unwrap();
        db.record(record("1.2.3.4:9030", 1000));
        db.forget(addr("1.2.3.4:9030"));
        let ops = storage.ops();
        assert!(ops.iter().any(|op| matches!(op, MemoryStorageOp::Delete(_))));
    }

    #[test]
    fn storage_delete_is_called_on_eviction() {
        let bl = Arc::new(Blacklist::new());
        let storage = Arc::new(MemoryPeerStorage::new());
        let mut db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage.clone())),
            bl,
            2,
        ).unwrap();
        db.record(record("1.0.0.1:9030", 100));
        db.record(record("1.0.0.2:9030", 200));
        db.record(record("1.0.0.3:9030", 300)); // evicts 1.0.0.1
        let ops = storage.ops();
        let delete_count = ops.iter().filter(|op| matches!(op, MemoryStorageOp::Delete(_))).count();
        assert_eq!(delete_count, 1, "exactly one delete from eviction");
    }

    #[test]
    fn load_all_repopulates_on_construction() {
        let storage = Arc::new(MemoryPeerStorage::new());
        storage.preload(record("1.0.0.1:9030", 100));
        storage.preload(record("1.0.0.2:9030", 200));
        let db = PeerDb::new(
            Box::new(MemoryPeerStorageHandle(storage)),
            Arc::new(Blacklist::new()),
            10,
        ).unwrap();
        assert_eq!(db.count(), 2);
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
