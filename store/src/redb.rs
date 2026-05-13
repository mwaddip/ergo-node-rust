use crate::{ModifierBatchEntry, ModifierStore};
use ::redb::{Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use parking_lot::RwLock;

const PRIMARY: TableDefinition<(u8, [u8; 32]), &[u8]> = TableDefinition::new("primary");
const HEIGHT_INDEX: TableDefinition<(u8, u32), [u8; 32]> = TableDefinition::new("height_index");
const HEADER_FORKS: TableDefinition<(u32, u32), [u8; 32]> = TableDefinition::new("header_forks");
const HEADER_SCORES: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("header_scores");
const BEST_CHAIN: TableDefinition<u32, [u8; 32]> = TableDefinition::new("best_chain");
const CHAIN_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chain_meta");

/// redb-backed modifier store.
pub struct RedbModifierStore {
    db: Database,
    tips: RwLock<HashMap<u8, (u32, [u8; 32])>>,
    best_header_tip: RwLock<Option<(u32, [u8; 32])>>,
}

/// Error type wrapping redb's various error kinds, plus a few logical
/// invariants this crate enforces above redb.
#[derive(Debug)]
pub enum StoreError {
    Database(::redb::DatabaseError),
    Transaction(::redb::TransactionError),
    Table(::redb::TableError),
    Storage(::redb::StorageError),
    Commit(::redb::CommitError),
    /// `put` was called with `type_id == 101`. Main-chain headers must
    /// go through `put_batch` so the cumulative score is carried
    /// alongside the data in a single atomic write.
    SingleHeaderPutForbidden,
    /// A `put_batch` entry with `type_id == 101` was missing its
    /// cumulative score (`score == None`). No entries were written.
    ScoreRequiredForHeader,
    /// `put_header_score` was called for a header ID that has no
    /// entry in PRIMARY. The migration that consumes this method
    /// walks BEST_CHAIN, which is invariant-consistent with PRIMARY,
    /// so this signals a caller bug.
    HeaderNotInPrimary([u8; 32]),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "database: {e}"),
            Self::Transaction(e) => write!(f, "transaction: {e}"),
            Self::Table(e) => write!(f, "table: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
            Self::Commit(e) => write!(f, "commit: {e}"),
            Self::SingleHeaderPutForbidden => write!(
                f,
                "put(type_id=101, ...) forbidden; use put_batch with score=Some(...)"
            ),
            Self::ScoreRequiredForHeader => write!(
                f,
                "put_batch entry with type_id=101 requires score=Some(big_endian_bigint_bytes)"
            ),
            Self::HeaderNotInPrimary(id) => {
                write!(f, "header_id 0x")?;
                for b in id {
                    write!(f, "{b:02x}")?;
                }
                write!(f, " not present in PRIMARY")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            Self::Transaction(e) => Some(e),
            Self::Table(e) => Some(e),
            Self::Storage(e) => Some(e),
            Self::Commit(e) => Some(e),
            Self::SingleHeaderPutForbidden
            | Self::ScoreRequiredForHeader
            | Self::HeaderNotInPrimary(_) => None,
        }
    }
}

impl From<::redb::DatabaseError> for StoreError {
    fn from(e: ::redb::DatabaseError) -> Self {
        Self::Database(e)
    }
}

impl From<::redb::TransactionError> for StoreError {
    fn from(e: ::redb::TransactionError) -> Self {
        Self::Transaction(e)
    }
}

impl From<::redb::TableError> for StoreError {
    fn from(e: ::redb::TableError) -> Self {
        Self::Table(e)
    }
}

