//! The serving reader: `reader().tx_bytes(id)` is `Some` exactly while `id` is
//! in the pool, hands back the pool entry's own allocation, and neither blocks
//! on the owner nor tears under concurrent mutation.
//!
//! Every removal path is driven through the real `Mempool` API with fixtures
//! from `common/`, because the property under test is that each path reaches
//! the pool's single removal step. A test against `OrderedPool` alone would
//! pass while a path that bypassed it left a stale entry behind.
//!
//! The poisoned-lock case is a unit test in `src/reader.rs`: only a holder of
//! the lock can poison it, and nothing outside that module holds it.

mod common;

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use common::{
    fee_tree, make_box, sigma_bool_tree, spend_tx_to, state_context_at, tx_id_bytes, StaticUtxo,
    TIP_HEIGHT,
};
use ergo_mempool::reader::MempoolReader;
use ergo_mempool::types::{MempoolConfig, ProcessingOutcome, UnconfirmedTx};
use ergo_mempool::Mempool;

/// Above the default `min_fee`, so these transactions reach the pool.
const FEE: u64 = 2_000_000;

/// A transaction spending `input` into change plus a fee output of `fee`.
fn paying(input: &ErgoBox, fee: u64) -> Transaction {
    spend_tx_to(
        std::slice::from_ref(input),
        &[
            (*input.value.as_u64() - fee, sigma_bool_tree(true)),
            (fee, fee_tree()),
        ],
        TIP_HEIGHT,
    )
}

fn serialize(tx: &Transaction) -> Vec<u8> {
    tx.sigma_serialize_bytes().expect("tx serialization")
}

/// Submit `tx` against `utxo`, returning the outcome and the bytes submitted.
fn submit(
    mempool: &mut Mempool,
    tx: &Transaction,
    utxo: &StaticUtxo,
) -> (ProcessingOutcome, Vec<u8>) {
    let bytes = serialize(tx);
    let outcome = mempool.process(
        tx.clone(),
        bytes.clone(),
        utxo,
        &state_context_at(TIP_HEIGHT),
        None,
    );
    (outcome, bytes)
}

/// Submit `tx`, asserting it was accepted and is served, so a removal test
/// starts from an entry the reader really had. Returns its id and submitted
/// bytes.
fn accept(mempool: &mut Mempool, tx: &Transaction, utxo: &StaticUtxo) -> ([u8; 32], Vec<u8>) {
    let (outcome, bytes) = submit(mempool, tx, utxo);
    let id = tx_id_bytes(tx);
    assert!(
        matches!(outcome, ProcessingOutcome::Accepted { tx_id } if tx_id == id),
        "setup: expected the tx to enter the pool, got {outcome:?}"
    );
    assert_eq!(
        mempool.reader().tx_bytes(&id).as_deref(),
        Some(&bytes[..]),
        "setup: an accepted tx must be served"
    );
    (id, bytes)
}

/// A pool entry for `return_to_pool`, sharing `bytes` rather than copying it.
fn unconfirmed(tx: &Transaction, bytes: &Arc<[u8]>) -> UnconfirmedTx {
    let now = Instant::now();
    UnconfirmedTx {
        tx: tx.clone(),
        tx_bytes: Arc::clone(bytes),
        fee: FEE,
        cost: bytes.len() as u32,
        validation_cost: None,
        created: now,
        last_checked: now,
        source: None,
    }
}

fn revalidating_config() -> MempoolConfig {
    MempoolConfig {
        // Revalidate on the next call rather than waiting out the interval.
        cleanup_interval: Duration::ZERO,
        ..MempoolConfig::default()
    }
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// The main crate moves the reader into the P2P router's serve closure, which
/// is `Send + Sync + 'static`.
#[test]
fn reader_is_clone_send_sync() {
    fn bounds<T: Clone + Send + Sync + 'static>() {}
    bounds::<MempoolReader>();
}

/// Handles taken before an insert, after it, and cloned from either all see
/// it, with exactly the bytes submitted.
#[test]
fn reader_sees_an_insert() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let early = mempool.reader();

    let input = make_box(true, 1, TIP_HEIGHT - 1);
    let tx = paying(&input, FEE);
    assert_eq!(early.tx_bytes(&tx_id_bytes(&tx)), None);

    let (id, bytes) = accept(&mut mempool, &tx, &StaticUtxo::new(&[input]));

    for (which, reader) in [
        ("taken before the insert", &early),
        ("taken after it", &mempool.reader()),
        ("cloned", &early.clone()),
    ] {
        assert_eq!(
            reader.tx_bytes(&id).as_deref(),
            Some(&bytes[..]),
            "a handle {which} must serve the submitted bytes"
        );
    }
}

