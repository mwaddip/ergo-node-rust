//! Transaction validation via ErgoScript evaluation.
//!
//! Validates transaction spending proofs (sigma protocols) using ergo-lib's
//! TransactionContext. This runs on top of AD proof verification — the proof
//! guarantees the state root transition, this validates that each input's
//! spending conditions are satisfied.

use std::collections::HashMap;
use std::io::Cursor;

use ergo_lib::chain::ergo_state_context::ErgoStateContext;
use ergo_lib::chain::parameters::Parameters;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::serialization::constant_store::ConstantStore;
use ergo_lib::ergotree_ir::serialization::sigma_byte_reader::SigmaByteReader;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::wallet::tx_context::TransactionContext;
use ergo_chain_types::{Header, PreHeader};

use crate::ValidationError;

/// Deserialize an ErgoBox from raw bytes (as returned by the AVL proof verifier).
pub fn deserialize_box(bytes: &[u8]) -> Result<ErgoBox, ValidationError> {
    let cursor = Cursor::new(bytes);
    let mut reader = SigmaByteReader::new(cursor, ConstantStore::empty());
    ErgoBox::sigma_parse(&mut reader).map_err(|e| ValidationError::TransactionInvalid {
        index: 0,
        reason: format!("box deserialization: {e}"),
    })
}

/// Build the `[Header; 10]` array required by ErgoStateContext.
///
/// `preceding` contains headers in newest-first order (up to 10).
/// Pads with the oldest available if fewer than 10 are provided.
fn build_headers_array(preceding: &[Header]) -> [Header; 10] {
    let mut headers: Vec<Header> = preceding.iter().take(10).cloned().collect();
    let pad = headers.last().unwrap().clone();
    while headers.len() < 10 {
        headers.push(pad.clone());
    }
    headers.try_into().unwrap()
}

/// Build an ErgoStateContext from a header and preceding headers.
///
/// Requires at least one preceding header. Pads to 10 headers by
/// repeating the oldest if fewer are provided.
pub fn build_state_context(
    header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> ErgoStateContext {
    let pre_header = PreHeader::from(header.clone());
    let headers_array = build_headers_array(preceding_headers);
    ErgoStateContext::new(pre_header, headers_array, parameters.clone())
}

/// Validate a single transaction against provided input and data-input boxes.
///
/// Runs full ErgoScript evaluation via ergo-lib's TransactionContext.
pub fn validate_single_transaction(
    tx: &Transaction,
    input_boxes: Vec<ErgoBox>,
    data_boxes: Vec<ErgoBox>,
    state_context: &ErgoStateContext,
) -> Result<(), ValidationError> {
    let tx_context = TransactionContext::new(tx.clone(), input_boxes, data_boxes)
        .map_err(|e| ValidationError::TransactionInvalid {
            index: 0,
            reason: format!("context: {e}"),
        })?;

    tx_context.validate(state_context).map_err(|e| {
        ValidationError::TransactionInvalid {
            index: 0,
            reason: format!("{e}"),
        }
    })?;

    Ok(())
}

/// Validate all transactions in a block using ErgoScript evaluation.
///
/// `proof_boxes`: input/data-input boxes extracted from the AD proof, keyed by box ID.
/// For intra-block spending (tx2 spends tx1's output), the box comes from
/// tx1's outputs — added to the lookup map alongside proof-returned boxes.
pub fn validate_transactions(
    transactions: &[Transaction],
    proof_boxes: &HashMap<[u8; 32], ErgoBox>,
    header: &Header,
    preceding_headers: &[Header],
    parameters: &Parameters,
) -> Result<(), ValidationError> {
    if transactions.is_empty() {
        return Ok(());
    }

    if preceding_headers.is_empty() {
        // Can't build ErgoStateContext without preceding headers.
        // Only happens at height 1 (genesis) which has no standard transactions.
        tracing::warn!(height = header.height, "skipping tx validation: no preceding headers");
        return Ok(());
    }

    let state_context = build_state_context(header, preceding_headers, parameters);

    // Box lookup: proof boxes (from UTXO set) + intra-block outputs
    let mut box_map: HashMap<[u8; 32], ErgoBox> = proof_boxes.clone();
    for tx in transactions {
        for output in tx.outputs.iter() {
            let id = box_id_bytes(&output.box_id());
            box_map.entry(id).or_insert_with(|| output.clone());
        }
    }

    for (tx_idx, tx) in transactions.iter().enumerate() {
        let input_boxes: Vec<ErgoBox> = tx
            .inputs
            .iter()
            .map(|input| {
                let id = box_id_bytes(&input.box_id);
                box_map.get(&id).cloned().ok_or_else(|| {
                    ValidationError::TransactionInvalid {
                        index: tx_idx,
                        reason: format!("input box {} not found", hex::encode(id)),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let data_boxes: Vec<ErgoBox> = tx
            .data_inputs
            .as_ref()
            .map(|dis| {
                dis.iter()
                    .map(|di| {
                        let id = box_id_bytes(&di.box_id);
                        box_map.get(&id).cloned().ok_or_else(|| {
                            ValidationError::TransactionInvalid {
                                index: tx_idx,
                                reason: format!("data input box {} not found", hex::encode(id)),
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        validate_single_transaction(tx, input_boxes, data_boxes, &state_context)
            .map_err(|e| match e {
                ValidationError::TransactionInvalid { reason, .. } => {
                    ValidationError::TransactionInvalid { index: tx_idx, reason }
                }
                other => other,
            })?;
    }

    Ok(())
}

fn box_id_bytes(box_id: &ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> [u8; 32] {
    let slice = box_id.as_ref();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(slice);
    arr
}
