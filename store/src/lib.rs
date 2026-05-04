mod redb;

pub use self::redb::{RedbModifierStore, StoreError};

/// Persistent storage for block-related modifiers.
///
/// A dumb persistence layer: stores pre-validated, pre-serialized bytes
/// keyed by `(type_id, modifier_id, height)`. Does not parse, validate,
/// or interpret modifier content.
pub trait ModifierStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Store a single modifier.
    fn put(
        &self,
        type_id: u8,
        id: &[u8; 32],
        height: u32,
        data: &[u8],
    ) -> Result<(), Self::Error>;

    /// Store a batch of modifiers atomically.
    /// All entries are written in a single transaction — all succeed or none do.
    fn put_batch(
        &self,
        entries: &[(u8, [u8; 32], u32, Vec<u8>)],
    ) -> Result<(), Self::Error>;

    /// Retrieve a modifier by type and ID.
    fn get(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Retrieve the modifier ID at a given height for a type.
    fn get_id_at(
        &self,
        type_id: u8,
        height: u32,
    ) -> Result<Option<[u8; 32]>, Self::Error>;

    /// Check whether a modifier exists without reading its data.
    fn contains(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> Result<bool, Self::Error>;

    /// Returns the tip (highest height and its modifier ID) for a type.
    /// None if no modifiers of that type have been stored.
    fn tip(
        &self,
        type_id: u8,
    ) -> Result<Option<(u32, [u8; 32])>, Self::Error>;

    /// Store a header with its fork number and cumulative score.
    /// Writes to PRIMARY (type_id=101), HEADER_FORKS, HEADER_SCORES.
    /// Writes to BEST_CHAIN only if no entry exists at this height yet
    /// (first header at a height is assumed best until a reorg says otherwise).
    fn put_header(
        &self,
        id: &[u8; 32],
        height: u32,
        fork: u32,
        score: &[u8],
        data: &[u8],
    ) -> Result<(), Self::Error>;

    /// Get all header IDs at a given height across all forks.
    /// Returns Vec<(header_id, fork_number)> sorted by fork number.
    fn header_ids_at_height(
        &self,
        height: u32,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error>;

    /// Get the cumulative score for a header.
    fn header_score(
        &self,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Get the best chain header ID at a height.
    fn best_header_at(
        &self,
        height: u32,
    ) -> Result<Option<[u8; 32]>, Self::Error>;

    /// Get the best chain tip (highest height and header ID).
    fn best_header_tip(&self) -> Result<Option<(u32, [u8; 32])>, Self::Error>;

    /// Read the stored header bytes at a best-chain height.
    ///
    /// Equivalent to `best_header_at(h)` followed by `get(101, &id)`, but
    /// served from a single read transaction. Returns `Ok(None)` when no
    /// best-chain header is recorded at `height`. The returned bytes are
    /// the caller-provided `data` passed to `put` / `put_batch` /
    /// `put_header`; this method does not parse them.
    fn read_header_at(
        &self,
        height: u32,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Force a durable commit — fsync all pending writes to disk.
    ///
    /// `put` / `put_batch` / `put_header` use `Durability::None` so normal
    /// commits skip fsync and batch through the OS page cache. Without an
    /// fsync, the redb commit pointer is not guaranteed to be on disk when
    /// the process exits — a SIGTERM that skips destructors can leave the
    /// store appearing empty on reopen. Call this periodically during
    /// long-running writes (e.g. paired with state-storage flushes in the
    /// sync loop) and on graceful shutdown to bound worst-case data loss
    /// to the interval between flushes.
    fn flush(&self) -> Result<(), Self::Error>;
}
