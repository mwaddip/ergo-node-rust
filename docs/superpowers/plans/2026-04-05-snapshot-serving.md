# Snapshot Serving Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve UTXO snapshots to peers — periodic AVL+ tree dump, redb-backed storage, P2P request handling.

**Architecture:** Three components: (1) `dump_snapshot()` in enr-state submodule (prompt for state session), (2) `SnapshotStore` module in main crate backed by `snapshots.redb`, (3) main crate integration — event demux for request handling, creation trigger after block application.

**Tech Stack:** Rust, redb 2, ergo_avltree_rust packed format, Scorex VLQ/ZigZag encoding (via existing `sync/src/snapshot/protocol.rs`).

**Spec:** `docs/superpowers/specs/2026-04-05-snapshot-serving-design.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `prompts/state-dump-snapshot.md` | Prompt for state session — `dump_snapshot()` + `SnapshotDump` |
| `src/snapshot_store.rs` | `SnapshotStore` — redb wrapper for manifest/chunk storage, pruning |
| `src/snapshot_serve.rs` | Event demux + request handlers — intercepts codes 76/78/80 |
| `src/lib.rs` | Export new modules |
| `src/main.rs` | Config fields, wiring, creation trigger |
| `tests/snapshot_store_test.rs` | Unit tests for SnapshotStore |
| `tests/snapshot_serve_test.rs` | Unit tests for request handlers |

---

### Task 1: Write submodule prompt for `dump_snapshot()`

This is a coordination task. The state submodule (`state/`) needs a new public method. We write the prompt, user sends it to the state session.

**Files:**
- Create: `prompts/state-dump-snapshot.md`

- [ ] **Step 1: Write the prompt**

```markdown
# Add `SnapshotReader` + `dump_snapshot()` to enr-state

## What

Add a lightweight read-only handle (`SnapshotReader`) that shares the database
`Arc<Database>` with `RedbAVLStorage`. The reader exposes `dump_snapshot()` which
dumps the current AVL+ tree into a manifest + chunks format compatible with the
Ergo UTXO snapshot P2P protocol.

**Why a separate reader?** The `RedbAVLStorage` is consumed by
`PersistentBatchAVLProver` during construction — the main crate can't call methods
on it after that. But redb supports concurrent readers on the same `Arc<Database>`.
The `SnapshotReader` is created before handoff and held by the snapshot creation task.

## Types

Add to `src/storage.rs`:

```rust
/// Lightweight read-only handle for snapshot operations.
/// Shares the underlying redb `Database` with `RedbAVLStorage` via `Arc`.
pub struct SnapshotReader {
    db: Arc<Database>,
    key_length: usize,
}

/// A serialized snapshot of the AVL+ tree, split into manifest and chunks.
pub struct SnapshotDump {
    /// Root node hash (32 bytes).
    pub root_hash: [u8; 32],
    /// AVL+ tree height (from metadata).
    pub tree_height: u8,
    /// Serialized manifest: 2-byte header + DFS node bytes down to `manifest_depth`.
    pub manifest: Vec<u8>,
    /// Serialized subtree chunks: (subtree_root_label, DFS node bytes).
    pub chunks: Vec<([u8; 32], Vec<u8>)>,
}
```

## Method signatures

```rust
impl RedbAVLStorage {
    /// Create a read-only snapshot reader that shares the database handle.
    /// Call this BEFORE handing the storage to PersistentBatchAVLProver.
    pub fn snapshot_reader(&self) -> SnapshotReader {
        SnapshotReader {
            db: Arc::clone(&self.db),
            key_length: self.tree_params.key_length,
        }
    }
}

impl SnapshotReader {
    /// Dump the AVL+ tree as a snapshot manifest + chunks.
    ///
    /// Opens a single read transaction for consistency. Walks the tree in
    /// pre-order DFS, serializing nodes into manifest bytes (root to
    /// `manifest_depth`) and chunk bytes (each subtree below the boundary).
    ///
    /// Returns `None` if the tree is empty (no root state).
    pub fn dump_snapshot(&self, manifest_depth: u8) -> Result<Option<SnapshotDump>>
}
```

## Algorithm

### Manifest serialization

1. Open ONE `self.db.begin_read()` transaction. Use it for ALL reads.
2. Read root hash and tree height from META_TABLE (`META_TOP_NODE_HASH`, `META_TOP_NODE_HEIGHT`).
   If missing, return `Ok(None)`.
