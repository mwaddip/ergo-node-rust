use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::path::Path;
use thiserror::Error;

const CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");
const METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

const META_MANIFEST_ID: &str = "manifest_id";
const META_SNAPSHOT_HEIGHT: &str = "snapshot_height";
const META_MANIFEST_BYTES: &str = "manifest_bytes";
const META_TOTAL_CHUNKS: &str = "total_chunks";

#[derive(Debug, Error)]
pub enum DownloadStoreError {
    #[error("database: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("table: {0}")]
    Table(#[from] redb::TableError),
    #[error("storage: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("commit: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("missing metadata key: {0}")]
    MissingMetadata(&'static str),
    #[error("invalid metadata for key {key}: expected {expected} bytes, got {got}")]
    InvalidMetadata {
        key: &'static str,
        expected: usize,
        got: usize,
    },
}

type Result<T> = std::result::Result<T, DownloadStoreError>;

/// Temporary redb store that persists downloaded snapshot chunks across crashes.
///
/// Two tables:
/// - `chunks`: SubtreeId (32B) -> raw chunk bytes
/// - `metadata`: string key -> Vec<u8>
pub struct ChunkDownloadStore {
    db: Database,
    manifest_id: [u8; 32],
    snapshot_height: u32,
    total_chunks: u32,
}

impl ChunkDownloadStore {
    /// Create a new download store, writing initial metadata.
    pub fn create(
        path: &Path,
        manifest_id: [u8; 32],
        snapshot_height: u32,
        manifest_bytes: &[u8],
        total_chunks: u32,
    ) -> Result<Self> {
        let db = Database::create(path)?;

        // Write all metadata in a single transaction.
        let txn = db.begin_write()?;
        {
            let mut meta = txn.open_table(METADATA)?;
            meta.insert(META_MANIFEST_ID, manifest_id.as_slice())?;
            meta.insert(META_SNAPSHOT_HEIGHT, &snapshot_height.to_be_bytes() as &[u8])?;
            meta.insert(META_MANIFEST_BYTES, manifest_bytes)?;
            meta.insert(META_TOTAL_CHUNKS, &total_chunks.to_be_bytes() as &[u8])?;
        }
        // Also create the chunks table so it exists even before any chunks are stored.
        txn.open_table(CHUNKS)?;
        txn.commit()?;

        Ok(Self {
            db,
            manifest_id,
            snapshot_height,
            total_chunks,
        })
    }

    /// Open an existing download store (crash recovery).
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::open(path)?;

        let txn = db.begin_read()?;
        let meta = txn.open_table(METADATA)?;

        let manifest_id = read_meta_fixed::<32>(&meta, META_MANIFEST_ID)?;
        let snapshot_height = u32::from_be_bytes(read_meta_fixed::<4>(&meta, META_SNAPSHOT_HEIGHT)?);
        let total_chunks = u32::from_be_bytes(read_meta_fixed::<4>(&meta, META_TOTAL_CHUNKS)?);

        drop(meta);
        drop(txn);

        Ok(Self {
            db,
            manifest_id,
            snapshot_height,
            total_chunks,
        })
    }

    /// Store a downloaded chunk.
    pub fn store_chunk(&self, id: &[u8; 32], data: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHUNKS)?;
            table.insert(id.as_slice(), data)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Retrieve a stored chunk by its subtree ID.
    pub fn get_chunk(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS)?;
        Ok(table.get(id.as_slice())?.map(|v| v.value().to_vec()))
    }

    /// Number of chunks currently stored.
    pub fn chunk_count(&self) -> Result<u32> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS)?;
        Ok(table.len()? as u32)
    }

    /// Total chunks expected for this snapshot.
    pub fn total_chunks(&self) -> u32 {
        self.total_chunks
    }

    /// The manifest ID this store was created for.
    pub fn manifest_id(&self) -> [u8; 32] {
        self.manifest_id
    }

    /// The block height of the snapshot.
    pub fn snapshot_height(&self) -> u32 {
        self.snapshot_height
    }

    /// Read the raw manifest bytes from metadata.
    pub fn manifest_bytes(&self) -> Result<Vec<u8>> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(METADATA)?;
        let guard = meta
            .get(META_MANIFEST_BYTES)?
            .ok_or(DownloadStoreError::MissingMetadata(META_MANIFEST_BYTES))?;
        Ok(guard.value().to_vec())
    }

    /// Whether all expected chunks have been downloaded.
    pub fn is_complete(&self) -> Result<bool> {
        Ok(self.chunk_count()? >= self.total_chunks)
    }

    /// Return the subtree IDs of all stored chunks.
    pub fn stored_chunk_ids(&self) -> Result<Vec<[u8; 32]>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS)?;
        let mut ids = Vec::with_capacity(table.len()? as usize);
        for entry in table.iter()? {
            let (key, _) = entry?;
            let bytes = key.value();
            let mut id = [0u8; 32];
            id.copy_from_slice(bytes);
            ids.push(id);
        }
        Ok(ids)
    }

    /// Return all stored chunks as (id, data) pairs.
    ///
    /// Collected into a Vec to avoid lifetime issues with the read transaction.
    pub fn iter_chunks(&self) -> Result<Vec<([u8; 32], Vec<u8>)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS)?;
        let mut chunks = Vec::with_capacity(table.len()? as usize);
        for entry in table.iter()? {
            let (key, value) = entry?;
            let mut id = [0u8; 32];
            id.copy_from_slice(key.value());
            chunks.push((id, value.value().to_vec()));
        }
        Ok(chunks)
    }

    /// Delete the download store file from disk.
    pub fn cleanup(path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// Read a fixed-size metadata value.
fn read_meta_fixed<const N: usize>(
    table: &redb::ReadOnlyTable<&str, &[u8]>,
    key: &'static str,
) -> Result<[u8; N]> {
    let guard = table
        .get(key)?
        .ok_or(DownloadStoreError::MissingMetadata(key))?;
    let bytes = guard.value();
    if bytes.len() != N {
        return Err(DownloadStoreError::InvalidMetadata {
            key,
            expected: N,
            got: bytes.len(),
        });
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(bytes);
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_create_and_accessors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("download.redb");
        let manifest_id = [0xAA; 32];
        let manifest_bytes = b"fake manifest data";
        let store =
            ChunkDownloadStore::create(&path, manifest_id, 500_000, manifest_bytes, 10).unwrap();

        assert_eq!(store.manifest_id(), manifest_id);
        assert_eq!(store.snapshot_height(), 500_000);
        assert_eq!(store.total_chunks(), 10);
        assert_eq!(store.manifest_bytes().unwrap(), manifest_bytes);
        assert_eq!(store.chunk_count().unwrap(), 0);
        assert!(!store.is_complete().unwrap());
    }
}
