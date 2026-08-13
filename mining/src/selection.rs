//! Transaction selection from mempool for block inclusion.

use std::collections::HashMap;

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_validation::{validate_single_transaction, ErgoStateContext};

/// Select transactions from mempool for block inclusion.
///
/// Takes prioritized candidates (highest-fee-rate first) and validates
/// each against the upcoming block state. Transactions whose inputs can't
/// be resolved or that fail validation are reported as invalid.
///
/// Bounded by both protocol limits from `Parameters`: cumulative serialized
/// size against `max_block_size`, and cumulative ErgoScript evaluation cost
/// against `max_block_cost`. Cost is bounded at exactly
/// `accumulated + tx_cost <= max_block_cost` with **no safety margin** — the
/// JVM's `safeGap` guards its own AOT costing divergence, not ours (see
/// `../facts/mining.md`, Step 3). Exceeding either limit skips that one
/// transaction and the scan continues, so a later cheaper or smaller
/// transaction can still use the remaining budget.
///
/// Returns `(selected_txs, invalid_tx_ids)`. Invalid IDs should be
/// reported to the mempool for cleanup. A transaction skipped for exceeding
/// a limit is *not* invalid — it stays in the mempool for a later block.
pub fn select_transactions(
    candidates: &[(Transaction, usize)],
    emission_outputs: &[ErgoBox],
    state_context: &ErgoStateContext,
    max_block_size: usize,
    max_block_cost: u64,
    utxo_lookup: &dyn Fn(&[u8; 32]) -> Option<ErgoBox>,
) -> (Vec<Transaction>, Vec<[u8; 32]>) {
    let mut selected = Vec::new();
    let mut invalid_ids = Vec::new();
    let mut accumulated_size: usize = 0;
    let mut accumulated_cost: u64 = 0;

    // Track outputs from emission tx and selected txs (available as inputs for later txs)
    let mut available_outputs: HashMap<[u8; 32], ErgoBox> = HashMap::new();
    for output in emission_outputs {
        let mut id = [0u8; 32];
        id.copy_from_slice(output.box_id().as_ref());
        available_outputs.insert(id, output.clone());
    }

    for (tx, tx_size) in candidates {
        // Check size limit
        if accumulated_size + tx_size > max_block_size {
            continue; // Skip, might fit smaller txs later
        }

        // Resolve input boxes
        let mut input_boxes = Vec::new();
        let mut inputs_found = true;
        for input in tx.inputs.iter() {
            let mut id = [0u8; 32];
            id.copy_from_slice(input.box_id.as_ref());
            if let Some(b) = available_outputs.get(&id).cloned().or_else(|| utxo_lookup(&id)) {
                input_boxes.push(b);
            } else {
                inputs_found = false;
                break;
            }
        }
        if !inputs_found {
            continue; // Inputs not available, skip (not necessarily invalid — might appear later)
        }

        // Resolve data input boxes
        let data_boxes: Vec<ErgoBox> = tx
            .data_inputs
            .as_ref()
            .map(|dis| {
                dis.iter()
                    .filter_map(|di| {
                        let mut id = [0u8; 32];
                        id.copy_from_slice(di.box_id.as_ref());
                        available_outputs.get(&id).cloned().or_else(|| utxo_lookup(&id))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Validate against upcoming state
        match validate_single_transaction(tx, input_boxes, data_boxes, state_context) {
            Ok(tx_cost) => {
                // The cost check cannot precede validation — validation is what
                // produces the number. `checked_add` reads an overflowing sum as
                // "does not fit" instead of wrapping into a spurious accept.
                let Some(total_cost) = accumulated_cost
                    .checked_add(tx_cost)
                    .filter(|total| *total <= max_block_cost)
                else {
                    // Over budget, but still a valid transaction: leave it in
                    // the mempool and keep scanning — a cheaper one may fit.
                    continue;
                };

                accumulated_cost = total_cost;
                accumulated_size += tx_size;

                // Track outputs for later txs (intra-block spending)
                for output in tx.outputs.iter() {
                    let mut id = [0u8; 32];
                    id.copy_from_slice(output.box_id().as_ref());
                    available_outputs.insert(id, output.clone());
                }

                selected.push(tx.clone());
            }
            Err(_) => {
                let tx_id = tx.id();
                let mut id = [0u8; 32];
                id.copy_from_slice(tx_id.as_ref());
                invalid_ids.push(id);
            }
        }
    }

    (selected, invalid_ids)
}