3. Write 2-byte manifest header: `[tree_height as u8, manifest_depth]`.
4. Start pre-order DFS from root at level 1 (NOT level 0 — this is a JVM convention).
5. For each node, read its packed bytes from NODES_TABLE using the node's label as key.
6. Append the packed bytes to the manifest buffer.
7. If the node is an internal node (first byte == 0x00):
   - If `level < manifest_depth`: recurse left child (level+1), then right child (level+1).
   - If `level == manifest_depth`: this is a boundary node. Record its `left_label` and
     `right_label` as subtree roots. Do NOT recurse further.
8. If the node is a leaf (first byte == 0x01): stop recursion (leaves have no children).

### Extracting child labels from packed bytes

Internal node packed format: `0x00 | balance: i8 | key: 32B | left_label: 32B | right_label: 32B`

- `left_label` is at bytes `[2+key_length .. 2+key_length+32]`
- `right_label` is at bytes `[2+key_length+32 .. 2+key_length+64]`

Use `self.key_length` for the key length (32 for Ergo).

### Chunk serialization

For each subtree root label collected from the manifest boundary:
1. Start a DFS from that label (no depth limit — walk to all leaves).
2. For each node, read packed bytes from NODES_TABLE, append to a chunk buffer.
3. For internal nodes: recurse left, then right (extract child labels from packed bytes).
4. For leaf nodes: stop.
5. The chunk's ID is the subtree root label.

### Consistency

All reads happen on the same redb read transaction. Concurrent block application
(which uses write transactions) does not affect the snapshot. No root hash
verification needed — redb transactions are isolated.

## Exports

Export `SnapshotReader` and `SnapshotDump` from `src/lib.rs` alongside the existing
`RedbAVLStorage` and `AVLTreeParams` exports.

## Testing

Add a test in `tests/storage_tests.rs`:

```rust
#[test]
fn dump_snapshot_round_trip() {
    // 1. Create a fresh storage + prover
    // 2. Create a SnapshotReader via storage.snapshot_reader() BEFORE handing
    //    storage to the prover
    // 3. Insert 100 random key-value pairs via prover
    // 4. Commit via PersistentBatchAVLProver
    // 5. Call reader.dump_snapshot(3) — depth 3 for a small tree
    // 6. Verify:
    //    - root_hash matches prover's root label
    //    - tree_height matches prover's height
    //    - manifest starts with [tree_height, 3]
    //    - manifest bytes can be parsed (first byte is 0x00 or 0x01)
    //    - chunks are non-empty
    //    - all chunk root labels can be found in the manifest's boundary nodes
    // 7. Collect all (label, packed_bytes) from manifest + chunks
    // 8. Create a second storage, call load_snapshot() with those nodes
    // 9. Verify second storage's root_state() matches original
}
```

For extracting (label, packed_bytes) from the manifest in the test:
parse the DFS byte stream the same way `get_node` would — check first byte for
0x00/0x01, extract the fixed-length fields, compute the label via Blake2b256.

Internal label: `Blake2b256(0x01 || balance || left_label || right_label)`
Leaf label: `Blake2b256(0x00 || key || value || next_key)`

Note the INVERTED prefixes: serialization uses 0x00 for internal, but the label
hash uses 0x01 for internal (ergo_avltree_rust convention).

## Dependencies

No new dependencies. Uses existing `redb`, `bytes`, `anyhow`.

## Contract

See `facts/snapshot.md` in the main repo, section "Serving: Snapshot Creation".
```

Save this to `prompts/state-dump-snapshot.md`.

- [ ] **Step 2: Commit**

```bash
git add prompts/state-dump-snapshot.md
git commit -m "prompts: state session — add dump_snapshot() to RedbAVLStorage"
```

---

### Task 2: `SnapshotStore` — redb storage for snapshots

**Files:**
- Create: `src/snapshot_store.rs`
- Create: `tests/snapshot_store_test.rs`
- Modify: `Cargo.toml` (add redb dependency)

- [ ] **Step 1: Add redb dependency to main crate**

In `Cargo.toml`, add under `[dependencies]`:

```toml
redb = "2"
```

- [ ] **Step 2: Write the failing test — store round-trip**

Create `tests/snapshot_store_test.rs`:

```rust
use ergo_node_rust::snapshot_store::SnapshotStore;
use tempfile::TempDir;