/// Shared, not copied: the reader hands back the pool entry's own allocation.
#[test]
fn reader_shares_the_pool_entrys_allocation() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let input = make_box(true, 2, TIP_HEIGHT - 1);
    let (id, _) = accept(
        &mut mempool,
        &paying(&input, FEE),
        &StaticUtxo::new(&[input]),
    );

    let served = reader.tx_bytes(&id).expect("pooled");
    let pooled = &mempool.get(&id).expect("pooled").tx_bytes;
    assert!(
        Arc::ptr_eq(pooled, &served),
        "the reader must return the entry's allocation, not a copy"
    );
}

/// The reorg path inserts through the same step, and keeps the caller's
/// allocation too.
#[test]
fn returned_to_pool_is_served_from_the_same_allocation() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let tx = paying(&make_box(true, 3, TIP_HEIGHT - 1), FEE);
    let id = tx_id_bytes(&tx);
    let bytes: Arc<[u8]> = serialize(&tx).into();
    mempool.return_to_pool(vec![unconfirmed(&tx, &bytes)]);

    let served = reader.tx_bytes(&id).expect("returned to the pool");
    assert!(Arc::ptr_eq(&bytes, &served));
}

// ---------------------------------------------------------------------------
// Every removal path
// ---------------------------------------------------------------------------

#[test]
fn invalidate_takes_it_out_of_the_reader() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let input = make_box(true, 10, TIP_HEIGHT - 1);
    let (id, _) = accept(
        &mut mempool,
        &paying(&input, FEE),
        &StaticUtxo::new(&[input]),
    );

    mempool.invalidate(&id);

    assert!(mempool.is_invalidated(&id));
    assert_eq!(mempool.len(), 0);
    assert_eq!(reader.tx_bytes(&id), None);
}

#[test]
fn capacity_eviction_takes_it_out_of_the_reader() {
    let mut mempool = Mempool::new(MempoolConfig {
        capacity: 1,
        ..MempoolConfig::default()
    });
    let reader = mempool.reader();

    let cheap_input = make_box(true, 11, TIP_HEIGHT - 1);
    let rich_input = make_box(true, 12, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(&[cheap_input.clone(), rich_input.clone()]);

    let (cheap, _) = accept(&mut mempool, &paying(&cheap_input, FEE), &utxo);
    let (rich, rich_bytes) = accept(&mut mempool, &paying(&rich_input, 2 * FEE), &utxo);

    assert_eq!(
        mempool.len(),
        1,
        "setup: the cheaper tx must have been evicted"
    );
    assert!(!mempool.contains(&cheap));
    assert_eq!(reader.tx_bytes(&cheap), None);
    assert_eq!(reader.tx_bytes(&rich).as_deref(), Some(&rich_bytes[..]));
}

/// A double-spend loser never enters the reader; a replaced transaction
/// leaves it.
#[test]
fn replace_by_fee_takes_the_loser_out_of_the_reader() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let input = make_box(true, 13, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let (first, first_bytes) = accept(&mut mempool, &paying(&input, FEE), &utxo);

    let cheaper = paying(&input, FEE * 3 / 4);
    let (outcome, _) = submit(&mut mempool, &cheaper, &utxo);
    assert!(
        matches!(outcome, ProcessingOutcome::DoubleSpendLoser { .. }),
        "setup: the cheaper double-spend must lose, got {outcome:?}"
    );
    assert_eq!(reader.tx_bytes(&tx_id_bytes(&cheaper)), None);
    assert_eq!(reader.tx_bytes(&first).as_deref(), Some(&first_bytes[..]));

    let richer = paying(&input, 2 * FEE);
    let (outcome, richer_bytes) = submit(&mut mempool, &richer, &utxo);
    assert!(
        matches!(&outcome, ProcessingOutcome::Replaced { removed, .. } if *removed == [first]),
        "setup: the richer double-spend must replace the first, got {outcome:?}"
    );
    assert_eq!(reader.tx_bytes(&first), None);
    assert_eq!(
        reader.tx_bytes(&tx_id_bytes(&richer)).as_deref(),
        Some(&richer_bytes[..])
    );
}

#[test]
fn block_confirmation_takes_it_out_of_the_reader() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let input = make_box(true, 14, TIP_HEIGHT - 1);
    let tx = paying(&input, FEE);
    let (id, _) = accept(&mut mempool, &tx, &StaticUtxo::new(&[input]));

    assert_eq!(mempool.apply_block(&[tx]), vec![id]);
    assert_eq!(reader.tx_bytes(&id), None);
}

