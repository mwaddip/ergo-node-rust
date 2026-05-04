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
    fn put_batch(
        &self,
        entries: &[(u8, [u8; 32], u32, Vec<u8>)],
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
    /// Main-chain headers written via put/put_batch have an empty
    /// placeholder score; real scores are only populated for fork
    /// headers by put_header.
    fn header_score(
        &self,
        id: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Get the best chain header ID at a height.
    fn best_header_at(
        &self,
        height: u32,
    ) -> Result<Option<[u8; 32]>, Self::Error>;

    /// Get the best chain tip (highest height and header ID).
    fn best_header_tip(&self) -> Result<Option<(u32, [u8; 32])>, Self::Error>;

    /// Read the stored header bytes at a best-chain height.
    /// Single read transaction combining best_header_at + get(101, id).
    /// Returns Ok(None) if no best-chain entry at that height.
    fn read_header_at(
        &self,
        height: u32,
    ) -> Result<Option<Vec<u8>>, Self::Error>;
}
```

## Header writes

Headers (`type_id == 101`) route through the fork-aware tables regardless of
which write method the caller uses:

| Caller intent | Methods | Tables written |
|---|---|---|
| Main-chain header (certified best by caller) | `put`, `put_batch` with `type_id=101` | PRIMARY + HEADER_FORKS (h, 0) + HEADER_SCORES (empty) + BEST_CHAIN (unconditional) |
| Fork header at an already-occupied height | `put_header` with `fork>0` | PRIMARY + HEADER_FORKS (h, fork) + HEADER_SCORES + BEST_CHAIN (only if absent) |

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
| `header_scores` | `header_id` | BigUint bytes | Cumulative difficulty per header (empty placeholder for main-chain) |
| `best_chain` | `height` | `header_id` | Current best chain, one entry per height |

On startup, the `best_header_tip` cache is seeded by scanning `best_chain`
for the highest key. A one-shot migration moves pre-deep-reorg `(101, h)`
entries from `height_index` into `best_chain` + `header_forks` +
`header_scores`, then removes them from `height_index`. The migration is
guarded by `header_forks.len() > 0` and runs until the guard trips.

## Preconditions

- **`put` / `put_batch`**: `data` is non-empty. `id` is the canonical modifier ID
  (Blake2b256 of the serialized bytes for headers). `height` is the block height
  this modifier belongs to, or `0` for "height unknown" (skips height indexing).
  The caller has already validated the modifier. For `type_id=101`, the caller
  asserts this header is on the best chain.
- **`put_header`**: Same as `put` plus `fork` is either 0 (best chain) or a
  positive fork number, and `score` is the cumulative difficulty encoded as
  big-endian BigUint bytes.
- **`get` / `get_id_at` / `contains` / `tip`**: No preconditions beyond valid type_id.

## Postconditions

- **`put`**: On Ok, the modifier is durable on disk. A subsequent `get` with the
  same `(type_id, id)` returns `Some(data)`. For non-headers,
  `get_id_at(type_id, height)` returns `Some(id)` and `tip(type_id).height` is
  at least `height`. For headers, `best_header_at(height)` returns `Some(id)`
  (unconditional overwrite) and `best_header_tip` is updated if `height` exceeds
  the previous tip.
- **`put_batch`**: Same as `put` for each entry, atomically. On Err, no entries
  are written.
- **`put_header` (fork=0)**: Inserts into BEST_CHAIN only if the height slot is
  empty. Always writes PRIMARY, HEADER_FORKS, HEADER_SCORES.
- **`put_header` (fork>0)**: Writes PRIMARY, HEADER_FORKS, HEADER_SCORES. Leaves
  BEST_CHAIN untouched unless the slot was empty.
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
- BEST_CHAIN is dense from 1 to `best_header_tip().height` after any sequence
  of `put` / `put_batch` calls for contiguous main-chain headers. A single-height
  hole inside this range is a store bug.
- The store survives `kill -9` at any point. On restart, it contains exactly the
  modifiers from completed transactions.
- Duplicate `put` with the same `(type_id, id)` is idempotent — overwrites with
  same data, no error.

## Does NOT own

- Modifier parsing or validation — that's the pipeline and chain crates.
- Deciding what to store or when — that's the pipeline.
- Chain state reconstruction — that's the chain crate, using data read from the store.
- Network I/O — that's P2P.

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

## Known follow-ups

- Non-header modifier `tip` uses an in-memory `HashMap<type_id, (height, id)>`
  cache separate from BEST_CHAIN. That cache is only updated by writes in
  the current process — a fresh open loads it from `HEIGHT_INDEX` via
  `load_tips`. This is fine for single-writer use but would need attention
  if the store ever has concurrent writers.

## SPECIAL Profile

```
S7  P5  E9  C6  I7  A8  L7
```

Internal component — no untrusted input (callers validate first). Durability and
performance are the primary concerns. See `facts/SPECIAL.md`.
