# UTXO State Contract

## Component: `state/` (enr-state)

Persistent, versioned, crash-safe AVL+ authenticated dictionary. Implements the
`VersionedAVLStorage` trait from `ergo_avltree_rust` over redb. Does not know
what an ErgoBox, transaction, or block is — it stores arbitrary `(ADKey, ADValue)`
pairs in an authenticated tree with rollback support.

Primary consumer: `validation/UtxoValidator`, which applies block state changes
and verifies state root transitions. The state crate provides the persistence
and versioning; validation provides the Ergo semantics.

## SPECIAL Profile

```
S8  P6  E10 C6  I9  A8  L9
```

Crash recovery IS the product. Architecture is critical — wrong abstraction
over the AVL tree cascades everywhere. Performance matters during initial sync.
Internal component — no untrusted input (callers validate first).

## Design Principles

- **Ergo-agnostic.** Keys are `ADKey` (bytes). Values are `ADValue` (bytes).
  The crate depends on `ergo_avltree_rust` for the tree algorithm, not on
  `ergo-lib`, `ergo-chain-types`, or any Ergo domain types.
- **Single-file ACID.** One redb database file with multiple tables. Every
  `update()` is a single redb write transaction — nodes, undo records, and
  metadata are committed atomically. No partial writes, ever.
- **Undo log for rollback.** Modified and removed nodes are recorded per
  version. Rollback replays undo records in reverse. Matches the JVM's
  `LDBVersionedStore` strategy, but atomic where the JVM is not (two
  separate LevelDB databases that can diverge on crash between writes).
- **Lazy node loading.** The tree starts with only the root in memory.
  Child nodes are loaded from redb on demand via the `Resolver` callback.
  The full UTXO tree (~4M+ entries on mainnet) never needs to be in memory.
- **Configurable version retention.** `keep_versions` controls how many
  blocks of rollback history to maintain. Set to 0 during initial sync
  (no rollback needed far from tip). Matches JVM's dynamic `keepVersions`
  optimization.

## Storage Layout

Single redb file: `<data_dir>/state.redb`

### Tables

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `nodes` | `Digest32` (32 bytes) | packed node bytes | Current tree nodes |
| `undo` | `u64` (version LSN) | serialized undo record | Reverse operations per version |
| `metadata` | `&str` key name | `Vec<u8>` | Top node hash, height, version list |

### Metadata keys

| Key | Value | Format |
|-----|-------|--------|
| `"top_node_hash"` | Root node's label | 32 bytes |
| `"top_node_height"` | AVL tree depth | 4 bytes BE (u32) |
| `"current_version"` | Current ADDigest | 33 bytes |
| `"lsn"` | Latest logical sequence number | 8 bytes BE (u64) |
| `"versions"` | Serialized version chain (LSN+digest per entry) | variable |
| `"block_height"` | Caller-supplied block height at current version | 4 bytes BE (u32) |

`top_node_height` is the AVL+ tree's depth (a structural property of the
tree).  `block_height` is the caller's notion of "which block produced
this state" — opaque to this crate.  Writers supply it via
`update_with_height()` / `load_snapshot()`; readers fetch it via
`block_height()`.  It is written in the same redb transaction as every
state change, so `block_height()` and `version()` can never disagree
across a crash boundary.

### Node serialization

Uses `AVLTree::pack()` / `AVLTree::unpack()` from `ergo_avltree_rust`. Format:

- **Internal node:** `[0x00 | balance: i8 | key: 32B | left_label: 32B | right_label: 32B]`
- **Leaf node:** `[0x01 | key: 32B | value_len: u32 | value | next_key: 32B]`

Nodes are keyed by their Blake2b256 label (content hash). A "modified" node is
really a new node with a new label — the old label becomes a deletion.

### Undo record format

Per-version, serialized as:

```
[removed_count: u32]
  for each removed node:
    [label: 32B] [packed_len: u32] [packed_bytes]
[inserted_count: u32]
  for each inserted node:
    [label: 32B]
[prev_top_node_hash: 32B]
[prev_top_node_height: u32]
[prev_version_len: u32]
[prev_version: prev_version_len bytes]
[prev_block_height: u32]    — appended trailing field
```

