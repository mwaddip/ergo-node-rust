# Deep Chain Reorganization Contract

## Problem

The node is stuck at height 264,601 because incoming headers belong to a fork
whose common ancestor is more than 1 block back. The current 1-deep reorg can
only swap the tip — it cannot unwind multiple blocks. This happens regularly
on testnet and will happen on mainnet.

A destructive reorg (throw away old headers, re-download) is a DoS vector:
one forged header could force the node to re-sync from genesis.

## Design Decision

**All validated headers are kept permanently.** Reorg is a local operation over
stored data, never a network round-trip. The "best chain" is a view, not a
structural property.

## Changes by Component

---

### 1. Store (`store/`)

#### New table: `HEADER_FORKS`

```
Key:   (height: u32, fork: u32)
Value: header_id: [u8; 32]
```

Replaces `HEIGHT_INDEX` for type_id 101 (headers). Non-header modifiers keep
using `HEIGHT_INDEX` unchanged.

- `fork = 0` is the first header seen at that height (genesis chain during initial sync).
- When a competing header arrives at an already-occupied height, it gets the next
  fork number for that height.
- Fork numbers are per-height, not global. Height 100 might have forks 0 and 1,
  height 101 might have forks 0, 1, and 2.

#### New table: `HEADER_SCORES`

```
Key:   header_id: [u8; 32]
Value: cumulative_score: [u8; N]  (BigUint big-endian bytes)
```

Cumulative difficulty score for each header:

```
score(header) = score(parent) + decode_compact_bits(header.n_bits)
score(genesis) = decode_compact_bits(genesis.n_bits)
```

Uses `decode_compact_bits` from `ergo-chain-types` — the actual difficulty value,
not the compact u32. Stored as big-endian `BigUint` bytes (variable length, typically
8-16 bytes).

#### New table: `BEST_CHAIN`

```
Key:   height: u32
Value: header_id: [u8; 32]
```

The current best chain — one header ID per height. This replaces the single
`HEIGHT_INDEX` entry for headers and makes "which header is best at height H?"
an O(1) lookup.

Updated atomically during reorg: demoted headers are removed, promoted headers
are inserted. The tip entry doubles as the `BestHeaderKey` equivalent.

#### New trait methods on `ModifierStore`

```rust
/// Store a header with its fork number and cumulative score.
fn put_header(
    &self,
    id: &[u8; 32],
    height: u32,
    fork: u32,
    score: &[u8],
    data: &[u8],
) -> Result<(), Self::Error>;

/// Batch version of put_header.
fn put_header_batch(
    &self,
    entries: &[(/* id */ [u8; 32], /* height */ u32, /* fork */ u32, /* score */ Vec<u8>, /* data */ Vec<u8>)],
) -> Result<(), Self::Error>;

/// Get all header IDs at a given height (all forks).
fn header_ids_at_height(
    &self,
    height: u32,
) -> Result<Vec<([u8; 32], u32)>, Self::Error>;
// Returns: Vec<(header_id, fork_number)>

/// Get the cumulative score for a header.
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

/// Atomically switch the best chain: remove old entries, insert new ones.
/// `demote`: heights where the current best header is being replaced.
/// `promote`: (height, header_id) pairs for the new best chain.
fn switch_best_chain(
    &self,
    demote: &[u32],
    promote: &[(u32, [u8; 32])],
) -> Result<(), Self::Error>;
```

#### Migration

Existing stores have headers in `HEIGHT_INDEX` with key `(101, height)`. On
first open after upgrade:

1. Scan `HEIGHT_INDEX` for all `(101, height)` entries.
2. For each, insert into `HEADER_FORKS` as `(height, 0)` and into `BEST_CHAIN`.
3. Compute and store cumulative scores by walking headers in height order.
4. Remove `(101, *)` entries from `HEIGHT_INDEX`.

This runs once, in the same write transaction.

---

### 2. HeaderChain (`chain/`)

The in-memory `HeaderChain` remains the **best chain view** — one header per
height, fast to query. It does NOT store fork alternatives. The store does.

#### Changes to `try_append`

Currently rejects any header whose `parent_id != tip().id`. New behavior:

- If `parent_id == tip().id`: append as before (extend best chain).
- If `parent_id` matches a header in the chain but not the tip: the header is
  on a fork. Return a new result variant indicating "valid but not on best chain."
  The caller stores it and checks if it triggers a reorg.
- If `parent_id` matches nothing in the chain: return `ParentNotFound` as before.

New return type to distinguish these cases:

```rust
pub enum AppendResult {
    /// Header extends the best chain. Chain height increased.
    Extended,
    /// Header is valid but forks from the best chain at the given height.
    /// Caller should store it and evaluate whether it triggers a reorg.
    Forked { fork_height: u32 },
}
```

