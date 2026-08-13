//! Cost and size bounding in `select_transactions` (contract Step 3, 4a–4e).
//!
//! Fixtures are the block-342,964 fee-contract spend vectors already proven in
//! `ergo-validation`: fee boxes in, one substituted miner-reward box out, empty
//! proof — the script reduces to true on its own, so every candidate has a
//! cheap, deterministic cost to bound against. Costs are measured at runtime
//! rather than hardcoded; the numbers move whenever sigma-rust's costing does,
//! and these tests are about the bound, not the tariff.

use std::collections::HashMap;

use ergo_chain_types::{ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes};
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::input::Input;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::chain::tx_id::TxId;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_mining::selection::select_transactions;
use ergo_validation::{
    build_state_context, validate_single_transaction, ErgoStateContext, Parameter, Parameters,
};

const FEE_CONTRACT_HEX: &str = "1005040004000e36100204a00b08cd0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798ea02d192a39a8cc7a701730073011001020402d19683030193a38cc7b2a57300000193c2b2a57301007473027303830108cdeeac93b1a57304";
const OUTPUT_TREE_HEX: &str = "100204a00b08cd02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6ea02d192a39a8cc7a70173007301";
const MINER_PK_HEX: &str = "02a27f37ca339c25a8ee65cbdb73fe7a7134dd89cd3e7c43e313a92c128859e4f6";
const BLOCK_HEIGHT: u32 = 342_964;

/// Mainnet default — the state context's own per-tx budget, deliberately kept
/// generous so every fixture validates. The bound under test is the argument
/// passed to `select_transactions`, not this.
const CONTEXT_MAX_COST: i32 = 1_000_000;

fn ec_point(hex_str: &str) -> EcPoint {
    EcPoint::sigma_parse_bytes(&hex::decode(hex_str).unwrap()).unwrap()
}

fn fee_box(seed: u8, value: u64) -> ErgoBox {
    let tree = ErgoTree::sigma_parse_bytes(&hex::decode(FEE_CONTRACT_HEX).unwrap()).unwrap();
    ErgoBox::new(
        BoxValue::try_from(value).unwrap(),
        tree,
        None,
        NonMandatoryRegisters::empty(),
        342_900,
        TxId::from(Digest32::from([seed; 32])),
        0,
    )
    .unwrap()
}

/// Spend every source box into one miner-reward output. More inputs means more
/// script evaluations, which is how the "expensive" candidate gets its cost.
fn fee_spend_tx(srcs: &[ErgoBox]) -> Transaction {
    let output_tree = ErgoTree::sigma_parse_bytes(&hex::decode(OUTPUT_TREE_HEX).unwrap()).unwrap();
    let total: i64 = srcs.iter().map(|b| b.value.as_i64()).sum();
    let output = ErgoBoxCandidate {
        value: BoxValue::try_from(u64::try_from(total).unwrap()).unwrap(),
        ergo_tree: output_tree,
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: BLOCK_HEIGHT,
    };
    let inputs: Vec<Input> = srcs
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
    Transaction::new_from_vec(inputs, vec![], vec![output]).unwrap()
}

fn block_header() -> Header {
    let parent_id_bytes: [u8; 32] =
        hex::decode("be5d64122592b6d2a07a3a619d4e68598e8df38e57ccbff732fc797bbdcf86ef")
            .unwrap()
            .try_into()
            .unwrap();
    Header {
        version: 1,
        id: BlockId(Digest32::from([0u8; 32])),
        parent_id: BlockId(parent_id_bytes.into()),
        ad_proofs_root: Digest32::from([0u8; 32]),
        state_root: ADDigest::from([0u8; 33]),
        transaction_root: Digest32::from([0u8; 32]),
        timestamp: 1603134264292,
        n_bits: 118099735,
        height: BLOCK_HEIGHT,
        extension_root: Digest32::from([0u8; 32]),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(ec_point(MINER_PK_HEX)),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([4, 3, 0]),
        unparsed_bytes: Box::new([]),
    }
}