- **Removed nodes**: labels + full packed bytes (to re-insert on rollback)
- **Inserted nodes**: labels only (to delete on rollback — bytes not needed)
- **Previous metadata**: restores the pre-update root state on rollback
- **prev_block_height** was appended after the original format shipped.
  Deserialization treats missing trailing bytes as `0` so pre-revision
  records still parse.  New records always carry the field.

## Trait Implementation: `VersionedAVLStorage`

```rust
pub trait VersionedAVLStorage {
    fn update(
        &mut self,
        prover: &mut BatchAVLProver,
        additional_data: Vec<(ADKey, ADValue)>,
    ) -> Result<()>;

    fn rollback(&mut self, version: &ADDigest) -> Result<(NodeId, usize)>;

    fn version(&self) -> Option<ADDigest>;

    fn rollback_versions<'a>(&'a self) -> Box<dyn Iterator<Item = ADDigest> + 'a>;

    /// Force a durable commit.  Default is a no-op.  Implementations using
    /// deferred durability (e.g. `Durability::None` on redb) must fsync
    /// the backing store here.  Callers invoke periodically during long
    /// write sequences and on graceful shutdown.
    fn flush(&self) -> Result<()> { Ok(()) }
}
```

The trait itself is block-height-unaware — it stays generic.  The block
height pathway is an inherent addition on `RedbAVLStorage`
(`update_with_height`, `block_height`, `load_snapshot` with the
`block_height` parameter).  Block-applying callers hold a concrete
`RedbAVLStorage` and call `update_with_height` directly rather than
going through `PersistentBatchAVLProver::generate_proof_and_update_storage`
— the prover's AD proof is fetched afterward via
`BatchAVLProver::generate_proof()`.

### `update()` / `update_with_height()` — persist tree changes

```rust
impl VersionedAVLStorage for RedbAVLStorage {
    fn update(&mut self, prover, additional_data) -> Result<()>;
}

impl RedbAVLStorage {
    pub fn update_with_height(
        &mut self,
        prover: &mut BatchAVLProver,
        additional_data: Vec<(ADKey, ADValue)>,
        block_height: u32,
    ) -> Result<()>;
}
```

Both delegate to a single internal routine.  The only difference:
`update_with_height` replaces the stored `block_height` with the
caller's value; `update()` preserves whatever was there (so plain
`update()` on non-empty storage never loses a previously-set
block_height).  On empty storage, plain `update()` initializes
block_height to `0`.

**Preconditions:**
- The prover has had operations applied via `perform_one_operation()`
- The prover was created with `collect_changed_nodes = true`

**Procedure (single redb write transaction):**

1. Compute the new digest: `prover.digest()`
2. Walk the prover's tree from root, collecting new/modified nodes:
   - Visit nodes where `is_new == true` or `visited == true` (root is always included)
   - Serialize each via `AVLTree::pack(node)`
   - Key = `node.label()` (32-byte Digest32)
3. Call `prover.removed_nodes()` to get labels of nodes no longer in the tree
4. Read `prev_block_height` from metadata (defaults to 0 if empty)
5. For each removed label, read its current packed bytes from `nodes` table
6. Write undo record:
   - Removed nodes: their labels + packed bytes (for re-insertion on rollback)
   - Inserted nodes: their labels (for deletion on rollback)
   - Previous top_node_hash, top_node_height, current_version, **block_height**
7. Write new/modified nodes to `nodes` table
8. Delete removed node labels from `nodes` table
9. Store `additional_data` entries in `nodes` table (metadata from caller)
10. Update metadata: top_node_hash, top_node_height, current_version, lsn++,
    **block_height** (caller-supplied for `update_with_height`, else the
    `prev_block_height` read in step 4)
11. Prune undo records older than `keep_versions` from the tip
12. Commit transaction

**Postconditions on Ok:**
- `version()` returns the new ADDigest
- `block_height()` returns the caller-supplied height (or the preserved
  prior value if `update()` was used) — same commit, cannot diverge from
  `version()` across a crash
- All new/modified nodes are durable in the `nodes` table
- All removed nodes are deleted from `nodes` but preserved in `undo`
- An undo record exists for this version (unless `keep_versions == 0`)

**Postconditions on Err:**
- Storage is unchanged (transaction rolled back)
- The prover's in-memory state may be inconsistent — caller should
  re-create or rollback the prover

