# Mempool Contract

## Component: `mempool/` (workspace crate)

In-memory pool of unconfirmed transactions with full JVM feature parity.
Stores validated transactions ordered by weighted fee, handles double-spend
detection via replace-by-fee, eviction, family weighting for chained
unconfirmed transactions, periodic revalidation, and fee statistics.

Not persistent — empty after restart, which is acceptable because peers
re-announce unconfirmed transactions.

Primary consumers: P2P layer (incoming txs from peers), REST API (user-submitted
txs), mining API (block template assembly). Primary dependency: validation crate
(single-tx validation) and a UTXO state reader trait (for input box resolution).

## SPECIAL Profile

```
S7  P7  E6  C6  I7  A8  L8
```

Performance matters (A8) — this is the throughput bottleneck for transaction
processing. Edge cases are the risk (L8) — conflicting transactions, chained
unconfirmed spends, reorg handling. Crash recovery is a non-concern (E6) —
empty after restart is fine. External input from peers (P7) needs validation
before acceptance.

## Design Principles

- **No persistence.** In-memory only. Crash = empty mempool.
- **Validate on entry.** The mempool validates transactions against the UTXO
  state augmented with current mempool outputs. Invalid transactions never
  enter the pool.
- **Replace-by-fee for double-spends.** When two transactions spend the same
  input, the higher weighted-fee transaction wins. Comparison is against the
  average weight of all conflicting transactions, matching JVM behavior.
- **Family weighting.** When a child transaction spends outputs of a parent
  already in the pool, the child's weight is propagated up to all ancestors.
  This ensures parents sort higher than children and are less likely to be
  evicted. Capped at 500 ancestor levels or 500ms scan time.
