use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_validation::{validate_single_transaction, ErgoStateContext};

use crate::family::propagate_family_weight;
use crate::types::*;
use crate::weight::TxWeight;

/// The transaction fee: the summed value of every output guarded by the fee
/// proposition.
///
/// **Not `input_sum - output_sum`.** Ergo has no implicit remainder that
/// becomes the fee — ergo-lib enforces exact ERG preservation
/// (`ErgPreservationError` when `input_sum != output_sum`,
/// `wallet/tx_context.rs:122`), so a difference-based fee is structurally zero
/// for every transaction that survives validation, and the minimum-fee check
/// then declines all of them. The fee is an explicit output instead.
///
/// Mirrors `ErgoMemPool.extractFee` (`ErgoMemPool.scala:304-309`), which
/// filters outputs on `chainSettings.monetary.feeProposition`.
///
/// Compared by `ErgoTree` value rather than by serialized bytes: that is the
/// JVM's own structural `==`, sigma-rust's `PartialEq` deliberately ignores
/// `ParsedErgoTree`'s memoisation field, and it avoids serializing every
/// output's script on a per-transaction path. An output whose script failed to
/// parse is an `ErgoTree::Unparsed` and correctly matches nothing.
pub fn extract_fee(tx: &Transaction, fee_proposition: &ErgoTree) -> u64 {
    tx.outputs
        .iter()
        .filter(|out| &out.ergo_tree == fee_proposition)
        .map(|out| *out.value.as_u64())
        .sum()
}

/// Extract transaction ID as [u8; 32].
pub fn tx_id_bytes(tx: &Transaction) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(tx.id().as_ref());
    arr
}

/// Extract input box IDs as Vec<[u8; 32]>.
pub fn input_box_ids(tx: &Transaction) -> Vec<[u8; 32]> {
    tx.inputs
        .iter()
        .map(|i| {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(i.box_id.as_ref());
            arr
        })
        .collect()
}

/// Extract a raw box_id reference as [u8; 32].
pub fn input_box_id_raw(box_id: &ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(box_id.as_ref());
    arr
}

/// The first output whose creation height sits above the context's preheader,
/// or `None` if every output is at or below it.
///
/// This is the *transient* half of ergo-lib's `InvalidHeightError`: the sender
/// built the transaction against a tip one block ahead of ours, and applying
/// the next block makes it valid. Callers must decline such a transaction, not
/// invalidate it — see the contract's step 6a.
///
/// The comparison is signed, mirroring `verify_output()` in ergo-lib's
/// `wallet/tx_context.rs` byte for byte. A creation height with bit 31 set is
/// "negative" under the V1 rules ergo-lib still honours, so it does *not* trip
/// that check; it trips `NegativeHeight` instead, which is permanent. Widening
/// this to an unsigned comparison would swallow those into the transient path
/// and re-validate the same garbage on every rebroadcast, forever.
pub fn output_above_preheader(tx: &Transaction, state_context: &ErgoStateContext) -> Option<u32> {
    let preheader_height = state_context.pre_header.height as i32;
    tx.outputs
        .iter()
        .map(|out| out.creation_height)
        .find(|h| *h as i32 > preheader_height)
}

/// Extract output box IDs and boxes as Vec<([u8; 32], ErgoBox)>.
pub fn output_boxes(tx: &Transaction) -> Vec<([u8; 32], ErgoBox)> {
    tx.outputs
        .iter()
        .map(|b| {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(b.box_id().as_ref());
            (arr, b.clone())
        })
        .collect()
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

        // 4. Bound input count to prevent expensive lookups
        const MAX_TX_INPUTS: usize = 5_000;
        if tx.inputs.len() > MAX_TX_INPUTS {
            return ProcessingOutcome::Declined {
                reason: format!("too many inputs: {} (max {MAX_TX_INPUTS})", tx.inputs.len()),
            };
        }

        // 5. Resolve input boxes
        let input_ids = input_box_ids(&tx);
        let mut input_boxes = Vec::with_capacity(input_ids.len());
        for id in &input_ids {
            match utxo_reader
                .box_by_id(id)
                .or_else(|| self.pool.unconfirmed_box(id).cloned())
            {
                Some(b) => input_boxes.push(b),
                None => {
                    return ProcessingOutcome::Declined {
                        reason: format!("input box {} not found", hex::encode(id)),
                    }
                }
            }
        }

        // 5. Resolve data-input boxes
        let data_boxes: Vec<ErgoBox> = tx
            .data_inputs
            .as_ref()
            .map(|dis| {
                dis.iter()
                    .filter_map(|di| {
                        let id = input_box_id_raw(&di.box_id);
                        utxo_reader
                            .box_by_id(&id)
                            .or_else(|| self.pool.unconfirmed_box(&id).cloned())
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 6a. Transient creation-height guard. A transaction built one block
        // ahead of us is not invalid, it is early — the next applied block
        // makes it valid. Letting it reach step 6 would cache it as invalid
        // for `invalidation_ttl`, so every rebroadcast in the next half hour
        // is dropped at step 1 without ever being re-validated. Same reasoning
        // as the missing-input decline above.
        if let Some(height) = output_above_preheader(&tx, state_context) {
            return ProcessingOutcome::Declined {
                reason: format!(
                    "creation height {height} above preheader height {}",
                    state_context.pre_header.height
                ),
            };
        }

        // 6. Validate (returns script evaluation cost in block cost units)
        let cost = match validate_single_transaction(
            &tx,
            input_boxes.clone(),
            data_boxes,
            state_context,
        ) {
            Ok(script_cost) => script_cost.max(tx_bytes.len() as u64) as u32,
            Err(e) => {
                self.invalidated.insert(tx_id);
                return ProcessingOutcome::Invalidated {
                    reason: format!("{e}"),
                };
            }
        };

        // Track validation cost for rate limiting
        if let Some(peer) = source {
            self.interblock_cost += cost as u64;
            *self.per_peer_cost.entry(peer).or_insert(0) += cost as u64;
        }

        // 7a. Compute fee from the outputs paying the fee proposition.
        let fee = extract_fee(&tx, &self.fee_proposition);

        // 7b. Check minimum fee
        if fee < self.config.min_fee {
            return ProcessingOutcome::Declined {
                reason: format!("fee {fee} below minimum {}", self.config.min_fee),
            };
        }

        // 8. Compute weight
        let weight = TxWeight::new(tx_id, fee, tx_bytes.len(), cost, self.config.fee_strategy);

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
            let total_conflict_weight: u64 = conflicts
                .iter()
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