### `rollback()` — restore tree to a previous version

**Preconditions:**
- `version` is present in `rollback_versions()`
- Undo records exist for all versions between current and target

**Procedure (single redb write transaction):**

1. Read undo records from current LSN backwards to the target version
2. For each undo record (newest first):
   - Delete inserted node labels from `nodes` table
   - Re-insert removed nodes (label → packed bytes) into `nodes` table
3. Restore metadata from the target version's undo record: top_node_hash,
   top_node_height, current_version, **block_height**
4. Delete undo records for rolled-back versions
5. Commit transaction
6. Unpack the root node from storage and return `(NodeId, height)`

**Postconditions on Ok:**
- `version()` returns the target ADDigest
- `block_height()` returns the value committed at the target version
- `nodes` table contains exactly the tree state at that version
- Rolled-back undo records are deleted (rollback is not reversible)

**Postconditions on Err:**
- Storage is unchanged

### `version()` — current ADDigest

Returns `None` if no updates have been applied (empty storage).
Returns `Some(digest)` matching the last successful `update()`.

### `block_height()` — caller-supplied block height at the current version

```rust
impl RedbAVLStorage {
    pub fn block_height(&self) -> Option<u32>;
}
```

Returns `None` iff `version()` is `None` (empty storage).  Otherwise
returns the `u32` the caller passed to the most recent
`update_with_height()` / `load_snapshot()`, or the value preserved by
`update()`, or the value restored by `rollback()`.

Invariant: `block_height().is_some() == version().is_some()`.  Once
set, block_height stays set — every mutating operation writes it in
the same transaction as the rest of the metadata, so there is no
crash-consistent state in which one exists and the other doesn't.

**Legacy migration:** a state.redb file written before the
block_height revision has `version()` set but no `block_height` key.
`open()` detects that and writes `block_height = 0` to restore the
invariant.  Callers that see `Some(0)` should treat it as ambiguous —
either a fresh genesis state, or legacy data — and fall back to a
header scan to disambiguate.  Non-zero values are always authoritative.

### `flush()` — force durable commit

```rust
impl VersionedAVLStorage {
    fn flush(&self) -> Result<()>;
}
```

`update()` commits with `Durability::None` — fast, but the commit
pointer may still be in the OS page cache when the process exits.
`flush()` runs an empty write transaction at `Durability::Immediate`,
which fsyncs all outstanding data and the metadata pointer, including
prior `None`-durability commits.  Call periodically in long write
sequences (every N blocks in the sync loop) and on graceful shutdown
to bound worst-case data loss to the flush interval.

### `rollback_versions()` — available rollback targets

Returns an iterator over ADDigests that `rollback()` can restore to.
Ordered newest-first. Length bounded by `keep_versions`.

## Struct: `RedbAVLStorage`

```rust
pub struct RedbAVLStorage {
    db: redb::Database,
    tree_params: AVLTreeParams,
    keep_versions: u32,
    // In-memory caches
    current_version: Option<ADDigest>,
    version_chain: VecDeque<(u64, ADDigest)>,  // (LSN, digest), newest first
}

pub struct AVLTreeParams {
    pub key_length: usize,    // 32 for Ergo (box ID length)
    pub value_length: Option<usize>,  // None (variable-length ErgoBox bytes)
}
```

### Construction

```rust
impl RedbAVLStorage {
    /// Open or create state storage.
    ///
    /// If the file exists, restores metadata from the database.
    /// If the file does not exist, creates it with empty tables.
    pub fn open(
        path: &Path,
        tree_params: AVLTreeParams,
        keep_versions: u32,
    ) -> Result<Self>;

    /// Update keep_versions at runtime.
    /// Used during initial sync: set to 0 far from tip, restore to
    /// configured value when approaching tip.
    pub fn set_keep_versions(&mut self, keep_versions: u32);

    /// Create the tree's Resolver function.
    /// The resolver loads nodes from the `nodes` table by label.
    /// See "Resolver Strategy" below.
    pub fn resolver(&self) -> Resolver;

    /// Read a single node's packed bytes by label.
    /// Used by the resolver and for direct lookups.
    pub fn get_node(&self, label: &Digest32) -> Result<Option<Bytes>>;

    /// Read top node hash and height from metadata.
    /// Returns None if storage is empty.
    pub fn root_state(&self) -> Option<(Digest32, usize)>;
}
```

