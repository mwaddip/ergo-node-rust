# Modifier Store Contract

## Component: `store/` (enr-store)

Persistent storage for block-related modifiers. A dumb persistence layer — receives
pre-validated, pre-serialized bytes from components above and writes them to disk.
Does not parse, validate, or interpret modifier content.

Primary consumers: validation pipeline (writes headers after validation), sync state
machine (reads on startup to rebuild chain state), future block validation (reads
block sections).

## Design Principles

- **Bytes in, bytes out.** The store does not know what a Header or Transaction looks like.
  It stores `(type_id, modifier_id, height, data)` tuples.
- **Backend-agnostic interface.** The public trait is implemented by redb today. A different
  backend (RocksDB, sled, etc.) is a new impl of the same trait — no adapters, no shims.
- **Crash-safe.** Every write is durable on return. A `kill -9` at any point leaves the
  store in a consistent state. redb's ACID transactions provide this.
- **Height is caller-provided.** The store maintains a height index but never derives
  height from the data. The caller knows the domain semantics.
- **Fork-aware headers.** Headers (`type_id=101`) have a dedicated multi-table layout
  so multiple headers per height, cumulative scores, and best-chain switching can all
  be represented without fighting the flat `(type_id, height) -> id` index used by
  non-header modifiers.

## Modifier Types

| Type ID | Name | Introduced |
|---------|------|-----------|
| 101 | Header | Phase 1 |
| 102 | BlockTransactions | Phase 4 |
| 104 | ADProofs | Phase 4 |
| 108 | Extension | Phase 4 |

Headers are a special case — see "Header writes" below. All other modifier types
share the generic flat layout.

## Trait: `ModifierStore`

