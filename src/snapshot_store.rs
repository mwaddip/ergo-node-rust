//! Persistent storage for UTXO snapshots served to peers.
//!
//! Wraps a dedicated redb file (`snapshots.redb`), separate from the main
//! state database. Stores manifests and chunks keyed by their 32-byte
//! Blake2b256 labels, plus metadata tracking which snapshots are available.

use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

// ── Tables ────────────────────────────────────────────────────────────────

const MANIFESTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("manifests");
const CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");
const METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

const SNAPSHOTS_INFO_KEY: &str = "snapshots_info";

/// Persistent snapshot store backed by redb.
pub struct SnapshotStore {
    db: Database,
}

impl SnapshotStore {
    /// Open or create the snapshot store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path).context("failed to open snapshots.redb")?;
        {
            let txn = db.begin_write()?;
            txn.open_table(MANIFESTS)?;
            txn.open_table(CHUNKS)?;
            txn.open_table(METADATA)?;
            txn.commit()?;
        }
        Ok(Self { db })
    }

    /// Write a snapshot (manifest + chunks) and prune old ones.
    ///
    /// `storing_snapshots` is the maximum number of snapshots to keep.
    pub fn write_snapshot(
        &self,
        height: u32,
        manifest_id: [u8; 32],
        manifest_bytes: &[u8],
        chunks: &[([u8; 32], Vec<u8>)],
        storing_snapshots: u32,
    ) -> Result<()> {
        let txn = self.db.begin_write()?;

        // Insert manifest
        {
            let mut table = txn.open_table(MANIFESTS)?;
            table.insert(manifest_id.as_slice(), manifest_bytes)?;
        }

        // Insert chunks
        {
            let mut table = txn.open_table(CHUNKS)?;
            for (id, data) in chunks {
                table.insert(id.as_slice(), data.as_slice())?;
            }
        }

        // Update metadata: snapshots_info + chunk list for this snapshot
        {
            let mut meta = txn.open_table(METADATA)?;

            // Read existing info, append new entry
            let mut info = meta.get(SNAPSHOTS_INFO_KEY)?
                .map(|g| parse_info(g.value()))
                .unwrap_or_default();
            info.push((height, manifest_id));
            meta.insert(SNAPSHOTS_INFO_KEY, serialize_info(&info).as_slice())?;

            // Store chunk ID list for this snapshot (for pruning)
            let chunk_key = snapshot_chunk_key(&manifest_id);
            let chunk_ids: Vec<u8> = chunks
                .iter()
                .flat_map(|(id, _)| id.iter().copied())
                .collect();
            meta.insert(chunk_key.as_str(), chunk_ids.as_slice())?;
        }

        txn.commit()?;

        // Prune if needed (separate transaction — prune failure doesn't roll back the write)
        let info = self.snapshots_info()?;
        if info.len() > storing_snapshots as usize {
            let to_prune = info.len() - storing_snapshots as usize;
            for entry in info.iter().take(to_prune) {
                if let Err(e) = self.prune_snapshot(entry.1) {
                    tracing::warn!("snapshot prune failed: {e}");
                }
            }
        }

        Ok(())
    }

    /// List available snapshots as (height, manifest_id) pairs, oldest first.
    pub fn snapshots_info(&self) -> Result<Vec<(u32, [u8; 32])>> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(METADATA)?;
        match meta.get(SNAPSHOTS_INFO_KEY)? {
            Some(guard) => Ok(parse_info(guard.value())),
            None => Ok(Vec::new()),
        }
    }

    /// Look up manifest bytes by ID.
    pub fn get_manifest(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(MANIFESTS)?;
        match table.get(id.as_slice())? {
            Some(guard) => Ok(Some(guard.value().to_vec())),
            None => Ok(None),
        }
    }

    /// Look up chunk bytes by subtree ID.
    pub fn get_chunk(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS)?;
        match table.get(id.as_slice())? {
            Some(guard) => Ok(Some(guard.value().to_vec())),
            None => Ok(None),
        }
    }

    /// Delete a snapshot and all its chunks.
    fn prune_snapshot(&self, manifest_id: [u8; 32]) -> Result<()> {
        let txn = self.db.begin_write()?;

        // Read chunk IDs for this snapshot
        let chunk_key = snapshot_chunk_key(&manifest_id);
        let chunk_ids: Vec<[u8; 32]> = {
            let meta = txn.open_table(METADATA)?;
            let result = meta.get(chunk_key.as_str())?;
            result.map(|g| parse_chunk_id_list(g.value())).unwrap_or_default()
        };

        // Delete chunks
        {
            let mut table = txn.open_table(CHUNKS)?;
            for id in &chunk_ids {
                table.remove(id.as_slice())?;
            }
        }

        // Delete manifest
        {
            let mut table = txn.open_table(MANIFESTS)?;
            table.remove(manifest_id.as_slice())?;
        }

        // Update metadata
        {
            let mut meta = txn.open_table(METADATA)?;

            // Remove chunk list
            let chunk_key = snapshot_chunk_key(&manifest_id);
            meta.remove(chunk_key.as_str())?;

            // Remove from snapshots_info
            let mut info = meta.get(SNAPSHOTS_INFO_KEY)?
                .map(|g| parse_info(g.value()))
                .unwrap_or_default();
            info.retain(|(_, id)| *id != manifest_id);
            meta.insert(SNAPSHOTS_INFO_KEY, serialize_info(&info).as_slice())?;
        }

        txn.commit()?;
        Ok(())
    }
}

// ── Serialization helpers ─────────────────────────────────────────────────

/// Serialize snapshots info: `[count: u32 BE, (height: u32 BE, id: 32B)*]`
fn serialize_info(info: &[(u32, [u8; 32])]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + info.len() * 36);
    buf.extend_from_slice(&(info.len() as u32).to_be_bytes());
    for (height, id) in info {
        buf.extend_from_slice(&height.to_be_bytes());
        buf.extend_from_slice(id);
    }
    buf
}

/// Parse snapshots info from binary.
fn parse_info(data: &[u8]) -> Vec<(u32, [u8; 32])> {
    if data.len() < 4 {
        return Vec::new();
    }
    let count = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut result = Vec::with_capacity(count);
    let mut offset = 4;
    for _ in 0..count {
        if offset + 36 > data.len() {
            break;
        }
        let height = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let mut id = [0u8; 32];
        id.copy_from_slice(&data[offset + 4..offset + 36]);
        result.push((height, id));
        offset += 36;
    }
    result
}

/// Metadata key for a snapshot's chunk ID list.
fn snapshot_chunk_key(manifest_id: &[u8; 32]) -> String {
    format!("snapshot:{}", hex::encode(manifest_id))
}

/// Parse flat chunk ID bytes into 32-byte arrays.
fn parse_chunk_id_list(data: &[u8]) -> Vec<[u8; 32]> {
    data.chunks_exact(32)
        .map(|chunk| {
            let mut id = [0u8; 32];
            id.copy_from_slice(chunk);
            id
        })
        .collect()
}
