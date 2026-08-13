//! Fee transaction construction — aggregate fee outputs into a single miner reward box.

use ergo_lib::chain::ergo_box::box_builder::ErgoBoxCandidateBuilder;
use ergo_lib::chain::ergo_tree_predef;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::{Input, Transaction};
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{
    BoxTokens, ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters,
};
use ergo_lib::ergotree_ir::chain::token::{Token, TokenId};
use ergo_lib::ergotree_ir::chain::tx_id::TxId;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
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
/// capped by the reward box's MEASURED serialized size, not by a token count;
/// the excess is burned rather than allowed to make the box unbuildable — or,
/// worse, buildable but invalid — and cost the miner every fee in the block.
/// See [`fit_tokens`].
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

    // Build the miner reward output.
    let reward_script =
        ergo_tree_predef::reward_output_script(reward_delay, miner_pk.clone())
            .map_err(|e| MiningError::Emission(format!("reward script: {e}")))?;
    let reward_value: BoxValue = (total_fee as u64)
        .try_into()
        .map_err(|e| MiningError::Emission(format!("fee value: {e}")))?;

    // Cap at what one box can hold, measured rather than counted — see
    // `fit_tokens`. The dropped tokens are burned: their fee boxes are still
    // spent as inputs and Ergo permits an output carrying fewer tokens than
    // its inputs. The JVM's `take(MaxAssetsPerBox)` burns them too, just at
    // the wrong limit.
    let kept = fit_tokens(&aggregated, reward_value, &reward_script, height)?;
    if kept.len() < aggregated.len() {
        tracing::warn!(
            height,
            distinct_tokens = aggregated.len(),
            collected = kept.len(),
            burned = aggregated.len() - kept.len(),
            "mining: fee boxes carry more distinct tokens than one reward box \
             holds; burning the excess so the block still collects its fees"
        );
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

    let mut reward_builder =
        ErgoBoxCandidateBuilder::new(reward_value, reward_script, height);
    for token in kept {
        reward_builder.add_token(token);
    }

    let reward_candidate = reward_builder
        .build()
        .map_err(|e| MiningError::Emission(format!("fee reward box build: {e}")))?;

    let tx = Transaction::new_from_vec(inputs, vec![], vec![reward_candidate])
        .map_err(|e| MiningError::Emission(format!("fee transaction: {e}")))?;

    Ok(Some(tx))
}

/// The longest PREFIX of `aggregated` that keeps the reward box inside the
/// box-size window — measured, not derived.
///
/// The rule is `txBoxSize`: `out.bytes.length <= MaxBoxSize` (4096), JVM
/// `ErgoTransaction.scala:175`, ergo-lib `BoxSizeExceeded`. It is checked
/// against the bytes a validator counts — `ErgoBox::sigma_serialize`,
/// transaction id and output index included, not the candidate body, which is
/// 33 bytes shorter and would put the cut one token too high.
///
/// ⚠ Nothing here is arithmetic over assumed widths. The reward tree's length
/// is a function of `reward_delay` and the miner key; a token entry is 32
/// bytes plus a VLQ amount, not a fixed 33. A table of constants would have to
/// be re-derived every time either moved, and being wrong by one byte means a
/// box that cannot validate and a block that collects nothing — the exact
/// failure this cap exists to prevent.
///
/// ⚠ A prefix, not a greedy fill. The surviving set stays a plain take in
/// first-seen traversal order, which is what makes two runs over identical fee
/// boxes produce identical box bytes — hence the same box id, transaction id,
/// and transactions root the miner is handed to hash.
///
/// The other rule on an output box — dust,
/// `out.value >= out.bytes.length * minValuePerByte` — cannot bind here and is
/// deliberately not checked. Every fee box collected is an output of a
/// transaction this node already validated, so it cleared that same rule
/// carrying those same tokens under a **105-byte** fee proposition; the reward
/// box carries them under a **54-byte** reward script. Values add across fee
/// boxes while the box overhead is paid once, and a token in two fee boxes
/// becomes one entry here, freeing 32 bytes against at most a byte or two of
/// VLQ growth on the summed amount. The reward box is therefore strictly
/// cheaper per byte than the boxes that fed it. A guard here would be
/// unreachable, and — pinned to a constant rather than the block's voted
/// `minValuePerByte`, which this function is not given — could only ever fire
/// wrongly, burning tokens that would have validated.
fn fit_tokens(
    aggregated: &[(TokenId, u64)],
    value: BoxValue,
    script: &ErgoTree,
    height: u32,
) -> Result<Vec<Token>, MiningError> {
    let mut kept: Vec<Token> = Vec::new();
    // `BoxTokens` is a bounded vec. The size rule stops the loop long before
    // this — 255 minimal tokens are 8415 bytes on their own — but the bound is
    // structural, not a consequence of the loop.
    for (token_id, amount) in aggregated.iter().take(ErgoBox::MAX_TOKENS_COUNT) {
        kept.push(Token {
            token_id: *token_id,
            amount: (*amount)
                .try_into()
                .map_err(|e| MiningError::Emission(format!("fee token amount: {e}")))?,
        });
        if reward_box_size(value, script, &kept, height)? > ErgoBox::MAX_BOX_SIZE {
            kept.pop();
            break;
        }
    }
    Ok(kept)
}

/// Serialized length of the reward box exactly as a validator measures it.
///
/// The placeholder transaction id measures the real box to the byte: an id is
/// a 32-byte digest whatever its value, and the index is 0 either way — the
/// reward box is the fee transaction's only output.
fn reward_box_size(
    value: BoxValue,
    script: &ErgoTree,
    tokens: &[Token],
    height: u32,
) -> Result<usize, MiningError> {
    let candidate = ErgoBoxCandidate {
        value,
        ergo_tree: script.clone(),
        tokens: match tokens {
            [] => None,
            t => Some(
                BoxTokens::from_vec(t.to_vec())
                    .map_err(|e| MiningError::Emission(format!("fee tokens: {e}")))?,
            ),
        },
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: height,
    };
    ErgoBox::from_box_candidate(&candidate, TxId::zero(), 0)
        .and_then(|b| b.sigma_serialize_bytes())
        .map(|bytes| bytes.len())
        .map_err(|e| MiningError::Emission(format!("fee reward box measure: {e}")))
}
