use std::time::Instant;
use crate::pool::OrderedPool;

/// Maximum ancestor depth for family weight propagation (JVM: 500).
const MAX_PARENT_SCAN_DEPTH: usize = 500;
/// Maximum time for family weight propagation (JVM: 500ms).
const MAX_PARENT_SCAN_MS: u64 = 500;

/// Propagate a child transaction's fee weight to its ancestors in the pool.
///
/// For each input of the child that spends an unconfirmed output, add the
/// child's fee_per_factor to the parent's weight. Recurse up to ancestors.
pub fn propagate_family_weight(
    pool: &mut OrderedPool,
    child_input_ids: &[[u8; 32]],
    child_fee_per_factor: u64,
) {
    let start = Instant::now();
    let mut to_visit: Vec<([u8; 32], u64)> = Vec::new();

    // Find direct parents: pool txs whose outputs are spent by this child
    for input_id in child_input_ids {
        if let Some((parent_weight, _)) = pool.by_output.get(input_id) {
            to_visit.push((parent_weight.tx_id, child_fee_per_factor));
        }
    }

    let mut depth = 0;
    while let Some((parent_id, weight_to_add)) = to_visit.pop() {
        if depth >= MAX_PARENT_SCAN_DEPTH {
            break;
        }
        if start.elapsed().as_millis() as u64 >= MAX_PARENT_SCAN_MS {
            break;
        }

        // Get current parent weight
        let current_weight = match pool.by_id.get(&parent_id) {
            Some(w) => w.weight,
            None => continue,
        };

        // Update parent's weight
        pool.update_weight(&parent_id, current_weight + weight_to_add);

        // Find grandparent: check if parent's inputs spend unconfirmed outputs
        if let Some(parent_utx) = pool.get(&parent_id) {
            for input in parent_utx.tx.inputs.iter() {
                let input_id = input_box_id(&input.box_id);
                if let Some((gp_weight, _)) = pool.by_output.get(&input_id) {
                    to_visit.push((gp_weight.tx_id, weight_to_add));
                }
            }
        }

        depth += 1;
    }
}

fn input_box_id(box_id: &ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> [u8; 32] {
    let mut arr = [0u8; 32];
    arr.copy_from_slice(box_id.as_ref());
    arr
}
