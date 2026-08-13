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
use ergo_lib::chain::transaction::{DataInput, Transaction};
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{
    BoxTokens, ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters,
};
use ergo_lib::ergotree_ir::chain::token::{Token, TokenId};
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
use sigma_ser::vlq_encode::WriteSigmaVlqExt;

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
    source_box_with_tokens(seed, value, None)
}

fn source_box_with_tokens(seed: u8, value: u64, tokens: Option<BoxTokens>) -> ErgoBox {
    ErgoBox::new(
        BoxValue::try_from(value).unwrap(),
        always_true(),
        tokens,
        NonMandatoryRegisters::empty(),
        PARENT_HEIGHT,
        TxId::from(Digest32::from([seed; 32])),
        0,
    )
    .unwrap()
}

/// `count` distinct token ids, deterministic and namespaced by `seed` so two
/// source boxes never overlap. Amount 1, so each token costs the minimal 33
/// serialized bytes and the box-size arithmetic in these tests is legible.
fn distinct_tokens(seed: u8, count: usize) -> Vec<Token> {
    (0..count)
        .map(|i| {
            let mut id = [0u8; 32];
            id[0] = seed;
            id[1] = i as u8;
            id[2] = (i >> 8) as u8;
            Token {
                token_id: TokenId::from(Digest32::from(id)),
                amount: 1u64.try_into().unwrap(),
            }
        })
        .collect()
}

fn token_source_box(seed: u8, value: u64, tokens: &[Token]) -> ErgoBox {
    source_box_with_tokens(
        seed,
        value,
        Some(BoxTokens::from_vec(tokens.to_vec()).unwrap()),
    )
}

fn token_ids(b: &ErgoBox) -> Vec<TokenId> {
    let tokens: &[Token] = match b.tokens.as_ref() {
        Some(t) => t.as_ref(),
        None => &[],
    };
    tokens.iter().map(|t| t.token_id).collect()
}

/// The token ids `build_fee_tx` should keep: every group in order, truncated
/// at `cap`.
fn expected_ids(groups: &[&Vec<Token>], cap: usize) -> Vec<TokenId> {
    groups
        .iter()
        .flat_map(|g| g.iter())
        .take(cap)
        .map(|t| t.token_id)
        .collect()
}

/// Every token the fee boxes offer, in the traversal order the reward box must
/// take them in.
fn ordered_tokens(groups: &[Vec<Token>]) -> Vec<Token> {
    groups.iter().flat_map(|g| g.iter().cloned()).collect()
}

/// A box's serialized length as a validator counts it — `ErgoBox` bytes,
/// transaction id and output index included, which is what `txBoxSize` is
/// measured against. The candidate body is 33 bytes shorter and would let a
/// box over the window look like it fits.
fn box_bytes(b: &ErgoBox) -> usize {
    b.sigma_serialize_bytes().unwrap().len()
}

/// The same box re-measured with one more token on it.
///
/// Built here from the shipped box rather than from anything the crate
/// returns, so "the cut is tight" is checked against the serializer directly
/// and not against the arithmetic under test.
fn box_bytes_with(b: &ErgoBox, extra: &Token) -> usize {
    let mut tokens: Vec<Token> = match b.tokens.as_ref() {
        Some(t) => <BoxTokens as AsRef<[Token]>>::as_ref(t).to_vec(),
        None => Vec::new(),
    };
    tokens.push(*extra);
    let candidate = ErgoBoxCandidate {
        value: b.value,
        ergo_tree: b.ergo_tree.clone(),
        tokens: Some(BoxTokens::from_vec(tokens).unwrap()),
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: b.creation_height,
    };
    box_bytes(&ErgoBox::from_box_candidate(&candidate, b.transaction_id, b.index).unwrap())
}

fn spend_into(src: &ErgoBox, out_tree: ErgoTree) -> Transaction {
    spend_into_with_data_inputs(src, &[], out_tree)
}