fn parent_header() -> Header {
    Header {
        version: 1,
        id: BlockId(Digest32::from([1u8; 32])),
        parent_id: BlockId(Digest32::from([0u8; 32])),
        ad_proofs_root: Digest32::from([0u8; 32]),
        state_root: ADDigest::from([0u8; 33]),
        transaction_root: Digest32::from([0u8; 32]),
        timestamp: 1603134202817,
        n_bits: 118099735,
        height: BLOCK_HEIGHT - 1,
        extension_root: Digest32::from([0u8; 32]),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(EcPoint::default()),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([4, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

fn state_context() -> ErgoStateContext {
    let mut params = Parameters::default();
    params
        .parameters_table
        .insert(Parameter::MaxBlockCost, CONTEXT_MAX_COST);
    build_state_context(&block_header(), &[parent_header()], &params)
}

/// UTXO resolver over a fixed box set — the "already on chain" side.
fn lookup_over(boxes: &[ErgoBox]) -> impl Fn(&[u8; 32]) -> Option<ErgoBox> + '_ {
    let map: HashMap<[u8; 32], ErgoBox> = boxes
        .iter()
        .map(|b| {
            let mut id = [0u8; 32];
            id.copy_from_slice(b.box_id().as_ref());
            (id, b.clone())
        })
        .collect();
    move |id: &[u8; 32]| map.get(id).cloned()
}

fn cost_of(tx: &Transaction, boxes: &[ErgoBox], ctx: &ErgoStateContext) -> u64 {
    validate_single_transaction(tx, boxes.to_vec(), vec![], ctx)
        .expect("fixture transaction must validate")
}

fn serialized_size(tx: &Transaction) -> usize {
    tx.sigma_serialize_bytes().unwrap().len()
}

fn ids(txs: &[Transaction]) -> Vec<String> {
    txs.iter().map(|tx| format!("{}", tx.id())).collect()
}

/// The half a `break` would silently fail: an over-budget transaction is
/// skipped, and the scan keeps going so a later cheaper one still gets in.
#[test]
fn over_budget_tx_is_skipped_but_a_later_cheaper_one_fits() {
    let ctx = state_context();

    // Expensive: three fee-box inputs, three script evaluations.
    let big_srcs = vec![
        fee_box(0xA1, 2_000_000),
        fee_box(0xA2, 1_000_000),
        fee_box(0xA3, 1_000_000),
    ];
    let big = fee_spend_tx(&big_srcs);
    // Cheap: one input.
    let small_srcs = vec![fee_box(0xB1, 1_000_000)];
    let small = fee_spend_tx(&small_srcs);

    let big_cost = cost_of(&big, &big_srcs, &ctx);
    let small_cost = cost_of(&small, &small_srcs, &ctx);
    assert!(
        big_cost > small_cost,
        "fixture invariant: {big_cost} must exceed {small_cost}"
    );

    let mut utxos = big_srcs.clone();
    utxos.extend(small_srcs.clone());
    let lookup = lookup_over(&utxos);

    // Budget fits the cheap tx exactly, and the expensive one not at all.
    let (selected, invalid) = select_transactions(
        &[
            (big.clone(), serialized_size(&big)),
            (small.clone(), serialized_size(&small)),
        ],
        &[],
        &ctx,
        524_288,
        small_cost,
        &lookup,
    );

    assert_eq!(
        ids(&selected),
        ids(&[small]),
        "the expensive tx must be skipped and the cheap one still selected"
    );
    assert!(
        invalid.is_empty(),
        "an over-budget tx is valid — it must not be reported for mempool eviction"
    );
}

/// The bound is `<=`. A candidate set landing on exactly `max_block_cost` is
/// accepted; one block unit less and the last transaction drops out.
#[test]
fn exactly_max_block_cost_is_accepted() {
    let ctx = state_context();

    let src_a = vec![fee_box(0xC1, 2_000_000)];
    let src_b = vec![fee_box(0xC2, 1_000_000)];
    let tx_a = fee_spend_tx(&src_a);
    let tx_b = fee_spend_tx(&src_b);

    let cost_a = cost_of(&tx_a, &src_a, &ctx);
    let cost_b = cost_of(&tx_b, &src_b, &ctx);
    assert!(cost_a > 0 && cost_b > 0, "fixtures must have nonzero cost");

    let mut utxos = src_a.clone();
    utxos.extend(src_b.clone());
    let lookup = lookup_over(&utxos);
    let candidates = [
        (tx_a.clone(), serialized_size(&tx_a)),
        (tx_b.clone(), serialized_size(&tx_b)),
    ];

    let (selected, invalid) =
        select_transactions(&candidates, &[], &ctx, 524_288, cost_a + cost_b, &lookup);
    assert_eq!(
        ids(&selected),
        ids(&[tx_a.clone(), tx_b.clone()]),
        "a candidate set summing to exactly max_block_cost must be accepted"
    );
    assert!(invalid.is_empty());

    // One unit under: the second transaction no longer fits.
    let (selected, invalid) = select_transactions(
        &candidates,
        &[],
        &ctx,
        524_288,
        cost_a + cost_b - 1,
        &lookup,
    );
    assert_eq!(
        ids(&selected),
        ids(&[tx_a]),
        "one block unit under the sum must drop the second transaction"
    );
    assert!(invalid.is_empty());
}

/// Size bounding is unchanged by the cost bound: still skip-and-continue,
/// still independent of it.
#[test]
fn size_bound_still_skips_and_keeps_scanning() {
    let ctx = state_context();

    let src_a = vec![fee_box(0xD1, 2_000_000)];
    let src_b = vec![fee_box(0xD2, 1_000_000)];
    let tx_a = fee_spend_tx(&src_a);
    let tx_b = fee_spend_tx(&src_b);

    let mut utxos = src_a.clone();
    utxos.extend(src_b.clone());
    let lookup = lookup_over(&utxos);

    // Declared sizes, not real ones: the caller supplies them, and the point
    // here is the arithmetic of the bound.
    let (selected, invalid) = select_transactions(
        &[(tx_a, 400), (tx_b.clone(), 100)],
        &[],
        &ctx,
        300,
        u64::from(u32::MAX),
        &lookup,
    );

    assert_eq!(
        ids(&selected),
        ids(&[tx_b]),
        "the oversized tx is skipped and the smaller one still selected"
    );
    assert!(invalid.is_empty());
}
