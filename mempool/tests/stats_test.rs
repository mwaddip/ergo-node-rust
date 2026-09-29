//! Fee statistics and the three fee queries, through the `Mempool` API
//! (`facts/mempool.md` § `FeeStats`, § Fee queries): `apply_block` records
//! each confirmation of a transaction the pool held, and `recommended_fee`,
//! `expected_wait_ms` and `pool_histogram` answer from the statistics and the
//! pool. The throughput window and the bins themselves are private, so their
//! tests are unit tests in `src/stats.rs`.
//!
//! Nothing sleeps: the `_at` forms take the time, and `return_to_pool` takes
//! each entry's `created`. Expected values are worked out by hand beside the
//! assertions.

mod common;

use std::time::{Duration, Instant};

use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use common::{make_box, spend_tx, tx_id_bytes, TIP_HEIGHT};
use ergo_mempool::stats::FeeHistogramBin;
use ergo_mempool::types::{FeeStrategy, MempoolConfig, UnconfirmedTx};
use ergo_mempool::Mempool;

/// Low enough that no answer here is floored, unless a test says otherwise.
const MIN_FEE: u64 = 1;

/// Entries are weighed `fee * 1024 / cost` (`FeePerCycle`), and `entry`
/// gives each a `cost` of 1024, so an entry's `fee_per_factor` is its `fee`.
fn mempool(min_fee: u64) -> Mempool {
    Mempool::new(MempoolConfig {
        fee_strategy: FeeStrategy::FeePerCycle,
        min_fee,
        ..MempoolConfig::default()
    })
}

/// A transaction of its own for each `seed`, spending a box of its own.
fn tx(seed: u8) -> Transaction {
    spend_tx(&[make_box(true, seed, TIP_HEIGHT - 1)], TIP_HEIGHT)
}

/// A pool entry for `tx` that entered the pool at `created`, with a
/// `fee_per_factor` of exactly `fee_per_factor`.
fn entry(tx: &Transaction, fee_per_factor: u64, created: Instant) -> UnconfirmedTx {
    UnconfirmedTx {
        tx: tx.clone(),
        tx_bytes: tx.sigma_serialize_bytes().expect("tx serialization").into(),
        fee: fee_per_factor,
        cost: 1024,
        validation_cost: None,
        created,
        last_checked: created,
        source: None,
    }
}

/// Pools `(seed, fee_per_factor, created)` entries.
fn pool(mempool: &mut Mempool, entries: &[(u8, u64, Instant)]) {
    mempool.return_to_pool(
        entries
            .iter()
            .map(|&(seed, fee_per_factor, created)| entry(&tx(seed), fee_per_factor, created))
            .collect(),
    );
}