/// `apply_block`'s second removal: a pooled transaction spending an input the
/// block confirmed under a different transaction.
#[test]
fn block_confirming_a_rival_spend_takes_it_out_of_the_reader() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let input = make_box(true, 15, TIP_HEIGHT - 1);
    let (pooled, _) = accept(
        &mut mempool,
        &paying(&input, FEE),
        &StaticUtxo::new(std::slice::from_ref(&input)),
    );

    let rival = paying(&input, 2 * FEE);
    assert_eq!(mempool.apply_block(&[rival]), vec![pooled]);
    assert_eq!(reader.tx_bytes(&pooled), None);
}

/// Revalidation's first removal: an input that is gone from the UTXO set.
#[test]
fn revalidation_of_a_vanished_input_takes_it_out_of_the_reader() {
    let mut mempool = Mempool::new(revalidating_config());
    let reader = mempool.reader();

    let input = make_box(true, 16, TIP_HEIGHT - 1);
    let (id, _) = accept(
        &mut mempool,
        &paying(&input, FEE),
        &StaticUtxo::new(&[input]),
    );

    let removed = mempool.revalidate(&StaticUtxo::new(&[]), &state_context_at(TIP_HEIGHT));

    assert_eq!(removed, vec![id]);
    assert_eq!(reader.tx_bytes(&id), None);
}

/// Revalidation's second removal: a script that no longer validates. Seeded
/// through `return_to_pool()`, the one way an unvalidated tx enters the pool.
#[test]
fn revalidation_of_an_invalid_script_takes_it_out_of_the_reader() {
    let mut mempool = Mempool::new(revalidating_config());
    let reader = mempool.reader();

    let input = make_box(false, 17, TIP_HEIGHT - 1);
    let tx = paying(&input, FEE);
    let id = tx_id_bytes(&tx);
    mempool.return_to_pool(vec![unconfirmed(&tx, &serialize(&tx).into())]);
    assert!(reader.tx_bytes(&id).is_some(), "setup: served once pooled");

    let removed = mempool.revalidate(&StaticUtxo::new(&[input]), &state_context_at(TIP_HEIGHT));

    assert_eq!(removed, vec![id]);
    assert!(mempool.is_invalidated(&id));
    assert_eq!(reader.tx_bytes(&id), None);
}

// ---------------------------------------------------------------------------
// Re-weighting
// ---------------------------------------------------------------------------

