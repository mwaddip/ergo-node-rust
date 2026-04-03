//! Convert transactions to AVL+ tree operations.

use std::collections::{HashMap, HashSet};

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use crate::ValidationError;

/// Summary of a transaction's inputs and outputs for state change computation.
/// Decoupled from ergo-lib's Transaction type to allow isolated testing.
pub struct TxSummary {
    pub input_ids: Vec<[u8; 32]>,
    pub data_input_ids: Vec<[u8; 32]>,
    pub output_entries: Vec<([u8; 32], Vec<u8>)>,
}

/// AVL+ tree operations derived from a block's transactions.
pub struct StateChanges {
    /// Data input lookups (box_id). Applied first.
    pub lookups: Vec<[u8; 32]>,
    /// Input removals (box_id). Applied second.
    pub removals: Vec<[u8; 32]>,
    /// Output insertions (box_id, serialized_box_bytes). Applied third.
    pub insertions: Vec<([u8; 32], Vec<u8>)>,
}

/// Compute AVL+ tree operations from transaction summaries.
///
/// Handles intra-block netting: if tx2 spends an output created by tx1 in
/// the same block, neither a Remove nor Insert hits the tree (net-zero).
/// Rejects intra-block double-spends.
///
/// Operation order matches JVM: lookups, then removals, then insertions.
pub fn compute_state_changes(
    tx_summaries: Vec<TxSummary>,
) -> Result<StateChanges, ValidationError> {
    // Track which output box IDs were created (for intra-block netting)
    let mut created: HashSet<[u8; 32]> = HashSet::new();
    // Track which box IDs were netted out (created + spent within the block)
    let mut netted: HashSet<[u8; 32]> = HashSet::new();
    let mut spent: HashSet<[u8; 32]> = HashSet::new();
    let mut removals: Vec<[u8; 32]> = Vec::new();
    let mut lookups: Vec<[u8; 32]> = Vec::new();
    // Preserve transaction output order for insertions
    let mut all_inserts: Vec<([u8; 32], Vec<u8>)> = Vec::new();

    for summary in &tx_summaries {
        for &input_id in &summary.input_ids {
            if !spent.insert(input_id) {
                return Err(ValidationError::IntraBlockDoubleSpend(
                    hex::encode(input_id),
                ));
            }
            if created.contains(&input_id) {
                // Intra-block: output created by earlier tx, now spent — net-zero
                netted.insert(input_id);
            } else {
                removals.push(input_id);
            }
        }

        for entry in &summary.output_entries {
            created.insert(entry.0);
            all_inserts.push((entry.0, entry.1.clone()));
        }

        for &data_input_id in &summary.data_input_ids {
            lookups.push(data_input_id);
        }
    }

    // Filter out netted inserts, then sort both removals and inserts by box ID.
    // The JVM uses TreeMap (sorted by hex-encoded ModifierId = lexicographic byte order).
    // Lookups preserve transaction order (data inputs don't modify the tree).
    removals.sort();

    let mut insertions: Vec<([u8; 32], Vec<u8>)> = all_inserts
        .into_iter()
        .filter(|(id, _)| !netted.contains(id))
        .collect();
    insertions.sort_by_key(|(id, _)| *id);

    Ok(StateChanges {
        lookups,
        removals,
        insertions,
    })
}

