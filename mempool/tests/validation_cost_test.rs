//! `UnconfirmedTx::validation_cost`: what the most recent successful
//! validation measured, with no floor, the cost `GET /transactions/unconfirmed*`
//! reports (`facts/mempool.md` § `UnconfirmedTx`, § Cleanup and
//! revalidation). Validation is real, so the costs are ergo-lib's own:
//! 10,000 to start the interpreter, 2,000 per input and 100 per output under
//! the default `Parameters`, and 1 to evaluate `sigmaProp(true)`.

mod common;

use std::time::{Duration, Instant};

use ergo_lib::chain::transaction::input::prover_result::ProverResult;
use ergo_lib::chain::transaction::input::Input;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::{ErgoBox, ErgoBoxCandidate, NonMandatoryRegisters};
use ergo_lib::ergotree_ir::mir::constant::Constant;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use common::{
    fee_tree, make_box, sigma_bool_tree, spend_tx, state_context_at, tx_id_bytes, StaticUtxo,
    BOX_VALUE, TIP_HEIGHT,
};
use ergo_mempool::types::{MempoolConfig, ProcessingOutcome, UnconfirmedTx};
use ergo_mempool::Mempool;

/// Above the default `min_fee`, so `bulky` reaches the pool.
const FEE: u64 = 2_000_000;

/// Spends `input` into four outputs that each carry a 3,500-byte register,
/// and a fee output: more bytes serialized than its validation costs, so the
/// weighting `cost` is floored at the size and `validation_cost` isn't.
fn bulky(input: &ErgoBox) -> Transaction {
    let output = |value: u64, ergo_tree, additional_registers| ErgoBoxCandidate {
        value: BoxValue::try_from(value).expect("output value above the minimum"),
        ergo_tree,
        tokens: None,
        additional_registers,
        creation_height: TIP_HEIGHT,
    };
    let bulk = || {
        NonMandatoryRegisters::try_from(vec![Constant::from(vec![7u8; 3_500])])
            .expect("one register")
    };
    let change = (*input.value.as_u64() - FEE) / 4;
    let outputs = vec![
        output(change, sigma_bool_tree(true), bulk()),
        output(change, sigma_bool_tree(true), bulk()),
        output(change, sigma_bool_tree(true), bulk()),
        output(change, sigma_bool_tree(true), bulk()),
        output(FEE, fee_tree(), NonMandatoryRegisters::empty()),
    ];
    let spend = Input::new(
        input.box_id(),
        ProverResult {
            proof: ProofBytes::Empty,
            extension: ContextExtension::empty(),
        },
    );
    Transaction::new_from_vec(vec![spend], vec![], outputs).expect("transaction construction")
}

/// A pool entry as a rollback hands it back, entered and last checked `at`.
fn returned(tx: &Transaction, validation_cost: Option<u64>, at: Instant) -> UnconfirmedTx {
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    UnconfirmedTx {
        cost: tx_bytes.len() as u32,
        tx: tx.clone(),
        tx_bytes: tx_bytes.into(),
        fee: 0,
        validation_cost,
        created: at,
        last_checked: at,
        source: None,
    }
}

/// One input and five outputs: 10,000 + 2,000 + 5 × 100 + 1 = 12,501, below
/// the transaction's size. The weighting `cost` is the size.
#[test]
fn entry_records_the_measured_cost_without_the_floor() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let input = make_box(true, 1, TIP_HEIGHT - 1);
    assert_eq!(*input.value.as_u64(), BOX_VALUE, "setup: splits evenly");
    let tx = bulky(&input);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    let size = tx_bytes.len();
    assert!(size > 12_501, "setup: {size} bytes must outweigh the cost");

    let outcome = mempool.process(
        tx,
        tx_bytes,
        &StaticUtxo::new(&[input]),
        &state_context_at(TIP_HEIGHT),
        None,
    );
    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { .. }),
        "setup: expected the tx to enter the pool, got {outcome:?}"
    );

    let utx = mempool.get(&tx_id).expect("pooled");
    assert_eq!(utx.validation_cost, Some(12_501));
    assert_eq!(utx.cost as usize, size, "the weighting cost is floored");
}