```rust
pub trait ModifierStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    // --- Generic modifier storage ---

    /// Store a single modifier.
    fn put(
        &self,
        type_id: u8,
        id: &[u8; 32],
        height: u32,
        data: &[u8],
    ) -> Result<(), Self::Error>;

    /// Store a batch of modifiers atomically. All entries written in a
    /// single transaction — all succeed or none do.
    ///
    /// Entry tuple: `(type_id, id, height, data, score)`.
    /// `score` is `Some(big_endian_bigint_bytes)` required when
    /// `type_id == 101`, `None` for all other modifier types. A
    /// `type_id == 101` entry with `score == None` is rejected.
    fn put_batch(
        &self,
        entries: &[(u8, [u8; 32], u32, Vec<u8>, Option<Vec<u8>>)],
    ) -> Result<(), Self::Error>;

    /// Retrieve a modifier by type and ID.
    fn get(
        &self,
        type_id: u8,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Retrieve the modifier ID at a given height for a type.
    /// For type_id=101 this routes to best_header_at.
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
    /// For type_id=101 this routes to best_header_tip.
    fn tip(
        &self,
        type_id: u8,
    ) -> Result<Option<(u32, [u8; 32])>, Self::Error>;

    // --- Fork-aware header storage ---

    /// Store a fork header with its fork number and cumulative score.
    /// Writes PRIMARY (101,id), HEADER_FORKS (height, fork), HEADER_SCORES.
    /// Writes BEST_CHAIN only if no entry exists at this height yet
    /// (first-arrival wins until a main-chain put_batch overwrite).
    ///
    /// Not used for main-chain headers — those go through put_batch,
    /// which routes type_id=101 through the fork-aware tables with
    /// an unconditional BEST_CHAIN insert.
    fn put_header(
        &self,
        id: &[u8; 32],
        height: u32,
        fork: u32,
        score: &[u8],
        data: &[u8],
    ) -> Result<(), Self::Error>;

    /// Get all header IDs at a given height across all known forks.
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
    /// caller bug — migration walks BEST_CHAIN which is consistent
    /// with PRIMARY).
    fn put_header_score(
        &self,
        id: &[u8; 32],
        score: &[u8],
    ) -> Result<(), Self::Error>;

    /// Batch variant of `put_header_score` — writes many `(id, score)`
    /// pairs in a single redb write transaction.
    ///
    /// Order is preserved but irrelevant; HEADER_SCORES is keyed by
    /// header id, not height. Each entry must satisfy the same
    /// preconditions as `put_header_score` (id present in PRIMARY,
    /// score is non-empty big-endian BigUint bytes).
    ///
    /// Atomic: on Ok all entries are committed; on Err none are.
    ///
    /// Used by the scores backfill migration to cut per-transaction
    /// overhead by ~100×. With 1.78M individual writes the migration
    /// also leaves substantial redb recovery work for the next
    /// unclean restart (~6 min open time observed) — chunking into
    /// ~50_000-entry batches collapses that.
    fn put_header_score_batch(
        &self,
        entries: &[([u8; 32], Vec<u8>)],
    ) -> Result<(), Self::Error>;

    // --- Chain metadata ---

    /// Read a value from the chain_meta table.
    ///
    /// Tiny key-value store inside the modifier store, used for
    /// migration sentinels and per-chain-state flags. Keys are
    /// stable byte strings (see "Chain metadata table" below).
    fn chain_meta_get(
        &self,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Write a value to the chain_meta table.
    fn chain_meta_put(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Self::Error>;

    /// Remove a value from the chain_meta table.
    ///
    /// Idempotent: removing a key that does not exist is `Ok(())`.
    /// Primary use case is operator-driven re-runs of one-shot
    /// migrations (delete the migration's sentinel and restart).
    fn chain_meta_delete(
        &self,
        key: &[u8],
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
    /// substantially faster than 1.76M point lookups when restoring
    /// chain state at startup. The returned Vec is `(height,
    /// header_id)` pairs starting at the lowest height in BEST_CHAIN
    /// and ending at `best_header_tip().height`. Empty Vec on an
    /// empty store.
    fn best_chain_entries(&self) -> Result<Vec<(u32, [u8; 32])>, Self::Error>;

    /// Read the stored header bytes at a best-chain height.
    /// Single read transaction combining best_header_at + get(101, id).
    /// Returns Ok(None) if no best-chain entry at that height.
    fn read_header_at(
        &self,
        height: u32,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    // --- Peer database ---

    /// Write or overwrite a peer record.
    ///
    /// Key is the encoded `SocketAddr` (see "Tables" / `peer_db`). Value
    /// is the serialized record body: `(last_seen_ms u64 LE,
    /// agent_name shortString, node_name shortString, version 3B,
    /// features_blob)`. Overwrites any prior value at the same address.
    ///
    /// The store treats record bytes as opaque — encoding is owned by
    /// the p2p crate. Only the key encoding (`SocketAddr → bytes`) is
    /// the store's concern, because it determines lookup and iteration
    /// behaviour.
    fn put_peer(
        &self,
        addr: SocketAddr,
        record: &[u8],
    ) -> Result<(), Self::Error>;

    /// Remove a peer record. Idempotent: removing an absent address
    /// is `Ok(())`.
    fn delete_peer(
        &self,
        addr: SocketAddr,
    ) -> Result<(), Self::Error>;

    /// Read every peer record. Single read transaction. Returns a
    /// `Vec<(addr, record_bytes)>` with no ordering guarantee.
    ///
    /// Called once at p2p startup to repopulate the in-memory PeerDb
    /// via `PeerStorage::load_all`. At <= ~1000 entries (the PeerDb
    /// soft cap) the linear scan is trivial; no incremental
    /// iteration is needed.
    fn list_peers(
        &self,
    ) -> Result<Vec<(SocketAddr, Vec<u8>)>, Self::Error>;
}
```

## Header writes

Headers (`type_id == 101`) route through the fork-aware tables regardless of
which write method the caller uses:

| Caller intent | Methods | Tables written |
|---|---|---|
| Main-chain header (certified best by caller) | `put_batch` with `type_id=101` and `score=Some(...)` | PRIMARY + HEADER_FORKS (h, 0) + HEADER_SCORES (real cumulative score) + BEST_CHAIN (unconditional) |
| Fork header at an already-occupied height | `put_header` with `fork>0` | PRIMARY + HEADER_FORKS (h, fork) + HEADER_SCORES (real cumulative score) + BEST_CHAIN (only if absent) |

`put` with `type_id=101` is rejected — main-chain headers must always
go through `put_batch` so the score is carried alongside the data in
a single atomic write. Single-header writes from the validation
pipeline use `put_batch` with a one-element slice.

For headers, `put` / `put_batch` **never write to `HEIGHT_INDEX`**. The
`(101, h)` slot in `HEIGHT_INDEX` is the legacy pre-deep-reorg schema and
is cleaned up by a one-shot migration on first open. Consequently:

- `get_id_at(101, h)` is routed to `best_header_at(h)`.
- `tip(101)` is routed to `best_header_tip()`.

so the generic lookup API still returns the best-chain header at a height.

**BEST_CHAIN insert for `put` / `put_batch` is unconditional.** Main-chain
headers are authoritative for their height slot and will overwrite any
stale fork-first-arrival entry left by an earlier `put_header`, or the
previous main-chain entry on a same-height reorg replacement. This is
the only way to keep BEST_CHAIN dense against the pipeline's real write
ordering.

For non-header modifiers (block sections, extensions, etc.), `put` /
`put_batch` continue to write PRIMARY + HEIGHT_INDEX as before. `tip(t)`
for non-header types is served from an in-memory cache populated from
HEIGHT_INDEX.

## Tables (redb backend)

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `primary` | `(type_id, id)` | `data` | Lookup by modifier ID |
| `height_index` | `(type_id, height)` | `id` | Lookup by height, non-header types only |
| `header_forks` | `(height, fork)` | `header_id` | All known headers per height; `fork=0` reserved for best chain |
| `header_scores` | `header_id` | BigUint bytes | Cumulative difficulty per header. Real values for all headers post-scores-migration. |
| `best_chain` | `height` | `header_id` | Current best chain, one entry per height |
| `chain_meta` | `key (bytes)` | `value (bytes)` | Tiny KV store for migration sentinels and per-chain-state flags |
| `peer_db` | `SocketAddr bytes` | `record bytes` | Persistent peer registry. Key is the encoded socket addr; value is the p2p crate's opaque record. |

On startup, the `best_header_tip` cache is seeded by scanning `best_chain`
for the highest key. Two one-shot migrations run before the store is
returned from `open`:

1. **`height_index` → `header_forks`** (pre-existing). Moves
   pre-deep-reorg `(101, h)` entries from `height_index` into
   `best_chain` + `header_forks` + `header_scores`, then removes them
   from `height_index`. Guarded by `header_forks.len() > 0`; runs
   until the guard trips.
2. **Empty-placeholder scores → real cumulative scores** (new in
   v0.5.0). Not run by the store crate (would require depending on
   `ergo-chain-types` for `decode_compact_bits` and on VLQ header
   parsing — violates the store's domain isolation). The main crate
   orchestrates: it iterates `best_chain` in height order, reads each
   header via `get(101, &id)`, decodes `n_bits` via chain, computes
   `parent_score + decode_compact_bits(header.n_bits)`, and writes
   the result via `put_header_score(id, score)`. Fork headers
   (`fork>0`) already carry real scores and are skipped — they keep
   whatever was written by `put_header`.

   The store crate provides the building blocks:
   - `chain_meta_get(b"scores_migrated_v1")` — sentinel check
   - `chain_meta_put(b"scores_migrated_v1", &[1u8])` — sentinel set on completion
   - `put_header_score(id, score)` — narrow write that touches only
     `header_scores`, leaving PRIMARY / HEADER_FORKS / BEST_CHAIN
     untouched. Required because the migration only needs to update
     scores; rewriting the full header record would be slow and
     pointless.

   The migration runs **synchronously** during startup, before any
   header reads that depend on scores. Progress is logged at every
   N=10_000 headers. For a 1.76M-header mainnet store the migration
   is roughly 1.76M `get(101,id)` calls + 1.76M `put_header_score`
   writes, which the main crate batches into transactions of ~50_000
   headers each via repeated `put_header_score` calls inside an
   in-progress write transaction (or repeated `put_header_score`
   calls if the store does its own auto-batching). Expected wall
   time on commodity SSD: ~5-15 minutes one-time on first v0.5.0
   start.

   The migration is **resumable**. If the process is killed mid-walk,
   the sentinel is not yet written and the next start re-runs from
   height 1. Headers already updated to real scores are overwritten
   with the same value (idempotent).

## Chain metadata table

Stable keys, all little-known to the store but consumed by the chain
crate via `chain_meta_get`:

| Key | Value semantics | Set by |
|---|---|---|
| `b"scores_migrated_v1"` | `[1u8]` once migration completes; absent before | Empty-placeholder → real scores migration |

## Peer DB key encoding

A `SocketAddr` is encoded as:

| Field | Bytes | Notes |
|---|---|---|
| family | 1 | `0x04` for IPv4, `0x06` for IPv6. |
| ip | 4 or 16 | Octets in network order. |
| port | 2 | Big-endian. |

Total: 7 bytes (IPv4) or 19 bytes (IPv6). The encoding is internal
to the store — callers pass `SocketAddr` and never see the bytes.

Future keys are added at the discretion of the integrator. The store
crate treats values as opaque byte strings.

## Preconditions

- **`put`**: `data` is non-empty. `id` is the canonical modifier ID
  (Blake2b256 of the serialized bytes for headers). `height` is the block height
  this modifier belongs to, or `0` for "height unknown" (skips height indexing).
  The caller has already validated the modifier. `type_id != 101` —
  main-chain headers must use `put_batch`.
- **`put_batch`**: Each entry satisfies the per-entry `put` preconditions.
  Additionally, for entries with `type_id == 101`: `score` is `Some(bytes)`
  where `bytes` is the cumulative difficulty as big-endian BigUint bytes,
  and the caller asserts this header is on the best chain. For
  `type_id != 101`: `score` is `None`.
- **`put_header`**: `fork` is either 0 (best chain) or a positive fork number,
  and `score` is the cumulative difficulty encoded as big-endian BigUint bytes
  (real value, never empty placeholder).
- **`put_header_score`**: `id` corresponds to a header previously
  written via `put_batch` (type=101) or `put_header`. `score` is
  non-empty big-endian BigUint bytes.
- **`put_header_score_batch`**: every entry's id corresponds to a
  header in PRIMARY; every entry's score is non-empty.
- **`best_chain_entries`**: no preconditions.
- **`chain_meta_put`** / **`chain_meta_get`**: `key` is non-empty.
- **`put_peer`**: `record` is non-empty. The caller (p2p crate) owns
  the record encoding; the store does not parse it.
- **`delete_peer`** / **`list_peers`**: no preconditions.
- **`get` / `get_id_at` / `contains` / `tip` / `header_score`**: No
  preconditions beyond valid `type_id` (or `id` for `header_score`).

## Postconditions

- **`put`**: On Ok, the modifier is durable on disk. A subsequent `get` with the
  same `(type_id, id)` returns `Some(data)`. `get_id_at(type_id, height)`
  returns `Some(id)` and `tip(type_id).height` is at least `height`.
- **`put_batch`**: Each entry's `put`-like postconditions hold,
  atomically. For `type_id == 101` entries: `best_header_at(height)`
  returns `Some(id)` (unconditional overwrite), `best_header_tip` is
  updated if `height` exceeds the previous tip, and
  `header_score(id)` returns `Some(score_bytes)` exactly as provided
  in the entry. On Err, no entries are written.
- **`put_header` (fork=0)**: Inserts into BEST_CHAIN only if the height slot is
  empty. Always writes PRIMARY, HEADER_FORKS, HEADER_SCORES with the
  provided real score.
- **`put_header` (fork>0)**: Writes PRIMARY, HEADER_FORKS, HEADER_SCORES with
  the provided real score. Leaves BEST_CHAIN untouched unless the slot
  was empty.
- **`header_score`**: Returns `Some(bytes)` for any header previously written
  via `put_batch` (type=101) or `put_header`, where `bytes` is the
  big-endian BigUint cumulative score. Returns `None` only if the
  header_id was never stored.
- **`put_header_score`**: On Ok, the header_scores entry for `id` is
  updated to `score`. Subsequent `header_score(id)` returns
  `Some(score)`. Touches no other table. Returns Err if `id` is not
  in PRIMARY.
- **`put_header_score_batch`**: On Ok, every entry's HEADER_SCORES
  row is updated to its score, atomically (single redb write tx).
  On Err, no entries are written. Touches no other table.
- **`best_chain_entries`**: Returns a Vec of `(height, header_id)`
  pairs covering every entry in BEST_CHAIN, in ascending height
  order. Empty when the chain is empty.
- **`chain_meta_put`**: On Ok, the value is durable. A subsequent
  `chain_meta_get(key)` returns `Some(value)`. Overwrites any previous value
  at the same key.
- **`chain_meta_get`**: Returns the stored bytes or `None` if the key
  was never written.
- **`put_peer`**: On Ok, the record is durable. A subsequent
  `list_peers` includes `(addr, record)`. Overwrites any previous
  record at the same address.
- **`delete_peer`**: On Ok, no entry for `addr` exists. Idempotent.
- **`list_peers`**: Returns every entry in the `peer_db` table.
  Ordering is not guaranteed.
- **`get`**: Returns the stored bytes or None. Never errors on missing data.
- **`get_id_at`**: Returns the modifier ID at that height or None. For type 101
  this is the best-chain header at that height.
- **`contains`**: Equivalent to `get(..).map(|o| o.is_some())` but avoids reading data.
- **`tip`**: Returns the highest `(height, id)` pair. For type 101 this is the
  best-chain tip.
- **`read_header_at`**: Returns the raw header bytes stored for the best-chain
  header at `height`, or `None` if no such entry exists. Consistent with
  `best_header_at` — if `best_header_at(h) == Some(id)` then
  `read_header_at(h) == get(101, &id)`.

## Invariants

- No method panics. All failures are returned as errors.
- A modifier written by `put` is immediately visible to `get`, `contains`, and `tip`.
- `put_batch` is atomic — partial writes never occur.
- For non-headers, the height index is consistent with the primary index: if
  `get_id_at(t, h)` returns `Some(id)` then `get(t, &id)` returns `Some(data)`.
- For headers, BEST_CHAIN is consistent with PRIMARY: if `best_header_at(h)`
  returns `Some(id)` then `get(101, &id)` returns `Some(data)`.
- HEADER_SCORES is consistent with BEST_CHAIN after migration: for every
  height `h` in `[1, best_header_tip().height]`,
  `header_score(best_header_at(h))` returns `Some(score_bytes)` with a
  non-empty real value. The score at height `h` equals the score at
  `h-1` plus `decode_compact_bits(header_at_h.n_bits)`; the score at
  height 1 equals `decode_compact_bits(genesis_header.n_bits)`.
- BEST_CHAIN is dense from 1 to `best_header_tip().height` after any sequence
  of `put_batch` / `put_header` calls for contiguous main-chain headers. A
  single-height hole inside this range is a store bug.
- The store survives `kill -9` at any point. On restart, it contains exactly the
  modifiers from completed transactions.
- Duplicate `put` with the same `(type_id, id)` is idempotent — overwrites with
  same data, no error.

## Does NOT own

- Modifier parsing or validation — that's the pipeline and chain crates.
- Deciding what to store or when — that's the pipeline.
- Chain state reconstruction — that's the chain crate, using data read from the store.
- Network I/O — that's P2P.
- The scores backfill migration — the main crate orchestrates (it
  has the chain dependency for `decode_compact_bits` and header
  parsing); the store crate just exposes the `best_chain_entries`,
  `header_score`, `put_header_score`, and `chain_meta_*` primitives
  the migration needs.
- Peer record format. `put_peer` / `list_peers` carry opaque bytes.
  The p2p crate owns the record schema and its evolution; the store
  is responsible only for `SocketAddr` key encoding and durable
  storage of the value bytes.

## Dependencies

- `redb` — storage backend (initial implementation)
- No dependency on `ergo-chain-types`, `ergo-lib`, or any Ergo domain crates.

## Reorg handling

There is no dedicated "switch best chain" API. Deep reorgs are
handled entirely through `put_batch`: after the caller has updated
the in-memory chain to the new branch, it re-emits every new-branch
header via `put_batch` with `type_id=101`, which unconditionally
overwrites BEST_CHAIN at each height. This is the single write path
for main-chain headers and keeps the invariant "main-chain is
authoritative for its height slot" without a second atomic-swap API.

## Open-time cost

`RedbModifierStore::new` must run in single-digit seconds even on a
multi-GB store after unclean shutdown. Two contributors dominate
open time and are both controlled here:

### load_tips

The `load_tips` helper that populates the per-type tip cache MUST
NOT iterate the entire `HEIGHT_INDEX` table — at a full mainnet
store (~5.3M entries across section types 102/104/108) that
iteration dominates open time.

Acceptable implementations:
- Per-type backward range scan: for each known modifier type id,
  `table.range((type, 0)..(type+1, 0)).rev().next()` reads at most
  one entry per type. O(K log N) total reads where K = number of
  modifier types and N = total entries.
- Separate `type_tips` table maintained write-through on every `put`
  for non-header types. `load_tips` then becomes a single small
  table scan (one row per type).

The implementation is not visible in the public API; either approach
satisfies the contract.

### redb quick-repair

Every redb `WriteTransaction` opened by the store MUST call
`set_quick_repair(true)` before committing. From the redb
documentation:

> By default, when reopening the database after a crash, redb
> needs to do a full repair. This involves walking the entire
> database to verify the checksums and reconstruct the allocator
> state, so it can be very slow if the database is large.
>
> Alternatively, you can enable quick-repair. In this mode, redb
> saves the allocator state as part of each commit (so it doesn't
> need to be reconstructed), and enables 2-phase commit (which
> guarantees that the primary commit slot is valid without needing
> to look at the checksums). This means commits are slower, but
> recovery after a crash is almost instant.

Measured on the laptop's full-mainnet modifier store: without
quick-repair, `Database::create` after `kill -9` takes ~5m43s. With
quick-repair enabled going forward, that drops to seconds.

Trade-off: per-commit cost is slightly higher (allocator state is
serialized into every commit). For our write profile — a handful of
small commits per block at-tip, occasional larger commits during
sync — the overhead is negligible. For the scores backfill
migration the per-commit cost is amortized across 50_000 score
writes per chunk.

quick-repair becomes effective from the first commit that sets it;
unrelated `Database::create` calls between two non-quick-repair
commits still need full repair.

## Known follow-ups

- The in-memory tip caches (`tips` and `best_header_tip`) are
  single-writer-safe. Concurrent writers would need them moved into
  the database or coordinated externally.

## SPECIAL Profile

```
S7  P5  E9  C6  I7  A8  L7
```

Internal component — no untrusted input (callers validate first). Durability and
performance are the primary concerns. See `facts/SPECIAL.md`.
