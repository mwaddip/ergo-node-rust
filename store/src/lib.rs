mod redb;

pub use self::redb::{RedbModifierStore, StoreError};

/// A single entry in a `put_batch` slice.
///
/// Tuple shape: `(type_id, id, height, data, score)`.
///
/// - `score` is `Some(big_endian_bigint_bytes)` when `type_id == 101`
///   (the cumulative-difficulty score for that header).
/// - `score` is `None` for all other modifier types.
///
/// Exposed as a type alias rather than a struct so callers can pass
/// inline tuple literals (`(101, id, h, data, Some(score))`) directly
/// without a builder.
pub type ModifierBatchEntry = (u8, [u8; 32], u32, Vec<u8>, Option<Vec<u8>>);

/// Persistent storage for block-related modifiers.
///
/// A dumb persistence layer: stores pre-validated, pre-serialized bytes
/// keyed by `(type_id, modifier_id, height)`. Does not parse, validate,
/// or interpret modifier content.
pub trait ModifierStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Store a single modifier.
    ///
    /// Rejects `type_id == 101` (returns `Err`) — main-chain headers must
    /// always go through `put_batch` so the cumulative score is carried
    /// alongside the data in a single atomic write. Single-header writes
    /// from the validation pipeline use `put_batch` with a one-element slice.
    fn put(
        &self,
        type_id: u8,
        id: &[u8; 32],
        height: u32,
        data: &[u8],
    ) -> Result<(), Self::Error>;

    /// Store a batch of modifiers atomically.
    /// All entries are written in a single transaction — all succeed or none do.
    ///
    /// Entry tuple: `(type_id, id, height, data, score)`.
    /// `score` is `Some(big_endian_bigint_bytes)` and required when
    /// `type_id == 101`; `None` for all other modifier types. A
    /// `type_id == 101` entry with `score == None` is rejected and
    /// no entries from the batch are written.
    fn put_batch(
        &self,
        entries: &[ModifierBatchEntry],
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
    ///
    /// Returns the cumulative difficulty score as big-endian BigUint
    /// bytes. Populated for **every** header — main-chain and forks
    /// alike — after the one-shot scores backfill migration runs at
    /// store open. Returns `None` only when `id` was never written.
    fn header_score(
        &self,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Update only the score for an existing header.
    ///
    /// Writes `header_scores[id] = score`. Does NOT touch PRIMARY,
    /// HEADER_FORKS, or BEST_CHAIN. Used by the scores backfill
    /// migration to upgrade empty-placeholder scores to real values
    /// without rewriting the full header record.
    ///
    /// Returns Err if `id` is not present in PRIMARY (would be a
    /// caller bug — the migration walks BEST_CHAIN which is consistent
    /// with PRIMARY).
    fn put_header_score(
        &self,
        id: &[u8; 32],
        score: &[u8],
    ) -> Result<(), Self::Error>;

    /// Read a value from the chain_meta table.
    ///
    /// Tiny key-value store inside the modifier store, used for
    /// migration sentinels and per-chain-state flags. Keys are
    /// stable byte strings (see `facts/store.md` for assigned keys);
    /// values are treated as opaque bytes by the store crate.
    fn chain_meta_get(
        &self,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Write a value to the chain_meta table. Overwrites any
    /// previous value at the same key.
    fn chain_meta_put(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Self::Error>;

    /// Get the best chain header ID at a height.
    fn best_header_at(
        &self,
        height: u32,
    ) -> Result<Option<[u8; 32]>, Self::Error>;

    /// Get the best chain tip (highest height and header ID).
    fn best_header_tip(&self) -> Result<Option<(u32, [u8; 32])>, Self::Error>;

    /// Read every entry in BEST_CHAIN, in ascending height order.
    ///
    /// Single read transaction; sequential B-tree traversal —
    /// substantially faster than O(N) point lookups when restoring
    /// chain state at startup or running the scores backfill
    /// migration. Returns `(height, header_id)` pairs from the
    /// lowest height in BEST_CHAIN to `best_header_tip().height`.
    /// Empty Vec on an empty store.
    fn best_chain_entries(&self) -> Result<Vec<(u32, [u8; 32])>, Self::Error>;

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