#[test]
fn store_and_retrieve_snapshot() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let manifest_id = [0xAA; 32];
    let manifest_bytes = vec![0x10, 0x0E, 0x00, 0x01, 0x02]; // fake manifest
    let chunk_id_1 = [0xBB; 32];
    let chunk_data_1 = vec![0x01, 0x03, 0x04];
    let chunk_id_2 = [0xCC; 32];
    let chunk_data_2 = vec![0x01, 0x05, 0x06];

    store
        .write_snapshot(
            1000,
            manifest_id,
            &manifest_bytes,
            &[(chunk_id_1, chunk_data_1.clone()), (chunk_id_2, chunk_data_2.clone())],
            2, // storing_snapshots
        )
        .unwrap();

    // Verify snapshots_info
    let info = store.snapshots_info().unwrap();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].0, 1000);
    assert_eq!(info[0].1, manifest_id);

    // Verify manifest lookup
    assert_eq!(
        store.get_manifest(&manifest_id).unwrap(),
        Some(manifest_bytes)
    );
    assert_eq!(store.get_manifest(&[0xFF; 32]).unwrap(), None);

    // Verify chunk lookup
    assert_eq!(
        store.get_chunk(&chunk_id_1).unwrap(),
        Some(chunk_data_1)
    );
    assert_eq!(
        store.get_chunk(&chunk_id_2).unwrap(),
        Some(chunk_data_2)
    );
    assert_eq!(store.get_chunk(&[0xFF; 32]).unwrap(), None);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --test snapshot_store_test -v`
Expected: compile error — `snapshot_store` module doesn't exist.

- [ ] **Step 4: Write SnapshotStore implementation**

Create `src/snapshot_store.rs`:

```rust
//! Persistent storage for UTXO snapshots served to peers.
//!
//! Wraps a dedicated redb file (`snapshots.redb`), separate from the main
//! state database. Stores manifests and chunks keyed by their 32-byte
//! Blake2b256 labels, plus metadata tracking which snapshots are available.

use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};

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

        // Insert chunks and collect IDs
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
            let mut info = self.read_info_from_table(&meta)?;
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
            for i in 0..to_prune {
                if let Err(e) = self.prune_snapshot(info[i].1) {
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
        self.read_info_from_table(&meta)
    }

    /// Look up manifest bytes by ID.
    pub fn get_manifest(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(MANIFESTS)?;
        match table.get(id.as_slice())? {
            Some(v) => Ok(Some(v.value().to_vec())),
            None => Ok(None),
        }
    }

    /// Look up chunk bytes by subtree ID.
    pub fn get_chunk(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS)?;
        match table.get(id.as_slice())? {
            Some(v) => Ok(Some(v.value().to_vec())),
            None => Ok(None),
        }
    }

    /// Delete a snapshot and all its chunks.
    fn prune_snapshot(&self, manifest_id: [u8; 32]) -> Result<()> {
        let txn = self.db.begin_write()?;

        // Read chunk IDs for this snapshot
        let chunk_ids: Vec<[u8; 32]> = {
            let meta = txn.open_table(METADATA)?;
            let chunk_key = snapshot_chunk_key(&manifest_id);
            match meta.get(chunk_key.as_str())? {
                Some(data) => parse_chunk_id_list(data.value()),
                None => Vec::new(),
            }
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
            let mut info = self.read_info_from_table(&meta)?;
            info.retain(|(_, id)| *id != manifest_id);
            meta.insert(SNAPSHOTS_INFO_KEY, serialize_info(&info).as_slice())?;
        }

        txn.commit()?;
        Ok(())
    }

    /// Read snapshots_info from an already-open table reference.
    fn read_info_from_table(
        &self,
        meta: &impl ReadableTable<&str, &[u8]>,
    ) -> Result<Vec<(u32, [u8; 32])>> {
        match meta.get(SNAPSHOTS_INFO_KEY)? {
            Some(data) => Ok(parse_info(data.value())),
            None => Ok(Vec::new()),
        }
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
```

- [ ] **Step 5: Export the module**

In `src/lib.rs`, add:

```rust
pub mod snapshot_store;
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test --test snapshot_store_test -v`
Expected: PASS

- [ ] **Step 7: Write the pruning test**

Add to `tests/snapshot_store_test.rs`:

```rust
#[test]
fn prune_oldest_snapshot() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    // Write 3 snapshots with storing_snapshots = 2
    for i in 0..3u8 {
        let manifest_id = [i + 1; 32];
        let chunk_id = [i + 100; 32];
        store
            .write_snapshot(
                (i as u32 + 1) * 1000,
                manifest_id,
                &[0x10, 0x0E],
                &[(chunk_id, vec![0x01, i])],
                2,
            )
            .unwrap();
    }

    let info = store.snapshots_info().unwrap();
    assert_eq!(info.len(), 2, "should have pruned to 2 snapshots");
    assert_eq!(info[0].0, 2000, "oldest remaining should be height 2000");
    assert_eq!(info[1].0, 3000, "newest should be height 3000");

    // Verify pruned snapshot's data is gone
    assert_eq!(store.get_manifest(&[1; 32]).unwrap(), None);
    assert_eq!(store.get_chunk(&[100; 32]).unwrap(), None);

    // Verify surviving snapshots' data is still there
    assert!(store.get_manifest(&[2; 32]).unwrap().is_some());
    assert!(store.get_manifest(&[3; 32]).unwrap().is_some());
    assert!(store.get_chunk(&[101; 32]).unwrap().is_some());
    assert!(store.get_chunk(&[102; 32]).unwrap().is_some());
}
```

- [ ] **Step 8: Run pruning test**

Run: `cargo test --test snapshot_store_test prune_oldest_snapshot -v`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add src/snapshot_store.rs src/lib.rs tests/snapshot_store_test.rs Cargo.toml
git commit -m "feat: SnapshotStore — redb-backed manifest/chunk storage with pruning"
```

