//! Contract steps 3–4 wired into `generate_candidate`: mempool selection,
//! fee collection, and the block limits counted over the ASSEMBLED set.
//!
//! The candidate transactions here spend boxes guarded by `sigmaProp(true)`,
//! so they validate with an empty proof and every cost in the block is
//! deterministic and cheap. Costs and sizes are measured at runtime rather
//! than hardcoded — the numbers move whenever sigma-rust's costing does, and
//! these tests are about the bound, not the tariff.

use std::collections::HashMap;
use std::time::Duration;

use ergo_chain_types::{
    ADDigest, AutolykosSolution, BlockId, Digest, Digest32, EcPoint, Header, Votes,
};
use ergo_lib::chain::emission::MonetarySettings;
use ergo_lib::chain::ergo_tree_predef;
use ergo_lib::chain::genesis;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::input::Input;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::chain::tx_id::TxId;
use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
use ergo_lib::ergotree_ir::mir::expr::Expr;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::{ProveDlog, SigmaBoolean, SigmaProp};
use ergo_mining::emission::ReemissionRules;
use ergo_mining::types::*;
use ergo_mining::{GeneratedCandidate, ValidatorProofsResult};
use ergo_validation::{
    build_state_context, validate_single_transaction, ErgoStateContext, Parameter, Parameters,
};

/// Trivial difficulty — the candidate's nBits, irrelevant to selection.
const INITIAL_N_BITS: u32 = 16842752;
const REWARD_DELAY: i32 = 720;
/// Genesis is height 1 in Ergo, so every candidate here is height 2.
const PARENT_HEIGHT: u32 = 1;
const CANDIDATE_HEIGHT: u32 = PARENT_HEIGHT + 1;

const PROOFS: &[&str] = &[
    "test-proof-1",
    "test-proof-2",
    "test-proof-3",
    "test-proof-4",
    "test-proof-5",
];
const FOUNDER_PKS: &[&str] = &[
    "039bb5fe52359a64c99a60fd944fc5e388cbdc4d37ff091cc841c3ee79060b8647",
    "031fb52cf6e805f80d97cde289f4f757d49accf0c83fb864b27d2cf982c37f9a8b",
    "0352ac2a471339b0d23b3d2c5ce0db0e81c969f77891b9edf0bda7fd39a78184e7",
];

fn founder_pks() -> Vec<ProveDlog> {
    FOUNDER_PKS
        .iter()
        .map(|h| ProveDlog::new(EcPoint::sigma_parse_bytes(&hex::decode(h).unwrap()).unwrap()))
        .collect()
}

fn miner_pk() -> ProveDlog {
    ProveDlog::new(EcPoint::sigma_parse_bytes(&hex::decode(FOUNDER_PKS[0]).unwrap()).unwrap())
}

fn config() -> MinerConfig {
    MinerConfig {
        miner_pk: miner_pk(),
        reward_delay: REWARD_DELAY,
        votes: [0, 0, 0],
        candidate_ttl: Duration::from_secs(15),
        reemission_rules: ReemissionRules::mainnet(),
    }
}

fn emission_box() -> ErgoBox {
    let (b, _, _) =
        genesis::genesis_boxes(&MonetarySettings::default(), &founder_pks(), 2, PROOFS).unwrap();
    b
}

fn parent_header() -> Header {
    Header {
        version: 2,
        id: BlockId(Digest::from([1u8; 32])),
        parent_id: BlockId(Digest::from([0u8; 32])),
        ad_proofs_root: Digest32::from([0u8; 32]),
        state_root: ADDigest::from([0u8; 33]),
        transaction_root: Digest32::from([0u8; 32]),
        timestamp: 1000,
        n_bits: INITIAL_N_BITS,
        height: PARENT_HEIGHT,
        extension_root: Digest32::from([0u8; 32]),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(*miner_pk().h),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes([0, 0, 0]),
        unparsed_bytes: Box::new([]),
    }
}

fn mock_proofs(_txs: &[Transaction]) -> ValidatorProofsResult {
    Some(Ok((vec![0u8; 64], ADDigest::from([0u8; 33]))))
}

// ---------------------------------------------------------------------------
// Fixture transactions
// ---------------------------------------------------------------------------

/// `sigmaProp(true)` — reduces to true on its own, so an empty spending proof
/// suffices and the candidate transactions cost next to nothing to validate.
fn always_true() -> ErgoTree {
    let prop = SigmaProp::new(SigmaBoolean::TrivialProp(true));
    ErgoTree::try_from(Expr::Const(prop.into())).unwrap()
}