- **Periodic revalidation.** A cleanup cycle revalidates pool transactions
  against the current state, removing those that became invalid (e.g., inputs
  spent in a block we didn't see the tx in). Cost-bounded to avoid stalling.
- **No networking.** The mempool is a data structure with a validation layer.
  P2P propagation and REST endpoints are wired by the main crate.

## Validation Crate Refactor

The validation crate currently exposes `validate_transactions()` which validates
all transactions in a block. The mempool needs single-transaction validation.

### Extract: `validate_single_transaction()`

From `validation/src/tx_validation.rs`, extract the inner loop body of
`validate_transactions()` into a public function:

```rust
/// Validate a single transaction against provided input and data-input boxes.
///
/// Returns the validation cost on success. This is the ErgoScript evaluation
/// cost, used for fee-per-cycle ordering in the mempool.
pub fn validate_single_transaction(
    tx: &Transaction,
    input_boxes: &[ErgoBox],
    data_boxes: &[ErgoBox],
    state_context: &ErgoStateContext,
) -> Result<u32, ValidationError>;
```

The existing `validate_transactions()` becomes a thin wrapper that iterates
transactions and calls `validate_single_transaction()` for each, with
intra-block output tracking.

Also expose the state context builder for mempool use:

```rust
/// Build an ErgoStateContext whose preheader describes the NEXT block.
pub fn build_upcoming_state_context(
    last_header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> ErgoStateContext;
```

⚠ **The mempool takes the *upcoming* context, never `build_state_context()`.**
An unconfirmed transaction is a candidate for the block after the tip, and
wallets set `creationHeight` accordingly. Validating against a preheader at the
current tip rejects every well-formed transaction on the network with `Creation
height H+1 > preheader height`. Full derivation and the JVM reference are in
`facts/validation.md` § "Free Functions: state context". The mempool does not
build this itself — the main crate publishes it after each applied block.

Both functions are pure — no mutable state, no side effects.

## UTXO State Reader Trait

The mempool needs to resolve input boxes from both the confirmed UTXO set and
unconfirmed mempool outputs. Define a trait that the main crate satisfies:

```rust
/// Read-only access to the UTXO set for transaction validation.
///
/// Implemented by the main crate, combining the persistent UTXO state
/// with unconfirmed mempool outputs.
pub trait UtxoReader {
    /// Look up a box by its ID. Returns the serialized ErgoBox bytes.
    /// Checks confirmed UTXO state first, then unconfirmed mempool outputs.
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<Vec<u8>>;
}
```

The main crate's implementation composes the persistent AVL+ tree reader
with the mempool's `unconfirmed_box()` method.

## Data Structures

### `Mempool`

```rust
pub struct Mempool {
    /// Transactions ordered by fee weight (highest first).
    pool: BTreeMap<TxWeight, UnconfirmedTx>,
    /// Transaction ID → TxWeight for O(log n) lookup.
    by_id: HashMap<[u8; 32], TxWeight>,
    /// Input box ID → TxWeight for double-spend detection.
    by_input: HashMap<[u8; 32], TxWeight>,
    /// Output box ID → (TxWeight, ErgoBox) for chained tx family tracking.
    by_output: HashMap<[u8; 32], (TxWeight, ErgoBox)>,
    /// Recently invalidated tx IDs. Expiring cache: entries older than
    /// `invalidation_ttl` are removed on access. Bounded to prevent
    /// unbounded growth.
    invalidated: ExpiringCache<[u8; 32]>,
    /// Statistics over confirmed pool transactions, for the fee queries.
    stats: FeeStats,
    /// Configuration.
    config: MempoolConfig,
}
```

### `TxWeight`

```rust
/// Ordering key for mempool transactions.
/// Sorted by weight descending, then tx_id for deterministic tiebreak.
#[derive(Clone, Eq, PartialEq)]
pub struct TxWeight {
    /// Effective weight — starts as fee_per_factor, increased by family weighting.
    pub weight: u64,
    /// Base fee per factor (before family adjustments).
    pub fee_per_factor: u64,
    /// Transaction ID — tiebreaker for deterministic ordering.
    pub tx_id: [u8; 32],
    /// Insertion timestamp (for statistics and cleanup ordering).
    pub created: Instant,
}

impl Ord for TxWeight {
    fn cmp(&self, other: &Self) -> Ordering {
        // Highest weight first, then lowest tx_id first
        other.weight.cmp(&self.weight)
            .then(self.tx_id.cmp(&other.tx_id))
    }
}
```

### `UnconfirmedTx`

```rust
/// A validated transaction in the mempool with metadata.
pub struct UnconfirmedTx {
    /// The transaction.
    pub tx: Transaction,
    /// Serialized transaction bytes: what a peer sent, or the node's own
    /// serialization for an API submission. One allocation, shared with the
    /// serving reader (§ Serving reader).
    pub tx_bytes: Arc<[u8]>,
    /// Transaction fee in nanoERG.
    pub fee: u64,
    /// Weighting cost: the entry validation's cost, floored at the serialized
    /// size. `FeePerCycle` divides by it and the rate limits sum it.
    pub cost: u32,
    /// The cost the transaction's most recent successful validation measured,
    /// at entry or at revalidation. `None` until one has run: a transaction
    /// handed to `return_to_pool` carries `None`. The JVM's
    /// `UnconfirmedTransaction.lastCost`; `GET /transactions/unconfirmed*`
    /// reports it as `cost`.
    pub validation_cost: Option<u64>,
    /// When this transaction entered the pool.
    pub created: Instant,
    /// When this transaction was last validated: at entry, then at each
    /// successful revalidation.
    pub last_checked: Instant,
    /// Peer that sent this transaction (None if locally submitted via API).
    pub source: Option<PeerId>,
}
```

### `FeeStrategy`

```rust
pub enum FeeStrategy {
    /// fee * 1024 / tx_byte_size
    FeePerByte,
    /// fee * 1024 / cost (the weighting `cost`, not `validation_cost`)
    FeePerCycle,
}
```

### `MempoolConfig`

```rust
pub struct MempoolConfig {
    /// Maximum number of transactions in the pool (JVM default: 1000).
    pub capacity: usize,
    /// Minimum fee in nanoERG to enter the pool (JVM default: 1,000,000 = 0.001 ERG).
    pub min_fee: u64,
    /// Fee sorting strategy.
    pub fee_strategy: FeeStrategy,
    /// Minimum interval between revalidation of a transaction (JVM: 30s).
    pub cleanup_interval: Duration,
    /// Number of transactions to rebroadcast per cleanup cycle (JVM: 3).
    pub rebroadcast_count: usize,
    /// Time-to-live for invalidated tx IDs in the expiring cache.
    pub invalidation_ttl: Duration,
    /// Maximum entries in the invalidation cache.
    pub invalidation_capacity: usize,
    /// Maximum validation cost budget per block interval for remote txs.
    pub cost_per_block: u64,
    /// Maximum validation cost budget per peer per block interval.
    pub cost_per_peer_per_block: u64,
    /// Miner reward delay, the only input to the fee proposition tree.
    /// Mainnet monetary constant: 720. Default 720.
    pub reward_delay: i32,
}
```

**The fee proposition is built once, not per transaction.** `Mempool::new`
derives it from `reward_delay` via
`ergo_lib::chain::ergo_tree_predef::fee_proposition()` and stores it; step 7a
compares each output's `ergo_tree` against the stored value. Deriving it per
output would recompile a tree for every output of every transaction.

`Mempool::new` stays **infallible** and panics if the tree cannot be built.
`fee_proposition()` is a pure function of one integer with no external input, so
a failure is a build-integrity fault rather than a runtime or configuration
condition, and the only cheap fallback — treat the fee as zero — silently
declines every transaction on the network, which is the defect step 7a exists to
fix. A loud startup failure for an unreachable case beats a quiet one for a
reachable one.

⚠ **That reasoning depends on `reward_delay` not being user-settable.** It is a
`MempoolConfig` field left at its default by both call sites today. If it is
ever wired to an operator-facing key, the panic becomes reachable from a config
file and `new` must become fallible.

⚠ **`reward_delay` is a monetary constant, not a mining setting.** The node
must extract fees whether or not mining is enabled, so this does **not** read
`[mining].reward_delay`. It mirrors the JVM's
`chainSettings.monetary.minerRewardDelay`. Networks with a different delay need
this threaded from chain settings; today both default to 720 and mainnet is the
only network this has been exercised on.

### `ExpiringCache<K>`

Time-bounded set. Entries expire after a configured TTL. Bounded by max capacity.

```rust
pub struct ExpiringCache<K: Eq + Hash> {
    entries: HashMap<K, Instant>,
    ttl: Duration,
    capacity: usize,
}

impl<K: Eq + Hash> ExpiringCache<K> {
    pub fn insert(&mut self, key: K);
    pub fn contains(&self, key: &K) -> bool;  // false if expired
    pub fn prune(&mut self);                   // remove all expired entries
}
```

### `FeeStats`

Statistics over the pool transactions that blocks confirmed: the JVM's
`MemPoolStatistics` (v6.0.6). `recommended_fee` and `expected_wait_ms` read
them (§ Fee queries); nothing else does.

```rust
/// Bins in the wait histogram: one per whole minute, 0 through 59.
pub const HISTOGRAM_BINS: usize = 60;
/// How often the throughput window may move (the JVM's `measurementIntervalMsec`).
pub const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

/// A count of transactions and the sum of their `fee_per_factor`.
/// JSON `{"nTxns": …, "totalFee": …}`, the JVM's `FeeHistogramBin`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistogramBin {
    pub n_txns: u64,
    /// Saturating.
    pub total_fee: u64,
}

pub struct FeeStats {
    /// Bin `m`: confirmed transactions that spent at least `m` and less than
    /// `m + 1` whole minutes in the pool.
    histogram: [FeeHistogramBin; HISTOGRAM_BINS],
    /// Throughput window: `taken` pool transactions confirmed since `window_start`.
    window_start: Instant,
    taken: u64,
    /// When the window last moved or declined to, and `taken` at that moment.
    snap_time: Instant,
    snap_taken: u64,
}

impl FeeStats {
    /// Empty histogram, `taken` and `snap_taken` 0, both instants `now`.
    pub fn new(now: Instant) -> Self;

    /// A pool transaction that entered the pool at `created`, with base
    /// weight `fee_per_factor`, was confirmed by a block at `now`.
    pub fn record_confirmation(&mut self, now: Instant, created: Instant, fee_per_factor: u64);
}
```

`record_confirmation` does, in order:
1. `taken += 1`.
2. If `now − snap_time > MEASUREMENT_INTERVAL`: when `snap_taken ≠ 0`,
   subtract `snap_taken` from `taken` and move `window_start` to `snap_time`.
   Either way, set `snap_taken = taken` and `snap_time = now`.
3. `m` = `now − created` in whole minutes. If `m < 60`, add 1 to
   `histogram[m].n_txns` and `fee_per_factor` to `histogram[m].total_fee`. A
   longer wait is left out of the histogram but was counted in step 1.

While blocks keep confirming pool transactions, the window holds one to two
intervals of them. The histogram is never pruned: it covers every
confirmation since the node started.

Where this departs from the JVM's code, it follows the JVM's own description
of that code (the doc comment on `MemPoolStatistics.add`):
- **Only confirmations are recorded,** by `apply_block`, for each confirmed
  transaction the pool held. A transaction removed as a double-spend of a
  confirmed one, or by revalidation, is not recorded, and nothing resets the
  statistics. The JVM's code records those removals too, and resets its
  statistics whenever a newly received transaction fails validation
  (`ErgoMemPool.scala` :109-113, :303).
- **Step 2 sets `snap_taken` after the subtraction.** The JVM sets it before
  (`MemPoolStatistics.scala` :32), so its count sinks to zero or below within
  a few windows, and its wait estimate then answers 0.

## Public API

### Processing transactions (validate + add)

```rust
/// Outcome of processing a transaction.
pub enum ProcessingOutcome {
    /// Transaction accepted and added to the pool.
    Accepted { tx_id: [u8; 32] },
    /// Transaction replaced one or more double-spending transactions.
    Replaced { tx_id: [u8; 32], removed: Vec<[u8; 32]> },
    /// Transaction rejected — a higher-fee transaction already spends the same input.
    DoubleSpendLoser { winner_ids: Vec<[u8; 32]> },
    /// Transaction temporarily declined — may succeed later (e.g., inputs not yet confirmed).
    Declined { reason: String },
    /// Transaction permanently invalid — added to invalidation cache.
    Invalidated { reason: String },
    /// Transaction already in pool.
    AlreadyInPool,
}

impl Mempool {
    /// Validate and add a transaction to the pool.
    ///
    /// Performs full validation:
    /// 1. Check invalidation cache and duplicate
    /// 2. Check minimum fee
    /// 3. Resolve input boxes from `utxo_reader` (confirmed + unconfirmed)
    /// 4. Run `validate_single_transaction()` for script evaluation
    /// 5. Double-spend resolution (replace-by-fee)
    /// 6. Insert with family weight propagation
    /// 7. Evict if over capacity
    ///
    /// This is the primary entry point for both P2P and API submissions.
    pub fn process(
        &mut self,
        tx: Transaction,
        tx_bytes: Vec<u8>,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
        source: Option<PeerId>,
    ) -> ProcessingOutcome;
```

### Queries

```rust
    /// Get a transaction by ID.
    pub fn get(&self, tx_id: &[u8; 32]) -> Option<&UnconfirmedTx>;

    /// Check if a transaction ID is in the pool or was recently invalidated.
    pub fn contains(&self, tx_id: &[u8; 32]) -> bool;

    /// Check if a transaction was recently invalidated.
    pub fn is_invalidated(&self, tx_id: &[u8; 32]) -> bool;

    /// Number of transactions in the pool.
    pub fn len(&self) -> usize;

    /// Get the top N transactions by weight (for block template).
    pub fn top(&self, limit: usize) -> Vec<&UnconfirmedTx>;

    /// Get all transactions in priority order.
    pub fn all_prioritized(&self) -> Vec<&UnconfirmedTx>;

    /// Get all transaction IDs.
    pub fn tx_ids(&self) -> Vec<[u8; 32]>;

    /// Get all weighted transaction IDs (for Inv messages).
    pub fn weighted_tx_ids(&self, limit: usize) -> Vec<TxWeight>;

    /// Get the unconfirmed box for a given output box ID (for chained tx validation).
    pub fn unconfirmed_box(&self, box_id: &[u8; 32]) -> Option<&ErgoBox>;

    /// Iterator over all input box IDs spent by pool transactions.
    pub fn spent_inputs(&self) -> impl Iterator<Item = &[u8; 32]>;

    /// Fee in nanoERG for a transaction of `tx_size` bytes to be confirmed
    /// within `wait_minutes`: `GET /transactions/getFee`. Never below `min_fee`.
    pub fn recommended_fee(&self, wait_minutes: u32, tx_size: u32) -> u64;

    /// Expected wait in milliseconds for a transaction of `tx_size` bytes
    /// paying `fee` nanoERG: `GET /transactions/waitTime`.
    /// Precondition: `tx_size > 0`.
    pub fn expected_wait_ms(&self, fee: u64, tx_size: u32) -> u64;

    /// The pool binned by how long each transaction has waited so far:
    /// `GET /transactions/poolHistogram`. Returns `bins + 1` bins.
    /// Preconditions: `bins >= 1`, `max_wait_ms >= bins`.
    pub fn pool_histogram(&self, bins: u32, max_wait_ms: u64) -> Vec<FeeHistogramBin>;
```

### Fee queries

Each reads the clock once, as `now`. The arithmetic is integer, every
division truncates, and nothing overflows: a product that could exceed `u64`
is computed wider, and a result that doesn't fit saturates.

- **`recommended_fee`** (JVM `ErgoMemPool.getRecommendedFee`, v6.0.6
  :350-360): take the first bin from `histogram[0]` through
  `histogram[min(wait_minutes, 59)]` whose `n_txns` is non-zero, and compute
  `(total_fee / n_txns) * tx_size / 1024`, in that order. The answer is that
  value or `min_fee`, whichever is larger, and `min_fee` when every bin in
  the range is empty. There are no bins past 59, so a longer wait answers as
  59 does.
- **`expected_wait_ms`** (JVM `getExpectedWaitTime`, :371-387): with
  `fee_per_kb = fee * 1024 / tx_size`, the position is the number of pool
  transactions whose `weight` exceeds `fee_per_kb`. With `elapsed` =
  `now − window_start`, capped at `MEASUREMENT_INTERVAL`, in milliseconds,
  the answer is `elapsed * position / taken`, or 0 while `taken` is 0.
- **`pool_histogram`** (JVM `HistogramStats.getFeeHistogram`): start from
  `bins + 1` empty bins, with `interval = max_wait_ms / bins`. For each pool
  transaction, `wait` = `now − created` in milliseconds; it adds 1 and its
  `fee_per_factor` to bin `min(wait / interval, bins)` when `wait <
  max_wait_ms`, and to bin `bins` otherwise. (When `max_wait_ms` isn't a
  multiple of `bins`, `wait / interval` can pass `bins`. The JVM then
  indexes past its array and fails, and here the transaction lands in the
  last bin.)

### Serving reader

```rust
    /// A handle for serving pool transactions to peers without the owner's lock.
    pub fn reader(&self) -> MempoolReader;

/// Clone + Send + Sync. Every handle from one `Mempool` sees the same index.
impl MempoolReader {
    /// The bytes the pool holds for `id`, or `None` if it isn't in the pool.
    pub fn tx_bytes(&self, id: &[u8; 32]) -> Option<Arc<[u8]>>;
}
```

The P2P router answers a `ModifierRequest` synchronously, inside the P2P
event loop (`facts/p2p-routing.md` § ModifierRequest), and the main crate
keeps the `Mempool` behind an async mutex that is held for whole validations.
The serve path can't wait for that lock, because the event loop must never
block (`facts/p2p-node.md` § Invariants), and a `try_lock` would miss exactly
when the pool is busy. The reader is the JVM's split: its synchronizer serves
transactions from a published mempool reader, not from the live pool
(`ErgoNodeViewSynchronizer.modifiersReq`, `mp.getAll(ids)`, v6.0.6
:1189-1194).

- **Never waits on the mempool's owner.** `tx_bytes` holds the index's own
  `std` lock for one map lookup, and nothing else.
- **Exactly the pool's membership.** The index changes in the same step as
  the pool, at insertion and at removal. Every eviction, replacement,
  confirmation and revalidation removal goes through the pool's single
  removal path, so none can leave a stale entry behind. A re-weight doesn't
  change membership and doesn't touch the index.
- **The bytes are the entry's `tx_bytes`, shared, not copied.** For a
  transaction relayed by a peer they are the bytes that peer sent; for an API
  submission, the node's serialization. The JVM serves the same:
  `transactionBytes.getOrElse(transaction.bytes)`.
- **A poisoned lock doesn't panic the reader.** Each index update is a single
  map operation, so the map is consistent even after a writer panicked. Read
  through the poison.

### Block interaction

```rust
    /// Remove confirmed transactions from an applied block.
    /// Also removes any pool transactions that double-spend confirmed inputs.
    /// Records each confirmed transaction the pool held in `FeeStats`
    /// (`record_confirmation`); the double-spends it removes are not recorded.
    /// Returns IDs of all removed transactions.
    pub fn apply_block(&mut self, confirmed_txs: &[Transaction]) -> Vec<[u8; 32]>;

    /// Return rolled-back transactions to the pool after a reorg.
    /// Transactions from removed blocks that aren't in the new chain go back.
    /// These skip validation (they were valid in the previous chain state),
    /// so each carries `validation_cost: None` until a revalidation measures it.
    pub fn return_to_pool(
        &mut self,
        txs: Vec<UnconfirmedTx>,
    );

    /// Mark a transaction as permanently invalid.
    pub fn invalidate(&mut self, tx_id: &[u8; 32]);
```

### Cleanup and revalidation

```rust
    /// Revalidate pool transactions against current state.
    ///
    /// Iterates transactions in priority order. For each:
    /// - Skip if `last_checked` is within `cleanup_interval`
    /// - Resolve inputs from `utxo_reader`
    /// - Re-run `validate_single_transaction()`
    /// - If valid: set `validation_cost` to the measured cost and
    ///   `last_checked` to now (the JVM's `UnconfirmedTransaction.withCost`)
    /// - If invalid: remove and invalidate
    /// - If inputs missing: remove (declined, not invalidated — may reappear)
    ///
    /// Stops when cumulative validation cost exceeds `cost_per_block` or
    /// all transactions have been checked. Returns IDs of removed transactions.
    pub fn revalidate(
        &mut self,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
    ) -> Vec<[u8; 32]>;

    /// Select random transactions for rebroadcast.
    /// Returns up to `rebroadcast_count` transactions whose inputs still
    /// exist in the UTXO set.
    pub fn select_for_rebroadcast(
        &self,
        utxo_reader: &dyn UtxoReader,
    ) -> Vec<&UnconfirmedTx>;
}
```

## Processing a Transaction: Detailed Flow

⚠ **The fee check runs *after* validation, not before it.** Steps 3 and 4 used
to be listed ahead of input resolution, which is not implementable: the fee is a
function of the resolved input boxes. They are numbered 7 below, matching the
code. Nothing may reorder them ahead of step 5.

1. **Check invalidated**: If `tx_id` is in `invalidated`, return `Invalidated`.
2. **Check duplicate**: If `tx_id` is in `by_id`, return `AlreadyInPool`.
5. **Resolve input boxes**: For each input, look up via `utxo_reader.box_by_id()`.
   If any input is missing, return `Declined` (not invalidated — input may appear later).
6. **Resolve data-input boxes**: Same lookup for data inputs.
6a. **Check creation height** (transient guard): if any output's `creation_height`
   exceeds `state_context.pre_header.height`, return `Declined` — **not**
   `Invalidated`. The transaction was built against a tip newer than ours and
   becomes valid as soon as we apply the next block. Caching it as invalid
   suppresses every rebroadcast for `invalidation_ttl` (1800s ≈ 15 blocks)
   without re-validating, so a momentary propagation race turns into a half-hour
   blackout for that transaction. Same reasoning as step 5's missing input.
7. **Validate**: Call `validate_single_transaction(tx, inputs, data_inputs, state_context)`.
   On failure: return `Invalidated` (add to expiring cache).
   On success: receive `cost`. The entry's `validation_cost` is `Some(cost)`,
   and its weighting `cost` is `cost` floored at `tx_bytes.len()`.
7a. **Compute fee**: sum the values of outputs whose `ergo_tree` equals the fee
   proposition. **Not `input_sum - output_sum`.** ergo-lib enforces exact ERG
   preservation (`ErgPreservationError` when `input_sum != output_sum`,
   `wallet/tx_context.rs:122`), so a difference-based fee is structurally zero
   for every transaction that reaches this point, and step 7b then declines all
   of them. Ergo has no implicit change-to-fee remainder: the fee is an explicit
   output guarded by the fee proposition. JVM parity —
   `ErgoMemPool.extractFee` filters on
   `settings.chainSettings.monetary.feeProposition`
   (`ErgoMemPool.scala:304-309`).
7b. **Check min fee**: If `fee < config.min_fee`, return `Declined` (fee may be
   raised by a replacement; this is not an invalidation).
8. **Compute weight**: `fee_per_factor = fee * 1024 / fee_factor` where `fee_factor`
   is `tx_bytes.len()` (FeePerByte) or `cost` (FeePerCycle, with `FakeCost = 1000`
   fallback if cost is 0).
9. **Check double-spends**: For each input box ID, check `by_input`:
   - Collect all conflicting transactions.
   - Compute `avg_conflict_weight = sum(weights) / count`.
   - If new weight > avg_conflict_weight: mark conflicts for removal.
   - If new weight <= avg_conflict_weight: return `DoubleSpendLoser`.
10. **Check capacity**: If pool is at capacity and new weight <= lowest weight,
    return `Declined`.
11. **Insert**: Add to `pool`, `by_id`, `by_input`, `by_output`.
12. **Family weight update**: If any input box ID is in `by_output` (spending
    an unconfirmed output), propagate this transaction's weight to the parent
    and all ancestors. Capped at 500 levels or 500ms.
13. **Remove conflicts**: Remove double-spend losers from step 9.
14. **Evict if over capacity**: Remove the lowest-weight entry.
15. Return `Accepted` or `Replaced { removed }`.

## Family Weight Propagation

When transaction C spends an output of transaction P already in the pool:

1. Look up P via `by_output[input_box_id]`.
2. Remove P from `pool` (BTreeMap is keyed by weight — weight is about to change).
3. Add C's `fee_per_factor` to P's `weight`.
4. Re-insert P into `pool` with updated weight.
5. For each of P's inputs, check if P itself spends an unconfirmed output
   (P's parent is also in the pool). If so, recursively propagate to P's parent.
6. Stop after 500 ancestor levels or 500ms elapsed.

This ensures parents always have higher effective weight than their children,
so they sort first in the priority order and are evicted last.

## Block Application

When a new block arrives:

1. For each confirmed transaction:
   - If in pool: record it in `FeeStats` (`record_confirmation` with its
     `created` and base `fee_per_factor`), then remove it from `pool`,
     `by_id`, `by_input`, `by_output`.
   - For each input of the confirmed tx: if a DIFFERENT pool tx also spends
     that input (double-spend), remove the pool tx too. It is not recorded.
2. Prune the `invalidated` cache (remove expired entries).

On reorg (block removed):
1. The main crate collects transactions from removed blocks that aren't in the
   new chain's blocks.
2. Passes them to `return_to_pool()` which re-inserts without validation
   (they were valid in the prior state — if they're now invalid, the next
   cleanup cycle catches them).

## Rate Limiting (for P2P integration)

The mempool tracks validation cost to rate-limit transaction acceptance from
peers between blocks:

- `interblock_cost: u64` — total validation cost since last block. Reset on
  block application. When >= `config.cost_per_block`, decline all remote txs.
- `per_peer_cost: HashMap<PeerId, u64>` — per-peer cost. Reset on block
  application. When >= `config.cost_per_peer_per_block`, decline txs from
  that peer.

These are checked in `process()` before validation. Locally submitted
transactions (source = None) bypass rate limiting.

## P2P Transaction Broadcast and Serving (main crate responsibility)

The mempool crate neither broadcasts nor serves transactions. The main crate
takes `reader()` handles at startup. With one it answers type-2 ids in the
local-serve closure it injects into the P2P router; block sections still
come from the modifier store. With another, sync's store adapter answers
whether the node already holds an announced transaction, so it is not
requested again (`facts/sync.md` § `SyncStore`). The mempool task handles broadcast after
`process()` returns. Every broadcast reaches all connected peers, inbound and
outbound, as the JVM's `SuccessfulTransaction` → `broadcastModifierInv`
does (`facts/p2p-node.md` § `broadcast`):

**On acceptance (Accepted or Replaced):**
- Build `Inv { modifier_type: 2, ids: [tx_id] }` message
- `broadcast()` to all connected peers
- For P2P-sourced transactions, this relays to peers that haven't seen it
- For API-sourced transactions, this announces the new tx to the network

**On cleanup (rebroadcast):**
- `select_for_rebroadcast()` returns up to `rebroadcast_count` transactions
  whose inputs still exist in the confirmed UTXO set
- Build `Inv { modifier_type: 2, ids: [tx_ids...] }` message
- `broadcast()` to all connected peers
- Peers that already have the tx in their pool will ignore the Inv

**Not broadcast:**
- Declined, Invalidated, DoubleSpendLoser, AlreadyInPool outcomes
- Transactions removed by `apply_block()` or `revalidate()`

## P2P Transaction Intake (main crate responsibility)

A transaction a peer delivers is size-checked before anything parses it, as
the JVM's `parseAndProcessTransaction` does (`ErgoNodeViewSynchronizer`
v6.0.6 :785-792):

- **Over `MAX_TRANSACTION_SIZE`** (98,304 bytes, the JVM's
  `maxTransactionSize` default): dropped unparsed, and the sender gets a
  `PENALTY` with kind `oversized_transaction`. That is a misbehavior kind:
  logged, not banned (`facts/journal-events.md` § `peer_penalised`).
- **At or under it:** forwarded to the mempool task.

The mempool crate exports the limit, so the P2P intake and the REST API
(`facts/api.md` § Transaction Submission Flow) share one definition:

```rust
/// Largest serialized transaction the node accepts from a peer or the API.
/// The JVM's `maxTransactionSize` default (`application.conf`).
pub const MAX_TRANSACTION_SIZE: usize = 98_304;
```

Two parts of the JVM's intake are not followed yet:
- It also penalizes a peer whose transaction fails to parse, or whose id
  differs from the one it declared (:794-805). Our parser still rejects some
  transactions the JVM accepts (context-extension values encoded as
  expressions), and our id can differ on a non-canonically encoded tree, so
  those penalties would land on honest peers. They wait for both fixes.
- It stops requesting a declared id that proved oversized (`setInvalid`).
  Transaction requests here keep no memory of invalid ids.

## Configuration

```toml
[node.mempool]
capacity = 1000               # max transactions (JVM default: 1000)
min_fee = 1000000              # minimum fee in nanoERG (0.001 ERG)
fee_strategy = "by_size"       # "by_size" or "by_cost"
cleanup_interval_secs = 30     # min seconds between tx revalidation
rebroadcast_count = 3          # txs rebroadcast per cleanup cycle
```

## Dependencies

- `ergo-lib` — `Transaction`, `ErgoBox` types
- `ergo-chain-types` — `Header`
- `ergo-validation` — `validate_single_transaction()`, `build_state_context()`
- No dependency on P2P, storage, or state crates
- `UtxoReader` trait defined here, implemented by the main crate

## Does NOT Own

- ErgoScript evaluation — that's `ergo-validation` (via `validate_single_transaction()`)
- UTXO state — that's `enr-state` (accessed via `UtxoReader` trait)
- P2P propagation — that's the main crate (Inv broadcast on acceptance)
- REST API — that's the API crate
- Mining / block template assembly — that's the mining API
- Configuration parsing — that's the main crate
- Reorg detection — that's the sync machine / pipeline

## Invariants

- No two transactions in the pool spend the same input box (enforced by
  replace-by-fee on every insertion).
- `by_id`, `by_input`, `by_output` are always consistent with `pool`.
- `reader().tx_bytes(id)` is `Some` exactly while `id` is in the pool.
- `len() <= capacity` after every mutation.
- Every transaction in the pool passed `validate_single_transaction()` at
  the time of insertion (may become invalid later — caught by revalidation).
- Family weights are consistent: a parent's weight >= its own `fee_per_factor`
  plus the sum of direct children's `fee_per_factor` values.
- The pool is never persisted. Restart = empty.

## Testing Strategy

1. **Add/remove**: Insert txs with known fees, verify ordering. Remove by ID,
   verify indexes cleaned up.
2. **Double-spend — reject loser**: Two txs spending same input. Lower fee
   rejected with `DoubleSpendLoser`.
3. **Double-spend — replace by fee**: Add low-fee tx, then high-fee tx spending
   same input. Verify replacement, verify old tx removed from all indexes.
4. **Eviction**: Fill pool to capacity, add higher-fee tx. Verify lowest-fee
   tx evicted.
5. **Block application**: Add txs, apply block with some confirmed. Verify
   confirmed removed, double-spends of confirmed inputs removed, fee stats
   recorded.
6. **Chained txs**: Tx B spends output of Tx A. Both in pool. Verify
   `unconfirmed_box()` returns A's output for B's resolution.
7. **Family weighting**: Tx A (fee 100) in pool. Tx B (fee 200) spends A's
   output. After B is added, A's weight should increase by B's fee_per_factor.
   A should sort higher than B.
8. **Family depth cap**: Chain of 600 linked txs. Verify propagation stops
   at depth 500.
9. **Invalidation + expiry**: Invalidate a tx, verify rejected on re-add.
   Wait past TTL, verify cache no longer rejects it.
10. **Revalidation**: Add tx, then simulate state change making its input
    unavailable. Run `revalidate()`, verify tx removed.
11. **Rate limiting**: Process txs from same peer until cost budget exhausted.
    Verify next tx from that peer is declined. Verify local tx still accepted.
12. **Reorg return**: Apply block removing tx A from chain. Return A to pool.
    Verify it's back in the pool without re-validation.
13. **Fee statistics**, against a driven clock rather than wall time, with
    expected values written out by hand rather than recomputed by the code
    under test: the histogram bin a confirmation lands in (59 minutes in,
    60 out); the window moving across intervals, including the case where
    the JVM's count would sink to zero; `recommended_fee` at the `min_fee`
    floor, from an empty histogram, and from the first non-empty bin;
    `expected_wait_ms` with `taken` 0 and with a known position; and
    `pool_histogram`'s last bin, including a `max_wait_ms` that isn't a
    multiple of `bins`.
14. **Capacity at zero**: Empty pool, verify all queries return empty/zero.
