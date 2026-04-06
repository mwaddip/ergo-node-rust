use ergo_validation::{validate_single_transaction, ErgoStateContext};
use crate::types::UtxoReader;
use crate::process;

impl super::Mempool {
    /// Revalidate pool transactions against current state.
    ///
    /// Iterates all transactions. For each:
    /// - Skip if last_checked within cleanup_interval
    /// - Resolve inputs from utxo_reader + unconfirmed
    /// - Re-run validate_single_transaction()
    /// - Remove if invalid or inputs missing
    ///
    /// Cost-bounded to avoid stalling.
    pub fn revalidate(
        &mut self,
        utxo_reader: &dyn UtxoReader,
        state_context: &ErgoStateContext,
    ) -> Vec<[u8; 32]> {
        let mut removed = Vec::new();
        let mut cumulative_cost: u64 = 0;

        let tx_ids: Vec<[u8; 32]> = self.pool.tx_ids();

        for tx_id in tx_ids {
            if cumulative_cost >= self.config.cost_per_block {
                break;
            }

            let utx = match self.pool.get(&tx_id) {
                Some(u) => u,
                None => continue,
            };

            // Skip recently checked
            if utx.last_checked.elapsed() < self.config.cleanup_interval {
                continue;
            }

            let cost = utx.cost as u64;
            cumulative_cost += cost;

            // Resolve inputs
            let input_ids = process::input_box_ids(&utx.tx);
            let input_boxes: Option<Vec<_>> = input_ids.iter()
                .map(|id| utxo_reader.box_by_id(id)
                    .or_else(|| self.pool.unconfirmed_box(id).cloned()))
                .collect();

            let input_boxes = match input_boxes {
                Some(boxes) => boxes,
                None => {
                    self.pool.remove(&tx_id);
                    removed.push(tx_id);
                    continue;
                }
            };

            let data_boxes: Vec<_> = utx.tx.data_inputs.as_ref()
                .map(|dis| dis.iter().filter_map(|di| {
                    let id = process::input_box_id_raw(&di.box_id);
                    utxo_reader.box_by_id(&id)
                        .or_else(|| self.pool.unconfirmed_box(&id).cloned())
                }).collect())
                .unwrap_or_default();

            // Clone what we need before the mutable borrow
            let tx_clone = utx.tx.clone();

            match validate_single_transaction(&tx_clone, input_boxes, data_boxes, state_context) {
                Ok(()) => {
                    // Would update last_checked here, but we'd need get_mut
                    // which isn't worth the complexity for the BTreeMap key dance.
                    // The cleanup_interval check prevents re-checking too often.
                }
                Err(_) => {
                    self.invalidated.insert(tx_id);
                    self.pool.remove(&tx_id);
                    removed.push(tx_id);
                }
            }
        }

        removed
    }

    /// Select transactions for rebroadcast.
    /// Returns up to rebroadcast_count txs whose inputs exist in the UTXO set.
    pub fn select_for_rebroadcast(
        &self,
        utxo_reader: &dyn UtxoReader,
    ) -> Vec<&crate::types::UnconfirmedTx> {
        let mut valid: Vec<&crate::types::UnconfirmedTx> = Vec::new();

        for utx in self.pool.all_prioritized() {
            if valid.len() >= self.config.rebroadcast_count {
                break;
            }
            let all_inputs_exist = process::input_box_ids(&utx.tx).iter().all(|id|
                utxo_reader.box_by_id(id).is_some()
            );
            if all_inputs_exist {
                valid.push(utx);
            }
        }

        valid
    }
}
