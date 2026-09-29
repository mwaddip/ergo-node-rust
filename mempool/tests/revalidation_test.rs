//! A revalidation pass on the JVM's budget, through the public API
//! (`facts/mempool.md` § Cleanup and revalidation). The pass's order and its
//! budget arithmetic are unit tests in `src/cleanup.rs`, which can run a pass
//! on a limit a few validations reach; the interval boundary is in
//! `validation_cost_test.rs`.

mod common;

use std::time::{Duration, Instant};

use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use common::{make_box, spend_tx, state_context_at, tx_id_bytes, StaticUtxo, TIP_HEIGHT};
use ergo_mempool::types::{MempoolConfig, UnconfirmedTx};
use ergo_mempool::{Mempool, CLEANUP_COST_LIMIT};

/// The JVM's `CleanupWorker.CostLimit`.
#[test]
fn a_pass_may_spend_the_jvms_cost_limit() {
    assert_eq!(CLEANUP_COST_LIMIT, 7_000_000);
}

/// `cost_per_block` limits what remote transactions may cost between blocks
/// and has no say over a pass. At 1 it used to end a pass after its first
/// transaction; all three are checked now.
#[test]
fn cost_per_block_does_not_bound_a_pass() {
    let mut mempool = Mempool::new(MempoolConfig {
        cost_per_block: 1,
        ..MempoolConfig::default()
    });
    let inputs = [1, 2, 3].map(|seed| make_box(true, seed, TIP_HEIGHT - 1));
    let t0 = Instant::now();
    let entries: Vec<UnconfirmedTx> = inputs
        .iter()
        .map(|input| {
            let tx = spend_tx(std::slice::from_ref(input), TIP_HEIGHT);
            let tx_bytes = tx.sigma_serialize_bytes().expect("tx serialization");
            UnconfirmedTx {
                cost: tx_bytes.len() as u32,
                tx,
                tx_bytes: tx_bytes.into(),
                fee: 0,
                validation_cost: None,
                created: t0,
                last_checked: t0,
                source: None,
            }
        })
        .collect();
    let ids: Vec<[u8; 32]> = entries.iter().map(|utx| tx_id_bytes(&utx.tx)).collect();
    mempool.return_to_pool(entries);

    let now = t0 + Duration::from_secs(3600);
    let removed = mempool.revalidate_at(
        now,
        &StaticUtxo::new(&inputs),
        &state_context_at(TIP_HEIGHT),
    );

    assert!(removed.is_empty());
    for id in &ids {
        let utx = mempool.get(id).expect("still pooled");
        // One input, one output: 10,000 + 2,000 + 100 + 1.
        assert_eq!((utx.validation_cost, utx.last_checked), (Some(12_101), now));
    }
}