---

### Task 3: Config additions

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add config fields**

In `src/main.rs`, add to the `NodeConfig` struct after `min_snapshot_peers`:

```rust
    /// How many UTXO snapshots to keep for serving (0 = disabled).
    #[serde(default)]
    storing_snapshots: u32,
    /// Blocks between snapshot creation points.
    #[serde(default = "default_snapshot_interval")]
    snapshot_interval: u32,
```

- [ ] **Step 2: Add default function**

After the existing default functions:

```rust
fn default_snapshot_interval() -> u32 {
    52224
}
```

- [ ] **Step 3: Update `Default` impl**

Add to the `Default` impl for `NodeConfig`:

```rust
            storing_snapshots: 0,
            snapshot_interval: default_snapshot_interval(),
```

- [ ] **Step 4: Log the new config values**

Update the `tracing::info!` line that logs node config to include the new fields:

```rust
    tracing::info!(
        state_type = ?state_type, verify_transactions, blocks_to_keep, revalidate,
        checkpoint_height = ?configured_checkpoint,
        storing_snapshots = node_config.storing_snapshots,
        snapshot_interval = node_config.snapshot_interval,
        "node config"
    );
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: add storing_snapshots and snapshot_interval config fields"
```

---

### Task 4: Event demux and request handlers

**Files:**
- Create: `src/snapshot_serve.rs`
- Create: `tests/snapshot_serve_test.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`

The P2P layer has a single subscriber channel. Currently all events flow to the sync machine. We insert a demultiplexer task between P2P and sync: it intercepts snapshot request codes (76/78/80), handles them from `SnapshotStore`, and forwards everything else to sync.

- [ ] **Step 1: Write the failing test — request handler**

Create `tests/snapshot_serve_test.rs`:

```rust
use ergo_node_rust::snapshot_serve::handle_snapshot_request;
use ergo_node_rust::snapshot_store::SnapshotStore;
use ergo_sync::snapshot::protocol::{SnapshotMessage, GET_SNAPSHOTS_INFO, GET_MANIFEST, GET_UTXO_SNAPSHOT_CHUNK};
use tempfile::TempDir;

#[test]
fn handle_get_snapshots_info_returns_stored_info() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let manifest_id = [0xAA; 32];
    store
        .write_snapshot(5000, manifest_id, &[0x10, 0x0E], &[], 2)
        .unwrap();

    let response = handle_snapshot_request(GET_SNAPSHOTS_INFO, &[], &store);
    assert!(response.is_some());

    let (code, body) = response.unwrap();
    let msg = SnapshotMessage::parse(code, &body).unwrap();
    match msg {
        SnapshotMessage::SnapshotsInfo(entries) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].height, 5000);
            assert_eq!(entries[0].manifest_id, manifest_id);
        }
        _ => panic!("expected SnapshotsInfo"),
    }
}

#[test]
fn handle_get_manifest_returns_stored_manifest() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let manifest_id = [0xBB; 32];
    let manifest_data = vec![0x10, 0x0E, 0x00, 0x01, 0x02];
    store
        .write_snapshot(6000, manifest_id, &manifest_data, &[], 2)
        .unwrap();

    let response = handle_snapshot_request(GET_MANIFEST, &manifest_id, &store);
    assert!(response.is_some());

    let (code, body) = response.unwrap();
    let msg = SnapshotMessage::parse(code, &body).unwrap();
    match msg {
        SnapshotMessage::Manifest(data) => {
            assert_eq!(data, manifest_data);
        }
        _ => panic!("expected Manifest"),
    }
}

#[test]
fn handle_get_manifest_unknown_id_returns_none() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let response = handle_snapshot_request(GET_MANIFEST, &[0xFF; 32], &store);
    assert!(response.is_none(), "unknown manifest should return no response");
}

#[test]
fn handle_get_chunk_returns_stored_chunk() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let chunk_id = [0xCC; 32];
    let chunk_data = vec![0x01, 0x03, 0x04, 0x05];
    store
        .write_snapshot(7000, [0xDD; 32], &[0x10], &[(chunk_id, chunk_data.clone())], 2)
        .unwrap();

    let response = handle_snapshot_request(GET_UTXO_SNAPSHOT_CHUNK, &chunk_id, &store);
    assert!(response.is_some());

    let (code, body) = response.unwrap();
    let msg = SnapshotMessage::parse(code, &body).unwrap();
    match msg {
        SnapshotMessage::UtxoSnapshotChunk(data) => {
            assert_eq!(data, chunk_data);
        }
        _ => panic!("expected UtxoSnapshotChunk"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test snapshot_serve_test -v`
Expected: compile error — `snapshot_serve` module doesn't exist.

- [ ] **Step 3: Write the request handler**

Create `src/snapshot_serve.rs`:

```rust
//! Snapshot serving — handles incoming P2P requests for UTXO snapshots.
//!
//! Request codes 76 (GetSnapshotsInfo), 78 (GetManifest), 80 (GetUtxoSnapshotChunk)
//! are intercepted from the P2P event stream and answered from the local
//! `SnapshotStore`. Unknown IDs get no response (matches JVM behavior).

use ergo_sync::snapshot::protocol::{
    SnapshotEntry, SnapshotMessage, GET_MANIFEST, GET_SNAPSHOTS_INFO, GET_UTXO_SNAPSHOT_CHUNK,
};

use crate::snapshot_store::SnapshotStore;

/// Handle a snapshot request. Returns `Some((response_code, response_body))` if
/// a response should be sent, or `None` for unknown IDs (no response = JVM behavior).
pub fn handle_snapshot_request(
    code: u8,
    body: &[u8],
    store: &SnapshotStore,
) -> Option<(u8, Vec<u8>)> {
    match code {
        GET_SNAPSHOTS_INFO => {
            let info = store.snapshots_info().ok()?;
            let entries: Vec<SnapshotEntry> = info
                .into_iter()
                .map(|(height, manifest_id)| SnapshotEntry {
                    height,
                    manifest_id,
                })
                .collect();
            if entries.is_empty() {
                return None; // nothing to serve
            }
            Some(SnapshotMessage::SnapshotsInfo(entries).encode())
        }

        GET_MANIFEST => {
            if body.len() < 32 {
                return None;
            }
            let mut id = [0u8; 32];
            id.copy_from_slice(&body[..32]);
            let data = store.get_manifest(&id).ok()??;
            Some(SnapshotMessage::Manifest(data).encode())
        }

        GET_UTXO_SNAPSHOT_CHUNK => {
            if body.len() < 32 {
                return None;
            }
            let mut id = [0u8; 32];
            id.copy_from_slice(&body[..32]);
            let data = store.get_chunk(&id).ok()??;
            Some(SnapshotMessage::UtxoSnapshotChunk(data).encode())
        }

        _ => None,
    }
}

/// Returns true if `code` is a snapshot request that should be intercepted
/// for serving (not forwarded to the sync machine).
pub fn is_snapshot_request(code: u8) -> bool {
    matches!(code, GET_SNAPSHOTS_INFO | GET_MANIFEST | GET_UTXO_SNAPSHOT_CHUNK)
}
```

- [ ] **Step 4: Export the module**

In `src/lib.rs`, add:

