pub mod types;
pub mod weight;
pub mod expiring_cache;
pub mod pool;
pub mod family;
pub mod process;
pub mod cleanup;
pub mod stats;

use std::collections::HashMap;

use ergo_lib::chain::transaction::Transaction;
use pool::OrderedPool;
use expiring_cache::ExpiringCache;
use stats::FeeStats;
use types::{MempoolConfig, UnconfirmedTx};
use weight::TxWeight;

pub struct Mempool {
    pool: OrderedPool,
    invalidated: ExpiringCache<[u8; 32]>,
    stats: FeeStats,
    config: MempoolConfig,
    /// Validation cost since last block (rate limiting).
    interblock_cost: u64,
    /// Per-peer validation cost since last block.
    per_peer_cost: HashMap<u64, u64>,
}

impl Mempool {
    pub fn new(config: MempoolConfig) -> Self {
        let capacity = config.capacity;
        Self {
            pool: OrderedPool::new(capacity),
            invalidated: ExpiringCache::new(config.invalidation_ttl, config.invalidation_capacity),
            stats: FeeStats::new(1000),
            config,
            interblock_cost: 0,
            per_peer_cost: HashMap::new(),
        }
    }

    // --- Query methods ---

    pub fn get(&self, tx_id: &[u8; 32]) -> Option<&UnconfirmedTx> { self.pool.get(tx_id) }
    pub fn contains(&self, tx_id: &[u8; 32]) -> bool { self.pool.contains(tx_id) || self.invalidated.contains(tx_id) }
    pub fn is_invalidated(&self, tx_id: &[u8; 32]) -> bool { self.invalidated.contains(tx_id) }
    pub fn len(&self) -> usize { self.pool.len() }
    pub fn top(&self, limit: usize) -> Vec<&UnconfirmedTx> { self.pool.top(limit) }
    pub fn all_prioritized(&self) -> Vec<&UnconfirmedTx> { self.pool.all_prioritized() }
    pub fn tx_ids(&self) -> Vec<[u8; 32]> { self.pool.tx_ids() }

    pub fn unconfirmed_box(&self, box_id: &[u8; 32]) -> Option<&ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox> {
        self.pool.unconfirmed_box(box_id)
    }

    pub fn spent_inputs(&self) -> impl Iterator<Item = &[u8; 32]> { self.pool.spent_inputs() }

    pub fn fee_histogram(&self, buckets: usize) -> Vec<stats::FeeBucket> { self.stats.histogram(buckets) }

    pub fn expected_wait_time(&self, fee: u64, tx_size: usize) -> Option<u64> {
        let fee_per_factor = fee * 1024 / tx_size.max(1) as u64;
        self.stats.expected_wait(fee_per_factor)
    }

    pub fn recommended_fee(&self, target_wait_ms: u64, tx_size: usize) -> Option<u64> {
        self.stats.recommended_fee(target_wait_ms)
            .map(|fpf| fpf * tx_size.max(1) as u64 / 1024)
    }

    pub fn invalidate(&mut self, tx_id: &[u8; 32]) {
        self.pool.remove(tx_id);
        self.invalidated.insert(*tx_id);
    }

    // --- Block interaction ---

    /// Remove confirmed transactions and their double-spends.
    pub fn apply_block(&mut self, confirmed_txs: &[Transaction]) -> Vec<[u8; 32]> {
        let mut removed = Vec::new();

        for tx in confirmed_txs {
            let tx_id = process::tx_id_bytes(tx);

            // Record stats if tx was in our pool
            if let Some(utx) = self.pool.get(&tx_id) {
                let wait_ms = utx.created.elapsed().as_millis() as u64;
                let fee_per_factor = self.pool.by_id.get(&tx_id)
                    .map(|w| w.fee_per_factor)
                    .unwrap_or(0);
                self.stats.record_confirmation(fee_per_factor, wait_ms);
            }

            // Remove the confirmed tx
            if self.pool.remove(&tx_id).is_some() {
                removed.push(tx_id);
            }

            // Remove any pool tx that double-spends confirmed inputs
            for input in tx.inputs.iter() {
                let input_id = process::input_box_id_raw(&input.box_id);
                if let Some(conflict_weight) = self.pool.spending_tx(&input_id).cloned() {
                    if conflict_weight.tx_id != tx_id {
                        if self.pool.remove(&conflict_weight.tx_id).is_some() {
                            removed.push(conflict_weight.tx_id);
                        }
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

    /// Return rolled-back transactions to the pool (no re-validation).
    pub fn return_to_pool(&mut self, txs: Vec<UnconfirmedTx>) {
        for utx in txs {
            let tx_id = process::tx_id_bytes(&utx.tx);
            if self.pool.contains(&tx_id) {
                continue;
            }
            let input_ids = process::input_box_ids(&utx.tx);
            let outputs = process::output_boxes(&utx.tx);
            let weight = TxWeight::new(
                tx_id, utx.fee, utx.tx_bytes.len(), utx.cost, self.config.fee_strategy,
            );
            self.pool.insert(weight, utx, &input_ids, outputs);
        }
    }

    // process(), revalidate(), select_for_rebroadcast() are in their respective modules.
}
