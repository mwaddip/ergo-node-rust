use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_validation::{validate_single_transaction, ErgoStateContext};

use crate::weight::TxWeight;
use crate::types::*;
use crate::family::propagate_family_weight;

/// Extract the transaction fee: input_sum - output_sum.
pub fn extract_fee(tx: &Transaction, input_boxes: &[ErgoBox]) -> u64 {
    let input_sum: u64 = input_boxes.iter().map(|b| *b.value.as_u64()).sum();
    let output_sum: u64 = tx.outputs.iter().map(|b| *b.value.as_u64()).sum();
    input_sum.saturating_sub(output_sum)
}

/// Extract transaction ID as [u8; 32].
pub fn tx_id_bytes(tx: &Transaction) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(tx.id().as_ref());
    arr
}

/// Extract input box IDs as Vec<[u8; 32]>.
pub fn input_box_ids(tx: &Transaction) -> Vec<[u8; 32]> {
    tx.inputs.iter().map(|i| {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(i.box_id.as_ref());
        arr
    }).collect()
}

/// Extract a raw box_id reference as [u8; 32].
pub fn input_box_id_raw(box_id: &ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(box_id.as_ref());
    arr
}

/// Extract output box IDs and boxes as Vec<([u8; 32], ErgoBox)>.
pub fn output_boxes(tx: &Transaction) -> Vec<([u8; 32], ErgoBox)> {
    tx.outputs.iter().map(|b| {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(b.box_id().as_ref());
        (arr, b.clone())
    }).collect()
}

impl super::Mempool {
    /// Validate and add a transaction to the pool.
    pub fn process(
        &mut self,
        tx: Transaction,
        tx_bytes: Vec<u8>,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
        source: Option<u64>,
    ) -> ProcessingOutcome {
        let tx_id = tx_id_bytes(&tx);

        // 1. Check invalidation cache
        if self.invalidated.contains(&tx_id) {
            return ProcessingOutcome::Invalidated {
                reason: "previously invalidated".into(),
            };
        }

        // 2. Check duplicate
        if self.pool.contains(&tx_id) {
            return ProcessingOutcome::AlreadyInPool;
        }

        // 3. Rate limiting for remote txs
        if source.is_some() {
            if self.interblock_cost >= self.config.cost_per_block {
                return ProcessingOutcome::Declined {
                    reason: "interblock cost budget exhausted".into(),
                };
            }
            if let Some(peer) = source {
                if let Some(&peer_cost) = self.per_peer_cost.get(&peer) {
                    if peer_cost >= self.config.cost_per_peer_per_block {
                        return ProcessingOutcome::Declined {
                            reason: "per-peer cost budget exhausted".into(),
                        };
                    }
                }
            }
        }

        // 4. Resolve input boxes
        let input_ids = input_box_ids(&tx);
        let mut input_boxes = Vec::with_capacity(input_ids.len());
        for id in &input_ids {
            match utxo_reader.box_by_id(id)
                .or_else(|| self.pool.unconfirmed_box(id).cloned())
            {
                Some(b) => input_boxes.push(b),
                None => return ProcessingOutcome::Declined {
                    reason: format!("input box {} not found", hex::encode(id)),
                },
            }
        }

        // 5. Resolve data-input boxes
        let data_boxes: Vec<ErgoBox> = tx.data_inputs.as_ref()
            .map(|dis| {
                dis.iter().filter_map(|di| {
                    let id = input_box_id_raw(&di.box_id);
                    utxo_reader.box_by_id(&id)
                        .or_else(|| self.pool.unconfirmed_box(&id).cloned())
                }).collect()
            })
            .unwrap_or_default();

        // 6. Validate
        let cost = tx_bytes.len() as u32;
        match validate_single_transaction(&tx, input_boxes.clone(), data_boxes, state_context) {
            Ok(()) => {}
            Err(e) => {
                self.invalidated.insert(tx_id);
                return ProcessingOutcome::Invalidated {
                    reason: format!("{e}"),
                };
            }
        }

        // Track validation cost for rate limiting
        if let Some(peer) = source {
            self.interblock_cost += cost as u64;
            *self.per_peer_cost.entry(peer).or_insert(0) += cost as u64;
        }

        // 7. Check minimum fee
        let fee = extract_fee(&tx, &input_boxes);
        if fee < self.config.min_fee {
            return ProcessingOutcome::Declined {
                reason: format!("fee {fee} below minimum {}", self.config.min_fee),
            };
        }

        // 8. Compute weight
        let weight = TxWeight::new(
            tx_id, fee, tx_bytes.len(), cost, self.config.fee_strategy,
        );

        // 9. Double-spend resolution
        let mut conflicts: Vec<[u8; 32]> = Vec::new();
        for id in &input_ids {
            if let Some(existing_weight) = self.pool.spending_tx(id) {
                if !conflicts.contains(&existing_weight.tx_id) {
                    conflicts.push(existing_weight.tx_id);
                }
            }
        }

        if !conflicts.is_empty() {
            let total_conflict_weight: u64 = conflicts.iter()
                .filter_map(|id| self.pool.by_id.get(id))
                .map(|w| w.weight)
                .sum();
            let avg_conflict_weight = total_conflict_weight / conflicts.len() as u64;

            if weight.weight <= avg_conflict_weight {
                return ProcessingOutcome::DoubleSpendLoser {
                    winner_ids: conflicts,
                };
            }

            // New tx wins — remove losers
            for id in &conflicts {
                self.pool.remove(id);
            }
        }

        // 10. Check capacity
        if self.pool.is_full() {
            if let Some(lowest) = self.pool.lowest_weight() {
                if weight.weight <= lowest {
                    return ProcessingOutcome::Declined {
                        reason: "pool full, fee too low".into(),
                    };
                }
            }
        }

        // 11. Insert
        let now = std::time::Instant::now();
        let outputs = output_boxes(&tx);
        let utx = UnconfirmedTx {
            tx,
            tx_bytes,
            fee,
            cost,
            created: now,
            last_checked: now,
            source,
        };
        self.pool.insert(weight.clone(), utx, &input_ids, outputs);

        // 12. Family weight propagation
        propagate_family_weight(&mut self.pool, &input_ids, weight.fee_per_factor);

        // 13. Evict if over capacity
        let mut evicted = Vec::new();
        while self.pool.len() > self.config.capacity {
            if let Some(evicted_id) = self.pool.evict_lowest() {
                evicted.push(evicted_id);
            }
        }

        if conflicts.is_empty() {
            ProcessingOutcome::Accepted { tx_id }
        } else {
            let mut removed = conflicts;
            removed.extend(evicted);
            ProcessingOutcome::Replaced { tx_id, removed }
        }
    }
}