/// `spend_into` with read-only data inputs attached. The fixture script is
/// `sigmaProp(true)` and never reads them, so all they change for selection is
/// whether the boxes they name have to resolve.
fn spend_into_with_data_inputs(
    src: &ErgoBox,
    data_boxes: &[&ErgoBox],
    out_tree: ErgoTree,
) -> Transaction {
    // Tokens carry through unchanged: the whole box goes to the one output, so
    // preservation holds without a change box.
    let output = ErgoBoxCandidate {
        value: src.value,
        ergo_tree: out_tree,
        tokens: src.tokens.clone(),
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
    let data_inputs: Vec<DataInput> = data_boxes.iter().map(|b| b.box_id().into()).collect();
    Transaction::new_from_vec(vec![input], data_inputs, vec![output]).unwrap()
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
/// cost is the sum over EVERY transaction in the block (`enforce_block_cost`
/// in `validation/src/tx_validation.rs`), fee and emission transactions
/// included; size is the serialized BlockTransactions **section**, framing and
/// all (`ErgoStateContext.scala:308-310`), which is 37 bytes more than that
/// sum on a current-version block.
fn measure_block(block: &CandidateBlock, utxos: &[ErgoBox], ctx: &ErgoStateContext) -> (u64, usize) {
    let mut boxes: HashMap<[u8; 32], ErgoBox> = utxos.iter().map(|b| (id_of(b), b.clone())).collect();
    boxes.insert(id_of(&emission_box()), emission_box());
    for tx in &block.transactions {
        for out in tx.outputs.iter() {
            boxes.entry(id_of(out)).or_insert_with(|| out.clone());
        }
    }

    let mut total_cost = 0u64;
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
    }
    (total_cost, serialized_section(block).len())
}

fn is_fee_tx(tx: &Transaction) -> bool {
    tx.outputs.len() == 1
        && tx.outputs.get(0).unwrap().ergo_tree
            == ergo_tree_predef::reward_output_script(REWARD_DELAY, miner_pk()).unwrap()
}

/// The BlockTransactions section as a validator measures it, built here from
/// the JVM serializer's own layout rather than from the crate's accounting:
///
/// ```text
/// [header_id: 32B][10_000_000 + version: VLQ][tx_count: VLQ][txs…]
/// ```
///
/// (`BlockTransactionsSerializer.serialize`; the sentinel offset is how a
/// reader tells a versioned section from a pre-v2 one.) Independent of
/// `block_transactions_overhead`, so it is free to disagree with it — which is
/// the only reason it is worth writing out.
fn serialized_section(block: &CandidateBlock) -> Vec<u8> {
    // The header id is not known until a solution lands; its 32 bytes are.
    let mut out: Vec<u8> = vec![0u8; 32];
    out.put_u64(10_000_000 + u64::from(block.version)).unwrap();
    out.put_u64(block.transactions.len() as u64).unwrap();
    for tx in &block.transactions {
        out.extend_from_slice(&tx.sigma_serialize_bytes().unwrap());
    }
    out
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

// ---------------------------------------------------------------------------
// 6. The size limit is over the serialized SECTION, not the sum of transactions
// ---------------------------------------------------------------------------

/// Validators enforce `max_block_size` against the BlockTransactions section —
/// `fb.blockTransactions.size <= currentParameters.maxBlockSize`
/// (`ErgoStateContext.scala:308-310`, rule `bsBlockTransactionsSize`) — and
/// that section carries framing no sum of transaction sizes contains: 32 bytes
/// of header id, the sentinel-offset block version, the transaction count.
///
/// So a candidate whose transactions sum to exactly the limit is **over** it by
/// the rule that decides validity, and every peer rejects the block. This test
/// sets the limit to exactly that sum: the accounting under test must shed
/// transactions, and an accounting that sums transaction sizes accepts the
/// whole block and ships it 37 bytes over.
#[test]
fn the_size_bound_is_over_the_section_not_the_sum_of_transactions() {
    let srcs = vec![source_box(0xF1, 4_000_000)];
    let txs = candidates(&[fee_paying_tx(&srcs[0])]);

    let generous = params_with(1_000_000, 524_288);
    let full = generate(&txs, &generous, &srcs);
    assert_eq!(full.block.transactions.len(), 3, "[emission, tx, fee]");

    let tx_bytes: usize = full.block.transactions.iter().map(size_of).sum();
    let section_bytes = serialized_section(&full.block).len();
    assert_eq!(
        section_bytes - tx_bytes,
        37,
        "a v{} section framing {} transactions costs 32 header-id bytes, a \
         4-byte version and a 1-byte count",
        full.block.version,
        full.block.transactions.len()
    );

    // The whole section on the limit: `<=`, so nothing sheds.
    let exact = params_with(1_000_000, section_bytes);
    let at_limit = generate(&txs, &exact, &srcs);
    assert_eq!(
        at_limit.block.transactions.len(),
        3,
        "a section landing exactly on max_block_size is accepted whole — the \
         framing is counted, not padded against"
    );
    assert_eq!(serialized_section(&at_limit.block).len(), section_bytes);

    // The limit is the sum of the transaction bytes — 37 short of the section.
    let undercount = params_with(1_000_000, tx_bytes);
    let bounded = generate(&txs, &undercount, &srcs);
    let shipped = serialized_section(&bounded.block).len();
    assert!(
        shipped <= tx_bytes,
        "the shipped section is {shipped} bytes against a {tx_bytes} limit — \
         the section framing was not counted, and this block is rejected by \
         every peer that checks it"
    );
    assert_eq!(
        bounded.block.transactions.len(),
        1,
        "37 bytes short of the whole set, the selected transaction and the fee \
         transaction it fed must both drop, leaving emission only"
    );
    assert!(
        bounded.invalid_txs.is_empty(),
        "a transaction dropped for size is not invalid — it stays in the mempool"
    );
}

// ---------------------------------------------------------------------------
// 7. Fee token aggregation, and the cap on it
// ---------------------------------------------------------------------------

/// A token-carrying fee box is collected like any other: the miner box takes
/// the ERG and the tokens with it, and the assembled block validates.
///
/// The regression this guards is the aggregation rewrite that the cap needed.
/// Tokens are summed in first-seen traversal order — fee boxes in block order,
/// tokens in box order — rather than in `HashMap` order, because that order
/// decides the miner box's serialized bytes, hence its id, the fee
/// transaction's id, and the transactions root the miner is handed to hash.
#[test]
fn fee_box_tokens_are_collected_onto_the_miner_box() {
    const PER_BOX: usize = 50;

    let tokens_a = distinct_tokens(0x10, PER_BOX);
    let tokens_b = distinct_tokens(0x20, PER_BOX);
    let srcs = vec![
        token_source_box(0xA7, 2_000_000, &tokens_a),
        token_source_box(0xA8, 3_000_000, &tokens_b),
    ];
    let txs = vec![fee_paying_tx(&srcs[0]), fee_paying_tx(&srcs[1])];

    // Token-heavy boxes are large and cost more to evaluate; neither limit is
    // what this test is about.
    let params = params_with(50_000_000, 524_288);
    let generated = generate(&candidates(&txs), &params, &srcs);
    let assembled = &generated.block.transactions;

    assert_eq!(assembled.len(), 4, "[emission, tx, tx, fee]");
    let fee_tx = assembled.last().unwrap();
    assert!(is_fee_tx(fee_tx), "the last transaction must be the fee transaction");

    let miner_box = fee_tx.outputs.get(0).unwrap();
    assert_eq!(miner_box.value.as_i64(), 5_000_000, "every ERG of fee collected");
    assert_eq!(
        token_ids(miner_box),
        expected_ids(&[&tokens_a, &tokens_b], 2 * PER_BOX),
        "every token collected, in traversal order"
    );

    // Re-validates every assembled transaction — a token-carrying fee
    // collection that does not validate fails here.
    measure_block(&generated.block, &srcs, &block_context(&params));
    assert!(generated.invalid_txs.is_empty());
}

/// The cap on aggregated fee tokens is the reward box's MEASURED serialized
/// size — `out.bytes.length <= MaxBoxSize` (4096), JVM rule `txBoxSize`,
/// `ErgoTransaction.scala:175`, ergo-lib `BoxSizeExceeded` — and never a token
/// count. A 255-token reward box is roughly 8.5 KB, so `MAX_TOKENS_COUNT` can
/// only move the failure from "box will not build" to "box will not validate";
/// see `fee_tokens_past_the_box_window_are_capped_and_the_fees_collected`.
///
/// The survivors are however many fit, in first-seen traversal order — a plain
/// take, no reordering by value, no hash-map iteration. Asserted on the
/// serialized length and on the tightness of the cut; the count is the measure
/// this change established is the wrong one.
#[test]
fn fee_tokens_are_capped_at_one_box_worth_in_traversal_order() {
    const PER_BOX: usize = 100;

    let groups = [
        distinct_tokens(0x10, PER_BOX),
        distinct_tokens(0x20, PER_BOX),
        distinct_tokens(0x30, PER_BOX),
    ];
    let block_txs: Vec<Transaction> = groups
        .iter()
        .enumerate()
        .map(|(i, tokens)| fee_paying_tx(&token_source_box(0xB0 + i as u8, 2_000_000, tokens)))
        .collect();

    let fee_tx = ergo_mining::fee::build_fee_tx(
        &block_txs,
        CANDIDATE_HEIGHT,
        REWARD_DELAY,
        &miner_pk(),
    )
    .expect("the cap is what keeps this buildable")
    .expect("three fee boxes must produce a fee transaction");

    let miner_box = fee_tx.outputs.get(0).unwrap();
    let ordered = ordered_tokens(&groups);
    let kept = token_ids(miner_box).len();

    let size = box_bytes(miner_box);
    assert!(
        size <= ErgoBox::MAX_BOX_SIZE,
        "the reward box is {size} bytes against a {}-byte window: it cannot \
         validate, and a fee box that cannot validate collects nothing",
        ErgoBox::MAX_BOX_SIZE
    );
    assert!(
        kept < ordered.len(),
        "fixture must overflow the box window: all {} tokens fit",
        ordered.len()
    );
    assert!(
        kept < ErgoBox::MAX_TOKENS_COUNT,
        "{kept} tokens in a {}-byte box — MAX_TOKENS_COUNT cannot be what bound, \
         so the cap has regressed to a count",
        ErgoBox::MAX_BOX_SIZE
    );
    assert!(
        box_bytes_with(miner_box, &ordered[kept]) > ErgoBox::MAX_BOX_SIZE,
        "the cut is loose: token {kept} still fits in {} bytes, so the cap \
         burned a token it did not have to",
        ErgoBox::MAX_BOX_SIZE
    );
    assert_eq!(
        token_ids(miner_box),
        expected_ids(&groups.iter().collect::<Vec<_>>(), kept),
        "the survivors are the first {kept} in traversal order — a plain take, \
         no reordering by value, no hash-map iteration"
    );
    assert_eq!(
        miner_box.value.as_i64(),
        3 * 2_000_000,
        "capping tokens must not cost the miner any ERG"
    );
    assert_eq!(
        fee_tx.inputs.len(),
        3,
        "all three fee boxes are still spent — the dropped tokens are burned, \
         which Ergo permits and the JVM's own take does too"
    );
}

/// ⚠ **The whole point, end to end.** 140 distinct tokens is *under*
/// `MAX_TOKENS_COUNT` and *over* the box window, so a count cap is never even
/// reached: the reward box built, failed `txBoxSize` at validation, and the
/// block shipped collecting nothing. That is what the JVM does — it caps by
/// the same wrong constant (`.take(MaxAssetsPerBox)`,
/// `CandidateGenerator.scala:811`) and loses the same fees.
///
/// **We diverge deliberately.** Which fee boxes a miner collects is miner
/// policy, not consensus: the rule constrains the box a miner produces, not
/// the choice of what to put in it, and uncollected fee boxes stay spendable
/// later. So the fee transaction is now built, valid, and collecting — asserted
/// through `generate_candidate` with every assembled transaction re-validated,
/// because "the box serializes small enough" and "the block a peer receives is
/// accepted" are not the same claim.
#[test]
fn fee_tokens_past_the_box_window_are_capped_and_the_fees_collected() {
    const PER_BOX: usize = 70;
    const {
        assert!(
            2 * PER_BOX < ErgoBox::MAX_TOKENS_COUNT,
            "fixture must stay UNDER the token cap, so the box window is what bites"
        )
    };

    let groups = [
        distinct_tokens(0x10, PER_BOX),
        distinct_tokens(0x20, PER_BOX),
    ];
    let srcs = vec![
        token_source_box(0xC1, 2_000_000, &groups[0]),
        token_source_box(0xC2, 3_000_000, &groups[1]),
    ];
    let txs = vec![fee_paying_tx(&srcs[0]), fee_paying_tx(&srcs[1])];

    let params = params_with(50_000_000, 524_288);
    let generated = generate(&candidates(&txs), &params, &srcs);
    let assembled = &generated.block.transactions;

    assert_eq!(assembled.len(), 4, "[emission, tx, tx, fee]");
    let fee_tx = assembled.last().unwrap();
    assert!(
        is_fee_tx(fee_tx),
        "the fee transaction must ship: {} distinct tokens is a reason to burn \
         the excess, not to hand the block's whole revenue back",
        2 * PER_BOX
    );

    let miner_box = fee_tx.outputs.get(0).unwrap();
    assert_eq!(
        miner_box.value.as_i64(),
        5_000_000,
        "every nanoERG of fee collected — the tokens are what got capped"
    );

    let ordered = ordered_tokens(&groups);
    let kept = token_ids(miner_box).len();
    let size = box_bytes(miner_box);
    assert!(
        size <= ErgoBox::MAX_BOX_SIZE,
        "the reward box is {size} bytes against a {}-byte window",
        ErgoBox::MAX_BOX_SIZE
    );
    assert!(
        kept < ordered.len(),
        "fixture must overflow the box window: all {} tokens fit",
        ordered.len()
    );
    assert!(
        box_bytes_with(miner_box, &ordered[kept]) > ErgoBox::MAX_BOX_SIZE,
        "the cut is loose: token {kept} still fits, so the cap burned a token \
         it did not have to"
    );
    assert_eq!(
        token_ids(miner_box),
        expected_ids(&groups.iter().collect::<Vec<_>>(), kept),
        "the survivors are the first {kept} in traversal order"
    );

    // Re-validates every assembled transaction. The reward box that used to
    // fail `txBoxSize` right here is the reason this test exists.
    measure_block(&generated.block, &srcs, &block_context(&params));
    assert!(
        generated.invalid_txs.is_empty(),
        "the fee boxes' creators are perfectly valid transactions"
    );
}

// ---------------------------------------------------------------------------
// 8. An unresolvable DATA input is skipped, not evicted
// ---------------------------------------------------------------------------

/// The same rule § 5 applies to conflicts, and the one selection already
/// applied to regular inputs: a box that will not resolve is not evidence the
/// transaction is invalid. It may be a reorg race, a fastsync gap, or the
/// at-tip storage swap window, and it may resolve on the next 15-second
/// rebuild.
///
/// Resolving data inputs with a filter that drops the misses yields a
/// `data_boxes` shorter than `tx.data_inputs`, which `TransactionContext::new`
/// rejects deterministically as `DataInputBoxNotFound`. So the transaction
/// comes back *invalid* rather than skipped, lands in `invalid_txs`, and the
/// caller routes it to `pool.invalidate` — evicting a valid transaction for
/// the mempool's invalidation TTL.
///
/// ⚠ The exclusion assertion passes either way: the truncated context is
/// caught up front, so a filtered transaction never reaches the block. Nothing
/// is validated against a mismatched context. The whole bug is **which of the
/// two return values** it lands in, so the `invalid_txs` assertion is the one
/// under test — restore the filter and it is the one that fails.
#[test]
fn an_unresolvable_data_input_is_skipped_not_evicted() {
    let src = source_box(0xDA, 4_000_000);
    // Referenced as a data input, and deliberately withheld from the lookup in
    // the first pass: a box the node cannot see right now.
    let referenced = source_box(0xDB, 1_000_000);
    let tx = spend_into_with_data_inputs(&src, &[&referenced], fee_tree());
    let mempool = candidates(std::slice::from_ref(&tx));

    let generated = generate(&mempool, &Parameters::default(), std::slice::from_ref(&src));
    assert!(
        !generated
            .block
            .transactions
            .iter()
            .any(|t| t.id() == tx.id()),
        "a transaction whose data input will not resolve cannot be selected"
    );
    assert!(
        generated.invalid_txs.is_empty(),
        "an unresolvable data input is not evidence of invalidity — reporting \
         it evicts a valid transaction from the mempool for the invalidation \
         TTL, when it would have selected fine on the next rebuild"
    );

    // Control: hand the lookup the same box and the transaction selects. The
    // skip above is about resolution, not about carrying a data input at all.
    let resolved = generate(&mempool, &Parameters::default(), &[src, referenced]);
    assert!(
        resolved
            .block
            .transactions
            .iter()
            .any(|t| t.id() == tx.id()),
        "fixture invariant: the transaction is otherwise selectable"
    );
    assert!(resolved.invalid_txs.is_empty());
}
