//! The script version the caller read or built the transaction at:
//! `UnconfirmedTx::id_version` and its precondition on `process()`
//! (`facts/mempool.md` § `UnconfirmedTx`, § Processing transactions, § Block
//! interaction).
//!
//! The X15 fixture is `Tuple(1, Upcast(1, Long))` in context variable 0,
//! because its serialization differs by version: at tree version 3 the
//! `Upcast` is written through; at 2 and below it folds to the constant. So
//! the same input, outputs and extension produce one id when built at V3 and
//! another when built at V2, and the pool's answer is "whatever the caller
//! named."

mod common;

use std::time::Instant;

use ergo_chain_types::Digest32;
use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::input::Input;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::chain::evaluated_value::{EvaluatedExpr, EvaluatedValue};
use ergo_lib::ergotree_ir::chain::tx_id::TxId;
use ergo_lib::ergotree_ir::ergo_tree::{ErgoTree, ErgoTreeVersion};
use ergo_lib::ergotree_ir::mir::constant::Constant;
use ergo_lib::ergotree_ir::mir::expr::Expr;
use ergo_lib::ergotree_ir::mir::upcast::Upcast;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::{SigmaBoolean, SigmaProp};
use ergo_lib::ergotree_ir::types::stype::SType;

use common::{fee_tree, state_context_at, tx_id_bytes, StaticUtxo, BOX_VALUE, TIP_HEIGHT};
use ergo_mempool::types::{MempoolConfig, ProcessingOutcome, UnconfirmedTx};
use ergo_mempool::Mempool;

/// SANTA X15 — `Tuple(1, Upcast(1, Long))`.
fn x15() -> EvaluatedValue {
    let one = || Expr::Const(1i32.into());
    EvaluatedValue::Expr(EvaluatedExpr::Tuple(
        [
            one(),
            Expr::Upcast(Upcast::new(one(), SType::SLong).unwrap()),
        ]
        .into(),
    ))
}

/// `sigmaProp(true)` as a `SigmaProp` constant: the interpreter short-circuits
/// a tree that is one, so a spending extension's values aren't converted and
/// X15 reaches the pool without the interpreter rejecting it (the api test's
/// `always_true`).
fn always_true() -> ErgoTree {
    let always = SigmaProp::new(SigmaBoolean::TrivialProp(true));
    ErgoTree::try_from(Expr::Const(Constant::from(always))).expect("always_true tree")
}

/// A UTXO guarded by [`always_true`], distinguished from its siblings by `seed`.
fn always_true_box(seed: u8, creation_height: u32) -> ErgoBox {
    ErgoBox::new(
        BoxValue::try_from(BOX_VALUE).expect("value above the minimum"),
        always_true(),
        None,
        NonMandatoryRegisters::empty(),
        creation_height,
        TxId::from(Digest32::from([seed; 32])),
        0,
    )
    .expect("box construction")
}

/// A transaction spending `input` with X15 as context variable 0, paying
/// `fee` into the fee proposition and the remainder back under
/// [`always_true`], built with its id written at `id_version`.
fn x15_tx_at(input: &ErgoBox, fee: u64, id_version: ErgoTreeVersion) -> Transaction {
    let mut extension = ContextExtension::empty();
    extension.values.insert(0, x15());
    let spend = Input::new(
        input.box_id(),
        ProverResult {
            proof: ProofBytes::Empty,
            extension,
        },
    );
    let change = ErgoBoxCandidate {
        value: BoxValue::try_from(*input.value.as_u64() - fee).expect("change above the minimum"),
        ergo_tree: always_true(),
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: TIP_HEIGHT,
    };
    let fee_output = ErgoBoxCandidate {
        value: BoxValue::try_from(fee).expect("fee above the minimum"),
        ergo_tree: fee_tree(),
        tokens: None,
        additional_registers: NonMandatoryRegisters::empty(),
        creation_height: TIP_HEIGHT,
    };
    Transaction::new_from_vec_at(vec![spend], vec![], vec![change, fee_output], id_version)
        .expect("transaction construction")
}

/// Enough above the default `min_fee` to clear step 7b.
const FEE: u64 = 2_000_000;