```rust
pub mod snapshot_serve;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --test snapshot_serve_test -v`
Expected: PASS (all 4 tests)

- [ ] **Step 6: Wire the event demux into main.rs**

In `src/main.rs`, after the P2P subscriber is created (around line 322) and before the transport is built:

Replace:
```rust
    let events = p2p.subscribe().await;
    // ...
    let transport = P2pTransport::new(p2p.clone(), events);
```

With:
```rust
    let raw_events = p2p.subscribe().await;

    // Event demux: intercept snapshot serving requests, forward rest to sync
    let (sync_events_tx, sync_events_rx) = tokio::sync::mpsc::channel(256);
    let snapshot_store = if node_config.storing_snapshots > 0 {
        let store = ergo_node_rust::snapshot_store::SnapshotStore::open(
            &data_dir.join("snapshots.redb"),
        )?;
        Some(std::sync::Arc::new(store))
    } else {
        None
    };

    {
        let snapshot_store = snapshot_store.clone();
        let p2p_serve = p2p.clone();
        tokio::spawn(async move {
            let mut events = raw_events;
            while let Some(event) = events.recv().await {
                // Check if this is a snapshot serving request
                let handled = if let enr_p2p::protocol::peer::ProtocolEvent::Message {
                    peer_id,
                    message: enr_p2p::protocol::messages::ProtocolMessage::Unknown { code, ref body },
                } = event
                {
                    if let Some(ref store) = snapshot_store {
                        if ergo_node_rust::snapshot_serve::is_snapshot_request(code) {
                            if let Some((resp_code, resp_body)) =
                                ergo_node_rust::snapshot_serve::handle_snapshot_request(
                                    code, body, store,
                                )
                            {
                                let msg = enr_p2p::protocol::messages::ProtocolMessage::Unknown {
                                    code: resp_code,
                                    body: resp_body,
                                };
                                let _ = p2p_serve.send_to(peer_id, msg).await;
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if !handled {
                    if sync_events_tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });
    }

    let transport = P2pTransport::new(p2p.clone(), sync_events_rx);
```

- [ ] **Step 7: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 8: Commit**

```bash
git add src/snapshot_serve.rs tests/snapshot_serve_test.rs src/lib.rs src/main.rs
git commit -m "feat: snapshot request handlers + P2P event demux for serving"
```

---

### Task 5: Creation trigger

**Files:**
- Modify: `src/main.rs`

The creation trigger fires after block application in UTXO mode. The
`ValidationPipeline` doesn't know about snapshots — the trigger lives in `main.rs`.

**Key constraint:** redb takes an exclusive file lock. We can't open a second
`RedbAVLStorage` on `state.redb` — the validator already holds it. Instead, we
create a `SnapshotReader` (via `storage.snapshot_reader()`) BEFORE handing the
storage to the prover. The reader shares the `Arc<Database>`, so reads don't
conflict with writes.

- [ ] **Step 1: Create SnapshotReader before prover construction**

In the UTXO state initialization section of `src/main.rs` (around the `StateType::Utxo`
match arm), create the reader before the storage is consumed:

For the resuming case (storage has a version):
```rust
            if storage.version().is_some() {
                let snapshot_reader = storage.snapshot_reader(); // BEFORE prover takes ownership
                let resolver = storage.resolver();
                // ... rest of prover construction ...
```

For the genesis bootstrap case:
```rust
            } else if !utxo_bootstrap {
                let snapshot_reader = storage.snapshot_reader(); // BEFORE prover takes ownership
                let resolver = storage.resolver();
                // ... rest of prover construction ...
```

Hold `snapshot_reader` in a variable that's accessible to the trigger task later:
```rust
    let snapshot_reader: Option<enr_state::SnapshotReader> = /* set in each match arm */;
```

- [ ] **Step 2: Add creation trigger task**

After the sync task spawn (around line 548), add:

```rust
    // Snapshot creation — periodic dump of UTXO state after block application
    if let Some(reader) = snapshot_reader {
        if node_config.storing_snapshots > 0 {
            let snapshot_store = snapshot_store
                .clone()
                .expect("snapshot_store must exist when storing_snapshots > 0");
            let storing_snapshots = node_config.storing_snapshots;
            let snapshot_interval = node_config.snapshot_interval;
            let chain_for_snapshot = chain.clone();
            let reader = std::sync::Arc::new(reader);

            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                    let height = chain_for_snapshot.lock().await.height();
                    if height == 0 {
                        continue;
                    }

                    // Check trigger conditions
                    if height % snapshot_interval != snapshot_interval - 1 {
                        continue;
                    }

                    // Check if we already have a snapshot at this height
                    if let Ok(info) = snapshot_store.snapshots_info() {
                        if info.iter().any(|(h, _)| *h == height) {
                            continue;
                        }
                    }

                    tracing::info!(height, "creating UTXO snapshot");

                    let reader = reader.clone();
                    let store = snapshot_store.clone();
                    let storing = storing_snapshots;

                    // Spawn blocking — DFS walk is CPU-bound
                    let result = tokio::task::spawn_blocking(move || {
                        let dump = reader.dump_snapshot(14)?;
                        match dump {
                            Some(d) => {
                                store.write_snapshot(
                                    height,
                                    d.root_hash,
                                    &d.manifest,
                                    &d.chunks,
                                    storing,
                                )?;
                                Ok::<_, anyhow::Error>(Some(height))
                            }
                            None => Ok(None),
                        }
                    })
                    .await;

                match result {
                    Ok(Ok(Some(h))) => {
                        tracing::info!(height = h, "UTXO snapshot created and stored");
                    }
                    Ok(Ok(None)) => {
                        tracing::debug!("snapshot skipped — state is empty");
                    }
                    Ok(Err(e)) => {
                        tracing::error!("snapshot creation failed: {e}");
                    }
                    Err(e) => {
                        tracing::error!("snapshot task panicked: {e}");
                    }
                }
            }
        });
    }
```

Note: This task uses a polling approach (check every 30s) rather than subscribing to
a progress channel. The progress channel is already consumed by the sync machine.
Polling is simple and sufficient since snapshot creation is infrequent (every 52k blocks
on mainnet).

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: won't compile until the state submodule adds `SnapshotReader` + `dump_snapshot()`.
If the submodule isn't updated yet, gate the entire trigger task behind a compile-time
comment and commit as a stub:

```rust
    // Snapshot creation trigger — uncomment after state submodule adds SnapshotReader
    // if let Some(reader) = snapshot_reader { ... }
```

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: periodic snapshot creation trigger (needs state submodule SnapshotReader)"
```

---

### Task 6: Update `facts/snapshot.md` serving status

**Files:**
- Modify: `facts/snapshot.md`

- [ ] **Step 1: Update the serving section**

In `facts/snapshot.md`, update the serving section header (around line 349) from:

```markdown
## Serving: Snapshot Creation and Responses

Lower priority. Requires a synced UTXO node.
```

To:

```markdown
## Serving: Snapshot Creation and Responses

**Status: Implemented** (pending state submodule `dump_snapshot()` integration).

Storage: `src/snapshot_store.rs` (`snapshots.redb`).
Request handlers: `src/snapshot_serve.rs` (codes 76/78/80).
Creation trigger: `src/main.rs` (polls every 30s, fires at `height % interval == interval - 1`).
```

- [ ] **Step 2: Commit**

```bash
git add facts/snapshot.md
git commit -m "docs: update snapshot contract — serving side implemented"
```

---

### Task 7: Integration test — dump → serve → receive round-trip

This test requires `dump_snapshot()` to exist in enr-state. Implement after the state submodule is updated.

**Files:**
- Create: `tests/snapshot_round_trip_test.rs`

- [ ] **Step 1: Write the round-trip test**

```rust
//! Integration test: create a tree, dump a snapshot, store it, serve it,
//! parse it with the receiver's code, and verify the root hash matches.

use std::sync::Arc;

use bytes::Bytes;
use enr_state::{AVLTreeParams, RedbAVLStorage};
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{AVLTree, Node, NodeHeader, Resolver};
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::persistent_batch_avl_prover::PersistentBatchAVLProver;
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_node_rust::snapshot_store::SnapshotStore;
use ergo_sync::snapshot::manifest::extract_subtree_ids;
use ergo_sync::snapshot::parser::parse_dfs_stream;
use tempfile::TempDir;

