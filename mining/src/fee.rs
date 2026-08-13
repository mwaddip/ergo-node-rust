//! Fee transaction construction — aggregate fee outputs into a single miner reward box.

use ergo_lib::chain::ergo_box::box_builder::ErgoBoxCandidateBuilder;
use ergo_lib::chain::ergo_tree_predef;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::{Input, Transaction};
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::chain::token::{Token, TokenId};
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use std::collections::{HashMap, HashSet};

use crate::MiningError;

/// Build the fee collection transaction.
///
/// Scans outputs of `block_txs` — every transaction the block carries so far,
/// emission transaction included, matching the JVM's `newTxs` — for boxes
/// matching the fee proposition. Aggregates all fee values into a single
/// miner reward box. Fee boxes that are spent by another transaction within
/// the same block are excluded (they belong to chained transactions, not the
/// miner).
///
/// `height` is the CANDIDATE's height: the fee proposition requires
/// `HEIGHT == creationHeight(OUTPUTS(0))`, so the miner box is created at the
/// height the block will have (JVM `collectRewards`, `nextHeight`).
///
/// Tokens carried by the fee boxes are aggregated onto the miner box and
/// capped at `ErgoBox::MAX_TOKENS_COUNT`; the excess is burned rather than
/// allowed to make the box unbuildable and cost the miner every fee in the
/// block. See the truncation site below.
///
/// Returns None if total fee is zero or no fee boxes exist — a zero-fee block
/// is a normal outcome, not an error.
///
/// Reference: JVM `CandidateGenerator.collectFees()`.
pub fn build_fee_tx(
    block_txs: &[Transaction],
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

    // Every box spent by the block so far — such an output is a chained spend, not a fee
    let spent_ids: HashSet<[u8; 32]> = block_txs
        .iter()
        .flat_map(|tx| tx.inputs.iter())
        .map(|input| {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(input.box_id.as_ref());
            arr
        })
        .collect();

    // Collect fee outputs from the block's transactions
    let mut fee_boxes: Vec<&ErgoBox> = Vec::new();
    for tx in block_txs {
        for output in tx.outputs.iter() {
            let output_tree_bytes = output
                .ergo_tree
                .sigma_serialize_bytes()
                .unwrap_or_default();
            if output_tree_bytes == fee_tree_bytes {
                let mut id = [0u8; 32];
                id.copy_from_slice(output.box_id().as_ref());
                // Only include if not spent by another transaction in the block
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

    // Aggregate the fee boxes' tokens, in first-seen order.
    //
    // Order rather than `HashMap` iteration because it decides two things that
    // must not depend on a hash seed: which tokens survive the cap below, and
    // the reward box's serialized bytes — hence its id, the fee transaction's
    // id, and the transactions root the miner is handed to hash. The traversal
    // matches the JVM's `feeBoxes.toArray.toColl.flatMap(_.additionalTokens)`:
    // fee boxes in block order, tokens in box order.
    let mut aggregated: Vec<(TokenId, u64)> = Vec::new();
    let mut position: HashMap<TokenId, usize> = HashMap::new();
    for fee_box in &fee_boxes {
        let Some(ref tokens) = fee_box.tokens else {
            continue;
        };
        let token_slice: &[Token] = tokens.as_ref();
        for token in token_slice {
            let amount = u64::from(token.amount);
            match position.get(&token.token_id) {
                Some(&at) => aggregated[at].1 = aggregated[at].1.saturating_add(amount),
                None => {
                    position.insert(token.token_id, aggregated.len());
                    aggregated.push((token.token_id, amount));
                }
            }
        }
    }

    // Cap at what one box can hold. Past `MAX_TOKENS_COUNT` the reward box
    // does not build at all (`BoxTokens` is a bounded vec), and an unbuildable
    // fee box means the block ships collecting NO fees — the miner loses the
    // whole block's revenue over the excess tokens, not just the excess.
    //
    // The JVM truncates for the same reason and in the same way:
    // `feeBoxes.toArray.toColl.flatMap(_.additionalTokens).take(MaxAssetsPerBox)`
    // (`CandidateGenerator.scala:811`) — a plain take, no ordering by value.
    //
    // The dropped tokens are burned: their fee boxes are still spent as inputs
    // and Ergo permits an output carrying fewer tokens than its inputs. The
    // JVM's `take` burns them too.
    if aggregated.len() > ErgoBox::MAX_TOKENS_COUNT {
        tracing::warn!(
            height,
            distinct_tokens = aggregated.len(),
            burned = aggregated.len() - ErgoBox::MAX_TOKENS_COUNT,
            "mining: fee boxes carry more distinct tokens than one box holds; \
             burning the excess so the block still collects its fees"
        );
        aggregated.truncate(ErgoBox::MAX_TOKENS_COUNT);
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
    for (token_id, amount) in aggregated {
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
