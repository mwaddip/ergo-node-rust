//! Per-input ErgoScript evaluation cost extraction.
//!
//! Exposes a single helper, [`oracle_cost`], that runs `reduce_to_crypto`
//! for one input of a transaction and returns the raw JitCost the
//! interpreter accumulated. Used by the api crate's
//! `/blocks/{headerId}/validation-fragments` endpoint to populate per-input
//! `oracleCost` / `oracleSucceeded` / `oracleError` fields without re-running
//! the full block validation pipeline.

use ergo_lib::chain::ergo_state_context::ErgoStateContext;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_interpreter::eval::{reduce_to_crypto, EvalError};
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::wallet::signing::make_context;
use ergo_lib::wallet::tx_context::TransactionContext;

/// Setup-level failure that prevented `oracle_cost` from running an
/// evaluation at all. A script that runs but errors mid-evaluation does
/// NOT surface here — it appears as `Ok((cost, Err(EvalError)))`.
#[derive(Debug, thiserror::Error)]
pub enum OracleCostError {
    /// `input_index` is past the end of `tx.inputs`. Surfaced rather than
    /// panicking so the helper is safe to call on HTTP-supplied indices.
    #[error("input index {index} out of range (tx has {input_count} inputs)")]
    InputIndexOutOfRange {
        /// The requested index.
        index: usize,
        /// The number of inputs the transaction actually has.
        input_count: usize,
    },

    /// `spent_boxes` or `data_input_boxes` did not contain every box
    /// referenced by the transaction, or some other `TransactionContext`
    /// construction failure.
    #[error("transaction context: {0}")]
    TransactionContext(String),
}

/// Run `reduce_to_crypto` for a single input and return the raw JitCost
/// accumulated plus the per-input evaluation result.
///
/// The returned cost is read from `ctx.jit_cost_value()` AFTER
/// `reduce_to_crypto` returns and is read whether the inner `Result` is
/// `Ok` or `Err` — sigma-rust preserves the accumulated cost through
/// error returns, so callers get a meaningful number on both paths.
///
/// Returns RAW JitCost (the `u64` the interpreter accumulates internally),
/// NOT block cost. Block cost is `jit_cost / 10`; substituting
/// `ReductionResult.cost` would be off-by-10 on every input. The
/// `validation-fragments` endpoint compares this directly against an
/// external harness's raw JitCost.
///
/// Panic-free on adversarial input: out-of-range `input_index` and
/// mismatched box collections surface as [`OracleCostError`], not panics.
/// The helper is safe to call from an HTTP handler on user-controlled
/// `(header_id, input_index)` tuples.
pub fn oracle_cost(
    tx: &Transaction,
    input_index: usize,
    state_ctx: &ErgoStateContext,
    spent_boxes: &[ErgoBox],
    data_input_boxes: &[ErgoBox],
) -> Result<(u64, Result<(), EvalError>), OracleCostError> {
    let input_count = tx.inputs.len();
    if input_index >= input_count {
        return Err(OracleCostError::InputIndexOutOfRange {
            index: input_index,
            input_count,
        });
    }

    let tx_context = TransactionContext::new(
        tx.clone(),
        spent_boxes.to_vec(),
        data_input_boxes.to_vec(),
    )
    .map_err(|e| OracleCostError::TransactionContext(format!("{e}")))?;

    let mut ctx = make_context(state_ctx, &tx_context, input_index)
        .map_err(|e| OracleCostError::TransactionContext(format!("{e}")))?;

    // Match production: TransactionContext::validate sets the per-tx cost
    // budget to MaxBlockCost * 10 (block cost → JitCost scale). The helper
    // applies the same cap so it surfaces CostError for scripts production
    // would also reject. Without it, an adversarial script could report an
    // unbounded oracleCost that the node would never actually pay.
    ctx.jit_cost_limit = Some(max_block_cost_jit(state_ctx));

    let eval_result = reduce_to_crypto(&ctx.self_box.ergo_tree, &ctx).map(|_| ());
    let cost = ctx.jit_cost_value();
    Ok((cost, eval_result))
}