fn fee_tree() -> ErgoTree {
    ergo_tree_predef::fee_proposition(REWARD_DELAY).unwrap()
}

/// A box on the "already on chain" side, spendable by anyone.
fn source_box(seed: u8, value: u64) -> ErgoBox {
    ErgoBox::new(
        BoxValue::try_from(value).unwrap(),
        always_true(),
        None,
        NonMandatoryRegisters::empty(),
        PARENT_HEIGHT,
        TxId::from(Digest32::from([seed; 32])),
        0,
    )
    .unwrap()
}

fn spend_into(src: &ErgoBox, out_tree: ErgoTree) -> Transaction {
    let output = ErgoBoxCandidate {
        value: src.value,
        ergo_tree: out_tree,
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: CANDIDATE_HEIGHT,
    };
    let input = Input::new(
        src.box_id(),
        ProverResult {
            proof: ProofBytes::Empty,
            extension: ContextExtension::empty(),
        },
    );
    Transaction::new_from_vec(vec![input], vec![], vec![output]).unwrap()
}

/// Spends `src` entirely into a fee-proposition box — the whole value becomes
/// a miner fee, so this transaction contributes a fee box to the block.
fn fee_paying_tx(src: &ErgoBox) -> Transaction {
    spend_into(src, fee_tree())
}

/// Spends `src` into another anyone-can-spend box: valid, but pays no fee.
fn feeless_tx(src: &ErgoBox) -> Transaction {
    spend_into(src, always_true())
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn lookup_over(boxes: &[ErgoBox]) -> impl Fn(&[u8; 32]) -> Option<ErgoBox> + '_ {
    let map: HashMap<[u8; 32], ErgoBox> = boxes.iter().map(|b| (id_of(b), b.clone())).collect();
    move |id: &[u8; 32]| map.get(id).cloned()
}

fn id_of(b: &ErgoBox) -> [u8; 32] {
    let mut id = [0u8; 32];
    id.copy_from_slice(b.box_id().as_ref());
    id
}

fn size_of(tx: &Transaction) -> usize {
    tx.sigma_serialize_bytes().unwrap().len()
}

fn candidates(txs: &[Transaction]) -> Vec<(Transaction, usize)> {
    txs.iter().map(|t| (t.clone(), size_of(t))).collect()
}

fn params_with(max_cost: u64, max_size: usize) -> Parameters {
    let mut p = Parameters::default();
    p.parameters_table
        .insert(Parameter::MaxBlockCost, i32::try_from(max_cost).unwrap());
    p.parameters_table
        .insert(Parameter::MaxBlockSize, i32::try_from(max_size).unwrap());
    p
}

fn generate(
    candidate_txs: &[(Transaction, usize)],
    parameters: &Parameters,
    utxos: &[ErgoBox],
) -> GeneratedCandidate {
    let lookup = lookup_over(utxos);
    ergo_mining::generate_candidate(
        &config(),
        &parent_header(),
        INITIAL_N_BITS,
        &[],
        &emission_box(),
        None,
        &[],
        candidate_txs,
        parameters,
        &[],
        &lookup,
        &mock_proofs,
    )
    .expect("candidate generation must succeed")
}

/// The state context the assembled block's transactions execute under.
///
/// Rebuilt here rather than reached for: `generate_candidate` derives its own
/// from the parent, and the only field that differs is the wall-clock
/// timestamp, which no fixture script reads and no cost depends on.
fn block_context(parameters: &Parameters) -> ErgoStateContext {
    let parent = parent_header();
    let upcoming = Header {
        height: CANDIDATE_HEIGHT,
        parent_id: parent.id,
        timestamp: parent.timestamp + 1,
        ..parent.clone()
    };
    build_state_context(&upcoming, &[parent], parameters)
}

/// Total block cost and size, counted exactly as a validator counts them:
/// the sum over EVERY transaction in the block (`enforce_block_cost` in
/// `validation/src/tx_validation.rs`), fee and emission transactions included.
fn measure_block(block: &CandidateBlock, utxos: &[ErgoBox], ctx: &ErgoStateContext) -> (u64, usize) {
    let mut boxes: HashMap<[u8; 32], ErgoBox> = utxos.iter().map(|b| (id_of(b), b.clone())).collect();
    boxes.insert(id_of(&emission_box()), emission_box());
    for tx in &block.transactions {
        for out in tx.outputs.iter() {
            boxes.entry(id_of(out)).or_insert_with(|| out.clone());
        }
    }

    let mut total_cost = 0u64;
    let mut total_size = 0usize;
    for (i, tx) in block.transactions.iter().enumerate() {
        let inputs: Vec<ErgoBox> = tx
            .inputs
            .iter()
            .map(|input| {
                let mut id = [0u8; 32];
                id.copy_from_slice(input.box_id.as_ref());
                boxes
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| panic!("tx {i}: input box {} unresolved", hex::encode(id)))
            })
            .collect();
        total_cost += validate_single_transaction(tx, inputs, vec![], ctx)
            .unwrap_or_else(|e| panic!("assembled tx {i} must validate: {e}"));
        total_size += size_of(tx);
    }
    (total_cost, total_size)
}

