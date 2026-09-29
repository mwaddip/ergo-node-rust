use ergo_mempool::types::FeeStrategy;
use ergo_mempool::weight::TxWeight;

fn make_tx_id(seed: u8) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[0] = seed;
    id
}

#[test]
fn fee_per_byte_computation() {
    // fee = 1_000_000, size = 500 bytes
    // fee_per_factor = fee * 1024 / size = 1_000_000 * 1024 / 500 = 2_048_000
    let w = TxWeight::new(make_tx_id(1), 1_000_000, 500, 0, FeeStrategy::FeePerByte);
    assert_eq!(w.fee_per_factor, 1_000_000 * 1024 / 500);
    assert_eq!(
        w.weight, w.fee_per_factor,
        "initial weight equals fee_per_factor"
    );
}

#[test]
fn fee_per_byte_zero_size() {
    // Zero-size tx should not divide by zero
    let w = TxWeight::new(make_tx_id(1), 1_000_000, 0, 0, FeeStrategy::FeePerByte);
    assert_eq!(
        w.fee_per_factor, 0,
        "zero-size tx gives zero fee_per_factor"
    );
}

#[test]
fn fee_per_cycle_computation() {
    // fee = 2_000_000, cost = 1000
    // fee_per_factor = fee * 1024 / cost = 2_000_000 * 1024 / 1000 = 2_048_000
    let w = TxWeight::new(
        make_tx_id(1),
        2_000_000,
        500,
        1000,
        FeeStrategy::FeePerCycle,
    );
    assert_eq!(w.fee_per_factor, 2_000_000 * 1024 / 1000);
}

#[test]
fn fee_per_cycle_zero_cost_uses_fake() {
    // When cost is 0, FeePerCycle uses FAKE_COST (1000)
    let w = TxWeight::new(make_tx_id(1), 2_000_000, 500, 0, FeeStrategy::FeePerCycle);
    // fee_per_factor = 2_000_000 * 1024 / 1000 = 2_048_000
    assert_eq!(w.fee_per_factor, 2_000_000 * 1024 / 1000);
}

#[test]
fn ordering_highest_first() {
    // Higher fee_per_factor should sort BEFORE lower (BTreeMap ascending = highest priority first)
    let high = TxWeight::new(make_tx_id(1), 5_000_000, 500, 0, FeeStrategy::FeePerByte);
    let low = TxWeight::new(make_tx_id(2), 1_000_000, 500, 0, FeeStrategy::FeePerByte);

    // Ord: high.cmp(&low) should be Less (high sorts before low)
    assert!(
        high < low,
        "higher-fee tx should sort before lower-fee tx in BTreeMap ordering"
    );
}

#[test]
fn tiebreak_by_tx_id() {
    // Same fee, different tx_ids — should break ties by tx_id ascending
    let id_a = {
        let mut id = [0u8; 32];
        id[0] = 0x01;
        id
    };
    let id_b = {
        let mut id = [0u8; 32];
        id[0] = 0x02;
        id
    };

    let w_a = TxWeight::new(id_a, 1_000_000, 500, 0, FeeStrategy::FeePerByte);
    let w_b = TxWeight::new(id_b, 1_000_000, 500, 0, FeeStrategy::FeePerByte);

    // Same weight, so tiebreak by tx_id ascending: id_a < id_b
    assert!(w_a < w_b, "same weight should tiebreak by tx_id ascending");
    assert_ne!(w_a, w_b, "different tx_ids should not be equal");
}

/// Equality is the ordering's, on `weight` and `tx_id`: `Ord` requires
/// `a == b` exactly when `a.cmp(&b)` is `Equal`. `fee_per_factor` takes no
/// part.
#[test]
fn equality_agrees_with_the_ordering() {
    use std::cmp::Ordering;
    let weight = |weight, fee_per_factor, seed| TxWeight {
        weight,
        fee_per_factor,
        tx_id: make_tx_id(seed),
    };
    let base = weight(5_000, 1_000, 1);
    let cases = [
        (weight(5_000, 1_000, 1), true),
        (weight(5_000, 4_000, 1), true),
        (weight(6_000, 1_000, 1), false),
        (weight(5_000, 1_000, 2), false),
    ];
    for (other, equal) in cases {
        assert_eq!(base == other, equal, "== against {other:?}");
        assert_eq!(
            base.cmp(&other) == Ordering::Equal,
            equal,
            "cmp against {other:?}"
        );
    }
}

/// `fee * 1024` is computed wider than `u64`. 2 × 10^16 nanoERG for 1000
/// bytes: the product, 2.048 × 10^19, passes `u64::MAX` (about 1.8 × 10^19),
/// but the quotient, 2.048 × 10^16, fits and comes out exact.
#[test]
fn a_fee_past_u64_times_1024_is_weighed_exactly() {
    let w = TxWeight::new(
        make_tx_id(1),
        20_000_000_000_000_000,
        1000,
        0,
        FeeStrategy::FeePerByte,
    );
    assert_eq!(w.fee_per_factor, 20_480_000_000_000_000);
}

/// A quotient that doesn't fit saturates, where the old `u64` product
/// panicked in a debug build. `u64::MAX` for one byte is `u64::MAX × 1024`;
/// under `FeePerCycle` with no cost it is `u64::MAX × 1024 / 1000`. For 2048
/// bytes it fits: `u64::MAX / 2`, 9,223,372,036,854,775,807.
#[test]
fn a_fee_near_u64_max_saturates_instead_of_overflowing() {
    let id = make_tx_id(1);

    let per_byte = TxWeight::new(id, u64::MAX, 1, 0, FeeStrategy::FeePerByte);
    assert_eq!(
        (per_byte.fee_per_factor, per_byte.weight),
        (u64::MAX, u64::MAX)
    );

    let per_cycle = TxWeight::new(id, u64::MAX, 1, 0, FeeStrategy::FeePerCycle);
    assert_eq!(per_cycle.fee_per_factor, u64::MAX);

    let halved = TxWeight::new(id, u64::MAX, 2048, 0, FeeStrategy::FeePerByte);
    assert_eq!(halved.fee_per_factor, 9_223_372_036_854_775_807);
}
