# Deep Chain Reorganization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable the node to handle multi-block chain reorganizations using stored fork headers and cumulative difficulty scoring, eliminating the DoS-vulnerable 1-deep-only reorg limitation.

**Architecture:** All validated headers are kept permanently in the store with a fork tag per height. The in-memory `HeaderChain` remains the best-chain view. Reorg is a local operation: compare cumulative scores, read fork headers from store, swap the in-memory chain, update store indexes. Zero network traffic on reorg. Block sections for forks are pre-fetched near the chain tip so full-block reorg is also local.

**Tech Stack:** Rust, redb (embedded KV store), num-bigint (BigUint for cumulative scores), tokio (async runtime)

**Spec:** `facts/reorg.md`, JVM reference: `facts/jvm-reorg-spec.md`

---

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `store/src/lib.rs` | Add header-specific trait methods |
| Modify | `store/src/redb.rs` | New tables, implement header methods, migration |
| Modify | `chain/Cargo.toml` | Add `num-bigint` (already present) |
| Modify | `chain/src/chain.rs` | `AppendResult`, cumulative scores, `try_reorg_deep` |
| Modify | `chain/src/error.rs` | New error variant for reorg |
| Modify | `chain/src/lib.rs` | Re-export new types |
| Modify | `sync/src/delivery.rs` | `DeliveryEvent::Reorg` variant |
| Modify | `sync/src/state.rs` | Handle reorg notification, two-mode section download |
| Modify | `src/pipeline.rs` | Fork detection, score comparison, reorg triggering |
| Modify | `src/main.rs` | Updated chain restore using new store API |
| Modify | `src/bridge.rs` | Pass store to pipeline (if needed for fork reads) |

---

### Task 1: Store — Header Tables and Trait Methods

**Files:**
- Modify: `store/src/lib.rs`
- Modify: `store/src/redb.rs`

This task adds the three new redb tables (`HEADER_FORKS`, `HEADER_SCORES`, `BEST_CHAIN`) and the trait methods to use them. No migration yet — that's Task 2.

- [ ] **Step 1: Write tests for the new store methods**

Add to `store/src/redb.rs` in the existing `mod tests` block:

```rust
#[test]
fn put_header_and_query() {
    let (store, _dir) = test_store();
    let id = test_id(1);
    let score = 1000u64.to_be_bytes().to_vec();

    store.put_header(&id, 1, 0, &score, b"header-data").unwrap();

    // Primary data accessible
    assert_eq!(store.get(101, &id).unwrap(), Some(b"header-data".to_vec()));

    // Score stored
    assert_eq!(store.header_score(&id).unwrap(), Some(score.clone()));

    // Best chain entry
    assert_eq!(store.best_header_at(1).unwrap(), Some(id));

    // Fork index
    let forks = store.header_ids_at_height(1).unwrap();
    assert_eq!(forks, vec![(id, 0)]);

    // Tip
    assert_eq!(store.best_header_tip().unwrap(), Some((1, id)));
}

#[test]
fn multiple_forks_at_same_height() {
    let (store, _dir) = test_store();
    let id_a = test_id(1);
    let id_b = test_id(2);
    let score_a = 1000u64.to_be_bytes().to_vec();
    let score_b = 2000u64.to_be_bytes().to_vec();

    store.put_header(&id_a, 5, 0, &score_a, b"header-a").unwrap();
    store.put_header(&id_b, 5, 1, &score_b, b"header-b").unwrap();

    let forks = store.header_ids_at_height(5).unwrap();
    assert_eq!(forks.len(), 2);
    assert!(forks.contains(&(id_a, 0)));
    assert!(forks.contains(&(id_b, 1)));

    // Best chain still points to first one until explicitly switched
    assert_eq!(store.best_header_at(5).unwrap(), Some(id_a));
}

#[test]
fn switch_best_chain() {
    let (store, _dir) = test_store();
    let old_id = test_id(1);
    let new_id = test_id(2);

    store.put_header(&old_id, 10, 0, &[0x01], b"old").unwrap();
    store.put_header(&new_id, 10, 1, &[0x02], b"new").unwrap();

    // Switch: demote height 10, promote new_id at height 10
    store.switch_best_chain(&[10], &[(10, new_id)]).unwrap();

    assert_eq!(store.best_header_at(10).unwrap(), Some(new_id));
}

#[test]
fn switch_best_chain_multi_height() {
    let (store, _dir) = test_store();
    // Old chain: heights 5, 6, 7
    store.put_header(&test_id(1), 5, 0, &[0x01], b"old5").unwrap();
    store.put_header(&test_id(2), 6, 0, &[0x02], b"old6").unwrap();
    store.put_header(&test_id(3), 7, 0, &[0x03], b"old7").unwrap();

    // New chain: heights 5, 6, 7, 8
    store.put_header(&test_id(11), 5, 1, &[0x11], b"new5").unwrap();
    store.put_header(&test_id(12), 6, 1, &[0x12], b"new6").unwrap();
    store.put_header(&test_id(13), 7, 1, &[0x13], b"new7").unwrap();
    store.put_header(&test_id(14), 8, 0, &[0x14], b"new8").unwrap();

    // Fork point is height 4 (not stored here). Demote 5-7, promote 5-8.
    store.switch_best_chain(
        &[5, 6, 7],
        &[(5, test_id(11)), (6, test_id(12)), (7, test_id(13)), (8, test_id(14))],
    ).unwrap();

    assert_eq!(store.best_header_at(5).unwrap(), Some(test_id(11)));
    assert_eq!(store.best_header_at(6).unwrap(), Some(test_id(12)));
    assert_eq!(store.best_header_at(7).unwrap(), Some(test_id(13)));
    assert_eq!(store.best_header_at(8).unwrap(), Some(test_id(14)));

    // Tip updated
    assert_eq!(store.best_header_tip().unwrap(), Some((8, test_id(14))));
}

#[test]
fn put_header_batch_works() {
    let (store, _dir) = test_store();
    let entries = vec![
        (test_id(1), 1, 0, 100u64.to_be_bytes().to_vec(), b"h1".to_vec()),
        (test_id(2), 2, 0, 200u64.to_be_bytes().to_vec(), b"h2".to_vec()),
    ];

    store.put_header_batch(&entries).unwrap();

    assert_eq!(store.best_header_at(1).unwrap(), Some(test_id(1)));
    assert_eq!(store.best_header_at(2).unwrap(), Some(test_id(2)));
    assert_eq!(store.header_score(&test_id(1)).unwrap(), Some(100u64.to_be_bytes().to_vec()));
    assert_eq!(store.best_header_tip().unwrap(), Some((2, test_id(2))));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-store 2>&1 | tail -20`