`try_append` returns `Result<AppendResult, ChainError>`.

#### New method: `try_reorg_deep`

```rust
/// Attempt a deep chain reorganization.
///
/// `fork_point_height`: the height of the last common ancestor.
/// `new_branch`: headers from fork_point_height+1 to the new tip,
///               in ascending height order. All must be pre-validated
///               for PoW. Chain validation (parent linkage, timestamps,
///               difficulty) is checked here.
///
/// On success: the chain is rewound to fork_point_height and the new
/// branch is applied. Returns the list of demoted header IDs.
///
/// On failure: the chain is unchanged.
pub fn try_reorg_deep(
    &mut self,
    fork_point_height: u32,
    new_branch: Vec<Header>,
) -> Result<Vec<BlockId>, ChainError>
```

Preconditions:
- `fork_point_height < self.height()`
- `self.header_at(fork_point_height)` exists
- `new_branch[0].parent_id == self.header_at(fork_point_height).id`
- `new_branch` is non-empty and in ascending height order
- Each header in `new_branch` passes validation (timestamp, difficulty, PoW)

Algorithm:
1. Save the current chain state (headers from `fork_point_height + 1` to tip).
2. Truncate `by_height` and `by_id` to `fork_point_height`.
3. Validate and append each header in `new_branch` sequentially.
4. If any validation fails: restore saved state, return error.
5. On success: return the IDs of demoted headers.

The old `try_reorg` (1-deep) can remain as a fast path or be removed — it's a
special case of `try_reorg_deep` with `fork_point_height = tip.height - 1`.

#### New method: `cumulative_score`

```rust
/// Cumulative difficulty score at the chain tip.
pub fn cumulative_score(&self) -> BigUint
```

Computed incrementally: each `try_append` adds `decode_compact_bits(header.n_bits)`
to a running total. Stored as a field on `HeaderChain`.

```rust
/// Cumulative difficulty score at a given height.
pub fn score_at(&self, height: u32) -> Option<BigUint>
```

Needed for comparing a fork's score against the current chain at the same height
range.

#### Internal changes

- Add `cumulative_score: BigUint` field, updated on append and reorg.
- Add `scores: Vec<BigUint>` parallel to `by_height`, one per header. Enables
  score lookup at any height without recomputation.

---

### 3. ValidationPipeline (`src/pipeline.rs`)

The pipeline is where fork detection and reorg triggering happen.

#### Header processing flow (revised)

For each validated header (PoW-checked, parsed):

1. **Try append** to best chain via `chain.try_append(header)`.
   - `Extended`: chained, store with fork=0 (or current best fork), update score.
   - `Forked { fork_height }`: valid but diverges. Go to step 2.
   - `ParentNotFound`: buffer in LRU as before.

2. **Store the fork header.** Write to store with the next fork number at its height.
   Compute and store its cumulative score.

3. **Check if fork is better.** Compare the fork tip's cumulative score against
   the best chain's score at the same height (or the best chain tip's score if
   the fork is longer).

   ```
   fork_score = score(fork_tip)
   best_score = chain.cumulative_score()  // or score at fork tip height if fork is shorter
   ```

   If `fork_score > best_score`: trigger reorg (step 4).
   If not: done. The fork header is stored for future reference.

4. **Trigger reorg.**
   - Read the fork branch from the store: walk parent_id from the fork tip back
     to the fork point (which is on our best chain).
   - Call `chain.try_reorg_deep(fork_point_height, new_branch)`.
   - On success: update store's `BEST_CHAIN` via `switch_best_chain`.
   - Notify sync machine of the reorg (heights affected).

#### Cumulative score computation

For a fork header whose parent is on the best chain:
```
score = chain.score_at(parent_height) + decode_compact_bits(header.n_bits)
```

For a fork header whose parent is also a fork header (deeper fork):
```
score = store.header_score(parent_id) + decode_compact_bits(header.n_bits)
```

The pipeline must handle both cases.

#### Fork chain assembly

When a fork header triggers a reorg, the pipeline needs the full fork branch
(fork_point+1 to fork_tip) as parsed `Header` objects. These are read from the
store by walking parent_id links backward from the fork tip to the fork point.

This requires the store to support reading header bytes by ID (already supported
via `get(101, id)`), and the pipeline to parse them.

#### Buffer interaction

The LRU buffer continues to handle out-of-order delivery within the current sync
session. Fork headers that can be chained (parent exists somewhere) bypass the
buffer and go directly to the store with their fork tag.

Headers in the buffer whose parent is a fork header (not on best chain) should
also be chained to the fork — they extend the alternative branch and might
push its score above the best chain.

---

### 4. Sync Machine (`sync/src/state.rs`)

#### Block section download strategy

