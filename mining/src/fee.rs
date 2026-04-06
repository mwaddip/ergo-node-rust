//! Fee transaction construction — aggregate fee outputs into a single miner reward box.

use ergo_lib::chain::ergo_box::box_builder::ErgoBoxCandidateBuilder;
use ergo_lib::chain::ergo_tree_predef;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::{Input, Transaction};
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::chain::token::Token;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use std::collections::{HashMap, HashSet};

use crate::MiningError;

/// Build the fee collection transaction.
///
/// Scans outputs of `selected_txs` for boxes matching the fee proposition.
/// Aggregates all fee values into a single miner reward box. Fee boxes
/// that are spent by other selected transactions within the same block are
/// excluded (they belong to chained transactions, not the miner).
///
/// Returns None if total fee is zero or no fee boxes exist.
///
/// Reference: JVM `CandidateGenerator.collectFees()`.
pub fn build_fee_tx(
    selected_txs: &[Transaction],
    height: u32,
    reward_delay: i32,
    miner_pk: &ProveDlog,
) -> Result<Option<Transaction>, MiningError> {
    // Get fee proposition ErgoTree bytes for matching
    let fee_tree = ergo_tree_predef::fee_proposition(reward_delay)
        .map_err(|e| MiningError::Emission(format!("fee proposition: {e}")))?;
    let fee_tree_bytes = fee_tree
        .sigma_serialize_bytes()
        .map_err(|e| MiningError::Emission(format!("fee tree serialize: {e}")))?;

    // Collect all input box IDs across selected txs (these outputs are spent, not fees)
    let spent_ids: HashSet<[u8; 32]> = selected_txs
        .iter()
        .flat_map(|tx| tx.inputs.iter())
        .map(|input| {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(input.box_id.as_ref());
            arr
        })
        .collect();

    // Collect fee outputs from selected transactions
    let mut fee_boxes: Vec<&ErgoBox> = Vec::new();
    for tx in selected_txs {
        for output in tx.outputs.iter() {
            let output_tree_bytes = output
                .ergo_tree
                .sigma_serialize_bytes()
                .unwrap_or_default();
            if output_tree_bytes == fee_tree_bytes {
                let mut id = [0u8; 32];
                id.copy_from_slice(output.box_id().as_ref());
                // Only include if not spent by another selected tx
                if !spent_ids.contains(&id) {
                    fee_boxes.push(output);
                }
            }
        }
    }

    if fee_boxes.is_empty() {
        return Ok(None);
    }

    let total_fee: i64 = fee_boxes.iter().map(|b| b.value.as_i64()).sum();
    if total_fee <= 0 {
        return Ok(None);
    }

    // Aggregate tokens from fee boxes (capped at what fits in a single box)
    let mut token_map: HashMap<ergo_lib::ergotree_ir::chain::token::TokenId, u64> = HashMap::new();
    for fee_box in &fee_boxes {
        if let Some(ref tokens) = fee_box.tokens {
            let token_slice: &[Token] = tokens.as_ref();
            for token in token_slice {
                *token_map.entry(token.token_id.clone()).or_insert(0) += u64::from(token.amount);
            }
        }
    }

    // Build inputs from fee boxes (empty proofs — fee proposition allows same-block spending)
    let inputs: Vec<Input> = fee_boxes
        .iter()
        .map(|b| {
            Input::new(
                b.box_id(),
                ProverResult {
                    proof: ProofBytes::Empty,
                    extension: ContextExtension::empty(),
                },
            )
        })
        .collect();

    // Build miner reward output
    let reward_script =
        ergo_tree_predef::reward_output_script(reward_delay, miner_pk.clone())
            .map_err(|e| MiningError::Emission(format!("reward script: {e}")))?;

    let mut reward_builder = ErgoBoxCandidateBuilder::new(
        (total_fee as u64)
            .try_into()
            .map_err(|e| MiningError::Emission(format!("fee value: {e}")))?,
        reward_script,
        height,
    );

    // Add aggregated tokens to reward box
    for (token_id, amount) in token_map {
        reward_builder.add_token(Token {
            token_id,
            amount: amount
                .try_into()
                .map_err(|e| MiningError::Emission(format!("fee token amount: {e}")))?,
        });
    }

    let reward_candidate = reward_builder
        .build()
        .map_err(|e| MiningError::Emission(format!("fee reward box build: {e}")))?;

    let tx = Transaction::new_from_vec(inputs, vec![], vec![reward_candidate])
        .map_err(|e| MiningError::Emission(format!("fee transaction: {e}")))?;

    Ok(Some(tx))
}
