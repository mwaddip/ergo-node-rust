//! Step 6a: a creation height above the preheader is transient, not fatal.
//!
//! Fixtures live in `common/` — real `ErgoStateContext`, real `ErgoTree`s,
//! transactions that reach ergo-lib's evaluator. The bug under test is
//! invisible to a stubbed validator: it lives precisely in *which* outcome a
//! real validation failure is mapped to.

mod common;

use std::time::Instant;

use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use common::{make_box, spend_tx, state_context_at, tx_id_bytes, StaticUtxo, TIP_HEIGHT};
use ergo_mempool::types::{MempoolConfig, ProcessingOutcome, UnconfirmedTx};
use ergo_mempool::Mempool;

/// These fixtures spend into a single always-true output and pay no fee output,
/// so their fee is legitimately 0 and step 7b would decline them before they
/// could demonstrate anything about creation height. `min_fee = 0` isolates the
/// guard. Fee extraction itself is covered in `fee_test.rs`.
fn test_config() -> MempoolConfig {
    MempoolConfig {
        min_fee: 0,
        ..MempoolConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The whole point. A transaction one block ahead of us is declined, leaves no
/// trace in the invalidation cache, and is accepted on the retry after a block
/// is applied. The bug was that the retry never got that far: the first attempt
/// cached the tx as invalid, and step 1 dropped every rebroadcast for the next
/// `invalidation_ttl` without re-validating.
#[test]
fn future_creation_height_declines_then_succeeds_next_block() {
    let mut mempool = Mempool::new(test_config());

    let input = make_box(true, 1, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    // The sender is a block ahead of us: they built against tip TIP_HEIGHT,
    // so their outputs carry creation height TIP_HEIGHT + 1. Our preheader is
    // still describing block TIP_HEIGHT.
    let tx = spend_tx(&[input], TIP_HEIGHT + 1);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx.clone(),
        tx_bytes.clone(),
        &utxo,
        &state_context_at(TIP_HEIGHT),
        Some(7),
    );

    match &outcome {
        ProcessingOutcome::Declined { reason } => {
            assert!(
                reason.contains(&(TIP_HEIGHT + 1).to_string())
                    && reason.contains(&TIP_HEIGHT.to_string()),
                "the reason should name both heights, got: {reason}"
            );
        }
        other => panic!("expected Declined, got {other:?}"),
    }

    // The load-bearing assertion: nothing was cached.
    assert!(
        !mempool.is_invalidated(&tx_id),
        "a transient creation height must not enter the invalidation cache"
    );
    assert!(
        !mempool.contains(&tx_id),
        "declined tx is neither pooled nor remembered as invalid"
    );
    assert_eq!(mempool.len(), 0);

    // One block later, the very same transaction — as a rebroadcast would
    // deliver it — is now on time.
    let outcome = mempool.process(
        tx,
        tx_bytes,
        &utxo,
        &state_context_at(TIP_HEIGHT + 1),
        Some(7),
    );

    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "the retry must be re-validated and accepted, got {outcome:?}"
    );
    assert_eq!(mempool.len(), 1);
}

/// A transaction at exactly the preheader height is on time and must pass
/// through 6a untouched into real validation.
#[test]
fn creation_height_at_preheader_is_accepted() {
    let mut mempool = Mempool::new(test_config());

    let input = make_box(true, 2, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT), Some(7));

    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id: id } if id == tx_id),
        "a transaction at the preheader height is on time, got {outcome:?}"
    );
    assert_eq!(mempool.len(), 1);
}

/// The guard must not blunt the real path: a transaction whose script cannot be
/// satisfied is still permanently invalid and is still cached as such.
#[test]
fn unsatisfiable_script_still_invalidates_and_caches() {
    let mut mempool = Mempool::new(test_config());

    // sigmaProp(false) cannot be proven, and the empty proof does not satisfy it.
    let input = make_box(false, 3, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(
        tx.clone(),
        tx_bytes.clone(),
        &utxo,
        &state_context_at(TIP_HEIGHT),
        Some(7),
    );

    assert!(
        matches!(outcome, ProcessingOutcome::Invalidated { .. }),
        "an unsatisfiable script is permanently invalid, got {outcome:?}"
    );
    assert!(
        mempool.is_invalidated(&tx_id),
        "a genuine failure must still be cached"
    );

    // And the cache must still short-circuit the resubmission.
    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT), Some(7));
    assert!(
        matches!(outcome, ProcessingOutcome::Invalidated { ref reason } if reason.contains("previously invalidated")),
        "resubmission should be dropped by the cache, got {outcome:?}"
    );
}

