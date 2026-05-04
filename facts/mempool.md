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
/// Build an ErgoStateContext from a header and preceding headers.
pub fn build_state_context(
    header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> ErgoStateContext;
```

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
    /// Fee statistics for histogram/recommendation queries.
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
    /// Serialized transaction bytes (cached for P2P propagation).
    pub tx_bytes: Vec<u8>,
    /// Transaction fee in nanoERG.
    pub fee: u64,
    /// Validation cost from ErgoScript evaluation.
    pub cost: u32,
    /// When this transaction entered the pool.
    pub created: Instant,
    /// When this transaction was last revalidated.
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
    /// fee * 1024 / validation_cost
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
}
```

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

Tracks fee information for histogram and recommendation queries.

```rust
pub struct FeeStats {
    /// Recent transaction wait times: (fee_per_factor, wait_duration_ms).
    /// Populated when a confirmed tx is removed from the pool.
    history: VecDeque<(u64, u64)>,
    /// Maximum history entries.
    max_history: usize,
}

impl FeeStats {
    /// Record that a transaction with given fee was confirmed after `wait_ms`.
    pub fn record_confirmation(&mut self, fee_per_factor: u64, wait_ms: u64);

    /// Fee histogram: bucket count, total fee, avg fee per bucket.
    pub fn histogram(&self, bucket_count: usize) -> Vec<FeeBucket>;

    /// Estimated wait time for a transaction with given fee and size.
    pub fn expected_wait(&self, fee: u64, size: usize) -> Option<u64>;

    /// Recommended fee to achieve target wait time.
    pub fn recommended_fee(&self, target_wait_ms: u64, size: usize) -> Option<u64>;
}
```

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

    /// Fee histogram for the current pool.
    pub fn fee_histogram(&self, buckets: usize) -> Vec<FeeBucket>;

    /// Estimated wait time for a transaction with given fee and size.
    pub fn expected_wait_time(&self, fee: u64, tx_size: usize) -> Option<u64>;

    /// Recommended fee to achieve target wait time.
    pub fn recommended_fee(&self, target_wait_ms: u64, tx_size: usize) -> Option<u64>;
```

### Block interaction

```rust
    /// Remove confirmed transactions from an applied block.
    /// Also removes any pool transactions that double-spend confirmed inputs.
    /// Records confirmation timing into fee statistics.
    /// Returns IDs of all removed transactions.
    pub fn apply_block(&mut self, confirmed_txs: &[Transaction]) -> Vec<[u8; 32]>;

    /// Return rolled-back transactions to the pool after a reorg.
    /// Transactions from removed blocks that aren't in the new chain go back.
    /// These skip validation (they were valid in the previous chain state).
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

1. **Check invalidated**: If `tx_id` is in `invalidated`, return `Invalidated`.
2. **Check duplicate**: If `tx_id` is in `by_id`, return `AlreadyInPool`.
3. **Compute fee**: Sum output values going to the fee proposition address.
4. **Check min fee**: If `fee < config.min_fee`, return `Declined`.
5. **Resolve input boxes**: For each input, look up via `utxo_reader.box_by_id()`.
   If any input is missing, return `Declined` (not invalidated — input may appear later).
6. **Resolve data-input boxes**: Same lookup for data inputs.
7. **Validate**: Call `validate_single_transaction(tx, inputs, data_inputs, state_context)`.
   On failure: return `Invalidated` (add to expiring cache).
   On success: receive `cost`.
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
   - If in pool: remove from `pool`, `by_id`, `by_input`, `by_output`.
   - Record confirmation timing in `FeeStats` (fee_per_factor, wait duration).
   - For each input of the confirmed tx: if a DIFFERENT pool tx also spends
     that input (double-spend), remove the pool tx too.
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

## P2P Transaction Broadcast (main crate responsibility)

The mempool crate does not broadcast transactions. The main crate's mempool
task handles broadcast after `process()` returns:

**On acceptance (Accepted or Replaced):**
- Build `Inv { modifier_type: 2, ids: [tx_id] }` message
- `broadcast_outbound()` to all connected outbound peers
- For P2P-sourced transactions, this relays to peers that haven't seen it
- For API-sourced transactions, this announces the new tx to the network

**On cleanup (rebroadcast):**
- `select_for_rebroadcast()` returns up to `rebroadcast_count` transactions
  whose inputs still exist in the confirmed UTXO set
- Build `Inv { modifier_type: 2, ids: [tx_ids...] }` message
- `broadcast_outbound()` to all connected outbound peers
- Peers that already have the tx in their pool will ignore the Inv

**Not broadcast:**
- Declined, Invalidated, DoubleSpendLoser, AlreadyInPool outcomes
- Transactions removed by `apply_block()` or `revalidate()`

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
13. **Fee statistics**: Add txs, confirm them, verify histogram reflects
    recorded wait times.
14. **Capacity at zero**: Empty pool, verify all queries return empty/zero.