impl From<::redb::StorageError> for StoreError {
    fn from(e: ::redb::StorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<::redb::CommitError> for StoreError {
    fn from(e: ::redb::CommitError) -> Self {
        Self::Commit(e)
    }
}

impl RedbModifierStore {
    /// Opens or creates a redb database at the given path.
    pub fn new(path: &Path) -> Result<Self, StoreError> {
        let db = Database::create(path)?;
        let tips = Self::load_tips(&db)?;
        let best_header_tip = Self::load_best_header_tip(&db)?;
        let store = Self {
            db,
            tips: RwLock::new(tips),
            best_header_tip: RwLock::new(best_header_tip),
        };
        store.migrate_headers_if_needed()?;
        Ok(store)
    }

    /// Scans the height index to reconstruct tip state per modifier type.
    /// Keys are sorted `(type_id, height)`, so the last entry per type_id wins.
    fn load_tips(db: &Database) -> Result<HashMap<u8, (u32, [u8; 32])>, StoreError> {
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(HEIGHT_INDEX) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
            Err(e) => return Err(StoreError::Table(e)),
        };

        let mut tips = HashMap::new();
        for result in table.iter()? {
            let (key_guard, value_guard) = result?;
            let (type_id, height) = key_guard.value();
            let id = value_guard.value();
            tips.insert(type_id, (height, id));
        }
        Ok(tips)
    }

    /// Scans BEST_CHAIN to find the highest entry.
    fn load_best_header_tip(db: &Database) -> Result<Option<(u32, [u8; 32])>, StoreError> {
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(BEST_CHAIN) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        // Keys are u32 sorted ascending; last entry is the tip.
        let result = match table.last()? {
            Some((key_guard, value_guard)) => {
                Some((key_guard.value(), value_guard.value()))
            }
            None => None,
        };
        Ok(result)
    }

    /// Migrates headers from HEIGHT_INDEX (type_id=101) to the new fork-aware tables.
    /// Runs once: skips if HEADER_FORKS already has entries.
    pub fn migrate_headers_if_needed(&self) -> Result<(), StoreError> {
        // Check if already migrated.
        {
            let read_txn = self.db.begin_read()?;
            match read_txn.open_table(HEADER_FORKS) {
                Ok(t) => {
                    if t.len()? > 0 {
                        return Ok(());
                    }
                }
                Err(::redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(StoreError::Table(e)),
            }
        }

        // Collect all (101, height) entries from HEIGHT_INDEX.
        let mut entries: Vec<(u32, [u8; 32])> = Vec::new();
        {
            let read_txn = self.db.begin_read()?;
            let table = match read_txn.open_table(HEIGHT_INDEX) {
                Ok(t) => t,
                Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(e) => return Err(StoreError::Table(e)),
            };
            for result in table.range((101, 0)..=(101, u32::MAX))? {
                let (key_guard, value_guard) = result?;
                let (_type_id, height) = key_guard.value();
                let id = value_guard.value();
                entries.push((height, id));
            }
        }

        if entries.is_empty() {
            return Ok(());
        }

        entries.sort_by_key(|(h, _)| *h);

        let write_txn = self.db.begin_write()?;
        {
            let mut forks = write_txn.open_table(HEADER_FORKS)?;
            let mut scores = write_txn.open_table(HEADER_SCORES)?;
            let mut best = write_txn.open_table(BEST_CHAIN)?;
            let mut height_idx = write_txn.open_table(HEIGHT_INDEX)?;

            for (height, id) in &entries {
                forks.insert((*height, 0u32), *id)?;
                scores.insert(*id, [].as_slice())?;
                best.insert(*height, *id)?;
                height_idx.remove((101u8, *height))?;
            }
        }
        write_txn.commit()?;

        // Update cache.
        if let Some((height, id)) = entries.last() {
            let mut tip = self.best_header_tip.write();
            *tip = Some((*height, *id));
        }

        Ok(())
    }
}

impl ModifierStore for RedbModifierStore {
    type Error = StoreError;

    fn put(
        &self,
        type_id: u8,
        id: &[u8; 32],
        height: u32,
        data: &[u8],
    ) -> Result<(), Self::Error> {
        // Headers must go through put_batch so the cumulative score is
        // written in the same atomic transaction as the header data.
        if type_id == 101 {
            return Err(StoreError::SingleHeaderPutForbidden);
        }
        self.put_batch(&[(type_id, *id, height, data.to_vec(), None)])
    }

    /// Stores a batch of modifiers atomically.
    ///
    /// For type_id == 101 (header) entries, writes to the fork-aware tables
    /// (PRIMARY + HEADER_FORKS + HEADER_SCORES + BEST_CHAIN) instead of
    /// HEIGHT_INDEX. The caller is expected to only put_batch headers that
    /// belong on the best chain (e.g., AppendResult::Extended in pipeline.rs);
    /// fork headers must go through put_header, which respects existing
    /// best-chain entries.
    ///
    /// Each header entry carries its cumulative-difficulty score in the
    /// 5th tuple element; this lands in HEADER_SCORES alongside the
    /// PRIMARY/HEADER_FORKS/BEST_CHAIN writes. A `type_id == 101` entry
    /// with `score == None` is rejected before any writes.
    ///
    /// BEST_CHAIN inserts for headers are unconditional: main-chain headers
    /// authoritatively own their height slot and will overwrite a stale
    /// entry left by an earlier fork-first arrival or a deep reorg.
    fn put_batch(
        &self,
        entries: &[ModifierBatchEntry],
    ) -> Result<(), Self::Error> {
        // Validate score precondition upfront so that an invalid batch
        // is rejected without partial writes. (redb would roll back an
        // un-committed txn anyway, but failing here is cheaper and the
        // failure mode is obvious from the call site.)
        for (type_id, _id, _height, _data, score) in entries {
            if *type_id == 101 && score.is_none() {
                return Err(StoreError::ScoreRequiredForHeader);
            }
        }

        let mut write_txn = self.db.begin_write()?;
        // Redb defaults to Durability::Immediate (fsync every commit). A
        // put_batch per block section saturates the disk's fsync budget on
        // encrypted/rotational storage. Skip fsync here; the sync loop
        // pairs `ModifierStore::flush` with state-storage flushes to bound
        // crash-recovery work. `set_durability` only fails if called after
        // writes — we call it on a fresh transaction, so this is infallible.
        write_txn
            .set_durability(Durability::None)
            .expect("set_durability on fresh txn");
        let mut new_best_tip: Option<(u32, [u8; 32])> = None;
        {
            let mut primary = write_txn.open_table(PRIMARY)?;
            let mut height_idx = write_txn.open_table(HEIGHT_INDEX)?;
            let mut forks = write_txn.open_table(HEADER_FORKS)?;
            let mut scores = write_txn.open_table(HEADER_SCORES)?;
            let mut best = write_txn.open_table(BEST_CHAIN)?;

            for (type_id, id, height, data, score) in entries {
                primary.insert((*type_id, *id), data.as_slice())?;

                if *type_id == 101 {
                    // Header — fork-aware tables. HEIGHT_INDEX is the legacy
                    // schema and is intentionally NOT written for type_id=101.
                    // The score is now caller-provided real cumulative
                    // difficulty (big-endian BigUint bytes), not an empty
                    // placeholder; validated as Some(...) above.
                    let score_bytes = score
                        .as_ref()
                        .expect("score validated as Some for type=101")
                        .as_slice();
                    scores.insert(*id, score_bytes)?;
                    if *height > 0 {
                        // height==0 means "height unknown" — update PRIMARY
                        // and HEADER_SCORES (per-id) but skip the
                        // height-indexed tables, matching the long-standing
                        // "data refresh" semantics of put/put_batch.
                        forks.insert((*height, 0u32), *id)?;
                        // Unconditional insert: main-chain is authoritative
                        // and overwrites any stale fork or reorged entry at
                        // this height.
                        best.insert(*height, *id)?;
                        if new_best_tip.is_none_or(|t| *height > t.0) {
                            new_best_tip = Some((*height, *id));
                        }
                    }
                } else if *height > 0 {
                    height_idx.insert((*type_id, *height), *id)?;
                }
            }
        }
        write_txn.commit()?;

        // Update non-header tips cache (HEIGHT_INDEX-backed).
        let mut tips = self.tips.write();
        for (type_id, id, height, _, _) in entries {
            if *type_id != 101
                && *height > 0
                && tips.get(type_id).is_none_or(|tip| *height > tip.0)
            {
                tips.insert(*type_id, (*height, *id));
            }
        }
        drop(tips);

        // Update best-header tip cache.
        if let Some(new_tip) = new_best_tip {
            let mut tip = self.best_header_tip.write();
            if tip.is_none_or(|t| new_tip.0 > t.0) {
                *tip = Some(new_tip);
            }
        }

        Ok(())
    }

    fn get(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PRIMARY) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let value = table.get((type_id, *id))?;
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn get_id_at(
        &self,
        type_id: u8,
        height: u32,
    ) -> Result<Option<[u8; 32]>, Self::Error> {
        // Headers (type_id=101) are looked up via BEST_CHAIN, not HEIGHT_INDEX.
        // HEIGHT_INDEX is the legacy schema for headers and is cleared by
        // migration on first open.
        if type_id == 101 {
            return self.best_header_at(height);
        }
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(HEIGHT_INDEX) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let value = table.get((type_id, height))?;
        Ok(value.map(|guard| guard.value()))
    }

    fn contains(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> Result<bool, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(PRIMARY) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(StoreError::Table(e)),
        };
        Ok(table.get((type_id, *id))?.is_some())
    }

    fn tip(
        &self,
        type_id: u8,
    ) -> Result<Option<(u32, [u8; 32])>, Self::Error> {
        // Headers (type_id=101) live in the fork-aware tables; their tip is
        // tracked separately. Route the lookup so the documented contract
        // ("highest stored modifier of this type") holds for headers too.
        if type_id == 101 {
            return self.best_header_tip();
        }
        let tips = self.tips.read();
        Ok(tips.get(&type_id).copied())
    }

    fn put_header(
        &self,
        id: &[u8; 32],
        height: u32,
        fork: u32,
        score: &[u8],
        data: &[u8],
    ) -> Result<(), Self::Error> {
        let mut write_txn = self.db.begin_write()?;
        // See `put_batch` — skip fsync; durability is enforced by explicit
        // `flush()` paired with state-storage flushes.
        write_txn
            .set_durability(Durability::None)
            .expect("set_durability on fresh txn");
        {
            let mut primary = write_txn.open_table(PRIMARY)?;
            primary.insert((101u8, *id), data)?;

            let mut forks = write_txn.open_table(HEADER_FORKS)?;
            forks.insert((height, fork), *id)?;

            let mut scores = write_txn.open_table(HEADER_SCORES)?;
            scores.insert(*id, score)?;

            let mut best = write_txn.open_table(BEST_CHAIN)?;
            if best.get(height)?.is_none() {
                best.insert(height, *id)?;
            }
        }
        write_txn.commit()?;

        // Update cache only if we actually wrote to BEST_CHAIN.
        // We wrote iff no entry existed at this height — which for a new height
        // means fork==0 is the first arrival. For fork>0 the height already had
        // an entry so we skipped the BEST_CHAIN insert.
        if fork == 0 {
            let mut tip = self.best_header_tip.write();
            if tip.is_none_or(|t| height > t.0) {
                *tip = Some((height, *id));
            }
        }

        Ok(())
    }

    fn header_ids_at_height(
        &self,
        height: u32,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(HEADER_FORKS) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Table(e)),
        };