fn is_fee_tx(tx: &Transaction) -> bool {
    tx.outputs.len() == 1
        && tx.outputs.get(0).unwrap().ergo_tree
            == ergo_tree_predef::reward_output_script(REWARD_DELAY, miner_pk()).unwrap()
}

// ---------------------------------------------------------------------------
// 1. Empty mempool — must not regress
// ---------------------------------------------------------------------------

#[test]
fn empty_mempool_still_produces_emission_only() {
    let generated = generate(&[], &Parameters::default(), &[]);

    assert_eq!(
        generated.block.transactions.len(),
        1,
        "an empty mempool must still yield the emission transaction alone"
    );
    assert!(
        generated.invalid_txs.is_empty(),
        "nothing was offered, so nothing can be invalid"
    );
    // Emission tx shape: spends the emission box, pays out the new emission
    // box plus the miner reward.
    let emission = &generated.block.transactions[0];
    assert_eq!(emission.inputs.len(), 1);
    assert_eq!(emission.outputs.len(), 2);
}

// ---------------------------------------------------------------------------
// 2. Selection appears, fee transaction goes last
// ---------------------------------------------------------------------------

#[test]
fn selected_transactions_appear_with_the_fee_transaction_last() {
    let srcs = vec![source_box(0xA1, 2_000_000), source_box(0xA2, 3_000_000)];
    let txs = vec![fee_paying_tx(&srcs[0]), fee_paying_tx(&srcs[1])];

    let generated = generate(&candidates(&txs), &Parameters::default(), &srcs);
    let assembled = &generated.block.transactions;

    assert_eq!(
        assembled.len(),
        4,
        "expected [emission, tx, tx, fee], got {} transactions",
        assembled.len()
    );
    assert_eq!(
        assembled[1].id(),
        txs[0].id(),
        "selected transactions keep priority order, after the emission tx"
    );
    assert_eq!(assembled[2].id(), txs[1].id());

    let fee_tx = assembled.last().unwrap();
    assert!(
        is_fee_tx(fee_tx),
        "the LAST transaction must be the fee transaction — it spends fee \
         boxes that are outputs of the selected transactions, so it cannot \
         precede them"
    );
    assert_eq!(
        fee_tx.inputs.len(),
        2,
        "both fee boxes must be collected into the one fee transaction"
    );
    assert_eq!(
        fee_tx.outputs.get(0).unwrap().value.as_i64(),
        5_000_000,
        "the miner box must carry the summed fees"
    );
    assert!(generated.invalid_txs.is_empty());
}

// ---------------------------------------------------------------------------
// 3. Zero total fee — no fee transaction, and not an error
// ---------------------------------------------------------------------------

#[test]
fn zero_total_fee_yields_no_fee_transaction() {
    let srcs = vec![source_box(0xB1, 2_000_000)];
    let txs = vec![feeless_tx(&srcs[0])];

    let generated = generate(&candidates(&txs), &Parameters::default(), &srcs);
    let assembled = &generated.block.transactions;

    assert_eq!(
        assembled.len(),
        2,
        "a selected transaction paying no fee yields [emission, tx] — no fee tx"
    );
    assert_eq!(assembled[1].id(), txs[0].id());
    assert!(
        !is_fee_tx(assembled.last().unwrap()),
        "nothing to collect must not synthesise a fee transaction"
    );
    assert!(generated.invalid_txs.is_empty());
}

// ---------------------------------------------------------------------------
// 4. The assembled block, fee transaction included, stays within both limits
// ---------------------------------------------------------------------------