Expected: compilation errors — methods don't exist yet.

- [ ] **Step 3: Add trait methods to `ModifierStore`**

In `store/src/lib.rs`, add these methods to the `ModifierStore` trait after the existing `tip` method:

```rust
    /// Store a header with its fork number and cumulative score.
    /// Also writes to PRIMARY with type_id=101, HEADER_FORKS, HEADER_SCORES,
    /// and BEST_CHAIN (if fork == 0, i.e. first header at this height).
    fn put_header(
        &self,
        id: &[u8; 32],
        height: u32,
        fork: u32,
        score: &[u8],
        data: &[u8],
    ) -> Result<(), Self::Error>;

    /// Batch version of put_header. All entries written atomically.
    fn put_header_batch(
        &self,
        entries: &[([u8; 32], u32, u32, Vec<u8>, Vec<u8>)],
    ) -> Result<(), Self::Error>;

    /// Get all header IDs at a given height across all forks.
    /// Returns Vec<(header_id, fork_number)> sorted by fork number.
    fn header_ids_at_height(
        &self,
        height: u32,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error>;

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
    /// Updates the best_header_tip cache.
    fn switch_best_chain(
        &self,
        demote: &[u32],
        promote: &[(u32, [u8; 32])],
    ) -> Result<(), Self::Error>;
```

- [ ] **Step 4: Add table definitions and implement in redb**

In `store/src/redb.rs`, add three new table definitions after `HEIGHT_INDEX`:

```rust
const HEADER_FORKS: TableDefinition<(u32, u32), [u8; 32]> = TableDefinition::new("header_forks");
const HEADER_SCORES: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("header_scores");
const BEST_CHAIN: TableDefinition<u32, [u8; 32]> = TableDefinition::new("best_chain");
```

Add a `best_header_tip` field to `RedbModifierStore`:

```rust
pub struct RedbModifierStore {
    db: Database,
    tips: RwLock<HashMap<u8, (u32, [u8; 32])>>,
    best_header_tip: RwLock<Option<(u32, [u8; 32])>>,
}
```

Update `new()` to initialize `best_header_tip` by scanning `BEST_CHAIN` for the highest entry (similar to `load_tips`).

Implement all seven trait methods. Key details:

