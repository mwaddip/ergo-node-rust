# Snapshot Serving Design

Serve UTXO snapshots to peers — the mirror of the receiving side built in session 7. Periodically dump the AVL+ tree into manifest + chunks, store in a dedicated redb file, and respond to P2P requests (codes 76/78/80).

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Tree walk location | `enr-state` submodule (`dump_snapshot()`) | Storage owns the data, can use a single read transaction for consistency |
| Snapshot storage | Separate redb file (`snapshots.redb`) | Transactional, consistent with project stack, no inode explosion |
| Code location | Main crate module (`src/snapshot_store.rs`) | Too small for its own crate |
| Pruning strategy | Simple delete — no shared subtree analysis | At most 1-2 snapshots stored, complexity not justified |

## Component 1: `enr-state` — `dump_snapshot()`

New public method on `RedbAVLStorage`:

```rust
pub fn dump_snapshot(&self, manifest_depth: u8) -> Result<SnapshotDump>

pub struct SnapshotDump {
    pub root_hash: [u8; 32],
    pub tree_height: u8,
    pub manifest: Vec<u8>,                    // 2-byte header + DFS node bytes
    pub chunks: Vec<([u8; 32], Vec<u8>)>,     // (subtree_id, DFS node bytes)
}
```

### Procedure

1. Open a single redb read transaction (point-in-time isolation).
2. Read `root_state()` for root hash + tree height.
3. Pre-order DFS from root, starting at level 1 (JVM convention):
   - Levels 1 to `manifest_depth`: serialize nodes into manifest buffer.
   - At `manifest_depth`: record boundary children as subtree roots.
   - Leaves encountered before `manifest_depth`: serialize and stop (no children).
4. For each subtree root: full DFS to leaves, serialize into chunk buffer.
5. Manifest buffer gets a 2-byte header prepended: `[tree_height, manifest_depth]`.

### Node serialization

Same format as `AVLTree::pack()` / the receiving side's parser:

- Internal: `0x00 | balance: i8 | key: 32B | left_label: 32B | right_label: 32B` (98 bytes)
- Leaf: `0x01 | key: 32B | value_len: u32 BE | value: [u8] | next_leaf_key: 32B` (69 + value_len)

### Consistency guarantee

redb read transactions are isolated — concurrent block application doesn't affect the snapshot. The JVM verifies root hash before and after; we get this for free.

### Delivery

This is a submodule change. A prompt goes to the state session with the full spec. The prompt will be written to `prompts/` as part of implementation.

## Component 2: `SnapshotStore` — main crate module

New file: `src/snapshot_store.rs`. Wraps `snapshots.redb`.

### Tables

| Table | Key | Value |
|-------|-----|-------|
| `manifests` | `[u8; 32]` (manifest_id) | raw manifest bytes |
| `chunks` | `[u8; 32]` (subtree_id) | raw chunk bytes |
| `metadata` | `&str` | `Vec<u8>` |

### Metadata keys

| Key | Value |
|-----|-------|
| `"snapshots_info"` | Serialized `Vec<(height, manifest_id)>` |
| `"snapshot:{manifest_id_hex}"` | List of chunk IDs belonging to this snapshot |

### Public API

```rust
impl SnapshotStore {
    pub fn open(path: &Path) -> Result<Self>
    pub fn write_snapshot(&self, dump: SnapshotDump, storing_snapshots: u32) -> Result<()>
    pub fn snapshots_info(&self) -> Result<Vec<SnapshotEntry>>
    pub fn get_manifest(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>>
    pub fn get_chunk(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>>
}
```

### `write_snapshot()` sequence

1. Insert manifest into `manifests` table keyed by `root_hash`.
2. Insert all chunks into `chunks` table keyed by subtree ID.
3. Record chunk ID list under `"snapshot:{id_hex}"` in metadata.
4. Append `(height, manifest_id)` to `"snapshots_info"`.
5. If info list exceeds `storing_snapshots`: prune oldest.

### Pruning

Simple deletion — no shared subtree analysis:
1. Load oldest snapshot's chunk ID list from metadata.
2. Delete all chunk IDs from `chunks` table.
3. Delete manifest from `manifests` table.
4. Remove chunk ID list from metadata.
5. Remove entry from `snapshots_info`.

All within a single write transaction for atomicity.

## Component 3: Main crate integration

### Config

New fields in `NodeConfig`:

```rust
#[serde(default)]
storing_snapshots: u32,                          // 0 = disabled
#[serde(default = "default_snapshot_interval")]
snapshot_interval: u32,                          // 52224 mainnet default
```

### Creation trigger

Located in `ValidationPipeline`. After applying a block in UTXO mode, check:
- `storing_snapshots > 0`
- `height % snapshot_interval == snapshot_interval - 1`
- `tip_height - height <= snapshot_interval` (near tip)

When triggered: spawn a blocking task that calls `RedbAVLStorage::dump_snapshot(14)` then `SnapshotStore::write_snapshot()`. The read transaction doesn't block ongoing block application.

### Request handlers

Intercept P2P request codes before they reach the sync machine:

| Incoming | Action | Response |
|----------|--------|----------|
| Code 76 (`GetSnapshotsInfo`) | `store.snapshots_info()` | Code 77 (`SnapshotsInfo`) |
| Code 78 (`GetManifest`) | `store.get_manifest(id)` | Code 79 (`Manifest`) |
| Code 80 (`GetUtxoSnapshotChunk`) | `store.get_chunk(id)` | Code 81 (`UtxoSnapshotChunk`) |

If lookup returns `None`: no response (matches JVM behavior).

Response encoding uses existing `SnapshotMessage::encode()` from `sync/src/snapshot/protocol.rs`.

### Threading model

- Request handling: cheap redb reads, runs in event loop.
- Snapshot creation: expensive but infrequent (once per 52k blocks mainnet), runs in spawned blocking task.

## Testing

### Unit tests

1. **`SnapshotStore` round-trip** — write synthetic snapshot, verify all lookups return correct data.
2. **Pruning** — write 3 snapshots with `storing_snapshots = 2`, verify oldest is fully deleted.
3. **Request handler encoding** — verify response messages encode correctly.

### Integration test (state submodule)

4. **`dump_snapshot()` round-trip** — build AVL+ tree, call `dump_snapshot(14)`, feed manifest + chunks through `parse_dfs_stream()` + `load_snapshot()`, verify root hash matches. This is the critical test.

### Live test (manual)

5. **Rust-to-Rust bootstrap** — Rust node A creates snapshot, Rust node B bootstraps from it. Not automated.

## SPECIAL profile

From `facts/snapshot.md`:

```
Serving: S7  P6  E7  C5  I7  A8  L6
```

Internal state is trusted. Performance matters for the tree walk. Low risk overall.

## Files touched

| File | Change |
|------|--------|
| `state/src/storage.rs` | Add `dump_snapshot()` + `SnapshotDump` (submodule prompt) |
| `src/snapshot_store.rs` | New — `SnapshotStore` module |
| `src/main.rs` | Config fields, `SnapshotStore` init, request handlers, creation trigger wiring |
| `src/lib.rs` | Export `snapshot_store` module |
| `facts/snapshot.md` | Update serving section status |