### Initialization flow (for callers)

```
1. RedbAVLStorage::open(path, params, keep_versions)
2. If storage.version().is_some():
   a. Get resolver from storage
   b. Create AVLTree with resolver, key_length, value_length
   c. Create BatchAVLProver with tree, collect_changed_nodes=true
   d. Create PersistentBatchAVLProver with prover + storage
      (PersistentBatchAVLProver::new calls rollback internally)
3. If storage.version().is_none():
   a. Create empty AVLTree with resolver
   b. Create BatchAVLProver
   c. Apply genesis operations (insert genesis boxes)
   d. Create PersistentBatchAVLProver (calls generate_proof_and_update_storage)
```

## Resolver Strategy

### The problem

`ergo_avltree_rust` defines `Resolver = fn(&Digest32) -> Node` — a bare
function pointer that cannot capture state. The prover needs to load nodes
from storage during tree traversal, but the resolver cannot hold a database
reference. The crate's test suite only uses an in-memory `VersionedAVLStorageMock`
that saves `Rc<RefCell<Node>>` references — the resolver is never exercised
for real because all nodes stay in memory. Nobody has implemented real
persistence against this crate before.

### Fix: fork `ergo_avltree_rust`

Change the `Resolver` type from a bare function pointer to an `Arc`-wrapped
closure:

```rust
// Before (batch_node.rs):
pub type Resolver = fn(&Digest32) -> Node;

// After:
pub type Resolver = Arc<dyn Fn(&Digest32) -> Node + Send + Sync>;
```

`Arc` rather than `Box` because `AVLTree` derives `Clone` — `Box<dyn Fn>`
isn't `Clone`, but `Arc<dyn Fn>` is (cheap shared reference). `Send + Sync`
allows the resolver to be shared across threads (redb supports concurrent
reads). All call sites (`AVLTree::new()`, the `resolve()` method) work
identically — `(self.resolver)(digest)` calls both `fn` pointers and
`Arc<dyn Fn>` the same way.

Fork: `github.com/mwaddip/ergo_avltree_rust`. Change is verified — all 22
tests pass.

With this fix, `RedbAVLStorage::resolver()` returns a closure that captures
an `Arc<redb::Database>` and reads from the `nodes` table on demand:

```rust
pub fn resolver(&self) -> Resolver {
    let db = Arc::clone(&self.db);
    let params = self.tree_params.clone();
    Arc::new(move |digest: &Digest32| {
        // Read from nodes table, unpack, return Node
        // On miss: return LabelOnly (preserves digest label)
    })
}
```

### Fork management

Add to the existing sigma-rust fork workflow. The fork is a prerequisite
for the `state/` crate — without it, `VersionedAVLStorage` cannot be
implemented for real storage backends. PR the fix upstream — it's a
genuine defect that makes the persistence API non-functional.

## Version Retention

### Configuration

```toml
[node.utxo]
keep_versions = 200   # blocks of rollback history (JVM default: 200)
```

### Initial sync optimization

During initial sync, the node is applying blocks far below the chain tip.
Rollback to 200 blocks ago is meaningless when you're at height 50,000 and
the tip is at 265,000. Writing undo records for these blocks is pure waste.

**Strategy (matches JVM):**
- When `current_height < estimated_tip - keep_versions`:
  call `storage.set_keep_versions(0)` — no undo records written
- When `current_height >= estimated_tip - keep_versions`:
  call `storage.set_keep_versions(configured_value)` — resume undo recording

With `keep_versions = 0`, `update()` skips steps 4-5 (reading old node bytes
and writing undo records) and step 10 (pruning). This significantly reduces
I/O during initial sync.

**Consequence:** rollback is impossible for blocks applied with
`keep_versions = 0`. The sync machine must never attempt rollback into the
un-versioned range. This is safe because reorgs deeper than `keep_versions`
require re-syncing from scratch anyway.

## Crash Recovery

The primary design constraint. The node WILL be killed mid-operation.

### Guarantees

1. **Atomic updates.** Each `update()` is a single redb write transaction.
   If the process dies mid-write, redb's WAL ensures either the complete
   update is applied or nothing is.