/// Setup check: X15 at version 3 and at version 2 are two different
/// transactions — same parts, different ids.
#[test]
fn x15_ids_differ_between_v3_and_v2() {
    let input = always_true_box(101, TIP_HEIGHT - 1);
    let v3 = x15_tx_at(&input, FEE, ErgoTreeVersion::V3);
    let v2 = x15_tx_at(&input, FEE, ErgoTreeVersion::V2);
    assert_ne!(v3.id(), v2.id(), "X15 must hash differently at V3 and V2");
}

/// `process()` stores the version the caller passed it. Built and processed at
/// version 3, the entry carries 3.
#[test]
fn process_stores_the_version_v3() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let input = always_true_box(102, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));
    let tx = x15_tx_at(&input, FEE, ErgoTreeVersion::V3);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx,
        tx_bytes,
        ErgoTreeVersion::V3,
        &utxo,
        &state_context_at(TIP_HEIGHT),
        None,
    );
    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "setup: expected the tx to enter the pool, got {outcome:?}"
    );
    assert_eq!(
        mempool.get(&tx_id).expect("pooled").id_version,
        ErgoTreeVersion::V3,
    );
}

/// The same transaction's parts built and processed at version 2: the entry
/// carries 2, and its id is the V2 one, not the V3 one.
#[test]
fn process_stores_the_version_v2() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let input = always_true_box(103, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));
    let tx = x15_tx_at(&input, FEE, ErgoTreeVersion::V2);
    let tx_id = tx_id_bytes(&tx);
    assert_ne!(
        tx_id,
        tx_id_bytes(&x15_tx_at(&input, FEE, ErgoTreeVersion::V3)),
        "setup: the V2 id must differ from the V3 one"
    );
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx,
        tx_bytes,
        ErgoTreeVersion::V2,
        &utxo,
        &state_context_at(TIP_HEIGHT),
        None,
    );
    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "setup: expected the tx to enter the pool, got {outcome:?}"
    );
    assert_eq!(
        mempool.get(&tx_id).expect("pooled").id_version,
        ErgoTreeVersion::V2,
    );
}

/// The precondition: an X15 tx built at V3 handed over as V0 trips the
/// `debug_assert!`. Gated on `cfg(debug_assertions)` because the assertion is
/// stripped in release.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "tx.id() must match the id of its parts rebuilt at id_version")]
fn mismatched_id_version_trips_the_debug_assert() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let input = always_true_box(104, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));
    let tx = x15_tx_at(&input, FEE, ErgoTreeVersion::V3);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    // Built at V3, handed over as V0 — rebuild at V0 hashes to a different id,
    // so the assertion fires. Any other version that drops the Upcast would do.
    let _ = mempool.process(
        tx,
        tx_bytes,
        ErgoTreeVersion::V0,
        &utxo,
        &state_context_at(TIP_HEIGHT),
        None,
    );
}

/// `return_to_pool` keeps each entry's `id_version`. Two entries go in, one
/// labeled V3 and one labeled V2, and the pool holds them under those versions
/// — the pool never relabels an entry it was handed.
#[test]
fn return_to_pool_keeps_each_entrys_version() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let input_a = always_true_box(105, TIP_HEIGHT - 1);
    let input_b = always_true_box(106, TIP_HEIGHT - 1);

    let tx_a = x15_tx_at(&input_a, FEE, ErgoTreeVersion::V3);
    let tx_b = x15_tx_at(&input_b, FEE, ErgoTreeVersion::V2);
    let id_a = tx_id_bytes(&tx_a);
    let id_b = tx_id_bytes(&tx_b);
    assert_ne!(id_a, id_b, "setup: the two entries have distinct ids");

    let now = Instant::now();
    let entry = |tx: Transaction, id_version: ErgoTreeVersion| {
        let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
        UnconfirmedTx {
            cost: tx_bytes.len() as u32,
            tx,
            tx_bytes: tx_bytes.into(),
            id_version,
            fee: FEE,
            validation_cost: None,
            created: now,
            last_checked: now,
            source: None,
        }
    };
    mempool.return_to_pool(vec![
        entry(tx_a, ErgoTreeVersion::V3),
        entry(tx_b, ErgoTreeVersion::V2),
    ]);

    assert_eq!(
        mempool.get(&id_a).expect("A is pooled").id_version,
        ErgoTreeVersion::V3,
    );
    assert_eq!(
        mempool.get(&id_b).expect("B is pooled").id_version,
        ErgoTreeVersion::V2,
    );
}
