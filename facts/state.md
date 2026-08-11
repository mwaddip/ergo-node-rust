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

**Cross-DB durability**: `block_height()` is the canonical record of
"this storage has applied blocks up through height H." Higher-level
code (see `facts/sync.md` "Cross-DB Durability Handshake") may persist
a durable mirror of this value in a sibling database for startup
reconciliation against unclean shutdown. This crate makes no claim
about external mirrors; the caller coordinates the flush ordering.
From this crate's perspective the canonical flush API is `flush()` and
the canonical height is `block_height()`.

### `rollback_versions()` — available rollback targets

Returns an iterator over ADDigests that `rollback()` can restore to.
Ordered newest-first. Length bounded by `keep_versions`.

## Page cache observability (added 2026-08-12)

### `RedbAVLStorage::cache_bytes_used(&self) -> u64`

`Database::cache_stats().used_bytes()` for `state.redb`, surfaced by
`GET /debug/memory` as `stateCacheBytes`.

Requires redb's `cache_metrics` feature, declared in this crate's `Cargo.toml`
as well as the workspace root — a root-only declaration is not unified into a
standalone `cargo test -p enr-state` build, and the accessor would silently
return 0.

### `SnapshotReader::cache_bytes_used(&self) -> u64`

Same figure, reachable from a reader rather than the storage.

Required because the API's only handle on state is `SwappableReader →
Arc<SnapshotReader>`; `RedbAVLStorage` itself is moved into the validator at
startup and is not reachable from `ergo_api::UtxoAccess`. `SnapshotReader`
already holds the same `Arc<Database>`, so this is the same `cache_stats()`
call from the type the API can actually see.

Returns the live figure even mid-swap: a reader that exists has a database.
When no reader exists at all (`SwappableReader::current()` is `None`), the
adapter reports `None` and the field is omitted — consistent with every other
lookup through that reader returning `None` during the reopen window.

`open()` already takes a `CacheSize` and needs no signature change; only the
value the caller passes changes, since `cache_mb` now describes a total shared
with `modifiers.redb` rather than this database alone.

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

## Offline Compaction

### Why

Two mechanisms have historically left unreachable rows in `nodes`:

1. **Rollback skip** (`storage.rs`, rollback path). Deleting an undo record's
   `inserted_labels` is unsafe while those labels may still be referenced by
   the rolled-back-to tree or by older versions in the chain, so the deletion
   is skipped. Fires only on reorg or a failed apply.
2. **Call-order regression** (node v0.7.5–v0.7.9). `generate_proof()` cleared
   the changed-node buffers before `update_internal` read them via
   `removed_nodes()`, so the per-block delete list was empty and *every*
   superseded node leaked. Fixed by restoring `update` → `generate_proof`.
   Observed: 235 GB at height ~1.66M against a ~4–8 GB live tree.

Both produce the same artefact: rows in `nodes` unreachable from any live
root. The tree itself stays consistent — this is wasted space, not corruption.
No consensus impact, and no resync is required for correctness.

Compaction reclaims that space offline.

### Approach: rewrite, not sweep

Compaction is a **rewrite of the reachable tree into a fresh database**, not a
mark-and-sweep of the existing one.

A sweep would have to mark every node reachable from all `keep_versions + 1`
retained roots. Those roots share nearly all their nodes, so correctness
demands a visited set spanning every reachable digest — at mainnet scale
plausibly hundreds of millions of 32-byte labels, tens of GB of RAM. It is not
guaranteed to run on operator hardware.

A rewrite walks the single current root. Every live node in an AVL+ tree has
exactly one parent, so a depth-first traversal visits each node exactly once
with no visited set at all. Memory is O(tree depth) — around 30 frames.

Because the output database is built fresh containing only live nodes, it is
minimal by construction. **`redb::Database::compact()` is not used and is not
required.**

### API

```rust
impl RedbAVLStorage {
    /// Rewrite the reachable tree from `source` into `dest`.
    ///
    /// Neither opens `source` for write. On any error `dest` is removed and
    /// `source` is left untouched.
    ///
    /// Returns statistics for the caller to report.
    pub fn compact_to(
        source: &Path,
        dest: &Path,
        progress: Option<&dyn Fn(CompactionProgress)>,
    ) -> Result<CompactionStats>;
}