2. **Consistent undo chain.** Undo records and node changes are in the same
   transaction. There is no window where the nodes have changed but the undo
   record hasn't been written (the JVM's two-DB design has this window).

3. **Recovery = just open the file.** redb handles WAL replay on open.
   No manual recovery code needed. `RedbAVLStorage::open()` reads metadata
   and is ready to use.

4. **Invariant after crash:** `version()` returns the digest of the last
   fully committed update. The `nodes` table is consistent with that version.
   Undo records exist for the last `keep_versions` updates.

### What callers must handle

- After restart, the prover's in-memory tree is gone. The caller creates a
  new `PersistentBatchAVLProver`, which calls `rollback()` to the stored
  version, restoring the root node from storage. Subsequent operations lazy-load
  the rest of the tree via the resolver.

## Bootstrap Modes

Not owned by this crate — bootstrap logic lives in the orchestration layer
(`src/main.rs` / sync machine). But the state crate's initialization API
supports all modes:

| Mode | How state/ is initialized | Requires |
|------|--------------------------|----------|
| `genesis` | Insert 3 genesis boxes into empty tree | All blocks from height 1 |
| `snapshot` | Bulk-load nodes from a UTXO snapshot | Snapshot download from peers |
| `trusted` | Bulk-load from a trusted source (testnet only) | Trusted API endpoint |

### Bulk loading

For snapshot and trusted bootstrap, the state crate provides:

```rust
impl RedbAVLStorage {
    /// Bulk-insert nodes without undo records.
    /// Used for initial population from a snapshot.
    ///
    /// Preconditions:
    ///   - Storage is empty (version() is None)
    ///   - Nodes form a valid AVL+ tree with the claimed root
    ///
    /// Postconditions:
    ///   - All nodes are in the `nodes` table
    ///   - version() returns the provided digest
    ///   - block_height() returns the provided block_height
    ///   - No undo records exist (rollback impossible before this point)
    pub fn load_snapshot(
        &mut self,
        nodes: impl Iterator<Item = (Digest32, Bytes)>,
        root_hash: Digest32,
        root_height: usize,
        version: ADDigest,
        block_height: u32,
    ) -> Result<()>;
}
```

`block_height` is written to metadata in the same transaction as the
bulk node load.  Pass the block height the snapshot was taken at — the
caller on resume reads `block_height()` to know exactly which block
the loaded UTXO set corresponds to.

The snapshot format (manifest + subtrees) is parsed by the caller. The state
crate receives pre-serialized `(label, packed_bytes)` pairs.

### Mainnet restriction