fn box_id_to_bytes(box_id: &ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> [u8; 32] {
    let slice = box_id.as_ref();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(slice);
    arr
}

/// Convert ergo-lib Transactions into TxSummaries for state change computation.
pub fn transactions_to_summaries(
    transactions: &[Transaction],
) -> Result<Vec<TxSummary>, ValidationError> {
    let mut summaries = Vec::with_capacity(transactions.len());

    for tx in transactions {
        let input_ids: Vec<[u8; 32]> = tx
            .inputs
            .iter()
            .map(|input| box_id_to_bytes(&input.box_id))
            .collect();

        let data_input_ids: Vec<[u8; 32]> = tx
            .data_inputs
            .as_ref()
            .map(|dis| dis.iter().map(|di| box_id_to_bytes(&di.box_id)).collect())
            .unwrap_or_default();

        let output_entries: Vec<([u8; 32], Vec<u8>)> = tx
            .outputs
            .iter()
            .map(|output| {
                let id = box_id_to_bytes(&output.box_id());
                let box_bytes = output.sigma_serialize_bytes().map_err(|e| {
                    ValidationError::SectionParse {
                        section_type: 102,
                        reason: format!("output serialization: {e}"),
                    }
                })?;
                Ok((id, box_bytes))
            })
            .collect::<Result<Vec<_>, ValidationError>>()?;

        summaries.push(TxSummary {
            input_ids,
            data_input_ids,
            output_entries,
        });
    }

    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_changes_no_intra_block_spend() {
        let box_a = [0xAA; 32];
        let box_b = [0xBB; 32];
        let box_c_id = [0xCC; 32];
        let box_d_id = [0xDD; 32];

        let tx_summaries = vec![
            TxSummary {
                input_ids: vec![box_a],
                data_input_ids: vec![],
                output_entries: vec![(box_c_id, vec![1, 2, 3])],
            },
            TxSummary {
                input_ids: vec![box_b],
                data_input_ids: vec![],
                output_entries: vec![(box_d_id, vec![4, 5, 6])],
            },
        ];

        let changes = compute_state_changes(tx_summaries).unwrap();
        assert_eq!(changes.lookups.len(), 0);
        assert_eq!(changes.removals.len(), 2);
        assert_eq!(changes.insertions.len(), 2);
        assert_eq!(changes.removals[0], box_a);
        assert_eq!(changes.removals[1], box_b);
    }

    #[test]
    fn state_changes_intra_block_netting() {
        let box_a = [0xAA; 32];
        let box_x_id = [0xCC; 32];
        let box_d_id = [0xDD; 32];

        let tx_summaries = vec![
            TxSummary {
                input_ids: vec![box_a],
                data_input_ids: vec![],
                output_entries: vec![(box_x_id, vec![1, 2, 3])],
            },
            TxSummary {
                input_ids: vec![box_x_id], // spends tx1's output
                data_input_ids: vec![],
                output_entries: vec![(box_d_id, vec![4, 5, 6])],
            },
        ];

        let changes = compute_state_changes(tx_summaries).unwrap();
        assert_eq!(changes.removals.len(), 1);
        assert_eq!(changes.removals[0], box_a);
        assert_eq!(changes.insertions.len(), 1);
        assert_eq!(changes.insertions[0].0, box_d_id);
    }

    #[test]
    fn state_changes_double_spend_rejected() {
        let box_a = [0xAA; 32];

        let tx_summaries = vec![
            TxSummary {
                input_ids: vec![box_a],
                data_input_ids: vec![],
                output_entries: vec![([0xBB; 32], vec![1, 2, 3])],
            },
            TxSummary {
                input_ids: vec![box_a], // double spend!
                data_input_ids: vec![],
                output_entries: vec![([0xCC; 32], vec![4, 5, 6])],
            },
        ];

        let result = compute_state_changes(tx_summaries);
        assert!(result.is_err());
    }

    #[test]
    fn state_changes_data_inputs_become_lookups() {
        let box_a = [0xAA; 32];
        let data_box = [0xDD; 32];

        let tx_summaries = vec![TxSummary {
            input_ids: vec![box_a],
            data_input_ids: vec![data_box],
            output_entries: vec![([0xBB; 32], vec![1, 2, 3])],
        }];

        let changes = compute_state_changes(tx_summaries).unwrap();
        assert_eq!(changes.lookups.len(), 1);
        assert_eq!(changes.lookups[0], data_box);
        assert_eq!(changes.removals.len(), 1);
        assert_eq!(changes.insertions.len(), 1);
    }
}
