//! The serving reader: pool transaction bytes for the P2P serve path, read
//! without the mempool owner's lock. See `facts/mempool.md` § Serving reader.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

type TxBytesMap = HashMap<[u8; 32], Arc<[u8]>>;

/// `tx_id → tx_bytes` for every transaction in the pool, shared between the
/// pool and each [`MempoolReader`] it hands out.
///
/// Every access holds the lock for one map operation, so the map is
/// consistent even if a holder panicked, and every access goes through the
/// poison instead of panicking on it: the reader runs inside the P2P event
/// loop, where a panic takes P2P down.
#[derive(Clone, Default)]
pub(crate) struct TxBytesIndex(Arc<RwLock<TxBytesMap>>);

impl TxBytesIndex {
    pub(crate) fn insert(&self, tx_id: [u8; 32], tx_bytes: Arc<[u8]>) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tx_id, tx_bytes);
    }

    pub(crate) fn remove(&self, tx_id: &[u8; 32]) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tx_id);
    }

    fn get(&self, tx_id: &[u8; 32]) -> Option<Arc<[u8]>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(tx_id)
            .cloned()
    }
}

/// A handle for serving pool transactions to peers without the owner's lock.
///
/// Every handle from one `Mempool` sees the same index, and only that
/// `Mempool`'s: another instance has an index of its own.
#[derive(Clone)]
pub struct MempoolReader {
    index: TxBytesIndex,
}

impl MempoolReader {
    pub(crate) fn new(index: TxBytesIndex) -> Self {
        Self { index }
    }

    /// The bytes the pool holds for `id`, or `None` if it isn't in the pool.
    ///
    /// Holds the index's own lock for one lookup, never the mempool owner's.
    pub fn tx_bytes(&self, id: &[u8; 32]) -> Option<Arc<[u8]>> {
        self.index.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a holder of the lock can poison it, and nothing outside this
    /// module holds it, hence a unit test. The map is still consistent after
    /// a holder died (each access is one map operation), so the reader keeps
    /// answering and the pool keeps writing, and neither panics.
    #[test]
    fn a_poisoned_lock_neither_panics_nor_blinds_the_reader() {
        let index = TxBytesIndex::default();
        let reader = MempoolReader::new(index.clone());
        index.insert([1; 32], Arc::from(&b"before"[..]));

        let holder = index.clone();
        let unwound = std::thread::spawn(move || {
            let _guard = holder.0.write().expect("not poisoned yet");
            // Unwinds like a panic, poisoning the lock on the way out, without
            // printing a panic message into the test output.
            std::panic::resume_unwind(Box::new("died holding the index lock"));
        })
        .join();
        assert!(unwound.is_err(), "setup: the holder must have unwound");
        assert!(index.0.is_poisoned(), "setup: the lock must be poisoned");

        assert_eq!(
            reader.tx_bytes(&[1; 32]).as_deref(),
            Some(&b"before"[..]),
            "the reader answers through the poison"
        );

        index.insert([2; 32], Arc::from(&b"after"[..]));
        index.remove(&[1; 32]);
        assert_eq!(
            reader.tx_bytes(&[1; 32]),
            None,
            "a removal lands through the poison"
        );
        assert_eq!(
            reader.tx_bytes(&[2; 32]).as_deref(),
            Some(&b"after"[..]),
            "an insertion lands through the poison"
        );
    }
}