`trusted` bootstrap MUST be rejected for mainnet configurations. This is
enforced at the config validation layer, not in the state crate (which is
Ergo-agnostic and doesn't know what "mainnet" means).

## Integration: How Validation Uses State

`validation/UtxoValidator` (not yet built) will compose with `state/` as follows:

```
UtxoValidator owns:
  - PersistentBatchAVLProver (contains RedbAVLStorage + BatchAVLProver)
  - checkpoint_height
  - current parameters (from Extension sections)

validate_block(header, block_txs, None, extension, preceding_headers):
  1. Parse BlockTransactions → Vec<Transaction>  (shared with DigestValidator)
  2. Compute stateChanges                        (shared with DigestValidator)
  3. For each operation:
       prover.perform_one_operation(op)
       → Remove/Lookup returns old value (input box bytes)
       → Insert stores new value (output box bytes)
  4. Verify: prover.digest() == header.state_root
  5. prover.generate_proof_and_update_storage(metadata)
     → persists tree changes + generates AD proof as side effect
  6. Transaction validation (above checkpoint_height)  (shared with DigestValidator)
  7. Return Ok — state is now at header.state_root

reset_to(height, digest):
  persistent_prover.rollback(&digest)
```

Steps 1, 2, and 6 are shared code with DigestValidator — only steps 3-5
differ (prover vs verifier, persistent vs ephemeral).

The generated AD proof from step 5 can optionally be served to digest-mode
peers, but this is a future concern (Phase 6).

## At-tip storage reopen contract (v0.4.x)

Operators may reopen the storage with a smaller redb cache once chain
sync reaches tip (drives RSS down by ~80% on mainnet at-tip). The
mechanism lives outside this crate (sync triggers, main rebuilds), but
the state crate's contract enables it through three guarantees:

1. **Drop-and-reopen safety.** `RedbAVLStorage` holds an
   `Arc<Database>` and exposes `snapshot_reader()` that clones it.
   When all `Arc<Database>` holders (the storage itself plus every
   `SnapshotReader` issued from it) drop, redb releases the OS-level
   exclusive file lock and a fresh `RedbAVLStorage::open(...)` on
   the same path with a different `cache_size` succeeds. The state
   crate makes no claims about *who* releases the Arcs — only that
   when they're all gone, reopen works.

2. **Version() and block_height() persistence across reopen.** A
   freshly-opened storage on a path that already had committed data
   reports the most recent committed `version()` and `block_height()`
   without further action. The caller can `storage.rollback(&v)` to
   the current version (no-op short-circuit) to materialize the
   prover's root, then resume normal operation. Same shape as the
   genesis-resume branch.

3. **Rollback target stability.** Versions in `rollback_versions()`
   from the old storage instance are preserved across reopen
   (versions live in the persisted UNDO_TABLE, not in `RedbAVLStorage`
   in-memory state). A reopen does not invalidate `rollback_versions`.

The integrator side: main repo holds a `SwappableReader` (a
`parking_lot::RwLock<Option<Arc<SnapshotReader>>>`) shared with
mempool, REST API, and the snapshot dump trigger. To reopen:

- `swap.take()` — releases the wrapper's Arc.
- The validator (which owns the storage) is dropped — releases its
  Arc.
- All other Arc holders must use the wrapper's `current()` (which
  returns `Option<Arc<SnapshotReader>>` per call), not cache the Arc
  across `.await` points.
- `RedbAVLStorage::open(path, params, keep_versions, new_cache)`
  succeeds.
- Build a new prover via the resume pattern; `swap.install(new_sr)`.

If any Arc leaks past the swap, redb's file lock blocks the new
open; the integrator's `open_state_with_retry(30 × 200ms = 6s budget)`
covers transient holders (e.g. an in-flight snapshot dump).

## Does NOT own

- Block parsing or validation — that's `validation/`
- Deciding when to apply blocks or rollback — that's `sync/`
- Bootstrap orchestration — that's `src/main.rs`
- Network I/O — that's `p2p/`
- Configuration parsing — that's `src/`
- Ergo-specific types — no ErgoBox, no Transaction, no Header

## Dependencies

- `ergo_avltree_rust` — `BatchAVLProver`, `VersionedAVLStorage`, `AVLTree`, node types
- `redb` — storage backend
- `bytes` — `Bytes`, `BytesMut` for node serialization
- `blake2` — only if needed for label verification (optional, tree computes labels)
- No dependency on `ergo-lib`, `ergo-chain-types`, or any Ergo domain crates

## Invariants

- No method panics.
- `version()` is always consistent with the `nodes` table.
- If `rollback_versions()` yields a digest, `rollback()` to that digest will succeed.
- The `nodes` table never contains orphaned entries (nodes not reachable from root).
  - Exception: crash during `update()` with `keep_versions = 0` could theoretically
    leave new nodes written but metadata not yet updated. redb's ACID prevents this.
- `update()` is not re-entrant. One update at a time.
- After `rollback()`, the returned `(NodeId, height)` reconstructs the exact tree
  state at that version — same root label, same tree structure.

## Testing Strategy

Because the crate is Ergo-agnostic, tests use synthetic key-value data:

1. **Unit tests:** Insert/remove/lookup sequences, verify digest changes, verify
   rollback restores exact prior digest.
2. **Crash simulation:** Write a partial update (kill the write transaction),
   reopen, verify storage is at the pre-update version.
3. **Undo chain:** Apply N blocks, rollback M (M ≤ keep_versions), verify state.
   Attempt rollback beyond keep_versions — verify error.
4. **keep_versions=0:** Apply blocks without undo, verify no undo records,
   verify rollback fails gracefully.
5. **Bulk load:** Load a synthetic snapshot, verify root digest matches,
   verify lookups work.
6. **Integration (in validation/):** Apply real testnet blocks via UtxoValidator,
   compare resulting digest with known header state_roots.
