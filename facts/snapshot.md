# UTXO Snapshot Sync Contract

## Overview

Bootstrap a UTXO state from a peer-served snapshot instead of applying every
block from genesis. Downloads a manifest (the top 14 levels of the AVL+ tree)
and ~32k subtree chunks, reconstructs the state in `enr-state`, then resumes
normal block sync from `snapshot_height + 1`.

This contract covers two capabilities:

1. **Receiving** (bootstrapping): discover, download, verify, and apply a snapshot.
   This is the priority — it eliminates the 267k+ block replay for new nodes.
2. **Serving** (creating snapshots): periodically dump the AVL+ tree and respond
   to peer requests. Lower priority — requires a synced UTXO node first.

## SPECIAL Profile

Receiving:
```
S9  P8  E9  C5  I8  A7  L8
```
A bad snapshot corrupts the entire UTXO state. Verification against `stateRoot`
is the only safety net. Crash recovery during multi-hour chunk downloads must
resume, not restart. Untrusted peers serve the data.

Serving:
```
S7  P6  E7  C5  I7  A8  L6
```
Internal state is trusted. Performance matters — snapshot dump walks the full
tree. Serving is just reading from storage, low risk.

## P2P Messages

Six new message codes. The P2P routing layer forwards unknown codes to all
peers of opposite direction — no changes to `p2p/` needed. Parsing and
handling happen in the sync machine (receiving) and main crate (serving).

| Code | Name | Direction | Payload | Max size |
|------|------|-----------|---------|----------|
| 76 | `GetSnapshotsInfo` | Request | Empty | 100 B |
| 77 | `SnapshotsInfo` | Response | `VLQ(count) + count × (ZigZag+VLQ(height) + 32B(manifest_id))` | 20,000 B |
| 78 | `GetManifest` | Request | `32B(manifest_id)` | 100 B |
| 79 | `Manifest` | Response | `VLQ(len) + manifest_bytes` | 4,000,000 B |
| 80 | `GetUtxoSnapshotChunk` | Request | `32B(subtree_id)` | 100 B |
| 81 | `UtxoSnapshotChunk` | Response | `VLQ(len) + subtree_bytes` | 4,000,000 B |

**All integer fields use Scorex serialization** (VLQ for unsigned, ZigZag+VLQ
for signed). This was verified against real testnet wire captures — the initial
assumption of BE u32/i32 was wrong. The JVM's `MessageSpecV1` uses `putUInt`
(VLQ) and `putInt` (ZigZag+VLQ) for all integer serialization.

### Peer filtering

Only peers with version >= 5.0.12 support snapshot messages. The sync machine
must filter peers before sending `GetSnapshotsInfo`. This version is available
from the handshake — the P2P layer already exposes it.

### Internal type IDs (delivery tracker)

| Type | ID | Purpose |
|------|-----|---------|
| `UtxoSnapshotChunk` | -126 (0x82) | Track chunk delivery/timeout |
| `SnapshotsInfo` | -125 (0x83) | Track info request |
| `Manifest` | -124 (0x84) | Track manifest delivery |

These are NOT wire codes — they're internal identifiers for the delivery tracker,
matching the JVM's `NetworkObjectTypeId` values.

## Manifest Format

The manifest is the AVL+ tree serialized from root to a fixed depth via
pre-order DFS traversal.

### Header

```
byte: root_height      // height of the full AVL+ tree
byte: manifest_depth   // depth cutoff (14 on mainnet)
```

### Node serialization

Same format as `enr-state`'s `AVLTree::pack()`:

**Internal node:**
```
0x00 | balance: i8 | key: 32B | left_label: 32B | right_label: 32B
```
98 bytes total.

**Leaf node:**
```
0x01 | key: 32B | value_len: u32 BE | value: [u8] | next_leaf_key: 32B
```
69 + value_len bytes total.

### Traversal rules

**Important:** The JVM starts the DFS at level = 1 (not 0). The root node is
at level 1. This was verified against real manifest data — starting at level 0
produces subtree IDs one level too deep that the JVM doesn't recognize.

```
// JVM's ManifestSerializer.serialize():
loop(rootNode, level = 1)

loop(node, level):
  write node bytes (internal or leaf format above)
  if node is leaf:
    return  // leaves terminate regardless of level
  if level == manifest_depth:
    return  // boundary — children become subtree roots
  if level < manifest_depth:
    loop(node.left,  level + 1)
    loop(node.right, level + 1)
```

At `level == manifest_depth`, the internal node's `left_label` and
`right_label` are the **subtree IDs** — the chunks to download.

### Manifest ID

