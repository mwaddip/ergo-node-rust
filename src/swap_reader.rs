//! Swappable handle to the AVL state's `SnapshotReader`.
//!
//! The at-tip transition reopens the redb state DB with a smaller cache to
//! drop steady-state RSS. That requires every consumer holding the old
//! `Arc<SnapshotReader>` (which transitively holds `Arc<Database>`) to release
//! its reference before `Database::create` will succeed on the same path.
//!
//! `SwappableReader` is the shared cell that mempool, REST API, and the
//! snapshot-creation trigger all read through. The at-tip handler in
//! `main.rs` calls `take()` to release the wrapper's reference, opens the
//! new storage, then `install()`s the freshly-built reader.
//!
//! Locking: `parking_lot::RwLock` chosen because every consumer holds the
//! reader briefly per call (no `.await` across the lock, no nested locking).
//! `tokio::sync::RwLock` would force `block_on` games in the sync trait
//! methods (`UtxoReader::box_by_id`, `UtxoAccess::box_by_id`) for no benefit.
//! `arc_swap` would be lock-free for reads but doesn't help the
//! drop-then-reinstall flow we actually need to drive the `Arc<Database>`
//! refcount to zero before reopening.

use std::sync::Arc;

use enr_state::SnapshotReader;
use parking_lot::RwLock;

pub struct SwappableReader {
    inner: RwLock<Option<Arc<SnapshotReader>>>,
}

impl SwappableReader {
    pub fn new(reader: SnapshotReader) -> Self {
        Self {
            inner: RwLock::new(Some(Arc::new(reader))),
        }
    }

    pub fn empty() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    /// Snapshot of the current reader, or `None` while the at-tip handler
    /// is mid-swap. Callers MUST tolerate `None` — return a 404, decline
    /// the tx, skip the iteration. Held only for the duration of one
    /// operation; never across `.await`.
    pub fn current(&self) -> Option<Arc<SnapshotReader>> {
        self.inner.read().clone()
    }

    /// Drop the wrapper's hold on the current reader. The returned Arc is
    /// the wrapper's last reference; the caller drops it (or holds it
    /// during the brief reopen window).
    pub fn take(&self) -> Option<Arc<SnapshotReader>> {
        self.inner.write().take()
    }

    /// Install a freshly-built reader. Replaces any prior value.
    pub fn install(&self, reader: SnapshotReader) {
        *self.inner.write() = Some(Arc::new(reader));
    }
}