/// Pools the entries, then confirms them all in one block at `at`.
fn confirm(mempool: &mut Mempool, at: Instant, entries: &[(u8, u64, Instant)]) {
    pool(mempool, entries);
    let block: Vec<Transaction> = entries.iter().map(|&(seed, ..)| tx(seed)).collect();
    let removed = mempool.apply_block_at(at, &block);
    assert_eq!(
        removed.len(),
        block.len(),
        "setup: the block confirms pooled transactions"
    );
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn mins(m: u64) -> Duration {
    Duration::from_secs(60 * m)
}

fn ms(m: u64) -> Duration {
    Duration::from_millis(m)
}

// ---------------------------------------------------------------------------
// recommended_fee
// ---------------------------------------------------------------------------

#[test]
fn recommended_fee_with_nothing_recorded_is_min_fee() {
    let mempool = mempool(1_000_000);
    assert_eq!(mempool.recommended_fee(0, 100), 1_000_000);
    assert_eq!(mempool.recommended_fee(59, 98_304), 1_000_000);
}

/// Bin 0 holds four confirmations after 30 s, of 1000, 1001, 1001 and 1001:
/// total 4003, average 4003 / 4 = 1000.
fn four_in_bin_zero(min_fee: u64) -> Mempool {
    let mut mempool = mempool(min_fee);
    let t0 = Instant::now();
    confirm(
        &mut mempool,
        t0 + secs(30),
        &[(1, 1000, t0), (2, 1001, t0), (3, 1001, t0), (4, 1001, t0)],
    );
    mempool
}

/// `(total_fee / n_txns) * tx_size / 1024`, in that order. For 10,000 bytes:
/// 1000 × 10,000 / 1024 = 9765.6, so 9765. Scaling the total first gives
/// 4003 × 10,000 / 1024 / 4 = 9772; dividing the size first gives
/// 1000 × (10,000 / 1024) = 1000 × 9 = 9000.
#[test]
fn recommended_fee_averages_then_scales_to_the_size() {
    assert_eq!(four_in_bin_zero(MIN_FEE).recommended_fee(0, 10_000), 9765);
}

/// The live node answered 61,255 against its `min_fee` of 1,000,000, so a
/// wallet that followed it built a transaction our own pool declines. Under
/// a `min_fee` of 5000, 10,000 bytes still answer 9765, but 1000 bytes,
/// 1000 × 1000 / 1024 = 976, answer 5000.
#[test]
fn recommended_fee_is_never_below_min_fee() {
    let mempool = four_in_bin_zero(5000);
    assert_eq!(mempool.recommended_fee(0, 10_000), 9765);
    assert_eq!(mempool.recommended_fee(0, 1000), 5000);
}

/// Bin 3 averages 2048 and bin 7 averages 1024. Through bin 2 there is
/// nothing, so `min_fee`; from bin 3 on, bin 3 answers although bin 7 is
/// cheaper. The first non-empty bin, not the cheapest in range, which was
/// the old statistic.
#[test]
fn recommended_fee_takes_the_first_non_empty_bin() {
    let mut mempool = mempool(MIN_FEE);
    let t0 = Instant::now();
    // Confirmed at t0 + 7:30, after 3:30 and 7:30 in the pool.
    confirm(
        &mut mempool,
        t0 + mins(7) + secs(30),
        &[(1, 2048, t0 + mins(4)), (2, 1024, t0)],
    );

    assert_eq!(mempool.recommended_fee(2, 1024), MIN_FEE);
    assert_eq!(mempool.recommended_fee(3, 1024), 2048);
    assert_eq!(mempool.recommended_fee(7, 1024), 2048);
    assert_eq!(mempool.recommended_fee(30, 1024), 2048);
}

/// Only bin 59 holds anything, 4096. 58 minutes find nothing; 59, 1000 and
/// `u32::MAX` all answer from bin 59, the last there is.
#[test]
fn recommended_fee_past_59_minutes_answers_as_59() {
    let mut mempool = mempool(MIN_FEE);
    let t0 = Instant::now();
    confirm(&mut mempool, t0 + mins(59) + secs(30), &[(1, 4096, t0)]);

    assert_eq!(mempool.recommended_fee(58, 1024), MIN_FEE);
    assert_eq!(mempool.recommended_fee(59, 1024), 4096);
    assert_eq!(mempool.recommended_fee(1000, 1024), 4096);
    assert_eq!(mempool.recommended_fee(u32::MAX, 1024), 4096);
}

// ---------------------------------------------------------------------------
// expected_wait_ms
// ---------------------------------------------------------------------------

/// Two pool transactions outweigh a fee of 0, but no block has confirmed a
/// pool transaction, so there is no rate yet.
#[test]
fn expected_wait_is_zero_before_any_confirmation() {
    let mut mempool = mempool(MIN_FEE);
    let t0 = Instant::now();
    pool(&mut mempool, &[(1, 5000, t0), (2, 4000, t0)]);

    assert_eq!(mempool.expected_wait_ms_at(t0 + mins(10), 0, 1), 0);
}

/// Pool weights 5000, 4000, 3000 and 1000; three confirmations at t0 + 10
/// min make `taken` 3. The window opened when the mempool was made, before
/// t0, so `elapsed` is capped at 60,000 ms.
/// - 1500 nanoERG for 512 bytes is 1500 × 1024 / 512 = 3000 per kB: 5000
///   and 4000 are ahead, 3000 is level and isn't. 60,000 × 2 / 3 = 40,000.
/// - 0 per kB: all four are ahead. 60,000 × 4 / 3 = 80,000.
/// - 6000 per kB: none is. 0.
#[test]
fn expected_wait_counts_only_heavier_transactions_as_ahead() {
    let mut mempool = mempool(MIN_FEE);
    let t0 = Instant::now();
    pool(
        &mut mempool,
        &[(1, 5000, t0), (2, 4000, t0), (3, 3000, t0), (4, 1000, t0)],
    );
    let at = t0 + mins(10);
    confirm(
        &mut mempool,
        at,
        &[(10, 700, t0), (11, 700, t0), (12, 700, t0)],
    );

    assert_eq!(mempool.expected_wait_ms_at(at, 1500, 512), 40_000);
    assert_eq!(mempool.expected_wait_ms_at(at, 0, 1), 80_000);
    assert_eq!(mempool.expected_wait_ms_at(at, 6000, 1024), 0);
}

// ---------------------------------------------------------------------------
// What apply_block records
// ---------------------------------------------------------------------------

/// A block confirming a rival spend of A's input removes A without recording
/// it. The control is the same block confirming A itself, which is recorded,
/// so the queries below do see a record when there is one.
#[test]
fn a_double_spend_removed_by_a_block_leaves_the_statistics_alone() {
    // A: 4096, spends box 1. B: 5000, spends box 2, and only waits.
    let setup = || {
        let mut mempool = mempool(MIN_FEE);
        let t0 = Instant::now();
        pool(&mut mempool, &[(1, 4096, t0), (2, 5000, t0)]);
        (mempool, t0)
    };
    let a = tx(1);

    let (mut rivalled, t0) = setup();
    // Box 1 again, into an output at another height: a different transaction.
    let rival = spend_tx(&[make_box(true, 1, TIP_HEIGHT - 1)], TIP_HEIGHT - 1);
    assert_ne!(tx_id_bytes(&rival), tx_id_bytes(&a));
    let at = t0 + mins(5);
    assert_eq!(rivalled.apply_block_at(at, &[rival]), vec![tx_id_bytes(&a)]);
    assert_eq!(
        rivalled.recommended_fee(59, 1024),
        MIN_FEE,
        "nothing binned"
    );
    assert_eq!(rivalled.expected_wait_ms_at(at, 0, 1), 0, "nothing taken");

    // Control: A confirmed after 5 minutes lands in bin 5, `taken` 1. B is
    // one ahead of a fee of 0: 60,000 × 1 / 1.
    let (mut confirmed, t0) = setup();
    let at = t0 + mins(5);
    assert_eq!(
        confirmed.apply_block_at(at, std::slice::from_ref(&a)),
        vec![tx_id_bytes(&a)]
    );
    assert_eq!(confirmed.recommended_fee(59, 1024), 4096);
    assert_eq!(confirmed.expected_wait_ms_at(at, 0, 1), 60_000);
}

// ---------------------------------------------------------------------------
// pool_histogram
// ---------------------------------------------------------------------------

/// 3 bins over 60,000 ms are 20,000 ms each, then one more for waits of
/// 60,000 ms or longer. Fees are powers of two, so each total says which
/// transactions its bin holds.
#[test]
fn pool_histogram_has_a_last_bin_for_waits_of_max_wait_or_longer() {
    let mut mempool = mempool(MIN_FEE);
    let now = Instant::now() + mins(60);
    let waited = |wait: Duration| now - wait;
    pool(
        &mut mempool,
        &[
            (1, 1, waited(ms(0))),
            (2, 2, waited(ms(19_999))),
            (3, 4, waited(ms(20_000))),
            (4, 8, waited(ms(59_999))),
            (5, 16, waited(ms(60_000))),
            (6, 32, waited(mins(60))),
        ],
    );

    assert_eq!(
        mempool.pool_histogram_at(now, 3, 60_000),
        vec![
            FeeHistogramBin {
                n_txns: 2,
                total_fee: 1 + 2
            },
            FeeHistogramBin {
                n_txns: 1,
                total_fee: 4
            },
            FeeHistogramBin {
                n_txns: 1,
                total_fee: 8
            },
            FeeHistogramBin {
                n_txns: 2,
                total_fee: 16 + 32
            },
        ]
    );
}

/// 4 bins over 7 ms: `interval` is 7 / 4 = 1 ms, so the first four bins
/// cover only 4 of the 7 ms. A 4 ms wait indexes 4, the last bin; a 6 ms
/// wait, short of `max_wait_ms`, indexes 6, past it. The JVM fails there;
/// here it lands in the last bin, with the 7 ms wait.
#[test]
fn pool_histogram_sends_an_overshooting_index_to_the_last_bin() {
    let mut mempool = mempool(MIN_FEE);
    let now = Instant::now() + secs(1);
    let waited = |wait: u64| now - ms(wait);
    pool(
        &mut mempool,
        &[
            (1, 1, waited(0)),
            (2, 2, waited(3)),
            (3, 4, waited(4)),
            (4, 8, waited(6)),
            (5, 16, waited(7)),
        ],
    );

    assert_eq!(
        mempool.pool_histogram_at(now, 4, 7),
        vec![
            FeeHistogramBin {
                n_txns: 1,
                total_fee: 1
            },
            FeeHistogramBin::default(),
            FeeHistogramBin::default(),
            FeeHistogramBin {
                n_txns: 1,
                total_fee: 2
            },
            FeeHistogramBin {
                n_txns: 3,
                total_fee: 4 + 8 + 16
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// Empty pool, and the preconditions
// ---------------------------------------------------------------------------

#[test]
fn an_empty_mempool_answers_min_fee_zero_and_empty_bins() {
    let mempool = mempool(1_000_000);
    assert_eq!(mempool.recommended_fee(10, 1000), 1_000_000);
    assert_eq!(mempool.expected_wait_ms(1_000_000, 1000), 0);
    assert_eq!(
        mempool.pool_histogram(2, 10),
        vec![FeeHistogramBin::default(); 3]
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "tx_size must be positive")]
fn expected_wait_states_its_size_precondition() {
    mempool(MIN_FEE).expected_wait_ms(1000, 0);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "bins must be at least 1")]
fn pool_histogram_states_its_bins_precondition() {
    mempool(MIN_FEE).pool_histogram(0, 10);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "max_wait_ms must be at least bins")]
fn pool_histogram_states_its_max_wait_precondition() {
    mempool(MIN_FEE).pool_histogram(10, 9);
}