#[test]
fn return_to_pool_carries_no_cost() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let tx = spend_tx(&[make_box(true, 2, TIP_HEIGHT - 1)], TIP_HEIGHT);

    mempool.return_to_pool(vec![returned(&tx, None, Instant::now())]);

    let utx = mempool
        .get(&tx_id_bytes(&tx))
        .expect("returned to the pool");
    assert_eq!(utx.validation_cost, None);
}

/// A successful revalidation records the cost it measured and the time it
/// ran, the JVM's `withCost`, and that time decides the next pass. A pass
/// checks a transaction only once more than `cleanup_interval` has passed
/// since its `last_checked`: exactly the interval is too soon, as for the JVM,
/// which re-checks past its `TimeLimit`.
#[test]
fn revalidation_records_cost_and_time_so_the_next_pass_skips() {
    let config = MempoolConfig::default();
    let interval = config.cleanup_interval;
    let millisecond = Duration::from_millis(1);
    let mut mempool = Mempool::new(config);
    let context = state_context_at(TIP_HEIGHT);

    let input = make_box(true, 3, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));
    let tx = spend_tx(std::slice::from_ref(&input), TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let t0 = Instant::now();
    mempool.return_to_pool(vec![returned(&tx, None, t0)]);

    // Exactly the interval after entry: too soon.
    assert!(mempool
        .revalidate_at(t0 + interval, &utxo, &context)
        .is_empty());
    let utx = mempool.get(&tx_id).expect("still pooled");
    assert_eq!(
        (utx.validation_cost, utx.last_checked),
        (None, t0),
        "skipped"
    );

    // A millisecond more: checked.
    let checked = t0 + interval + millisecond;
    let removed = mempool.revalidate_at(checked, &utxo, &context);
    assert!(removed.is_empty(), "setup: the transaction is valid");
    let utx = mempool.get(&tx_id).expect("still pooled");
    // One input, one output: 10,000 + 2,000 + 100 + 1.
    assert_eq!(utx.validation_cost, Some(12_101));
    assert_eq!(utx.last_checked, checked);
    assert_eq!(utx.created, t0, "entering the pool happened once");

    // With its input gone, any pass that checks the transaction removes it.
    // Through exactly `cleanup_interval` after the last check, none does.
    let gone = StaticUtxo::new(&[]);
    assert!(mempool.revalidate_at(checked, &gone, &context).is_empty());
    assert!(mempool
        .revalidate_at(checked + interval, &gone, &context)
        .is_empty());
    assert_eq!(
        mempool.revalidate_at(checked + interval + millisecond, &gone, &context),
        vec![tx_id]
    );
}

/// The cost is the latest measurement, not the first one kept. Seeded with
/// a cost nothing measured, which `return_to_pool` keeps as given.
#[test]
fn revalidation_replaces_the_cost_the_entry_carried() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let input = make_box(true, 4, TIP_HEIGHT - 1);
    let tx = spend_tx(std::slice::from_ref(&input), TIP_HEIGHT);
    let t0 = Instant::now();
    mempool.return_to_pool(vec![returned(&tx, Some(1), t0)]);
    assert_eq!(
        mempool
            .get(&tx_id_bytes(&tx))
            .expect("pooled")
            .validation_cost,
        Some(1),
        "setup: kept as given"
    );

    let removed = mempool.revalidate_at(
        t0 + Duration::from_secs(60),
        &StaticUtxo::new(&[input]),
        &state_context_at(TIP_HEIGHT),
    );

    assert!(removed.is_empty(), "setup: the transaction is valid");
    assert_eq!(
        mempool
            .get(&tx_id_bytes(&tx))
            .expect("pooled")
            .validation_cost,
        Some(12_101)
    );
}
