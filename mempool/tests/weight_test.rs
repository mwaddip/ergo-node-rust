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
    assert_eq!(w.weight, w.fee_per_factor, "initial weight equals fee_per_factor");
}

#[test]
fn fee_per_byte_zero_size() {
    // Zero-size tx should not divide by zero
    let w = TxWeight::new(make_tx_id(1), 1_000_000, 0, 0, FeeStrategy::FeePerByte);
    assert_eq!(w.fee_per_factor, 0, "zero-size tx gives zero fee_per_factor");
}

#[test]
fn fee_per_cycle_computation() {
    // fee = 2_000_000, cost = 1000
    // fee_per_factor = fee * 1024 / cost = 2_000_000 * 1024 / 1000 = 2_048_000
    let w = TxWeight::new(make_tx_id(1), 2_000_000, 500, 1000, FeeStrategy::FeePerCycle);
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

#[test]
fn ord_consistent_for_same_weight_and_id() {
    // TxWeight derives Eq over all fields including `created: Instant`,
    // so two separate `new()` calls are never structurally equal.
    // But Ord only considers weight and tx_id, so they compare as Equal
    // in ordering terms.
    use std::cmp::Ordering;
    let id = make_tx_id(1);
    let w1 = TxWeight::new(id, 1_000_000, 500, 0, FeeStrategy::FeePerByte);
    let w2 = TxWeight::new(id, 1_000_000, 500, 0, FeeStrategy::FeePerByte);
    assert_eq!(w1.cmp(&w2), Ordering::Equal, "same weight+id should be Ordering::Equal");
    assert_eq!(w1.weight, w2.weight);
    assert_eq!(w1.fee_per_factor, w2.fee_per_factor);
    assert_eq!(w1.tx_id, w2.tx_id);
}