#[test]
fn dump_serve_receive_round_trip() {
    let dir = TempDir::new().unwrap();

    // ── Build a tree with content ────────────────────────────────────────
    let state_path = dir.path().join("state.redb");
    let params = AVLTreeParams {
        key_length: 32,
        value_length: None,
    };
    let storage = RedbAVLStorage::open(&state_path, params, 0).unwrap();
    // Create snapshot reader BEFORE prover takes ownership of storage
    let snapshot_reader = storage.snapshot_reader();
    let resolver: Resolver =
        Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
    let tree = AVLTree::new(resolver, 32, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // Insert enough entries to have a non-trivial tree
    for i in 0u32..200 {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(&i.to_be_bytes());
        let value = vec![i as u8; 50]; // 50-byte values
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: Bytes::copy_from_slice(&key),
                value: Bytes::copy_from_slice(&value),
            }))
            .unwrap();
    }

    let mut persistent = PersistentBatchAVLProver::new(prover, Box::new(storage), vec![]).unwrap();
    let digest = persistent.digest().unwrap();
    let original_root: [u8; 32] = digest[..32].try_into().unwrap();
    let original_height = digest[32];

    // ── Dump snapshot (using reader that shares Arc<Database>) ───────────
    let dump = snapshot_reader.dump_snapshot(3).unwrap().expect("tree is not empty");

    assert_eq!(dump.root_hash, original_root);
    assert_eq!(dump.tree_height, original_height);
    assert!(dump.manifest.len() > 2, "manifest should have content beyond header");
    assert!(!dump.chunks.is_empty(), "should have chunks");

    // ── Store in SnapshotStore ───────────────────────────────────────────
    let snap_path = dir.path().join("snapshots.redb");
    let snap_store = SnapshotStore::open(&snap_path).unwrap();
    snap_store
        .write_snapshot(1000, dump.root_hash, &dump.manifest, &dump.chunks, 2)
        .unwrap();

    // ── "Serve" — read back from store ───────────────────────────────────
    let served_manifest = snap_store.get_manifest(&dump.root_hash).unwrap().unwrap();
    let mut served_chunks: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    for (id, _) in &dump.chunks {
        let data = snap_store.get_chunk(id).unwrap().unwrap();
        served_chunks.push((*id, data));
    }

    // ── "Receive" — parse with sync crate's receiver code ────────────────
    let subtree_ids = extract_subtree_ids(&served_manifest, 32).unwrap();
    let manifest_nodes = parse_dfs_stream(&served_manifest[2..], 32).unwrap();

    let mut all_nodes: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    for node in &manifest_nodes {
        all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
    }
    for (_, chunk_data) in &served_chunks {
        let chunk_nodes = parse_dfs_stream(chunk_data, 32).unwrap();
        for node in &chunk_nodes {
            all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
        }
    }

    // ── Load into fresh state ────────────────────────────────────────────
    let state2_path = dir.path().join("state2.redb");
    let params3 = AVLTreeParams {
        key_length: 32,
        value_length: None,
    };
    let mut storage2 = RedbAVLStorage::open(&state2_path, params3, 0).unwrap();
    let mut version_bytes = Vec::with_capacity(33);
    version_bytes.extend_from_slice(&dump.root_hash);
    version_bytes.push(dump.tree_height);
    let version = Bytes::from(version_bytes);

    storage2
        .load_snapshot(
            all_nodes
                .into_iter()
                .map(|(label, packed)| (label, Bytes::from(packed))),
            dump.root_hash,
            dump.tree_height as usize,
            version,
        )
        .unwrap();

    // ── Verify ───────────────────────────────────────────────────────────
    let (loaded_root, loaded_height) = storage2.root_state().unwrap();
    assert_eq!(loaded_root, original_root, "root hash must match after round-trip");
    assert_eq!(loaded_height, original_height as usize, "tree height must match");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --test snapshot_round_trip_test -v`
Expected: PASS (only after state submodule has `dump_snapshot()`)

- [ ] **Step 3: Commit**

```bash
git add tests/snapshot_round_trip_test.rs
git commit -m "test: snapshot dump → store → serve → receive round-trip"
```

---

## Dependency Graph

```
Task 1 (submodule prompt) ──────────────────────────┐
                                                     │
Task 2 (SnapshotStore)  ─┬─── Task 4 (handlers) ────┤
                         │                           │
Task 3 (config)  ────────┘                           │
                                                     ▼
                              Task 5 (trigger) ── needs dump_snapshot()
                                                     │
                              Task 6 (contract) ─────┤
                                                     │
                              Task 7 (round-trip) ── needs dump_snapshot()
```

Tasks 1, 2, and 3 can run in parallel. Task 4 depends on Task 2. Tasks 5 and 7 depend on the state submodule being updated (Task 1 delivered). Task 6 is standalone.