pub struct CompactionStats {
    pub nodes_written: u64,
    pub source_bytes: u64,
    pub dest_bytes: u64,
    pub block_height: u32,
    pub digest: ADDigest,
}

pub struct CompactionProgress {
    pub nodes_written: u64,
}
```

### Preconditions

- No process holds `source` open. redb's file lock enforces this; surface the
  lock error as "stop the node first", not as a raw redb error.
- **`source` was last closed cleanly.** A database whose final commit left no
  allocator-state table — i.e. the node was SIGKILLed rather than stopped —
  cannot be opened read-only at all: redb returns `RepairAborted`, and a
  read-only open is not permitted to run the repair. Surface this as "start
  the node once and stop it gracefully, then retry", not as a raw redb error.
  This is a live risk on any first compaction of a database from a crashed
  node.
- `source` has a `current_version` and a `top_node_hash` — i.e. it is not an
  empty/uninitialised database. Empty source is an error, not a no-op.
- `dest` does not already exist.

### Tree parameters

`compact_to` takes no `AVLTreeParams`. Leaf label verification needs
`key_length` and `value_length`, which are recovered rather than supplied:

- `key_length` is derived from the root node's packed bytes and then confirmed
  cryptographically — a wrong derivation cannot produce matching labels, so it
  fails closed.
- `value_length` is assumed `None` (variable-length values), which is what
  every database this crate writes uses.

A source built with a fixed `value_length` is therefore **unsupported**, but
unsupported safely: leaf parsing would misread, recomputed labels would not
match, and verification returns `Err`. There is no path to a false `Ok`.

Keeping the parameters out of the signature is deliberate — an offline tool
that requires the operator to remember the parameters a file was created with
is its own footgun, and the file is self-describing enough to avoid it.

### Algorithm

1. Open `source` read-only. Read `top_node_hash`, `top_node_height`,
   `current_version`, `block_height`, `lsn` from `metadata`.
2. Create `dest`. Depth-first from `top_node_hash`: read the packed bytes for
   each label from the source `nodes` table, write them to the dest `nodes`
   table under the same label, recurse into `left_label` / `right_label` for
   internal nodes. Leaves terminate.
   - A label present in the tree but missing from source `nodes` is a
     **fatal** error — the source is genuinely corrupt, not merely bloated.
     Do not skip it.
3. Write `metadata`: `top_node_hash`, `top_node_height`, `current_version`,
   `block_height` and `lsn` carried over verbatim; `versions` set to a single
   entry `(lsn, current_version)`.
4. Leave `undo` empty.
5. Commit with `Durability::Immediate` and `set_quick_repair(true)`.
6. **Verify before returning** (see below). On failure, delete `dest` and
   return `Err`.

### Verification (mandatory, in-process)

After committing `dest`, reopen it and walk the whole reachable tree again,
recomputing **every** node's label from that node's own stored bytes and
comparing it against the label its parent references. The root's recomputed
label plus `top_node_height` must equal `current_version` read from `source`.

`compact_to` MUST NOT return `Ok` without this check passing.

**Do not shortcut this to "load the root and recompute the digest."** That
verifies exactly one row. Unpacking a node yields `LabelOnly` children whose
labels are read out of the *parent's* packed bytes, not out of the children's
own stored rows — so re-hashing the root merely re-hashes bytes that were
copied verbatim from source, and a missing, truncated, or altered node
anywhere below the root goes undetected. An earlier revision of this contract
asserted the shortcut was sufficient; it is not.

The recursive form is what makes the check cryptographic rather than a
heuristic. Each label is a Blake2b256 over the node's contents including its
children's labels, so recomputing bottom-up chains every stored row into the
root digest. A match then genuinely proves the rewrite preserved every
reachable node exactly.

Cost is a second full pass over `dest` — but `dest` is the *compacted* database,
so on a heavily bloated source the pass is close to free. Measured at mainnet
scale: copy 15m24s (random reads across a 247 GB source), verify **30s** (2.16 GB
dest, largely page-cached). Verification was **3%** of total runtime. There is
no efficiency argument for trading the guarantee away.

### Postconditions

On `Ok`:

- `source` is byte-identical to before the call.
- `dest` contains exactly the nodes reachable from the current root.
- `dest`'s digest equals `source`'s `current_version`.
- `dest`'s `block_height` equals `source`'s.
- `dest` has one version in its chain and an empty `undo` table.
- Any rows `update()` wrote into `nodes` from its `additional_data` argument
  that are not reachable from the root are **dropped**, being
  indistinguishable from orphans. Every caller in this workspace passes
  `vec![]`, so this is currently vacuous — but a future caller that uses
  `additional_data` for side-band storage must not expect it to survive
  compaction.

On `Err`:

- `source` is byte-identical to before the call.
- `dest` does not exist.

### What compaction drops

The rollback window. `dest` retains a single version, so rollback is
impossible until the node has applied `keep_versions` further blocks and
rebuilt the chain. A reorg deeper than the rebuilt window during that period
requires a resync.

This is the deliberate trade for O(depth) memory. Retaining the window would
reintroduce the multi-root visited set that makes sweeping impractical.

### Swap protocol

`compact_to` never touches the live path. Swapping is the caller's decision:

1. Stop the node.
2. `compact_to("state.redb", "state.redb.compacted")`.
3. Inspect `CompactionStats` — in particular that `digest` matches the
   `stateRoot` of the header at `block_height`, which can be cross-checked
   against the chain independently of this crate.
4. Move `state.redb` aside (do not delete), rename `state.redb.compacted` into
   place, start the node, confirm it resumes at `block_height` and applies the
   next block.
5. Only then remove the set-aside original.

### Characteristics

**Memory is engineered, not inherent.** The traversal itself is O(tree depth)
— an explicit stack, ~35 frames at mainnet size, no visited set. But redb
defaults to a **1 GiB page cache per handle**, and with that default peak RSS
tracks source file size rather than live-set size. Every handle opened by
`compact_to` must therefore pin its cache explicitly.

Measured on a synthetic 800k-node tree (1.08 GB source → 269 MB dest):

| Page cache | Peak RSS | Runtime |
|---|---|---|
| 1 GiB (redb default) | 414 MB | 9.6 s |
| 256 MB (chosen) | 275 MB | 10.0 s |
| 32 MB | 39 MB | 16.5 s |

At 32 MB the peak is flat — 4× the nodes moved RSS by ~5% — which isolates the
page cache as the only size-dependent term. 256 MB is the knee: 4× more cache
buys ~4% throughput.

Resulting profile: peak RSS roughly constant regardless of source size, plus
redb allocator bookkeeping that scales with the **live** tree only.

**Validated at mainnet scale 2026-08-03** — a 247.4 GB source, 229× larger than
the synthetic fixture the cache was tuned on:

| | |
|---|---|
| Source / dest | 247,439,298,560 → 2,155,876,352 B (**99.1%** reclaimed) |
| Live nodes | 6,596,937 |
| Peak RSS | **404 MB** |
| Wall clock | 15m54s (copy 15m24s, verify 30s) |
| CPU | 35% — I/O bound, 56.8M filesystem reads |
| Digest | matched the applied block's `stateRoot` exactly |
| Source | byte-identical afterwards |

Peak RSS came in ~25% above the extrapolation from the 1 GB fixture (~300–325 MB
predicted). The important property held: it tracks the **live tree**, not the
source file. Under redb's default 1 GiB-per-handle cache this run would have
consumed tens of GB.

Any future change that adds a handle, or that stops pinning the cache, silently
reintroduces the dependence on file size. Treat the pinning as part of the
contract, not an implementation detail.

- I/O: one random read per live node from source, one insert per live node
  into dest, then a second full pass for verification.
- Runtime scales with the live node count, not the bloat. A heavily bloated
  database compacts in roughly the same time as a clean one of the same
  height.

### Progress reporting

`progress` is `Option<&dyn Fn(CompactionProgress)>`. Because it is `Fn` and not
`FnMut`, an accumulating callback needs interior mutability (`RefCell` or an
atomic); a plain printer needs nothing. This is adequate for the CLI and not
worth a signature change.

`progress` covers the **copy pass only**. Verification is a second pass of
comparable length during which the callback is silent, so the implementation
additionally emits an `info!` line every 1M nodes verified. An operator
watching a long run should expect callback output to stop and log output to
continue. Adding a phase discriminator to `CompactionProgress` would close the
gap if callback coverage of both passes is ever wanted.

### Traversal safety

The explicit stack is capped at 255 levels, matching the bound implied by the
single height byte in an AVL digest. A corrupt source cannot drive the
traversal into an unbounded loop or exhaust the stack.

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

### Open-time cost (v0.5.0+)

Every redb `WriteTransaction` opened by `RedbAVLStorage` MUST call
`set_quick_repair(true)` before committing. Without quick-repair,
`Database::open` after `kill -9` does a full file scan to
reconstruct the allocator state — measured ~1m57s on the laptop's
full-mainnet state.redb. With quick-repair, recovery is "almost
instant" per redb's docs.

This applies to every write path: `update`, `flush`, genesis
bootstrap, version pruning, and any metadata writes. Pruning a
single call site (e.g. only the bulk `update` path) leaves the
database non-quick-repair-ready after any other commit, defeating
the purpose.

Trade-off: per-commit cost is slightly higher (allocator state
serialized into every commit, 2-phase commit). For state.redb's
write profile (one update per block at-tip, larger updates during
sync) the overhead is negligible against the recovery-time win.

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

### ⚠ An in-place resize moves only 90% of the budget (found 2026-08-12)

`Builder::set_cache_size(n)` does **not** set one cache. It splits the budget
(`patches/redb/src/db.rs:1177`):

```rust
self.read_cache_size_bytes  = bytes / 10 * 9;   // 90%
self.write_cache_size_bytes = bytes / 10;       // 10%
```

The in-place path (`resize_cache` → `Database::set_read_cache_limit`) reaches
**only the read half**. `max_write_buffer_bytes` is fixed at `open()` and there
is no setter — redb carries a `TODO: allow dynamic expansion of the read/write
cache` immediately above `set_cache_size`.

Consequences, both real:

- After an at-tip resize, the process still holds a write buffer sized from the
  **original** cold-sync `cache_mb`, not from `synced_cache_mb`. An operator
  setting `synced_cache_mb = 128` gets roughly 115 MB of read cache plus 10% of
  whatever the cold-sync total was — not 128 MB.
- `stateCacheBytes` on `/debug/memory` is `read_cache_bytes +
  write_buffer_bytes`, so after a resize it reports a figure that the current
  limit does not bound. Read it as "occupancy", never as "within the configured
  ceiling".

A full drop-and-reopen (guarantee 1 above) *does* move both halves, because the
new `open()` re-runs `set_cache_size`. Only the in-place path is partial.

Not fixed here: bounding the write half at runtime needs a redb patch, which is
out of scope for the cache-budget work. Documented so the budget arithmetic and
the endpoint are both read correctly.

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
- Every node reachable from `top_node_hash` is present in the `nodes` table.
  A missing reachable node is corruption.
- The converse does NOT hold: `nodes` may contain entries unreachable from any
  root. This is expected, not a bug — see [Offline Compaction](#offline-compaction)
  for the mechanisms that produce them. Unreachable rows waste space; they never
  affect correctness, because lookups and traversals only ever follow labels
  from a root. Nothing in this crate may assume `nodes` contains only live
  entries — in particular, row count is not a proxy for tree size.
  - Crash during `update()` with `keep_versions = 0` could theoretically leave
    new nodes written but metadata not yet updated. redb's ACID prevents this.
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
7. **Compaction:** build a tree, churn it so unreachable rows accumulate
   (repeated overwrite of the same keys), record the digest, `compact_to()` a
   fresh path. Assert: digest unchanged, `block_height` carried over, dest row
   count equals the reachable node count, source unmodified, dest smaller.
   Also assert the failure path — point the root at a label absent from
   `nodes` and verify `compact_to` returns `Err` and removes `dest`.