        let mut results = Vec::new();
        for result in table.range((height, 0u32)..=(height, u32::MAX))? {
            let (key_guard, value_guard) = result?;
            let (_h, fork) = key_guard.value();
            let id = value_guard.value();
            results.push((id, fork));
        }
        Ok(results)
    }

    fn header_score(
        &self,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(HEADER_SCORES) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let value = table.get(*id)?;
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn put_header_score(
        &self,
        id: &[u8; 32],
        score: &[u8],
    ) -> Result<(), Self::Error> {
        // Verify the header exists in PRIMARY before writing. The
        // scores backfill migration walks BEST_CHAIN — which is
        // invariant-consistent with PRIMARY — so a missing PRIMARY
        // entry indicates a caller bug.
        {
            let read_txn = self.db.begin_read()?;
            let primary = match read_txn.open_table(PRIMARY) {
                Ok(t) => t,
                Err(::redb::TableError::TableDoesNotExist(_)) => {
                    return Err(StoreError::HeaderNotInPrimary(*id));
                }
                Err(e) => return Err(StoreError::Table(e)),
            };
            if primary.get((101u8, *id))?.is_none() {
                return Err(StoreError::HeaderNotInPrimary(*id));
            }
        }

        let mut write_txn = self.db.begin_write()?;
        // See `put_batch` — skip fsync; durability is enforced by
        // explicit `flush()` paired with state-storage flushes.
        write_txn
            .set_durability(Durability::None)
            .expect("set_durability on fresh txn");
        {
            let mut scores = write_txn.open_table(HEADER_SCORES)?;
            scores.insert(*id, score)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn chain_meta_get(
        &self,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(CHAIN_META) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let value = table.get(key)?;
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn chain_meta_put(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Self::Error> {
        let mut write_txn = self.db.begin_write()?;
        write_txn
            .set_durability(Durability::None)
            .expect("set_durability on fresh txn");
        {
            let mut table = write_txn.open_table(CHAIN_META)?;
            table.insert(key, value)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn best_header_at(
        &self,
        height: u32,
    ) -> Result<Option<[u8; 32]>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BEST_CHAIN) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let value = table.get(height)?;
        Ok(value.map(|guard| guard.value()))
    }

    fn best_header_tip(&self) -> Result<Option<(u32, [u8; 32])>, Self::Error> {
        let tip = self.best_header_tip.read();
        Ok(*tip)
    }

    fn best_chain_entries(&self) -> Result<Vec<(u32, [u8; 32])>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BEST_CHAIN) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let mut result = Vec::with_capacity(table.len()? as usize);
        for entry in table.iter()? {
            let (k, v) = entry?;
            result.push((k.value(), v.value()));
        }
        Ok(result)
    }

    fn read_header_at(
        &self,
        height: u32,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let read_txn = self.db.begin_read()?;

        let best_chain = match read_txn.open_table(BEST_CHAIN) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        let id = match best_chain.get(height)? {
            Some(guard) => guard.value(),
            None => return Ok(None),
        };

        let primary = match read_txn.open_table(PRIMARY) {
            Ok(t) => t,
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StoreError::Table(e)),
        };
        Ok(primary.get((101u8, id))?.map(|guard| guard.value().to_vec()))
    }

    /// Empty write transaction committed with `Durability::Immediate` to
    /// fsync all prior `Durability::None` commits still held in the page
    /// cache. Mirrors the pattern used by the state-storage crate.
    fn flush(&self) -> Result<(), Self::Error> {
        let mut write_txn = self.db.begin_write()?;
        write_txn
            .set_durability(Durability::Immediate)
            .expect("set_durability on fresh txn");
        write_txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store() -> (RedbModifierStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = RedbModifierStore::new(&dir.path().join("test.redb")).unwrap();
        (store, dir)
    }

    fn test_id(byte: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = byte;
        id
    }

    /// Standard score for tests that don't care about the bytes.
    fn s() -> Option<Vec<u8>> {
        Some(vec![0x01])
    }

    #[test]
    fn round_trip() {
        let (store, _dir) = test_store();
        let id = test_id(1);
        let data = b"hello world";

        store.put_batch(&[(101, id, 1, data.to_vec(), s())]).unwrap();
        let result = store.get(101, &id).unwrap();
        assert_eq!(result, Some(data.to_vec()));
    }

    #[test]
    fn batch_atomicity() {
        let (store, _dir) = test_store();
        let entries = vec![
            (101, test_id(1), 1, b"data1".to_vec(), s()),
            (101, test_id(2), 2, b"data2".to_vec(), s()),
            (102, test_id(3), 1, b"data3".to_vec(), None),
        ];

        store.put_batch(&entries).unwrap();

        assert_eq!(store.get(101, &test_id(1)).unwrap(), Some(b"data1".to_vec()));
        assert_eq!(store.get(101, &test_id(2)).unwrap(), Some(b"data2".to_vec()));
        assert_eq!(store.get(102, &test_id(3)).unwrap(), Some(b"data3".to_vec()));
    }

    #[test]
    fn height_index() {
        let (store, _dir) = test_store();
        let id = test_id(1);

        store
            .put_batch(&[(101, id, 42, b"block data".to_vec(), s())])
            .unwrap();
        let result = store.get_id_at(101, 42).unwrap();
        assert_eq!(result, Some(id));
    }

    #[test]
    fn tip_tracking() {
        let (store, _dir) = test_store();

        store
            .put_batch(&[(101, test_id(1), 10, b"a".to_vec(), s())])
            .unwrap();
        assert_eq!(store.tip(101).unwrap(), Some((10, test_id(1))));

        store
            .put_batch(&[(101, test_id(2), 20, b"b".to_vec(), s())])
            .unwrap();
        assert_eq!(store.tip(101).unwrap(), Some((20, test_id(2))));

        // Lower height should not update tip
        store
            .put_batch(&[(101, test_id(3), 5, b"c".to_vec(), s())])
            .unwrap();
        assert_eq!(store.tip(101).unwrap(), Some((20, test_id(2))));
    }

    #[test]
    fn contains_present_and_absent() {
        let (store, _dir) = test_store();
        let id = test_id(1);

        assert!(!store.contains(101, &id).unwrap());
        store
            .put_batch(&[(101, id, 1, b"data".to_vec(), s())])
            .unwrap();
        assert!(store.contains(101, &id).unwrap());
    }

    #[test]
    fn idempotent_put() {
        let (store, _dir) = test_store();
        let id = test_id(1);
        let data = b"same data";

        store.put_batch(&[(101, id, 1, data.to_vec(), s())]).unwrap();
        store.put_batch(&[(101, id, 1, data.to_vec(), s())]).unwrap();
        assert_eq!(store.get(101, &id).unwrap(), Some(data.to_vec()));
    }

    #[test]
    fn height_zero_skips_index_and_tip() {
        let (store, _dir) = test_store();
        let id = test_id(1);

        // Put with real height — establishes height index and tip
        store
            .put_batch(&[(101, id, 5, b"original".to_vec(), s())])
            .unwrap();
        assert_eq!(store.get_id_at(101, 5).unwrap(), Some(id));
        assert_eq!(store.tip(101).unwrap(), Some((5, id)));

        // Re-put same (type_id, id) with height=0 and new data
        store
            .put_batch(&[(101, id, 0, b"updated".to_vec(), s())])
            .unwrap();

        // Primary data updated
        assert_eq!(store.get(101, &id).unwrap(), Some(b"updated".to_vec()));
        // Height index not clobbered
        assert_eq!(store.get_id_at(101, 5).unwrap(), Some(id));
        // No spurious entry at height 0
        assert_eq!(store.get_id_at(101, 0).unwrap(), None);
        // Tip unchanged
        assert_eq!(store.tip(101).unwrap(), Some((5, id)));
    }

    #[test]
    fn empty_store() {
        let (store, _dir) = test_store();

        assert_eq!(store.tip(101).unwrap(), None);
        assert_eq!(store.get(101, &test_id(1)).unwrap(), None);
        assert_eq!(store.get_id_at(101, 0).unwrap(), None);
        assert!(!store.contains(101, &test_id(1)).unwrap());
    }

    // --- Fork-aware header tests ---

    #[test]
    fn put_header_and_query() {
        let (store, _dir) = test_store();
        let id = test_id(1);
        let score = vec![0x00, 0x01, 0xFF];
        let data = b"header bytes";

        store.put_header(&id, 100, 0, &score, data).unwrap();

        // PRIMARY populated (type_id=101)
        assert_eq!(store.get(101, &id).unwrap(), Some(data.to_vec()));

        // HEADER_FORKS populated
        let ids = store.header_ids_at_height(100).unwrap();
        assert_eq!(ids, vec![(id, 0)]);

        // HEADER_SCORES populated
        assert_eq!(store.header_score(&id).unwrap(), Some(score));

        // BEST_CHAIN populated (first at this height)
        assert_eq!(store.best_header_at(100).unwrap(), Some(id));

        // best_header_tip cache updated
        assert_eq!(store.best_header_tip().unwrap(), Some((100, id)));
    }

    #[test]
    fn multiple_forks_at_same_height() {
        let (store, _dir) = test_store();
        let id_a = test_id(0xAA);
        let id_b = test_id(0xBB);
        let score_a = vec![0x01];
        let score_b = vec![0x02];

        // First header at height 50 — becomes best
        store.put_header(&id_a, 50, 0, &score_a, b"fork0").unwrap();
        // Second header at height 50 — does NOT replace best
        store.put_header(&id_b, 50, 1, &score_b, b"fork1").unwrap();

        // Both queryable via header_ids_at_height, sorted by fork number
        let ids = store.header_ids_at_height(50).unwrap();
        assert_eq!(ids, vec![(id_a, 0), (id_b, 1)]);

        // best_header_at still returns first (fork=0)
        assert_eq!(store.best_header_at(50).unwrap(), Some(id_a));

        // Both have scores
        assert_eq!(store.header_score(&id_a).unwrap(), Some(score_a));
        assert_eq!(store.header_score(&id_b).unwrap(), Some(score_b));

        // Both readable from PRIMARY
        assert_eq!(store.get(101, &id_a).unwrap(), Some(b"fork0".to_vec()));
        assert_eq!(store.get(101, &id_b).unwrap(), Some(b"fork1".to_vec()));
    }

    // --- Diagnostic tests for the BEST_CHAIN gap bug ---

    #[test]
    fn put_batch_populates_best_chain_for_headers() {
        // pipeline.rs writes main-chain headers (type_id=101) via put_batch.
        // For BEST_CHAIN to track the chain tip, put_batch must persist them
        // to the fork-aware tables, not just PRIMARY/HEIGHT_INDEX.
        let (store, _dir) = test_store();
        let id = test_id(1);

        store
            .put_batch(&[(101, id, 100, b"header".to_vec(), s())])
            .unwrap();

        // PRIMARY populated
        assert_eq!(store.get(101, &id).unwrap(), Some(b"header".to_vec()));
        // BEST_CHAIN populated
        assert_eq!(store.best_header_at(100).unwrap(), Some(id));
        // HEADER_FORKS populated (fork=0)
        assert_eq!(store.header_ids_at_height(100).unwrap(), vec![(id, 0)]);
        // best_header_tip cache updated
        assert_eq!(store.best_header_tip().unwrap(), Some((100, id)));
    }

    #[test]
    fn happy_path_sync_no_forks_grows_best_chain_across_restarts() {
        // Simulates what the test server does on a clean sync with no forks:
        // each "session" writes some main-chain headers via put_batch and is
        // restarted. After every restart, BEST_CHAIN should be dense from 1
        // up to the latest height ever written.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("happy.redb");

        // Session 1: heights 1..=100
        {
            let store = RedbModifierStore::new(&path).unwrap();
            let entries: Vec<ModifierBatchEntry> = (1..=100u32)
                .map(|h| (101, test_id(h as u8), h, format!("h{h}").into_bytes(), s()))
                .collect();
            store.put_batch(&entries).unwrap();
        }

        // Session 2: restart, write heights 101..=200
        {
            let store = RedbModifierStore::new(&path).unwrap();
            assert_eq!(
                store.best_header_tip().unwrap(),
                Some((100, test_id(100))),
                "after session 1, tip should be 100"
            );
            let entries: Vec<ModifierBatchEntry> = (101..=200u32)
                .map(|h| (101, test_id(h as u8), h, format!("h{h}").into_bytes(), s()))
                .collect();
            store.put_batch(&entries).unwrap();
        }

        // Session 3: restart, verify BEST_CHAIN is dense 1..=200
        {
            let store = RedbModifierStore::new(&path).unwrap();
            assert_eq!(
                store.best_header_tip().unwrap(),
                Some((200, test_id(200))),
                "after session 2, tip should be 200"
            );
            for h in 1..=200u32 {
                assert_eq!(
                    store.best_header_at(h).unwrap(),
                    Some(test_id(h as u8)),
                    "BEST_CHAIN missing entry at height {h}"
                );
            }
        }
    }

    #[test]
    fn put_batch_grows_best_chain_after_fork_arrives() {
        // Regression for the original write-path bug: in the broken code,
        // a single fork header populated HEADER_FORKS, which made the
        // migration's "already migrated?" guard trip. Subsequent main-chain
        // headers landed by put_batch were stranded in HEIGHT_INDEX and
        // never reached BEST_CHAIN, freezing best_header_tip.
        //
        // After the fix, put_batch writes type_id=101 entries directly to
        // the fork-aware tables, so the migration is irrelevant for new
        // entries and BEST_CHAIN keeps tracking the chain tip even when
        // forks are interleaved with main-chain growth.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("forked.redb");

        // Phase 1: fresh start, sync 100 main-chain headers via put_batch.
        {
            let store = RedbModifierStore::new(&path).unwrap();
            let entries: Vec<ModifierBatchEntry> = (1..=100u32)
                .map(|h| (101, test_id(h as u8), h, format!("h{h}").into_bytes(), s()))
                .collect();
            store.put_batch(&entries).unwrap();
        }

        // Phase 2: restart, fork header arrives, then more main-chain via put_batch.
        {
            let store = RedbModifierStore::new(&path).unwrap();
            assert_eq!(store.best_header_tip().unwrap(), Some((100, test_id(100))));

            // A fork header arrives at height 50 (a height that already has
            // a main-chain header). put_header with fork>0 must NOT overwrite
            // the existing best entry.
            let fork_id = test_id(0xF1);
            store.put_header(&fork_id, 50, 1, &[0x99], b"fork").unwrap();
            assert_eq!(
                store.best_header_at(50).unwrap(),
                Some(test_id(50)),
                "fork at existing height must not displace main-chain entry"
            );

            // More main-chain headers arrive via put_batch.
            let entries: Vec<ModifierBatchEntry> = (101..=200u32)
                .map(|h| (101, test_id(h as u8), h, format!("h{h}").into_bytes(), s()))
                .collect();
            store.put_batch(&entries).unwrap();
        }

        // Phase 3: restart, verify the new heights reached BEST_CHAIN.
        {
            let store = RedbModifierStore::new(&path).unwrap();
            assert_eq!(
                store.best_header_tip().unwrap(),
                Some((200, test_id(200))),
                "BEST_CHAIN must track put_batch headers across restarts even \
                 after a fork header populated HEADER_FORKS"
            );
            for h in 1..=200u32 {
                assert_eq!(
                    store.best_header_at(h).unwrap(),
                    Some(test_id(h as u8)),
                    "BEST_CHAIN missing entry at height {h}"
                );
            }
            // The fork header is still queryable via header_ids_at_height.
            let ids = store.header_ids_at_height(50).unwrap();
            assert!(
                ids.iter().any(|(_, fork)| *fork == 1),
                "fork header should still be in HEADER_FORKS at fork=1"
            );
        }
    }

    #[test]
    fn fork_first_then_main_chain_leaves_best_chain_empty_at_height() {
        // The "fork-first arrival" scenario from the prompt: a fork header
        // arrives at height H before the main-chain header at H is persisted.
        // put_header with fork>0 hits the `if best.get(height)?.is_none()` guard
        // and inserts the fork as the BEST_CHAIN entry at H. Later when the
        // main-chain header at H lands via put_batch, BEST_CHAIN is not touched
        // (put_batch doesn't write BEST_CHAIN), and the height is left wired to
        // the fork header — or, if the fork hadn't arrived first, left empty
        // entirely.
        //
        // This also reproduces a single-height gap: if put_header is called
        // for a fork at height H+1 with fork>0 BEFORE put_batch lands the
        // main-chain headers at H and H+1, the guard prevents the fork from
        // overwriting any existing entry (none exists), so BEST_CHAIN[H+1] ==
        // fork_id but BEST_CHAIN[H] == None.
        let (store, _dir) = test_store();

        // Main-chain header at height 100 lands via put_batch (no BEST_CHAIN).
        let main_100 = test_id(100);
        store
            .put_batch(&[(101, main_100, 100, b"main100".to_vec(), s())])
            .unwrap();

        // Fork header at height 100 arrives via put_header. The guard says
        // "BEST_CHAIN[100] is empty, so insert me." Now BEST_CHAIN[100] points
        // at the FORK header, not the main-chain header.
        let fork_100 = test_id(0xF0);
        store.put_header(&fork_100, 100, 1, &[0x99], b"fork100").unwrap();

        // The main-chain header is the rightful occupant of best_header_at(100).
        let best = store.best_header_at(100).unwrap();
        assert_eq!(
            best,
            Some(main_100),
            "best_header_at(100) should be the main-chain header, not the fork"
        );
    }

    #[test]
    fn migration_from_height_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("migrate.redb");

        // Phase 1: populate old-style HEIGHT_INDEX with type 101 headers.
        {
            let db = Database::create(&path).unwrap();
            let write_txn = db.begin_write().unwrap();
            {
                let mut primary = write_txn.open_table(PRIMARY).unwrap();
                let mut height_idx = write_txn.open_table(HEIGHT_INDEX).unwrap();

                for h in 1..=3u32 {
                    let id = test_id(h as u8);
                    primary.insert((101u8, id), format!("data{h}").as_bytes()).unwrap();
                    height_idx.insert((101u8, h), id).unwrap();
                }
                // Also insert a non-header entry (type 102) that should NOT migrate.
                let other_id = test_id(0xFF);
                primary.insert((102u8, other_id), b"other".as_slice()).unwrap();
                height_idx.insert((102u8, 1), other_id).unwrap();
            }
            write_txn.commit().unwrap();
        }

        // Phase 2: open with RedbModifierStore — triggers migration.
        let store = RedbModifierStore::new(&path).unwrap();

        // New tables populated
        for h in 1..=3u32 {
            let id = test_id(h as u8);
            let ids = store.header_ids_at_height(h).unwrap();
            assert_eq!(ids, vec![(id, 0)], "height {h}");
            assert_eq!(store.best_header_at(h).unwrap(), Some(id));
            // Score is empty placeholder
            assert_eq!(store.header_score(&id).unwrap(), Some(vec![]));
        }

        // best_header_tip is height 3
        assert_eq!(store.best_header_tip().unwrap(), Some((3, test_id(3))));

        // After migration, headers are looked up via the fork-aware path.
        // get_id_at(101, h) now routes through BEST_CHAIN, so it returns the
        // migrated header — same value as best_header_at(h).
        for h in 1..=3u32 {
            assert_eq!(store.get_id_at(101, h).unwrap(), Some(test_id(h as u8)));
        }

        // The legacy HEIGHT_INDEX entries themselves are gone — verify by
        // direct table access so we don't depend on get_id_at's routing.
        {
            let read_txn = store.db.begin_read().unwrap();
            let height_idx = read_txn.open_table(HEIGHT_INDEX).unwrap();
            for h in 1..=3u32 {
                assert!(
                    height_idx.get((101u8, h)).unwrap().is_none(),
                    "legacy (101, {h}) entry should be removed from HEIGHT_INDEX"
                );
            }
        }

        // Non-header entry (type 102) untouched
        assert_eq!(store.get_id_at(102, 1).unwrap(), Some(test_id(0xFF)));

        // PRIMARY data still accessible
        assert_eq!(store.get(101, &test_id(1)).unwrap(), Some(b"data1".to_vec()));
    }

    // --- read_header_at: height-indexed best-chain header bytes ---

    #[test]
    fn read_header_at_returns_stored_header_bytes() {
        let (store, _dir) = test_store();
        let id = test_id(1);
        let data = b"header bytes";

        store
            .put_batch(&[(101, id, 42, data.to_vec(), s())])
            .unwrap();

        assert_eq!(store.read_header_at(42).unwrap(), Some(data.to_vec()));
    }

    #[test]
    fn read_header_at_returns_none_for_empty_height() {
        let (store, _dir) = test_store();
        assert_eq!(store.read_header_at(42).unwrap(), None);
    }

    #[test]
    fn read_header_at_follows_main_chain_after_reorg_overwrite() {
        // A fork-first arrival writes into BEST_CHAIN via put_header's
        // "slot empty" guard. When the main-chain header for the same
        // height lands via put_batch, BEST_CHAIN is unconditionally
        // overwritten. read_header_at must return the main-chain bytes,
        // not the fork bytes.
        let (store, _dir) = test_store();
        let fork_id = test_id(0xF0);
        let main_id = test_id(0x01);

        store
            .put_header(&fork_id, 100, 1, &[0x99], b"fork bytes")
            .unwrap();
        store
            .put_batch(&[(101, main_id, 100, b"main bytes".to_vec(), s())])
            .unwrap();

        assert_eq!(
            store.read_header_at(100).unwrap(),
            Some(b"main bytes".to_vec())
        );
    }

    // --- v0.5.0: real cumulative scores + chain_meta + migration primitives ---

    #[test]
    fn put_batch_header_persists_real_score() {
        let (store, _dir) = test_store();
        let id = test_id(1);
        let score = vec![0x0A, 0xBC, 0xDE, 0xF0];

        store
            .put_batch(&[(101, id, 1, b"hdr".to_vec(), Some(score.clone()))])
            .unwrap();

        assert_eq!(store.header_score(&id).unwrap(), Some(score));
        // And the rest of the header tables are still populated as before.
        assert_eq!(store.best_header_at(1).unwrap(), Some(id));
        assert_eq!(store.get(101, &id).unwrap(), Some(b"hdr".to_vec()));
    }

    #[test]
    fn put_batch_header_without_score_is_rejected_and_writes_nothing() {
        let (store, _dir) = test_store();
        let header_id = test_id(1);
        let other_id = test_id(2);

        // Batch mixes a valid non-header entry with an invalid type=101
        // entry (score=None). The whole batch must roll back; atomicity
        // says either all-or-none.
        let entries = vec![
            (102, other_id, 1, b"valid".to_vec(), None),
            (101, header_id, 2, b"header".to_vec(), None),
        ];
        let err = store.put_batch(&entries).unwrap_err();
        assert!(
            matches!(err, StoreError::ScoreRequiredForHeader),
            "expected ScoreRequiredForHeader, got {err:?}"
        );

        // Neither entry should be visible.
        assert!(!store.contains(101, &header_id).unwrap());
        assert!(!store.contains(102, &other_id).unwrap());
        assert_eq!(store.best_header_at(2).unwrap(), None);
        assert_eq!(store.header_score(&header_id).unwrap(), None);
    }

    #[test]
    fn put_single_with_header_type_is_rejected() {
        let (store, _dir) = test_store();
        let id = test_id(1);

        let err = store.put(101, &id, 1, b"header").unwrap_err();
        assert!(
            matches!(err, StoreError::SingleHeaderPutForbidden),
            "expected SingleHeaderPutForbidden, got {err:?}"
        );

        // No write should have leaked through.
        assert!(!store.contains(101, &id).unwrap());
        assert_eq!(store.best_header_at(1).unwrap(), None);
        assert_eq!(store.header_score(&id).unwrap(), None);
    }

    #[test]
    fn put_header_score_updates_only_header_scores() {
        let (store, _dir) = test_store();
        let id = test_id(1);
        let initial_score = vec![0x01];

        // Establish a header with an initial score via put_batch.
        store
            .put_batch(&[(101, id, 10, b"hdr".to_vec(), Some(initial_score))])
            .unwrap();

        // Snapshot the other tables before the score-only update.
        let primary_before = store.get(101, &id).unwrap();
        let forks_before = store.header_ids_at_height(10).unwrap();
        let best_before = store.best_header_at(10).unwrap();
        let tip_before = store.best_header_tip().unwrap();

        // Replace just the score.
        let real_score = vec![0xFF, 0xEE, 0xDD, 0xCC];
        store.put_header_score(&id, &real_score).unwrap();

        // HEADER_SCORES updated, everything else untouched.
        assert_eq!(store.header_score(&id).unwrap(), Some(real_score));
        assert_eq!(store.get(101, &id).unwrap(), primary_before);
        assert_eq!(store.header_ids_at_height(10).unwrap(), forks_before);
        assert_eq!(store.best_header_at(10).unwrap(), best_before);
        assert_eq!(store.best_header_tip().unwrap(), tip_before);
    }

    #[test]
    fn put_header_score_unknown_id_returns_err() {
        let (store, _dir) = test_store();
        let unknown = test_id(0xAB);

        let err = store.put_header_score(&unknown, &[0x01, 0x02]).unwrap_err();
        match err {
            StoreError::HeaderNotInPrimary(reported) => assert_eq!(reported, unknown),
            other => panic!("expected HeaderNotInPrimary, got {other:?}"),
        }
        // Nothing was written.
        assert_eq!(store.header_score(&unknown).unwrap(), None);
    }

    #[test]
    fn put_header_score_unknown_id_returns_err_on_empty_store() {
        // Same as above but with a completely empty store — the PRIMARY
        // table may not even exist yet. The check must still reject the
        // write rather than silently succeed.
        let (store, _dir) = test_store();
        let unknown = test_id(0xCD);

        let err = store.put_header_score(&unknown, &[0x01]).unwrap_err();
        assert!(
            matches!(err, StoreError::HeaderNotInPrimary(reported) if reported == unknown),
            "expected HeaderNotInPrimary({unknown:?}), got {err:?}"
        );
        assert_eq!(store.header_score(&unknown).unwrap(), None);
    }

    #[test]
    fn best_chain_entries_returns_ascending_height_pairs() {
        let (store, _dir) = test_store();

        // Empty store returns empty Vec.
        let empty = store.best_chain_entries().unwrap();
        assert_eq!(empty, Vec::<(u32, [u8; 32])>::new());

        // Write a handful of main-chain headers in non-ascending order to
        // make sure the result is sorted by height, not by insert order.
        let heights = [3u32, 1, 5, 2, 4];
        let entries: Vec<_> = heights
            .iter()
            .map(|h| {
                (
                    101,
                    test_id(*h as u8),
                    *h,
                    format!("h{h}").into_bytes(),
                    Some(vec![*h as u8]),
                )
            })
            .collect();
        store.put_batch(&entries).unwrap();

        let got = store.best_chain_entries().unwrap();
        let expected: Vec<(u32, [u8; 32])> =
            (1..=5u32).map(|h| (h, test_id(h as u8))).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn chain_meta_round_trips_and_overwrites() {
        let (store, _dir) = test_store();

        // Absent key returns None even before the table exists on disk.
        assert_eq!(store.chain_meta_get(b"absent").unwrap(), None);

        // Put + get round-trip.
        store.chain_meta_put(b"k", b"v1").unwrap();
        assert_eq!(store.chain_meta_get(b"k").unwrap(), Some(b"v1".to_vec()));

        // Overwrite at the same key.
        store.chain_meta_put(b"k", b"v2-longer-bytes").unwrap();
        assert_eq!(
            store.chain_meta_get(b"k").unwrap(),
            Some(b"v2-longer-bytes".to_vec())
        );

        // Independent keys.
        store.chain_meta_put(b"other", b"x").unwrap();
        assert_eq!(
            store.chain_meta_get(b"k").unwrap(),
            Some(b"v2-longer-bytes".to_vec())
        );
        assert_eq!(store.chain_meta_get(b"other").unwrap(), Some(b"x".to_vec()));

        // Sentinel-style write used by the v0.5.0 scores backfill migration.
        assert_eq!(store.chain_meta_get(b"scores_migrated_v1").unwrap(), None);
        store
            .chain_meta_put(b"scores_migrated_v1", &[1u8])
            .unwrap();
        assert_eq!(
            store.chain_meta_get(b"scores_migrated_v1").unwrap(),
            Some(vec![1u8])
        );
    }
}