- `put_header`: single write transaction writing to `PRIMARY` (key `(101, *id)`), `HEADER_FORKS` (key `(height, fork)`), `HEADER_SCORES` (key `*id`), and `BEST_CHAIN` (key `height`, only if this is the first header at this height — check if `BEST_CHAIN` already has an entry at this height).
- `put_header_batch`: same but in one transaction for all entries.
- `header_ids_at_height`: range scan on `HEADER_FORKS` from `(height, 0)` to `(height, u32::MAX)`.
- `header_score`: point lookup on `HEADER_SCORES`.
- `best_header_at`: point lookup on `BEST_CHAIN`.
- `best_header_tip`: return cached value from `self.best_header_tip`.
- `switch_best_chain`: single write transaction — remove `demote` heights from `BEST_CHAIN`, insert `promote` entries, update `best_header_tip` cache.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-store 2>&1 | tail -30`
Expected: all tests pass including new ones.

- [ ] **Step 6: Commit**

```bash
git add store/src/lib.rs store/src/redb.rs
git commit -m "feat(store): add header fork tables, scores, and best chain index"
```

---

### Task 2: Store — Migration from HEIGHT_INDEX to Header Tables

**Files:**
- Modify: `store/src/redb.rs`

Existing stores have headers in `HEIGHT_INDEX` with key `(101, height)`. This task adds a one-time migration that runs on first open after upgrade.

- [ ] **Step 1: Write migration test**

```rust
#[test]
fn migration_from_height_index() {
    let (store, _dir) = test_store();

    // Simulate old-format data: headers in HEIGHT_INDEX with type_id=101
    {
        let write_txn = store.db.begin_write().unwrap();
        {
            let mut primary = write_txn.open_table(PRIMARY).unwrap();
            let mut height_idx = write_txn.open_table(HEIGHT_INDEX).unwrap();
            // Two headers at heights 1 and 2
            primary.insert((101u8, test_id(1)), b"genesis-header".as_slice()).unwrap();
            height_idx.insert((101u8, 1u32), test_id(1)).unwrap();
            primary.insert((101u8, test_id(2)), b"second-header".as_slice()).unwrap();
            height_idx.insert((101u8, 2u32), test_id(2)).unwrap();
        }
        write_txn.commit().unwrap();
    }

    // Run migration
    store.migrate_headers_if_needed().unwrap();

    // Old HEIGHT_INDEX entries for type 101 should be gone
    {
        let read_txn = store.db.begin_read().unwrap();
        let table = read_txn.open_table(HEIGHT_INDEX).unwrap();
        assert!(table.get((101u8, 1u32)).unwrap().is_none());
        assert!(table.get((101u8, 2u32)).unwrap().is_none());
    }

    // New tables should have the data
    assert_eq!(store.best_header_at(1).unwrap(), Some(test_id(1)));
    assert_eq!(store.best_header_at(2).unwrap(), Some(test_id(2)));
    let forks = store.header_ids_at_height(1).unwrap();
    assert_eq!(forks, vec![(test_id(1), 0)]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-store migration_from_height_index 2>&1 | tail -10`
Expected: FAIL — method doesn't exist.

- [ ] **Step 3: Implement migration**

Add a `pub fn migrate_headers_if_needed(&self) -> Result<(), StoreError>` method to `RedbModifierStore`:

```rust
/// Migrate headers from old HEIGHT_INDEX format to new header tables.
/// Runs once: if HEADER_FORKS is empty but HEIGHT_INDEX has type_id=101 entries.
/// All writes happen in a single transaction.
pub fn migrate_headers_if_needed(&self) -> Result<(), StoreError> {
    // Check if migration needed: HEADER_FORKS empty, HEIGHT_INDEX has 101 entries
    let read_txn = self.db.begin_read()?;
    let has_new = match read_txn.open_table(HEADER_FORKS) {
        Ok(t) => t.iter()?.next().is_some(),
        Err(::redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(StoreError::Table(e)),
    };
    if has_new {
        return Ok(()); // Already migrated
    }

    // Collect old entries: (height, header_id)
    let old_entries: Vec<(u32, [u8; 32])> = match read_txn.open_table(HEIGHT_INDEX) {
        Ok(t) => {
            let mut entries = Vec::new();
            for result in t.iter()? {
                let (key, val) = result?;
                let (type_id, height) = key.value();
                if type_id == 101 {
                    entries.push((height, val.value()));
                }
            }
            entries
        }
        Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(StoreError::Table(e)),
    };
    drop(read_txn);

    if old_entries.is_empty() {
        return Ok(());
    }

    // Sort by height for score computation (should already be sorted)
    let mut sorted = old_entries;
    sorted.sort_by_key(|(h, _)| *h);

    // Write all new tables + remove old entries in one transaction
    let write_txn = self.db.begin_write()?;
    {
        let mut forks = write_txn.open_table(HEADER_FORKS)?;
        let mut scores = write_txn.open_table(HEADER_SCORES)?;
        let mut best = write_txn.open_table(BEST_CHAIN)?;
        let mut height_idx = write_txn.open_table(HEIGHT_INDEX)?;

        // Note: cumulative scores require parsing headers to get n_bits.
        // Since we don't have decode_compact_bits in the store crate,
        // store a zero-length score placeholder. The main crate will
        // recompute scores on chain restore.
        for (height, id) in &sorted {
            forks.insert((*height, 0u32), *id)?;
            scores.insert(*id, &[] as &[u8])?;
            best.insert(*height, *id)?;
            height_idx.remove((101u8, *height))?;
        }
    }
    write_txn.commit()?;

    // Update tip cache
    if let Some((height, id)) = sorted.last() {
        let mut tip = self.best_header_tip.write().unwrap_or_else(|e| e.into_inner());
        *tip = Some((*height, *id));
    }

    tracing::info!(count = sorted.len(), "migrated headers to fork-aware tables");
    Ok(())
}
```

Update `RedbModifierStore::new()` to call `migrate_headers_if_needed()` after initialization.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-store 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add store/src/redb.rs
git commit -m "feat(store): add migration from HEIGHT_INDEX to header fork tables"
```

---

### Task 3: HeaderChain — Cumulative Score Tracking

**Files:**
- Modify: `chain/src/chain.rs`
- Modify: `chain/src/lib.rs`

Add cumulative score computation to `HeaderChain`. Each header adds `decode_compact_bits(n_bits)` to a running total. Scores are stored per-height for fork comparison.

- [ ] **Step 1: Write tests for cumulative score**

Add to `chain/src/tests.rs` (or create a new test if the existing test infrastructure supports it). Since we need real headers with valid PoW for chain tests, use the existing testnet chain test pattern:

```rust
#[test]
fn cumulative_score_increases_on_append() {
    // Uses the test chain builder from existing tests
    let config = ChainConfig::testnet();
    let chain = HeaderChain::new(config);
    // Empty chain has zero score
    assert_eq!(chain.cumulative_score(), BigUint::from(0u32));
}

#[test]
fn score_at_returns_none_for_empty_chain() {
    let config = ChainConfig::testnet();
    let chain = HeaderChain::new(config);
    assert!(chain.score_at(1).is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-chain cumulative_score 2>&1 | tail -10`
Expected: FAIL — methods don't exist.

- [ ] **Step 3: Add BigUint dependency and score fields**

In `chain/Cargo.toml`, `num-bigint` is already present. Add `num-traits` if not present (needed for `Zero`):

```toml
num-traits = "0.2"
```

In `chain/src/chain.rs`, add to the `HeaderChain` struct:

```rust
use num_bigint::BigUint;
use ergo_chain_types::autolykos_pow_scheme::decode_compact_bits;

pub struct HeaderChain {
    config: ChainConfig,
    by_height: Vec<Header>,
    by_id: HashMap<BlockId, u32>,
    /// Cumulative difficulty score per header, parallel to by_height.
    scores: Vec<BigUint>,
}
```

- [ ] **Step 4: Implement score methods and update try_append**

Add public methods:

```rust
/// Cumulative difficulty score at the chain tip.
pub fn cumulative_score(&self) -> BigUint {
    self.scores.last().cloned().unwrap_or_default()
}

/// Cumulative difficulty score at a given height.
pub fn score_at(&self, height: u32) -> Option<&BigUint> {
    if self.by_height.is_empty() {
        return None;
    }
    let base = self.by_height[0].height;
    let idx = height.checked_sub(base)? as usize;
    self.scores.get(idx)
}
```

Update `try_append` to compute score after successful validation:

```rust
// After pushing to by_height:
let parent_score = self.scores.last().cloned().unwrap_or_default();
let difficulty = decode_compact_bits(header.n_bits);
// decode_compact_bits returns BigInt, convert to BigUint (difficulty is always positive)
let diff_uint = difficulty.to_biguint().unwrap_or_default();
self.scores.push(parent_score + diff_uint);
```

Update `new()` to initialize `scores: Vec::new()`.

Update `restore_tip` and other internal methods that push/pop `by_height` to also push/pop `scores`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-chain 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 6: Export BigUint from lib.rs**

In `chain/src/lib.rs`, add:

```rust
pub use num_bigint::BigUint;
```

- [ ] **Step 7: Commit**

```bash
git add chain/src/chain.rs chain/src/lib.rs chain/Cargo.toml
git commit -m "feat(chain): add cumulative difficulty score tracking"
```

---

### Task 4: HeaderChain — AppendResult and Fork Detection

**Files:**
- Modify: `chain/src/chain.rs`
- Modify: `chain/src/lib.rs`

Change `try_append` to return `AppendResult` distinguishing between extending the best chain and forking from it.

- [ ] **Step 1: Write tests for fork detection**

```rust
#[test]
fn try_append_returns_forked_for_non_tip_parent() {
    // Build a chain with genesis + a few blocks using no-pow test helpers
    let config = ChainConfig::testnet();
    let mut chain = HeaderChain::new(config);
    // ... append genesis and block 2 using try_append_no_pow ...

    // Create a header whose parent is genesis (height 1), not the tip (height 2)
    // This forks from height 1
    // ... construct fork header at height 2 with parent_id = genesis.id ...

    match chain.try_append_no_pow(fork_header) {
        Ok(AppendResult::Forked { fork_height }) => {
            assert_eq!(fork_height, 1); // forks from genesis
        }
        other => panic!("expected Forked, got {other:?}"),
    }
    // Chain should be unchanged — fork header is NOT added
    assert_eq!(chain.height(), 2);
}
```

Note: The exact test construction depends on existing test helper patterns in `chain/src/tests.rs`. The implementing agent should follow those patterns.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-chain try_append_returns_forked 2>&1 | tail -10`
Expected: FAIL — `AppendResult` doesn't exist.

- [ ] **Step 3: Define AppendResult and update try_append**

In `chain/src/chain.rs`:

```rust
/// Result of attempting to append a header to the chain.
#[derive(Debug)]
pub enum AppendResult {
    /// Header extends the best chain. Chain height increased.
    Extended,
    /// Header is valid but forks from the best chain.
    /// `fork_height` is the height of the common ancestor (the fork point).
    /// The header itself is NOT added to the chain — caller stores it.
    Forked { fork_height: u32 },
}
```

Update `try_append` signature to `-> Result<AppendResult, ChainError>`.

In `validate_child` (or in `try_append` before calling `validate_child`), change the `parent_id != tip.id` check:

```rust
pub fn try_append(&mut self, header: Header) -> Result<AppendResult, ChainError> {
    if self.by_height.is_empty() {
        self.validate_genesis(&header)?;
        // ... push header, compute score ...
        return Ok(AppendResult::Extended);
    }

    // Check if parent is the tip (best chain extension)
    if header.parent_id == self.tip().id {
        self.validate_child(&header)?;
        // ... push header, compute score ...
        return Ok(AppendResult::Extended);
    }

    // Check if parent is in the chain but not the tip (fork)
    if let Some(&parent_height) = self.by_id.get(&header.parent_id) {
        // Basic validation: height must be parent + 1
        if header.height != parent_height + 1 {
            return Err(ChainError::NonSequentialHeight {
                expected: parent_height + 1,
                got: header.height,
            });
        }
        return Ok(AppendResult::Forked { fork_height: parent_height });
    }

    // Parent not in chain at all
    Err(ChainError::ParentNotFound { parent_id: header.parent_id })
}
```

- [ ] **Step 4: Fix all callers of try_append**

Every call site that currently does `chain.try_append(header)?` now gets a `Result<AppendResult, _>`. Callers that only care about extending need to match:

- `chain/src/chain.rs` — `try_reorg_impl` and test helpers: match on `Extended`, treat `Forked` as an error in reorg context.
- `chain/src/tests.rs` — update existing tests to handle `AppendResult`.
- `src/pipeline.rs` — this is where the real fork handling goes (Task 6).
- `src/main.rs` — chain restore loop: during restore all headers are sequential, so `Extended` is expected; treat `Forked` as a restore error.

For now, update everything except `pipeline.rs` (that's Task 6).

- [ ] **Step 5: Export AppendResult from lib.rs**

In `chain/src/lib.rs`:

```rust
pub use chain::{HeaderChain, AppendResult};
```

- [ ] **Step 6: Run all tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test 2>&1 | tail -30`
Expected: all tests pass. Pipeline compiles because its `try_append` calls still work — they just return `AppendResult` now.

- [ ] **Step 7: Commit**

```bash
git add chain/src/chain.rs chain/src/lib.rs chain/src/tests.rs src/main.rs src/pipeline.rs
git commit -m "feat(chain): return AppendResult from try_append, detect forks"
```

---

### Task 5: HeaderChain — Deep Reorg

**Files:**
- Modify: `chain/src/chain.rs`
- Modify: `chain/src/error.rs`

- [ ] **Step 1: Write tests for try_reorg_deep**

```rust
#[test]
fn try_reorg_deep_switches_to_longer_fork() {
    let config = ChainConfig::testnet();
    let mut chain = HeaderChain::new(config);
    // Build chain: genesis → B2 → B3 → B4 (best chain)
    // ... append using try_append_no_pow ...

    // Build fork branch: fork from genesis, headers F2 → F3 → F4 → F5
    // ... construct headers with parent linkage from genesis ...

    // Fork point is height 1 (genesis). New branch has 4 headers.
    let demoted = chain.try_reorg_deep_no_pow(1, vec![f2, f3, f4, f5]).unwrap();

    assert_eq!(demoted.len(), 3); // B2, B3, B4 demoted
    assert_eq!(chain.height(), 5); // New tip is F5
    assert_eq!(chain.tip().id, f5.id);
    // Genesis still there
    assert_eq!(chain.header_at(1).unwrap().id, genesis.id);
}

#[test]
fn try_reorg_deep_rolls_back_on_validation_failure() {
    let config = ChainConfig::testnet();
    let mut chain = HeaderChain::new(config);
    // Build chain: genesis → B2 → B3
    // ... append ...

    // Fork with invalid header (wrong difficulty) at position 2
    // ... construct f2 valid, f3 with wrong n_bits ...

    let original_tip = chain.tip().id;
    let result = chain.try_reorg_deep_no_pow(1, vec![f2, f3_bad]);
    assert!(result.is_err());
    // Chain unchanged
    assert_eq!(chain.tip().id, original_tip);
    assert_eq!(chain.height(), 3);
}

#[test]
fn try_reorg_deep_rejects_empty_branch() {
    let config = ChainConfig::testnet();
    let mut chain = HeaderChain::new(config);
    // ... build chain with at least 2 headers ...

    let result = chain.try_reorg_deep_no_pow(1, vec![]);
    assert!(result.is_err());
}

#[test]
fn try_reorg_deep_rejects_wrong_parent() {
    let config = ChainConfig::testnet();
    let mut chain = HeaderChain::new(config);
    // ... build chain ...

    // Fork branch whose first header has wrong parent_id
    let result = chain.try_reorg_deep_no_pow(1, vec![wrong_parent_header]);
    assert!(result.is_err());
    // Chain unchanged
}
```

Note: Exact header construction depends on existing test patterns. The implementing agent should follow the patterns in `chain/src/tests.rs` for building test headers with controlled parent IDs and heights.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-chain try_reorg_deep 2>&1 | tail -10`
Expected: FAIL — method doesn't exist.

- [ ] **Step 3: Implement try_reorg_deep**

In `chain/src/chain.rs`:

```rust
/// Attempt a deep chain reorganization.
///
/// Rewinds the chain to `fork_point_height` and applies `new_branch`
/// on top. Returns the IDs of demoted headers on success.
/// On failure, the chain is unchanged.
pub fn try_reorg_deep(
    &mut self,
    fork_point_height: u32,
    new_branch: Vec<Header>,
) -> Result<Vec<BlockId>, ChainError> {
    self.try_reorg_deep_impl(fork_point_height, new_branch, true)
}

#[cfg(test)]
pub(crate) fn try_reorg_deep_no_pow(
    &mut self,
    fork_point_height: u32,
    new_branch: Vec<Header>,
) -> Result<Vec<BlockId>, ChainError> {
    self.try_reorg_deep_impl(fork_point_height, new_branch, false)
}

fn try_reorg_deep_impl(
    &mut self,
    fork_point_height: u32,
    new_branch: Vec<Header>,
    verify_pow: bool,
) -> Result<Vec<BlockId>, ChainError> {
    if new_branch.is_empty() {
        return Err(ChainError::Reorg("new branch is empty".into()));
    }

    if self.by_height.is_empty() {
        return Err(ChainError::Reorg("chain is empty".into()));
    }

    let base = self.by_height[0].height;

    if fork_point_height < base || fork_point_height >= self.height() {
        return Err(ChainError::Reorg(format!(
            "fork point height {} out of range [{}, {})",
            fork_point_height, base, self.height()
        )));
    }

    // Verify first header connects to fork point
    let fork_point = self.header_at(fork_point_height)
        .ok_or_else(|| ChainError::Reorg("fork point not in chain".into()))?;
    if new_branch[0].parent_id != fork_point.id {
        return Err(ChainError::Reorg(format!(
            "first fork header parent {} != fork point {}",
            new_branch[0].parent_id, fork_point.id
        )));
    }

    // Save state for rollback
    let fork_idx = (fork_point_height - base) as usize + 1;
    let saved_headers: Vec<Header> = self.by_height.drain(fork_idx..).collect();
    let saved_scores: Vec<BigUint> = self.scores.drain(fork_idx..).collect();
    let saved_ids: Vec<(BlockId, u32)> = saved_headers.iter()
        .map(|h| (h.id, h.height))
        .collect();

    // Remove demoted IDs from index
    for (id, _) in &saved_ids {
        self.by_id.remove(id);
    }

    // Apply new branch
    for header in &new_branch {
        let result = if verify_pow {
            self.validate_reorg_header(header, true)
        } else {
            self.validate_child_no_pow(header) // reuse existing no-pow validator
        };

        if let Err(e) = result {
            // Rollback: restore saved state
            for (i, h) in saved_headers.into_iter().enumerate() {
                self.by_id.insert(h.id, h.height);
                self.by_height.push(h);
                self.scores.push(saved_scores[i].clone());
            }
            return Err(e);
        }

        let parent_score = self.scores.last().cloned().unwrap_or_default();
        let diff = decode_compact_bits(header.n_bits)
            .to_biguint().unwrap_or_default();
        self.scores.push(parent_score + diff);
        self.by_id.insert(header.id, header.height);
        self.by_height.push(header.clone());
    }

    let demoted: Vec<BlockId> = saved_ids.into_iter().map(|(id, _)| id).collect();
    Ok(demoted)
}
```

- [ ] **Step 4: Run all tests**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p enr-chain 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add chain/src/chain.rs chain/src/tests.rs
git commit -m "feat(chain): implement try_reorg_deep for multi-block reorganization"
```

---

### Task 6: DeliveryEvent::Reorg Variant

**Files:**
- Modify: `sync/src/delivery.rs`

- [ ] **Step 1: Add Reorg variant to DeliveryEvent**

In `sync/src/delivery.rs`, add to the `DeliveryEvent` enum:

```rust
/// A chain reorg occurred. The sync machine should adjust its section
/// queue and full_block_height watermark.
Reorg {
    fork_point: u32,
    old_tip: u32,
    new_tip: u32,
},
```

- [ ] **Step 2: Verify compilation**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo check 2>&1 | tail -20`
Expected: may have non-exhaustive match warnings in `sync/src/state.rs` if the sync machine matches on `DeliveryEvent`. Fix any compilation errors by adding a match arm.

- [ ] **Step 3: Handle Reorg in the sync machine's delivery event handler**

In `sync/src/state.rs`, wherever `DeliveryEvent` variants are matched (look for `DeliveryEvent::Received` and `DeliveryEvent::Evicted`), add:

```rust
DeliveryEvent::Reorg { fork_point, old_tip, new_tip } => {
    tracing::info!(fork_point, old_tip, new_tip, "reorg: adjusting section queue and watermark");

    // 1. Remove section queue entries for demoted heights
    self.section_queue.retain(|(_, _)| {
        // We can't easily check height from modifier ID alone.
        // Instead, clear the entire queue and re-queue from fork_point+1.
        false
    });

    // 2. Reset sections_queued_to to fork point
    self.sections_queued_to = fork_point;

    // 3. Reset full_block_height if it was above fork point
    if self.full_block_height > fork_point {
        tracing::info!(
            old = self.full_block_height,
            new = fork_point,
            "resetting full_block_height to fork point"
        );
        self.full_block_height = fork_point;
    }

    // 4. Re-queue sections for new best chain from fork_point+1 to new_tip
    self.queue_sections_for_range(fork_point + 1, new_tip).await;

    // 5. Re-scan watermark
    self.advance_full_block_height().await;
}
```

- [ ] **Step 4: Run all tests**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add sync/src/delivery.rs sync/src/state.rs
git commit -m "feat(sync): handle DeliveryEvent::Reorg — reset section queue and watermark"
```

---

### Task 7: Pipeline — Fork Detection and Reorg Triggering

**Files:**
- Modify: `src/pipeline.rs`

This is the big one. The pipeline needs to detect forks, compute scores, and trigger reorgs via the store and chain.

- [ ] **Step 1: Update pipeline to handle AppendResult::Forked**

In `src/pipeline.rs`, the existing `process_batch` method has a `match chain.try_append(header.clone())` block. Update it:

Where the current code does:
```rust
match chain.try_append(header.clone()) {
    Ok(()) => { ... }
    Err(ChainError::ParentNotFound { .. }) => { ... }
    ...
}
```

Change to:
```rust
match chain.try_append(header.clone()) {
    Ok(AppendResult::Extended) => {
        // Existing chaining logic, unchanged.
        // Additionally: compute and store cumulative score.
        chained += 1;
        let score = chain.cumulative_score().to_bytes_be();
        // Store via put_header with fork=0
        store_entries.push(StoreEntry::Header {
            id: header_id.0.0,
            height: header_height,
            fork: 0,
            score,
            data: raw,
        });
        // ... rest of existing logic (buffer drain, etc.) ...
    }
    Ok(AppendResult::Forked { fork_height }) => {
        // Header forks from the best chain at fork_height.
        // Store it, compute score, check if fork is better.
        self.handle_fork_header(
            &chain, header, raw, fork_height,
            &mut store_entries, &mut chained,
        ).await;
    }
    Err(ChainError::ParentNotFound { .. }) => {
        // Existing buffer logic, unchanged.
        // But also check if parent is a known fork header in the store.
        // ... existing code ...
    }
    // ... rest unchanged ...
}
```

- [ ] **Step 2: Implement handle_fork_header**

Add a new method to `ValidationPipeline`:

```rust
/// Handle a header that forks from the best chain.
/// Stores it, computes its cumulative score, and triggers reorg if the
/// fork has higher cumulative difficulty.
async fn handle_fork_header(
    &mut self,
    chain: &tokio::sync::MutexGuard<'_, HeaderChain>,
    header: Header,
    raw: Vec<u8>,
    fork_height: u32,
    store_entries: &mut Vec<StoreEntry>,
    chained: &mut u32,
) {
    let header_id = header.id;
    let header_height = header.height;

    // Compute cumulative score for this fork header
    let parent_score = chain.score_at(fork_height)
        .cloned()
        .unwrap_or_default();
    let difficulty = decode_compact_bits(header.n_bits)
        .to_biguint()
        .unwrap_or_default();
    let fork_score = parent_score + difficulty;

    // Determine fork number at this height
    let fork_num = match self.store.header_ids_at_height(header_height) {
        Ok(forks) => forks.last().map(|(_, f)| f + 1).unwrap_or(1),
        Err(_) => 1,
    };

    // Store the fork header
    let score_bytes = fork_score.to_bytes_be();
    if let Err(e) = self.store.put_header(
        &header_id.0.0, header_height, fork_num,
        &score_bytes, &raw,
    ) {
        tracing::error!(height = header_height, "store fork header failed: {e}");
        return;
    }

    // Compare against best chain score
    let best_score = chain.cumulative_score();
    if fork_score <= best_score {
        tracing::debug!(
            height = header_height,
            fork_height,
            "fork stored but not better (fork_score <= best_score)"
        );
        return;
    }

    tracing::info!(
        height = header_height,
        fork_height,
        "fork has higher score — triggering reorg"
    );

    // Assemble fork branch from store: walk from fork tip back to fork point
    // For a single-header fork (fork_height + 1 == header_height), branch is just [header]
    // For deeper forks, we'd need to read intermediate headers from store.
    // For now, this handles the common case where the fork header's parent
    // is on the best chain (single-height fork extension).
    let new_branch = vec![header.clone()];

    // Note: for multi-header forks where intermediate headers are also fork
    // headers (stored earlier), we need to walk parent_id through the store.
    // This is a TODO for when we see deeper forks in practice — the single-
    // header case covers the immediate testnet problem.

    // Drop chain lock temporarily — try_reorg_deep needs exclusive access
    // Actually we already hold it. Proceed with the reorg.
    // (The caller holds the MutexGuard — we can't call try_reorg_deep through
    // the guard. We need to restructure so reorg happens after the batch loop.)
}
```

**Important architectural note:** The current `process_batch` holds the chain lock for the entire batch. Reorg needs mutable access to the chain. The reorg should be triggered AFTER the batch processing loop releases the lock, or within the same lock scope. The implementing agent should structure this so:

1. Batch loop detects fork-is-better, records `(fork_height, new_branch)`.
2. After the batch loop (still holding the lock), call `chain.try_reorg_deep()`.
3. On success, update the store via `switch_best_chain()`.
4. Send `DeliveryEvent::Reorg` notification.

- [ ] **Step 3: Restructure process_batch for reorg support**

The key change: after the main header processing loop, check if a pending reorg was detected. If so, execute it while still holding the chain lock:

```rust
// After the for loop over valid_headers:

// Execute pending reorg if detected
if let Some((fork_point, new_branch, fork_score_bytes)) = pending_reorg {
    match chain.try_reorg_deep(fork_point, new_branch.clone()) {
        Ok(demoted_ids) => {
            let new_tip = chain.height();
            tracing::info!(
                fork_point,
                demoted = demoted_ids.len(),
                new_tip,
                "deep reorg succeeded"
            );

            // Update store: switch best chain
            let demote_heights: Vec<u32> = (fork_point + 1..=height_before).collect();
            let promote: Vec<(u32, [u8; 32])> = new_branch.iter()
                .map(|h| (h.height, h.id.0.0))
                .collect();
            if let Err(e) = self.store.switch_best_chain(&demote_heights, &promote) {
                tracing::error!("store switch_best_chain failed: {e}");
            }

            // Store the new branch headers
            for h in &new_branch {
                // Already stored as fork headers in handle_fork_header
            }

            // Notify sync machine
            if self.delivery_tx.try_send(DeliveryEvent::Reorg {
                fork_point,
                old_tip: height_before,
                new_tip,
            }).is_err() {
                tracing::warn!("delivery channel full, dropped Reorg notification");
            }

            // Update height tracking
            chained += new_branch.len() as u32;
        }
        Err(e) => {
            tracing::warn!(fork_point, "deep reorg failed: {e}");
        }
    }
}
```

- [ ] **Step 4: Remove old 1-deep reorg code**

The old 1-deep reorg logic in `process_batch` (the `try_reorg` call path checking `chain.tip().parent_id` in the buffer) is now superseded. Remove it — or keep it as a fast path that feeds into the same `pending_reorg` mechanism.

Given that `try_reorg_deep` with `fork_point = tip.height - 1` handles the 1-deep case, removing the old code simplifies the pipeline significantly.

- [ ] **Step 5: Run all tests**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/pipeline.rs
git commit -m "feat(pipeline): detect forks via score comparison, trigger deep reorg"
```

---

### Task 8: Main — Updated Chain Restore

**Files:**
- Modify: `src/main.rs`

The chain restore loop in `main.rs` currently uses `store.get_id_at(HEADER_TYPE_ID, height)` which reads from `HEIGHT_INDEX`. After migration, headers are in `BEST_CHAIN`. Update the restore to use the new store API.

- [ ] **Step 1: Update chain restore loop**

Replace the restore section (lines ~97-128) with:

```rust
// Restore chain from stored headers
if let Some((tip_height, _)) = store.best_header_tip()? {
    let mut loaded = 0u32;
    for height in 1..=tip_height {
        let id = match store.best_header_at(height)? {
            Some(id) => id,
            None => {
                tracing::warn!(height, "gap in best chain, stopping load");
                break;
            }
        };
        let data = match store.get(HEADER_TYPE_ID, &id)? {
            Some(d) => d,
            None => {
                tracing::warn!(height, "stored header ID but no data, stopping load");
                break;
            }
        };
        let header = match enr_chain::parse_header(&data) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(height, "stored header parse failed: {e}, stopping load");
                break;
            }
        };
        match chain.try_append(header) {
            Ok(enr_chain::AppendResult::Extended) => {}
            Ok(enr_chain::AppendResult::Forked { .. }) => {
                tracing::error!(height, "stored best-chain header forks — store corrupted?");
                break;
            }
            Err(e) => {
                tracing::error!(height, "stored header chain failed: {e}, stopping load");
                break;
            }
        }
        loaded += 1;
    }
    tracing::info!(loaded, tip = chain.height(), "restored header chain from store");
}
```

- [ ] **Step 2: Verify compilation and run**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo build 2>&1 | tail -20`
Expected: compiles cleanly.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(main): restore chain from BEST_CHAIN table instead of HEIGHT_INDEX"
```

---

### Task 9: Integration Test — End-to-End Reorg

**Files:**
- Modify: `src/pipeline.rs` (test section)

- [ ] **Step 1: Write integration test**

Add to the `#[cfg(test)] mod tests` block in `src/pipeline.rs`:

```rust
/// Verify that when a fork header with higher cumulative score arrives,
/// the pipeline triggers a deep reorg and the chain switches.
#[tokio::test]
async fn pipeline_triggers_reorg_on_better_fork() {
    // This test requires constructing real headers with valid PoW,
    // which is impractical for a unit test. Instead, test the
    // score comparison logic and chain.try_reorg_deep path
    // using the no-pow chain variant.
    //
    // The implementing agent should design this test based on the
    // actual header construction patterns available in the test suite.
    //
    // Key assertions:
    // 1. Build a pipeline with a 3-header chain
    // 2. Feed a fork header whose parent is at height 1
    // 3. Fork header has higher difficulty (crafted n_bits)
    // 4. After process_batch, chain.height() reflects the reorg
    // 5. DeliveryEvent::Reorg was sent
}
```

Note: The exact test construction depends heavily on whether we can craft headers with controlled PoW solutions. If not, this becomes a manual integration test on testnet.

- [ ] **Step 2: Run all tests**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/pipeline.rs
git commit -m "test: add integration test skeleton for deep reorg pipeline"
```

---

### Task 10: Verification — Build, Test, Deploy

**Files:** None — verification only.

- [ ] **Step 1: Full build**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo build 2>&1 | tail -20`
Expected: clean compilation.

- [ ] **Step 2: Full test suite**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 3: Verify store migration path**

If a test store exists with old-format data, verify the migration runs correctly.
Otherwise, trust the Task 2 migration test.

- [ ] **Step 4: Deploy to test server (manual)**

Build release binary, deploy to 216.128.144.28, restart with existing store.
The migration should run on first start, converting HEIGHT_INDEX headers to the
new fork-aware tables. The node should resume syncing and handle the stuck fork
at height 264,601.

---

## Deferred (not in this plan)

- **Two-mode section download** (catch-up vs near-tip fork section pre-fetching).
  The spec describes this in `facts/reorg.md` section 4. It's an optimization
  that makes full-block reorg zero-network-traffic. Not required for the immediate
  problem (header-level reorg to unstick the node). Add in a follow-up once the
  core reorg works.

## Task Dependency Order

```
Task 1 (store tables) → Task 2 (migration) → Task 8 (main restore)
Task 3 (scores) → Task 4 (AppendResult) → Task 5 (deep reorg) → Task 7 (pipeline)
Task 6 (DeliveryEvent::Reorg) → Task 7 (pipeline)
Task 7 → Task 9 (integration test) → Task 10 (verification)
```

Tasks 1-2 and Tasks 3-6 can be worked in parallel since they touch different crates.