/// A creation height with bit 31 set is "negative" under the V1 rules ergo-lib
/// still honours, so it is *not* the transient condition — it is permanently
/// malformed and must reach the invalidation cache. This pins the signed
/// comparison: an unsigned one would swallow these into the declined path and
/// re-validate the same garbage on every rebroadcast forever.
#[test]
fn negative_creation_height_is_invalidated_not_declined() {
    let mut mempool = Mempool::new(test_config());

    let input = make_box(true, 4, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], 1 << 31);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");

    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT), Some(7));

    assert!(
        matches!(outcome, ProcessingOutcome::Invalidated { .. }),
        "a negative creation height is malformed, not early, got {outcome:?}"
    );
    assert!(
        mempool.is_invalidated(&tx_id),
        "malformed heights belong in the cache"
    );
}

/// `revalidate()` re-runs validation against the current context and caches the
/// failures, so a pooled transaction ahead of our preheader — which a reorg
/// produces, since `return_to_pool()` re-inserts without validating — would be
/// invalidated by the same transient condition. It must be left alone instead.
#[test]
fn revalidate_skips_pooled_tx_ahead_of_preheader() {
    let mut config = test_config();
    // Revalidate on the next call rather than waiting out the interval.
    config.cleanup_interval = std::time::Duration::ZERO;
    let mut mempool = Mempool::new(config);

    let input = make_box(true, 5, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    // Accepted while the preheader was at TIP_HEIGHT + 1 ...
    let tx = spend_tx(&[input], TIP_HEIGHT + 1);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    let outcome = mempool.process(tx, tx_bytes, &utxo, &state_context_at(TIP_HEIGHT + 1), None);
    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { .. }),
        "setup: expected the tx to enter the pool, got {outcome:?}"
    );

    // ... and a reorg drops the preheader back a block underneath it.
    let removed = mempool.revalidate(&utxo, &state_context_at(TIP_HEIGHT));

    assert!(
        removed.is_empty(),
        "a transaction that is merely early must not be evicted"
    );
    assert!(
        !mempool.is_invalidated(&tx_id),
        "and it must certainly not be cached as invalid"
    );
    assert_eq!(
        mempool.len(),
        1,
        "it stays pooled until the chain catches up"
    );
}

/// The cleanup guard must not blunt cleanup either: a pooled transaction that
/// is genuinely invalid is still evicted and cached. Seeded through
/// `return_to_pool()`, which is how an unvalidated transaction really gets into
/// the pool — a reorg hands back the contents of the rolled-back blocks.
#[test]
fn revalidate_still_invalidates_genuine_failures() {
    let mut config = test_config();
    config.cleanup_interval = std::time::Duration::ZERO;
    let mut mempool = Mempool::new(config);

    // Unsatisfiable script, on time. Only the script is wrong.
    let input = make_box(false, 6, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let tx = spend_tx(&[input], TIP_HEIGHT);
    let tx_id = tx_id_bytes(&tx);
    let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
    let now = Instant::now();
    mempool.return_to_pool(vec![UnconfirmedTx {
        cost: tx_bytes.len() as u32,
        tx,
        tx_bytes,
        fee: 0,
        created: now,
        last_checked: now,
        source: None,
    }]);
    assert_eq!(mempool.len(), 1, "setup: return_to_pool skips validation");

    let removed = mempool.revalidate(&utxo, &state_context_at(TIP_HEIGHT));

    assert_eq!(removed, vec![tx_id], "a real failure is still evicted");
    assert!(
        mempool.is_invalidated(&tx_id),
        "and still cached as invalid"
    );
    assert_eq!(mempool.len(), 0);
}