/// The failure this whole step exists to avoid: `select_transactions` counts
/// mempool transactions only, so selecting right up to `max_block_cost` and
/// then appending a fee transaction pushes the assembled block over the limit.
///
/// Measured against the exact boundary. One unit of budget short of
/// `emission + selected + fee`, the selected transaction must drop out —
/// dropping it also drops the fee box it created, so the block falls back to
/// emission-only. An arrangement that forgets to count the fee transaction
/// keeps it, and the assembled block exceeds the budget it was built for.
#[test]
fn assembled_block_including_the_fee_transaction_stays_within_the_cost_limit() {
    let srcs = vec![source_box(0xC1, 4_000_000)];
    let txs = candidates(&[fee_paying_tx(&srcs[0])]);

    // Measure the full block first, with budgets nothing can reach.
    let generous = params_with(1_000_000, 524_288);
    let full = generate(&txs, &generous, &srcs);
    assert_eq!(full.block.transactions.len(), 3, "[emission, tx, fee]");
    let (full_cost, _) = measure_block(&full.block, &srcs, &block_context(&generous));

    // Exactly enough: everything fits.
    let exact = params_with(full_cost, 524_288);
    let at_limit = generate(&txs, &exact, &srcs);
    let (cost, _) = measure_block(&at_limit.block, &srcs, &block_context(&exact));
    assert_eq!(
        at_limit.block.transactions.len(),
        3,
        "a block summing to exactly max_block_cost must be accepted whole"
    );
    assert!(cost <= full_cost, "{cost} must fit {full_cost}");

    // One unit short: the fee transaction is what no longer fits, and the
    // transaction that created its fee box goes with it.
    let tight_cost = full_cost - 1;
    let tight = params_with(tight_cost, 524_288);
    let dropped = generate(&txs, &tight, &srcs);
    let (cost, _) = measure_block(&dropped.block, &srcs, &block_context(&tight));
    assert!(
        cost <= tight_cost,
        "assembled block cost {cost} exceeds the {tight_cost} limit it was \
         built for — the fee transaction was not counted against the budget"
    );
    assert_eq!(
        dropped.block.transactions.len(),
        1,
        "one unit short of the whole set, the selected transaction and its \
         fee transaction must both drop, leaving emission only"
    );
    assert!(
        dropped.invalid_txs.is_empty(),
        "a transaction dropped for budget is not invalid — it stays in the mempool"
    );
}

/// Same bound, counted in bytes.
#[test]
fn assembled_block_including_the_fee_transaction_stays_within_the_size_limit() {
    let srcs = vec![source_box(0xD1, 4_000_000)];
    let txs = candidates(&[fee_paying_tx(&srcs[0])]);

    let generous = params_with(1_000_000, 524_288);
    let full = generate(&txs, &generous, &srcs);
    assert_eq!(full.block.transactions.len(), 3, "[emission, tx, fee]");
    let (_, full_size) = measure_block(&full.block, &srcs, &block_context(&generous));

    let exact = params_with(1_000_000, full_size);
    let at_limit = generate(&txs, &exact, &srcs);
    assert_eq!(
        at_limit.block.transactions.len(),
        3,
        "a block summing to exactly max_block_size must be accepted whole"
    );

    let tight_size = full_size - 1;
    let tight = params_with(1_000_000, tight_size);
    let dropped = generate(&txs, &tight, &srcs);
    let (_, size) = measure_block(&dropped.block, &srcs, &block_context(&tight));
    assert!(
        size <= tight_size,
        "assembled block size {size} exceeds the {tight_size} limit it was \
         built for — the fee transaction's bytes were not counted"
    );
    assert_eq!(
        dropped.block.transactions.len(),
        1,
        "one byte short of the whole set, the selected transaction and its \
         fee transaction must both drop"
    );
}

// ---------------------------------------------------------------------------
// 5. Conflicting mempool transactions never both land in the candidate
// ---------------------------------------------------------------------------

/// Two transactions spending the same box both validate in isolation — the
/// contested box is unspent on chain and `validate_single_transaction` sees
/// one transaction at a time. Without a conflict check against the
/// accumulated block, both get selected and the assembled block carries a
/// double-spend.
#[test]
fn conflicting_transactions_are_not_both_selected() {
    let src = source_box(0xE1, 4_000_000);
    let first = fee_paying_tx(&src);
    // Same input, different output script → a different transaction id.
    let second = feeless_tx(&src);
    assert_ne!(first.id(), second.id(), "fixture: two distinct transactions");

    let generated = generate(
        &candidates(&[first.clone(), second.clone()]),
        &Parameters::default(),
        &[src],
    );
    let assembled = &generated.block.transactions;

    let selected: Vec<_> = assembled
        .iter()
        .filter(|tx| tx.id() == first.id() || tx.id() == second.id())
        .collect();
    assert_eq!(
        selected.len(),
        1,
        "exactly one of two conflicting transactions may be selected"
    );
    assert_eq!(selected[0].id(), first.id(), "priority order decides");
    assert!(
        generated.invalid_txs.is_empty(),
        "losing a mempool conflict is not evidence of invalidity — the \
         mempool owns conflict resolution"
    );
}