/// A child spending a pooled parent's output re-keys the parent in the pool.
/// Membership doesn't change, so neither does anything the reader returns:
/// same bytes, same allocation.
#[test]
fn family_reweight_changes_nothing_the_reader_returns() {
    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();

    let input = make_box(true, 20, TIP_HEIGHT - 1);
    let utxo = StaticUtxo::new(std::slice::from_ref(&input));

    let parent = paying(&input, FEE);
    let (parent_id, parent_bytes) = accept(&mut mempool, &parent, &utxo);
    let before = reader.tx_bytes(&parent_id).expect("pooled");

    // The child pays twice the parent's fee, so without the re-weight it
    // would sort first. It spends the parent's change, resolved from the pool.
    let change = parent
        .outputs
        .as_slice()
        .first()
        .expect("change output")
        .clone();
    let (child_id, child_bytes) = accept(&mut mempool, &paying(&change, 2 * FEE), &utxo);

    let order: Vec<[u8; 32]> = mempool
        .all_prioritized()
        .iter()
        .map(|utx| tx_id_bytes(&utx.tx))
        .collect();
    assert_eq!(
        order,
        vec![parent_id, child_id],
        "setup: the parent must have been re-weighted above its child"
    );

    let after = reader.tx_bytes(&parent_id).expect("still pooled");
    assert_eq!(&*after, &parent_bytes[..]);
    assert!(
        Arc::ptr_eq(&before, &after),
        "a re-weight must leave the index entry alone"
    );
    assert_eq!(
        reader.tx_bytes(&child_id).as_deref(),
        Some(&child_bytes[..])
    );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Readers on other threads look up transactions while the owner flips them
/// in and out of the pool. Every answer is either `None` or exactly that
/// transaction's bytes, and every thread finishes. Threads report over a
/// channel with a deadline, so a deadlock fails the test instead of hanging it.
#[test]
fn concurrent_reads_while_the_owner_mutates() {
    const READERS: usize = 4;
    const MIN_ROUNDS: usize = 200;
    const DEADLINE: Duration = Duration::from_secs(30);

    let txs: Vec<Transaction> = (0..8)
        .map(|seed| paying(&make_box(true, 100 + seed, TIP_HEIGHT - 1), FEE))
        .collect();
    let ids: Vec<[u8; 32]> = txs.iter().map(tx_id_bytes).collect();
    // An independent copy of each transaction's bytes: what a reader must
    // return whenever it returns anything.
    let expected: Arc<Vec<([u8; 32], Vec<u8>)>> = Arc::new(
        txs.iter()
            .map(|tx| (tx_id_bytes(tx), serialize(tx)))
            .collect(),
    );
    let pooled: Vec<(Transaction, Arc<[u8]>)> = txs
        .iter()
        .map(|tx| (tx.clone(), serialize(tx).into()))
        .collect();

    let mut mempool = Mempool::new(MempoolConfig::default());
    let reader = mempool.reader();
    let stop = Arc::new(AtomicBool::new(false));
    let saw_hit = Arc::new(AtomicBool::new(false));
    let saw_miss = Arc::new(AtomicBool::new(false));
    let (done, finished) = mpsc::channel::<thread::Result<()>>();

    for _ in 0..READERS {
        let reader = reader.clone();
        let expected = Arc::clone(&expected);
        let stop = Arc::clone(&stop);
        let saw_hit = Arc::clone(&saw_hit);
        let saw_miss = Arc::clone(&saw_miss);
        let done = done.clone();
        thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                for (id, want) in expected.iter().cycle() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match reader.tx_bytes(id) {
                        Some(got) => {
                            assert_eq!(&*got, &want[..], "torn answer for {}", hex::encode(id));
                            saw_hit.store(true, Ordering::Relaxed);
                        }
                        None => saw_miss.store(true, Ordering::Relaxed),
                    }
                }
            }));
            let _ = done.send(result);
        });
    }

    {
        let stop = Arc::clone(&stop);
        let saw_hit = Arc::clone(&saw_hit);
        let saw_miss = Arc::clone(&saw_miss);
        let done = done.clone();
        thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let own = mempool.reader();
                let give_up = Instant::now() + DEADLINE;
                let mut rounds = 0;
                // Until both states have been seen from another thread: a run
                // that only ever answered `None` would prove nothing.
                while rounds < MIN_ROUNDS
                    || !(saw_hit.load(Ordering::Relaxed) && saw_miss.load(Ordering::Relaxed))
                {
                    assert!(
                        Instant::now() < give_up,
                        "readers never saw both a pooled and an unpooled state"
                    );
                    mempool.return_to_pool(
                        pooled
                            .iter()
                            .map(|(tx, bytes)| unconfirmed(tx, bytes))
                            .collect(),
                    );
                    for (id, (_, bytes)) in ids.iter().zip(&pooled) {
                        let served = own.tx_bytes(id).expect("pooled this round");
                        assert!(Arc::ptr_eq(bytes, &served));
                    }
                    mempool.apply_block(&txs);
                    for id in &ids {
                        assert_eq!(own.tx_bytes(id), None);
                    }
                    rounds += 1;
                }
            }));
            stop.store(true, Ordering::Relaxed);
            let _ = done.send(result);
        });
    }
    drop(done);

    let give_up = Instant::now() + DEADLINE + Duration::from_secs(10);
    for _ in 0..=READERS {
        match finished.recv_timeout(give_up.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => {}
            Ok(Err(panicked)) => panic::resume_unwind(panicked),
            Err(e) => panic!("a thread never finished ({e}): deadlock"),
        }
    }
}