Blake2b256 hash of the root node (same as the root's label in the AVL+ tree).
This must match the first 32 bytes of the block header's `stateRoot` at the
snapshot height. The header's `stateRoot` is 33 bytes: `[root_hash: 32B | tree_height: 1B]`.

### Verification

```
verify(manifest, expected_state_root: [u8; 33]):
  let (expected_root, expected_height) = split_digest(expected_state_root)
  assert manifest root label == expected_root
  assert manifest root_height == expected_height
```

### Subtree extraction

Walk the deserialized manifest. Collect `(left_label, right_label)` from every
internal node at `depth == manifest_depth`. These are the subtree IDs. For a
balanced tree at depth 14: up to 2^14 = 16,384 boundary nodes, yielding up to
32,768 subtree IDs. In practice, fewer — the tree isn't perfectly balanced and
some branches terminate in leaves before depth 14.

## Subtree (Chunk) Format

A subtree is the full serialization of all nodes below a boundary node in the
manifest. Same pre-order DFS traversal, same node format, no depth cutoff —
recurse until all leaves are reached.

```
[node bytes...]  // pre-order DFS, self-describing via 0x00/0x01 prefix
```

No header, no length prefix in the serialized bytes themselves (the P2P message
wraps them with a u32 length).

### Chunk ID

Blake2b256 hash of the subtree root node's label. Must match the `left_label`
or `right_label` from the manifest's boundary nodes.

### Chunk verification

Each chunk's root label must match the subtree ID that was requested. After
deserialization, the tree structure is self-consistent (parent labels are
Blake2b256 of their children). No additional verification needed per chunk —
the manifest's verified root transitively authenticates all chunks.

## Receiving: Sync State Machine

### Preconditions for snapshot sync

All must be true:
- `utxo_bootstrap` enabled in config
- Header chain is synced (existing header sync reached `Synced` state)
- No full blocks have been applied (`validated_height == 0`)
- No snapshot has been applied yet

### State machine

```
┌─────────────────┐
│  HeaderSync      │  (existing)
│  reaches Synced  │
└────────┬────────┘
         │ utxo_bootstrap && validated_height == 0
         ▼
┌─────────────────┐
│  Discovery       │  GetSnapshotsInfo → collect responses → validate
└────────┬────────┘
         │ quorum reached (≥ min_snapshot_peers for same manifest)
         ▼
┌─────────────────┐
│  ManifestDownload│  GetManifest → verify against stateRoot
└────────┬────────┘
         │ manifest verified
         ▼
┌─────────────────┐
│  ChunkDownload   │  GetUtxoSnapshotChunk × N, parallel
└────────┬────────┘
         │ all chunks received
         ▼
┌─────────────────┐
│  StateInit       │  Reconstruct AVL+ tree in enr-state
└────────┬────────┘
         │ state initialized, validated_height = snapshot_height
         ▼
┌─────────────────┐
│  BlockSync       │  Normal block download from snapshot_height + 1
└────────┘
```

### Discovery

1. Send `GetSnapshotsInfo` (code 76) to all connected peers with version >= 5.0.12.
2. For each `SnapshotsInfo` response (code 77):
   - For each `(height, manifest_id)` pair:
     - Look up the header at `height` via `SyncChain::header_at(height)`
     - Split the header's `state_root` into `(root_hash, tree_height)`
     - Verify `manifest_id == root_hash`
     - If valid: record `(manifest_id → (height, peer))` in available manifests map
3. After each response, check quorum:
   - Group by `manifest_id`
   - Select the manifest with the **highest height** that has `>= min_snapshot_peers` announcing peers
   - If found: transition to ManifestDownload

**Config:** `min_snapshot_peers` (default: 2). Matches JVM's `p2pUtxoSnapshots`.

**Timeout:** If no quorum after 60 seconds, re-broadcast `GetSnapshotsInfo` to
any newly connected peers. Do not spam the same peers.

### Manifest download

1. Pick a random peer from the quorum set for the selected manifest.
2. Send `GetManifest` (code 78) with the `manifest_id`.
3. On response (code 79):
   - Parse the manifest bytes (2-byte header + DFS node traversal)
   - Verify against the header's `state_root` at the snapshot height
   - Extract subtree IDs from boundary nodes
   - Transition to ChunkDownload
4. On timeout: retry with a different peer from the quorum set.

### Chunk download

Parallel download with bounded concurrency:

- **`chunks_in_parallel`**: 16 (minimum in-flight requests)
- **`chunks_per_peer`**: 4 (requests batched per peer per round)
- **Delivery timeout**: 4× normal delivery timeout

Scheduler:
1. Maintain a set of `pending_subtree_ids` (not yet requested or timed out).
2. Maintain a set of `in_flight` (requested, awaiting response).
3. While `in_flight.len() < chunks_in_parallel` and `pending_subtree_ids` is non-empty:
   - Pick a random peer from the quorum set
   - Take up to `chunks_per_peer` IDs from `pending_subtree_ids`
   - Send `GetUtxoSnapshotChunk` (code 80) for each
   - Move IDs to `in_flight`
4. On chunk response (code 81):
   - Verify the chunk was in `in_flight`
   - Store the raw chunk bytes keyed by subtree ID
   - Remove from `in_flight`
   - If `pending_subtree_ids` is empty AND `in_flight` is empty: all done
   - Else: schedule more requests
5. On timeout: move timed-out IDs back to `pending_subtree_ids`, pick different peer.

### Chunk storage during download

Chunks are stored in a temporary redb file (`<data_dir>/snapshot_download.redb`)
that tracks both the raw chunk bytes and download progress. No in-memory
alternative — the mainnet UTXO set is ~1-2 GB of chunk data, and holding that
in a HashMap invites OOM on constrained hardware. The redb file is strictly
better: flat memory usage, crash-safe, and enables resume on restart.

#### Tables

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `chunks` | `SubtreeId` (32B) | raw chunk bytes | Downloaded chunk data |
| `metadata` | `&str` key name | `Vec<u8>` | Download state |

#### Metadata keys

| Key | Value |
|-----|-------|
| `"manifest_id"` | 32B — which manifest this download is for |
| `"snapshot_height"` | u32 BE — block height of the snapshot |
| `"manifest_bytes"` | raw manifest (to re-extract subtree IDs on resume) |
| `"total_chunks"` | u32 BE — expected number of chunks |

#### Lifecycle

1. **Created** when manifest is verified — write manifest bytes and metadata.
2. **Appended** as chunks arrive — one write transaction per chunk.
3. **Read** on state initialization — iterate all chunks, feed to `load_snapshot()`.
4. **Deleted** after `load_snapshot()` succeeds.

#### Crash recovery

On startup, if `snapshot_download.redb` exists:
- Read metadata to recover `manifest_id` and `snapshot_height`.
- Verify the manifest still matches the header chain's `state_root` at that height
  (headers may have been reorganized while we were down).
- If valid: count stored chunks vs. `total_chunks`, rebuild `pending_subtree_ids`
  from the manifest for any missing chunks, resume download.
- If invalid (reorg changed the state root): delete the file, restart discovery.

This means chunk download survives `kill -9` at any point and resumes from where
it left off. No chunk is ever re-downloaded unless the snapshot itself is invalidated.

### State initialization

After all chunks are downloaded:

1. Parse the manifest into individual nodes: `Vec<(Digest32, Vec<u8>)>`
   where each entry is `(node_label, packed_node_bytes)`.
2. Parse each chunk into individual nodes: same format.
3. Concatenate manifest nodes + all chunk nodes.
4. Call `RedbAVLStorage::load_snapshot(nodes, root_hash, root_height, version_digest)`.
5. Create `PersistentBatchAVLProver` from the populated storage.
6. Set `validated_height = snapshot_height`.
7. Set `downloaded_height = snapshot_height`.
8. Discard downloaded chunk data (the nodes are now in `state.redb`).
9. Transition to normal block section download from `snapshot_height + 1`.

### Integration with SyncTransport

The sync machine communicates via `SyncTransport`. New message types (76-81) need
to flow through the existing trait. Two options:

**Option A**: Extend `ProtocolEvent` to include snapshot message variants. The main
crate's transport implementation maps raw P2P message codes to these variants.
The sync machine handles them in its event loop.

**Option B**: Separate transport trait for snapshot sync. The sync machine gets
a `SnapshotTransport` alongside `SyncTransport`.

Option A is preferred — snapshot sync is a phase of the sync state machine, not
a separate component. Adding variants to `ProtocolEvent` keeps the event loop
unified.

### Progress reporting

During chunk download, report progress as `chunks_received / total_chunks`.
This is display-only — no correctness dependency on the progress channel.

## Serving: Snapshot Creation and Responses

**Status: Implemented** (pending state submodule `dump_snapshot()` integration).

Storage: `src/snapshot_store.rs` (`snapshots.redb`, redb 4).
Request handlers: `src/snapshot_serve.rs` (codes 76/78/80).
Creation trigger: `src/main.rs` (polls every 30s, fires at `height % interval == interval - 1`).
Prompt for state submodule: `prompts/state-dump-snapshot.md`.

### Snapshot creation

**When:** After applying a full block, if:
- `storing_snapshots > 0` in config (default: 0 = disabled)
- `height % snapshot_interval == snapshot_interval - 1`
- Node is near the chain tip (`tip - height <= snapshot_interval`)

**Snapshot interval:** 52,224 blocks on mainnet. Configurable for testnet.

**Procedure:**
1. Take a consistent read snapshot of the AVL+ tree (redb read transaction).
2. Serialize the manifest: DFS from root to depth 14, using `AVLTree::pack()`.
3. Serialize each subtree: DFS from each boundary node's children to leaves.
4. Store manifest + subtrees keyed by their Blake2b256 labels.
5. Update `SnapshotsInfo` metadata: `(height, manifest_id)`.
6. Prune old snapshots, keeping only the most recent `storing_snapshots`.

**Storage:** Separate from `state.redb`. Snapshots are read-only after creation
and can be stored in a dedicated redb file or flat files. Implementation choice.

### Responding to requests

Handled in the main crate (not sync machine — serving is not a sync concern):

- `GetSnapshotsInfo` (76) → respond with `SnapshotsInfo` (77) listing stored snapshots
- `GetManifest` (78) → look up manifest bytes by ID, respond with `Manifest` (79)
- `GetUtxoSnapshotChunk` (80) → look up chunk bytes by ID, respond with `UtxoSnapshotChunk` (81)

If the requested ID is not found, send no response (matches JVM behavior —
no explicit "not found" message in the protocol).

## Configuration

```toml
[node.utxo]
# Existing
state_type = "utxo"
keep_versions = 200

# New — receiving
utxo_bootstrap = false          # Enable snapshot bootstrapping
min_snapshot_peers = 2          # Quorum before downloading

# New — serving
storing_snapshots = 0           # How many snapshots to keep (0 = disabled)
snapshot_interval = 52224       # Blocks between snapshots (mainnet default)
```

## Dependencies

### enr-state (state/)

Already has `load_snapshot()` in its contract. No new trait methods needed.
The snapshot sync code parses manifest/subtree bytes into `(Digest32, Vec<u8>)`
pairs and feeds them to `load_snapshot()`.

### ergo-sync (sync/)

New sync phase. Needs:
- Extended `ProtocolEvent` for snapshot messages
- `SnapshotSyncState` struct managing discovery → download → init
- Integration into the main sync loop (between header sync and block download)

### SyncChain trait

Needs one addition:
```rust
/// Split a header's state_root into (root_hash, tree_height).
fn split_state_root(state_root: &[u8; 33]) -> ([u8; 32], u8);
```

Or this can be a free function — it's pure byte slicing, no chain state needed.

### Main crate (src/)

- Serving: handle incoming codes 76, 78, 80 and respond
- Config: new fields in `[node.utxo]`
- Startup: pass `utxo_bootstrap` flag to sync machine

## Does NOT own

- AVL+ tree internals — that's `enr-state` and `ergo_avltree_rust`
- Header validation — that's `enr-chain`
- P2P framing — that's `p2p/` (messages flow through as unknown codes)
- Block validation — that's `ergo-validation`
- When to create snapshots — that's the main crate's block application loop

## Invariants

- State is never initialized from a snapshot unless the manifest verifies
  against a header's `state_root` that exists in the validated header chain.
- Chunk download never writes to `state.redb` — state initialization is atomic,
  after all chunks are verified and available.
- A crash during chunk download leaves `state.redb` empty (or at its prior state).
  `snapshot_download.redb` preserves progress. On restart, the sync machine
  detects the partial download, validates it against the header chain, and
  resumes from where it left off.
- Snapshot serving never interferes with state writes — snapshots are created
  from read-only views and stored separately.
- `validated_height` is only set to `snapshot_height` after `load_snapshot()`
  succeeds. No intermediate states.

## Testing Strategy

1. **Manifest parsing**: Serialize a known AVL+ tree at depth 14, parse it back,
   verify subtree IDs match expected labels.
2. **Chunk parsing**: Serialize subtrees, parse back, verify node labels.
3. **Round-trip**: Create a tree, dump manifest + chunks, reconstruct via
   `load_snapshot()`, verify root digest matches.
4. **Verification**: Tamper with manifest bytes, verify rejection. Tamper with
   a chunk, verify the reconstructed tree has wrong root.
5. **Integration**: If a JVM testnet peer is available, request a real snapshot
   and verify reconstruction produces the expected state root.
6. **Crash recovery**: Kill during chunk download, restart, verify state.redb
   is clean and snapshot sync restarts.
