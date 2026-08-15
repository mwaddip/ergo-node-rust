//! Step 7a: the fee is the value of the outputs paying the fee proposition.
//!
//! The defect these pin down: `extract_fee` used to return
//! `input_sum - output_sum`, but ergo-lib enforces *exact* ERG preservation, so
//! that difference is structurally zero for every transaction that survives
//! validation. Under the default `min_fee` of 1,000,000 the mempool therefore
//! declined the entire network. Ergo has no implicit change-to-fee remainder —
//! the fee is an explicit output guarded by the fee proposition, which is what
//! the JVM's `ErgoMemPool.extractFee` filters on.

mod common;

use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use common::{
    fee_tree, make_box, sigma_bool_tree, spend_tx_to, state_context_at, tx_id_bytes, StaticUtxo,
    BOX_VALUE, TIP_HEIGHT,
};
use ergo_mempool::process::extract_fee;
use ergo_mempool::types::{MempoolConfig, ProcessingOutcome};
use ergo_mempool::Mempool;

const FEE: u64 = 2_000_000;

/// An explicit fee output is counted at its own value.
#[test]
fn fee_output_is_counted() {
    let input = make_box(true, 1, TIP_HEIGHT - 1);
    let tx = spend_tx_to(
        &[input],
        &[(BOX_VALUE - FEE, sigma_bool_tree(true)), (FEE, fee_tree())],
        TIP_HEIGHT,
    );

    assert_eq!(extract_fee(&tx, &fee_tree()), FEE);
}

/// The regression test for the original defect. Every transaction that ergo-lib
/// accepts has `input_sum == output_sum` — that is enforced, not incidental — so
/// the old difference-based implementation returned 0 here. Asserting the
/// balance explicitly is the point: it is exactly the condition under which the
/// old code was wrong.
#[test]
fn exact_erg_preservation_still_yields_a_nonzero_fee() {
    let input = make_box(true, 2, TIP_HEIGHT - 1);
    let tx = spend_tx_to(
        &[input],
        &[(BOX_VALUE - FEE, sigma_bool_tree(true)), (FEE, fee_tree())],
        TIP_HEIGHT,
    );

    let input_sum = BOX_VALUE;
    let output_sum: u64 = tx.outputs.iter().map(|b| *b.value.as_u64()).sum();
    assert_eq!(
        input_sum, output_sum,
        "ergo-lib enforces this for every valid transaction"
    );

    assert_eq!(
        extract_fee(&tx, &fee_tree()),
        FEE,
        "input_sum - output_sum would be 0 here — that was the bug"
    );
}

/// Several fee outputs sum, matching the JVM's `.filter(...).map(_.value).sum`.
#[test]
fn multiple_fee_outputs_sum() {
    let input = make_box(true, 3, TIP_HEIGHT - 1);
    let tx = spend_tx_to(
        &[input],
        &[
            (BOX_VALUE - 2 * FEE, sigma_bool_tree(true)),
            (FEE, fee_tree()),
            (FEE, fee_tree()),
        ],
        TIP_HEIGHT,
    );

    assert_eq!(extract_fee(&tx, &fee_tree()), 2 * FEE);
}

/// Only the proposition decides. An ordinary output of identical value is not a
/// fee — the filter is on the guarding tree, never on the amount.
#[test]
fn non_fee_output_of_equal_value_is_not_counted() {
    let input = make_box(true, 4, TIP_HEIGHT - 1);
    let tx = spend_tx_to(
        &[input],
        &[
            (BOX_VALUE - FEE, sigma_bool_tree(true)),
            // Same value as the fee in the other tests, ordinary script.
            (FEE, sigma_bool_tree(true)),
        ],
        TIP_HEIGHT,
    );

    assert_eq!(
        extract_fee(&tx, &fee_tree()),
        0,
        "value alone must not make an output a fee"
    );
}

/// No fee output means no fee. Declining at 7b is correct behaviour, not a
/// regression — the transaction genuinely pays the miner nothing.
#[test]
fn no_fee_output_yields_zero_and_is_declined() {
    let mut mempool = Mempool::new(MempoolConfig::default());

    let input = make_box(true, 5, TIP_HEIGHT - 1);
    let tx = spend_tx_to(
        std::slice::from_ref(&input),
        &[(BOX_VALUE, sigma_bool_tree(true))],
        TIP_HEIGHT,
    );
    assert_eq!(extract_fee(&tx, &fee_tree()), 0);

    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    let outcome = mempool.process(
        tx,
        tx_bytes,
        &StaticUtxo::new(&[input]),
        &state_context_at(TIP_HEIGHT),
        Some(7),
    );

    match &outcome {
        ProcessingOutcome::Declined { reason } => {
            assert!(
                reason.contains("fee 0"),
                "should decline on the fee, got: {reason}"
            );
        }
        other => panic!("expected Declined, got {other:?}"),
    }
    assert_eq!(mempool.len(), 0);
}

/// End to end at the real default `min_fee`: a fee-paying transaction now
/// reaches the pool. This is the whole point of the change — before it, no
/// transaction on the network could clear step 7b at all.
#[test]
fn fee_paying_tx_is_accepted_at_the_default_min_fee() {
    let config = MempoolConfig::default();
    assert_eq!(
        config.min_fee, 1_000_000,
        "guarding the premise of this test"
    );
    assert!(FEE >= config.min_fee);
    let mut mempool = Mempool::new(config);

    let input = make_box(true, 6, TIP_HEIGHT - 1);
    let tx = spend_tx_to(
        std::slice::from_ref(&input),
        &[(BOX_VALUE - FEE, sigma_bool_tree(true)), (FEE, fee_tree())],
        TIP_HEIGHT,
    );
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx,
        tx_bytes,
        &StaticUtxo::new(&[input]),
        &state_context_at(TIP_HEIGHT),
        Some(7),
    );

    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "a fee-paying transaction must reach the pool, got {outcome:?}"
    );
    assert_eq!(mempool.get(&tx_id).expect("pooled").fee, FEE);
}
