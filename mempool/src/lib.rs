pub mod cleanup;
pub mod expiring_cache;
pub mod family;
pub mod pool;
pub mod process;
pub mod reader;
pub mod stats;
pub mod types;
pub mod weight;

// The unit tests share the integration tests' fixtures, which name this
// crate `ergo_mempool` as a dependent would.
#[cfg(test)]
extern crate self as ergo_mempool;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;

use std::collections::HashMap;
use std::time::Instant;

use ergo_lib::chain::ergo_tree_predef;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use expiring_cache::ExpiringCache;
use pool::OrderedPool;
use reader::MempoolReader;
use stats::{FeeHistogramBin, FeeStats};
use types::{MempoolConfig, UnconfirmedTx};
use weight::TxWeight;

/// Largest serialized transaction the node accepts from a peer or the API.
/// The JVM's `maxTransactionSize` default (`application.conf`).
pub const MAX_TRANSACTION_SIZE: usize = 98_304;

/// Validation cost one revalidation pass may spend: the JVM's
/// `CleanupWorker.CostLimit` (v6.0.6 :27). Separate from `cost_per_block`,
/// which limits what remote peers' transactions may cost between blocks.
pub const CLEANUP_COST_LIMIT: u64 = 7_000_000;

pub struct Mempool {
    pool: OrderedPool,
    invalidated: ExpiringCache<[u8; 32]>,
    stats: FeeStats,
    config: MempoolConfig,
    /// Fee output guard tree, derived once from `config.reward_delay`.
    fee_proposition: ErgoTree,
    /// Validation cost since last block (rate limiting).
    interblock_cost: u64,
    /// Per-peer validation cost since last block.
    per_peer_cost: HashMap<u64, u64>,
}

impl Mempool {
    /// # Panics
    ///
    /// Panics on startup if the fee proposition cannot be built — see
    /// `facts/mempool.md` § fee proposition.
    pub fn new(config: MempoolConfig) -> Self {
        let capacity = config.capacity;
        let fee_proposition = ergo_tree_predef::fee_proposition(config.reward_delay)
            .unwrap_or_else(|e| {
                panic!(
                    "fee proposition for reward_delay {}: {e}",
                    config.reward_delay
                )
            });
        Self {
            pool: OrderedPool::new(capacity),
            invalidated: ExpiringCache::new(config.invalidation_ttl, config.invalidation_capacity),
            stats: FeeStats::new(Instant::now()),
            config,
            fee_proposition,
            interblock_cost: 0,
            per_peer_cost: HashMap::new(),
        }
    }

    // --- Query methods ---

    pub fn get(&self, tx_id: &[u8; 32]) -> Option<&UnconfirmedTx> {
        self.pool.get(tx_id)
    }
    pub fn contains(&self, tx_id: &[u8; 32]) -> bool {
        self.pool.contains(tx_id) || self.invalidated.contains(tx_id)
    }
    pub fn is_invalidated(&self, tx_id: &[u8; 32]) -> bool {
        self.invalidated.contains(tx_id)
    }
    pub fn len(&self) -> usize {
        self.pool.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }
    pub fn top(&self, limit: usize) -> Vec<&UnconfirmedTx> {
        self.pool.top(limit)
    }
    pub fn all_prioritized(&self) -> Vec<&UnconfirmedTx> {
        self.pool.all_prioritized()
    }
    pub fn tx_ids(&self) -> Vec<[u8; 32]> {
        self.pool.tx_ids()
    }

    /// A handle for serving pool transactions to peers without this owner's
    /// lock — see `facts/mempool.md` § Serving reader.
    pub fn reader(&self) -> MempoolReader {
        self.pool.reader()
    }

    pub fn unconfirmed_box(
        &self,
        box_id: &[u8; 32],
    ) -> Option<&ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox> {
        self.pool.unconfirmed_box(box_id)
    }

    pub fn spent_inputs(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.pool.spent_inputs()
    }

    // --- Fee queries: facts/mempool.md § Fee queries ---

    /// Fee in nanoERG for a transaction of `tx_size` bytes to be confirmed
    /// within `wait_minutes`: `GET /transactions/getFee`. Never below
    /// `min_fee`. Reads no clock: nothing in the answer depends on the time.
    pub fn recommended_fee(&self, wait_minutes: u32, tx_size: u32) -> u64 {
        let min_fee = self.config.min_fee;
        self.stats
            .fee_for_wait(wait_minutes, tx_size)
            .map_or(min_fee, |fee| fee.max(min_fee))
    }

    /// Expected wait in milliseconds for a transaction of `tx_size` bytes
    /// paying `fee` nanoERG: `GET /transactions/waitTime`.
    /// Precondition: `tx_size > 0`.
    pub fn expected_wait_ms(&self, fee: u64, tx_size: u32) -> u64 {
        self.expected_wait_ms_at(Instant::now(), fee, tx_size)
    }