fn max_block_cost_jit(state_ctx: &ErgoStateContext) -> u64 {
    let raw = state_ctx.parameters.max_block_cost();
    if raw <= 0 {
        return 0;
    }
    (raw as u64).saturating_mul(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    use ergo_chain_types::{BlockId, EcPoint, Votes};
    use ergo_lib::chain::ergo_state_context::ErgoStateContext;
    use ergo_lib::chain::parameters::Parameters;
    use ergo_lib::chain::transaction::input::prover_result::ProverResult as ChainProverResult;
    use ergo_lib::chain::transaction::input::Input;
    use ergo_lib::chain::transaction::Transaction;
    use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
    use ergo_lib::ergotree_ir::chain::ergo_box::{
        ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters,
    };
    use ergo_lib::ergotree_ir::chain::tx_id::TxId;
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
    use ergo_chain_types::{Header, PreHeader};
    use ergotree_interpreter::sigma_protocol::prover::ProofBytes;
    use ergotree_ir::chain::context_extension::ContextExtension;

    // A real mainnet P2PK script — bare `ProveDlog(pubkey)`. Hits the
    // `trivial_reduce` fast path and reports `EVAL_SIGMA_PROP_CONSTANT`
    // (50 raw JitCost).
    const P2PK_TREE_HEX: &str =
        "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5";
    const MINER_PK_HEX: &str =
        "02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6";
    const SRC_TX_HEX: &str =
        "9302a2983d9cc3f2b9e271097aa3128581c6cad8b59f7b6bc3e08fa6cb63ad3f";

    fn make_pre_header() -> PreHeader {
        let miner_pk = EcPoint::from_base16_str(MINER_PK_HEX.to_string()).unwrap();
        let parent_id_bytes: [u8; 32] = hex::decode(
            "be5d64122592b6d2a07a3a619d4e68598e8df38e57ccbff732fc797bbdcf86ef",
        )
        .unwrap()
        .try_into()
        .unwrap();
        PreHeader {
            version: 1,
            parent_id: BlockId(parent_id_bytes.into()),
            timestamp: 1603134264292,
            n_bits: 118099735,
            height: 342_964,
            miner_pk: Box::new(miner_pk),
            votes: Votes([4, 3, 0]),
        }
    }

    fn make_dummy_header() -> Header {
        use ergo_chain_types::{ADDigest, AutolykosSolution, Digest32};
        let miner_pk = EcPoint::from_base16_str(
            "03163a845c33cccd5e7fe7cf8467d449cacc3c8362e29a50bbed7c4d5b4b5b1311".to_string(),
        )
        .unwrap();
        Header {
            version: 1,
            id: BlockId(Digest32::from([0u8; 32])),
            parent_id: BlockId(Digest32::from([0u8; 32])),
            ad_proofs_root: Digest32::from([0u8; 32]),
            state_root: ADDigest::from([0u8; 33]),
            transaction_root: Digest32::from([0u8; 32]),
            timestamp: 1603134202817,
            n_bits: 118099735,
            height: 342_963,
            extension_root: Digest32::from([0u8; 32]),
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(miner_pk),
                pow_onetime_pk: Some(Box::new(EcPoint::default())),
                nonce: vec![0u8; 8],
                pow_distance: None,
            },
            votes: Votes([4, 0, 0]),
            unparsed_bytes: Box::new([]),
        }
    }

    fn make_state_context() -> ErgoStateContext {
        let dummy = make_dummy_header();
        let headers: [Header; 10] = std::array::from_fn(|_| dummy.clone());
        ErgoStateContext::new(make_pre_header(), headers, Parameters::default())
    }

    /// State context with `max_block_cost = 0` so the very first cost
    /// charge inside the interpreter trips `CostLimitExceeded`. Used to
    /// exercise the helper's err-path cost preservation behavior.
    fn make_state_context_with_zero_budget() -> ErgoStateContext {
        let dummy = make_dummy_header();
        let headers: [Header; 10] = std::array::from_fn(|_| dummy.clone());
        let parameters = Parameters::new(
            /* block_version */ 1,
            /* storage_fee_factor */ 1_250_000,
            /* min_value_per_byte */ 30 * 12,
            /* max_block_size */ 512 * 1024,
            /* max_block_cost */ 0,
            /* token_access_cost */ 100,
            /* input_cost */ 2000,
            /* data_input_cost */ 100,
            /* output_cost */ 100,
        );
        ErgoStateContext::new(make_pre_header(), headers, parameters)
    }

    fn make_input(box_id: ergo_lib::ergotree_ir::chain::ergo_box::BoxId) -> Input {
        Input::new(
            box_id,
            ChainProverResult {
                proof: ProofBytes::Empty,
                extension: ContextExtension::empty(),
            },
        )
    }

    fn make_box(value: u64, ergo_tree: ErgoTree) -> ErgoBox {
        let tx_id_bytes: [u8; 32] = hex::decode(SRC_TX_HEX).unwrap().try_into().unwrap();
        let tx_id = TxId::from(ergo_chain_types::Digest32::from(tx_id_bytes));
        ErgoBox::new(
            BoxValue::try_from(value).unwrap(),
            ergo_tree,
            None,
            NonMandatoryRegisters::empty(),
            342_960,
            tx_id,
            0,
        )
        .unwrap()
    }

    fn p2pk_tree() -> ErgoTree {
        ErgoTree::sigma_parse_bytes(&hex::decode(P2PK_TREE_HEX).unwrap()).unwrap()
    }


    fn make_output(value: u64, ergo_tree: ErgoTree) -> ErgoBoxCandidate {
        ErgoBoxCandidate {
            value: BoxValue::try_from(value).unwrap(),
            ergo_tree,
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 342_964,
        }
    }

    /// A bare P2PK input hits sigma-rust's `EVAL_SIGMA_PROP_CONSTANT` fast
    /// path (50 raw JitCost) and reduces to `Ok`. We assert the helper
    /// surfaces a positive cost and a successful inner result.
    #[test]
    fn oracle_cost_returns_nonzero_for_simple_input() {
        let tree = p2pk_tree();
        let input_box = make_box(1_000_000, tree.clone());
        let output = make_output(900_000, tree);
        let inputs = vec![make_input(input_box.box_id())];
        let tx = Transaction::new_from_vec(inputs, vec![], vec![output]).unwrap();
        let state_ctx = make_state_context();

        let (cost, result) =
            oracle_cost(&tx, 0, &state_ctx, &[input_box], &[]).expect("helper Err setup-side");

        assert!(cost > 0, "expected positive JitCost, got {cost}");
        assert!(result.is_ok(), "expected Ok eval, got {result:?}");
    }

    /// With `max_block_cost = 0`, the helper's per-tx JitCost cap is also
    /// 0 — so the very first `add_jit_cost` charge inside the interpreter
    /// returns `CostLimitExceeded`, which `reduce_to_crypto` surfaces as
    /// `Err(EvalError::CostError)`. `add_jit_cost` writes the new value
    /// to the accumulator before checking the limit, so the helper must
    /// still report a positive cost alongside the Err — that's the
    /// property `validation-fragments` relies on to populate `oracleCost`
    /// next to `oracleError`.
    #[test]
    fn oracle_cost_preserves_jit_cost_through_err() {
        let tree = p2pk_tree();
        let input_box = make_box(1_000_000, tree.clone());
        let output = make_output(900_000, tree);
        let inputs = vec![make_input(input_box.box_id())];
        let tx = Transaction::new_from_vec(inputs, vec![], vec![output]).unwrap();
        let state_ctx = make_state_context_with_zero_budget();

        let (cost, result) =
            oracle_cost(&tx, 0, &state_ctx, &[input_box], &[]).expect("helper Err setup-side");

        assert!(result.is_err(), "expected Err eval, got {result:?}");
        assert!(
            matches!(result, Err(EvalError::CostError(_))),
            "expected CostError, got {result:?}",
        );
        assert!(
            cost > 0,
            "expected positive JitCost preserved through Err, got {cost}",
        );
    }

    /// HTTP handlers will sometimes pass an `input_index` past the end of
    /// the transaction (e.g., a stale client iterating against an
    /// already-reorged block). The helper must surface this as Err
    /// without panicking.
    #[test]
    fn oracle_cost_out_of_range_input_index() {
        let tree = p2pk_tree();
        let input_box = make_box(1_000_000, tree.clone());
        let output = make_output(900_000, tree);
        let inputs = vec![make_input(input_box.box_id())];
        let tx = Transaction::new_from_vec(inputs, vec![], vec![output]).unwrap();
        let state_ctx = make_state_context();

        let result = oracle_cost(&tx, 99, &state_ctx, &[input_box], &[]);

        assert!(matches!(
            result,
            Err(OracleCostError::InputIndexOutOfRange {
                index: 99,
                input_count: 1
            })
        ));
    }
}