The JVM downloads fork block sections only near the chain tip, not during
initial sync. This is the right trade-off: during catch-up, fork sections are
noise — you're thousands of blocks behind and the fork is almost certainly
resolved by the time you get there. Near the tip, fork sections are essential
— without them, a reorg requires a network round trip to fetch the competing
chain's blocks.

**Two modes:**

1. **Catching up** (`downloaded_height` far from `chain_height`): request
   sections for best-chain headers only. "Far" = more than 128 blocks behind
   the chain tip. This matches the JVM's `farAwayFromBeingSynced` threshold.

2. **Near tip** (`downloaded_height` within 128 of `chain_height`): request
   sections for ALL headers at each height — best chain and forks. Scan
   backward from the best full block by up to 100 heights, requesting
   sections for every header ID at each height (via `store.header_ids_at_height`).

   The store already handles this — block sections are keyed by
   `(type_id, modifier_id)`, not by height or fork number. Storing sections
   for fork headers requires no schema change.

**Why this matters for reorg:** `processBetterChain` in the JVM requires ALL
full blocks in the competing chain to be available before switching (the
`.ensuring` assertion). If any are missing, the reorg can't fire. Pre-fetching
fork sections near the tip ensures the competing chain is fully assembled
locally when the score comparison triggers a switch. Zero network traffic on
reorg — same property as headers.

#### Section queue on reorg

When a reorg demotes heights H₁..Hₙ:

1. **Remove** section queue entries for demoted heights. These block sections
   are for the wrong branch — requesting them is wasteful (the peer might not
   even have them for the demoted branch).

2. **Re-queue** sections for promoted heights. The new best chain at H₁..Hₘ
   needs its block sections downloaded — unless they were already pre-fetched
   (check the store first).

3. **Adjust `sections_queued_to`** if it was within the reorged range.

#### Watermarks on reorg

If `downloaded_height >= fork_point_height`:

1. **Reset** `downloaded_height` to `fork_point_height`. Block sections for the
   old branch above the fork point are invalid for the new branch — they have
   different modifier IDs (derived from different header IDs).

2. **Re-scan** from `fork_point_height + 1` forward. Some sections might already
   be in the store if fork sections were pre-fetched.

If `downloaded_height < fork_point_height`: no change needed.

If `validated_height >= fork_point_height`:

1. **Get** the header at the fork point from the chain.
2. **Call** `validator.reset_to(fork_point, header.state_root)`.
3. **Set** `validated_height` to `fork_point_height`.

The validator will re-validate from `fork_point + 1` as sections become available.

#### Reorg notification

The pipeline notifies the sync machine of a reorg via the control channel:

```rust
DeliveryControl::Reorg {
    fork_point: u32,
    old_tip: u32,
    new_tip: u32,
}
```

Sent via unbounded `mpsc::UnboundedSender` — Reorg events must never be dropped.
The sync machine checks the control channel with `biased;` priority in every
`tokio::select!` loop (both `sync_from_peer()` and `synced()`). It handles the
event by adjusting its section queue, watermarks (both `downloaded_height` and
`validated_height`), resetting the block validator, and re-downloading sections
for the new branch.

---

## Invariants

1. **Headers are never deleted from the store.** Reorg changes which headers are
   in `BEST_CHAIN`, never removes from `PRIMARY` or `HEADER_FORKS`.

2. **Cumulative score is strictly monotonic on any valid chain.** Each header adds
   a positive difficulty value. A chain with more cumulative work is always preferred.

3. **Strictly greater score wins.** Equal score = no reorg. First-seen chain wins
   on ties. Matches JVM behavior.

4. **Reorg is atomic.** Either the in-memory chain AND the store both switch, or
   neither does. If the in-memory reorg succeeds but the store update fails,
   the in-memory chain is rolled back.

5. **Fork numbers are per-height, monotonically increasing, u32.** They never
   wrap (saturating arithmetic at `u32::MAX`). A fork number is assigned once and
   never reused.

6. **The in-memory `HeaderChain` always reflects the best chain.** Fork headers
   exist only in the store. The pipeline reads them from the store when needed.

7. **Section queue and watermark are consistent with the best chain.** After a
   reorg, both are adjusted to reflect the new best chain before any new section
   requests go out.

## Does NOT cover

- Full block reorg (UTXO state rollback) — that's `ergo-validation` + `state/`.
  This contract covers header-level reorg only. Full block reorg will use the
  same `ProgressInfo` pattern as the JVM when those components exist.
- Peer penalties for invalid forks — future work.
- Pruning old fork headers — unnecessary for now. Headers are ~200 bytes each;
  even thousands of forks are negligible.
- Pruning old fork data (headers + block sections) — the JVM uses `keepVersions`
  to limit how far back non-best chain data is retained. We can add this later
  when disk usage matters. For now, keep everything.