    /// [`Self::expected_wait_ms`] as of `now`.
    pub fn expected_wait_ms_at(&self, now: Instant, fee: u64, tx_size: u32) -> u64 {
        debug_assert!(tx_size > 0, "expected_wait_ms: tx_size must be positive");
        let fee_per_kb = stats::saturating_u64(u128::from(fee) * 1024 / u128::from(tx_size));
        // Heaviest first, so the transactions ahead are a prefix. Equal
        // weight isn't ahead.
        let position = self
            .pool
            .ordered
            .keys()
            .take_while(|w| w.weight > fee_per_kb)
            .count();
        self.stats.wait_for_position(now, position as u64)
    }

    /// The pool binned by how long each transaction has waited so far:
    /// `GET /transactions/poolHistogram`. Returns `bins + 1` bins, the last
    /// for waits of `max_wait_ms` or longer.
    /// Preconditions: `bins >= 1`, `max_wait_ms >= bins`.
    pub fn pool_histogram(&self, bins: u32, max_wait_ms: u64) -> Vec<FeeHistogramBin> {
        self.pool_histogram_at(Instant::now(), bins, max_wait_ms)
    }

    /// [`Self::pool_histogram`] as of `now`.
    pub fn pool_histogram_at(
        &self,
        now: Instant,
        bins: u32,
        max_wait_ms: u64,
    ) -> Vec<FeeHistogramBin> {
        debug_assert!(bins >= 1, "pool_histogram: bins must be at least 1");
        debug_assert!(
            max_wait_ms >= u64::from(bins),
            "pool_histogram: max_wait_ms must be at least bins"
        );
        let last = u64::from(bins);
        let interval = max_wait_ms / last;
        let mut histogram = vec![FeeHistogramBin::default(); bins as usize + 1];
        for (weight, utx) in &self.pool.ordered {
            let wait =
                stats::saturating_u64(now.saturating_duration_since(utx.created).as_millis());
            // Short of `max_wait_ms`, `wait / interval` can still pass `bins`
            // when `max_wait_ms` isn't a multiple of it. The JVM indexes past
            // its array there; here the transaction lands in the last bin.
            let bin = if wait < max_wait_ms {
                (wait / interval).min(last)
            } else {
                last
            };
            histogram[bin as usize].add(weight.fee_per_factor);
        }
        histogram
    }

    pub fn invalidate(&mut self, tx_id: &[u8; 32]) {
        self.pool.remove(tx_id);
        self.invalidated.insert(*tx_id);
    }

    // --- Block interaction ---

    /// Remove confirmed transactions and their double-spends. Each confirmed
    /// transaction the pool held is recorded in the fee statistics; the
    /// double-spends are not.
    pub fn apply_block(&mut self, confirmed_txs: &[Transaction]) -> Vec<[u8; 32]> {
        self.apply_block_at(Instant::now(), confirmed_txs)
    }

    /// [`Self::apply_block`] as of `now`, the confirmation time the fee
    /// statistics record.
    pub fn apply_block_at(&mut self, now: Instant, confirmed_txs: &[Transaction]) -> Vec<[u8; 32]> {
        let mut removed = Vec::new();

        for tx in confirmed_txs {
            let tx_id = process::tx_id_bytes(tx);

            // A confirmation of a transaction the pool held: the only thing
            // the fee statistics record.
            if let Some((weight, utx)) = self.pool.get_weighted(&tx_id) {
                self.stats
                    .record_confirmation(now, utx.created, weight.fee_per_factor);
            }

            // Remove the confirmed tx
            if self.pool.remove(&tx_id).is_some() {
                removed.push(tx_id);
            }

            // Remove any pool tx that double-spends confirmed inputs. It
            // wasn't confirmed, so it isn't recorded.
            for input in tx.inputs.iter() {
                let input_id = process::input_box_id_raw(&input.box_id);
                if let Some(conflict_weight) = self.pool.spending_tx(&input_id).cloned() {
                    if conflict_weight.tx_id != tx_id
                        && self.pool.remove(&conflict_weight.tx_id).is_some()
                    {
                        removed.push(conflict_weight.tx_id);
                    }
                }
            }
        }

        // Reset rate limiting
        self.interblock_cost = 0;
        self.per_peer_cost.clear();

        // Prune invalidation cache
        self.invalidated.prune();

        removed
    }

    /// Return rolled-back transactions to the pool (no re-validation). Each
    /// goes in as given: callers pass `validation_cost: None`, and the next
    /// revalidation measures it.
    pub fn return_to_pool(&mut self, txs: Vec<UnconfirmedTx>) {
        for utx in txs {
            let tx_id = process::tx_id_bytes(&utx.tx);
            if self.pool.contains(&tx_id) {
                continue;
            }
            let input_ids = process::input_box_ids(&utx.tx);
            let outputs = process::output_boxes(&utx.tx);
            let weight = TxWeight::new(
                tx_id,
                utx.fee,
                utx.tx_bytes.len(),
                utx.cost,
                self.config.fee_strategy,
            );
            self.pool.insert(weight, utx, &input_ids, outputs);
        }
    }

    // process(), revalidate(), select_for_rebroadcast() are in their respective modules.
}
