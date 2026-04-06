use ergo_mempool::stats::FeeStats;

#[test]
fn empty_stats_returns_none() {
    let stats = FeeStats::new(100);

    assert!(stats.histogram(5).is_empty(), "empty stats should produce empty histogram");
    assert!(stats.expected_wait(1000).is_none(), "empty stats should return None for expected_wait");
    assert!(stats.recommended_fee(1000).is_none(), "empty stats should return None for recommended_fee");
}

#[test]
fn record_and_histogram() {
    let mut stats = FeeStats::new(100);

    // Record 10 confirmations with varying fees and wait times
    for i in 0..10 {
        let fee = 1000 + i * 100; // 1000, 1100, ..., 1900
        let wait = 5000 + i * 500; // 5000, 5500, ..., 9500
        stats.record_confirmation(fee, wait);
    }

    let buckets = stats.histogram(3);
    assert!(!buckets.is_empty(), "histogram should not be empty after recording data");

    // Total entries across all buckets should equal 10
    let total: usize = buckets.iter().map(|b| b.count).sum();
    assert_eq!(total, 10, "total bucket counts should equal number of recorded confirmations");

    // Buckets should be non-overlapping and cover the fee range
    for b in &buckets {
        assert!(b.count > 0, "empty buckets should be filtered out");
        assert!(b.min_fee < b.max_fee, "bucket min_fee should be less than max_fee");
        assert!(b.avg_wait_ms > 0, "avg_wait should be positive");
    }
}

#[test]
fn histogram_single_fee_value() {
    let mut stats = FeeStats::new(100);

    // All entries have the same fee
    for _ in 0..5 {
        stats.record_confirmation(1000, 3000);
    }

    let buckets = stats.histogram(3);
    assert_eq!(buckets.len(), 1, "single fee value produces a single bucket");
    assert_eq!(buckets[0].count, 5);
    assert_eq!(buckets[0].avg_wait_ms, 3000);
}

#[test]
fn histogram_zero_buckets() {
    let mut stats = FeeStats::new(100);
    stats.record_confirmation(1000, 3000);

    let buckets = stats.histogram(0);
    assert!(buckets.is_empty(), "zero buckets should produce empty result");
}

#[test]
fn expected_wait() {
    let mut stats = FeeStats::new(100);

    // Record confirmations with known fee/wait pairs
    stats.record_confirmation(1000, 5000); // low fee, high wait
    stats.record_confirmation(5000, 1000); // high fee, low wait

    // Query for exact fee match
    let wait = stats.expected_wait(5000).unwrap();
    assert_eq!(wait, 1000, "exact fee match should return exact wait time");

    let wait_low = stats.expected_wait(1000).unwrap();
    assert_eq!(wait_low, 5000, "exact low fee match should return high wait");

    // Query for a fee between the two — should find closest
    let wait_mid = stats.expected_wait(3000).unwrap();
    // Distance to 1000 is 2000, distance to 5000 is 2000 — tie, average of both
    assert!(wait_mid > 0, "mid-range fee should return a wait time");
}

#[test]
fn expected_wait_closest_fee() {
    let mut stats = FeeStats::new(100);

    stats.record_confirmation(1000, 10000);
    stats.record_confirmation(2000, 5000);
    stats.record_confirmation(9000, 500);

    // Query for fee 1800 — closest to 2000 (diff=200), not 1000 (diff=800)
    let wait = stats.expected_wait(1800).unwrap();
    assert_eq!(wait, 5000, "should return wait for closest fee");
}

#[test]
fn recommended_fee() {
    let mut stats = FeeStats::new(100);

    stats.record_confirmation(1000, 10000); // low fee, 10s wait
    stats.record_confirmation(3000, 5000);  // mid fee, 5s wait
    stats.record_confirmation(5000, 2000);  // high fee, 2s wait
    stats.record_confirmation(8000, 500);   // very high fee, 0.5s wait

    // Target 6000ms — fees 1000 (10000ms) and 3000 (5000ms) and 5000 (2000ms) and 8000 (500ms)
    // Fees with wait <= 6000: 3000 (5000ms), 5000 (2000ms), 8000 (500ms)
    // Lowest qualifying fee: 3000
    let fee = stats.recommended_fee(6000).unwrap();
    assert_eq!(fee, 3000, "should recommend the lowest fee that achieved target wait");

    // Target 100ms — only fee 8000 has wait <= 100... actually 500 > 100
    // None qualify
    let fee = stats.recommended_fee(100);
    assert!(fee.is_none(), "no fee achieves very short wait target");

    // Target 500ms — fee 8000 qualifies
    let fee = stats.recommended_fee(500).unwrap();
    assert_eq!(fee, 8000);
}

#[test]
fn history_capacity_evicts_oldest() {
    let mut stats = FeeStats::new(3); // max 3 entries

    stats.record_confirmation(1000, 100);
    stats.record_confirmation(2000, 200);
    stats.record_confirmation(3000, 300);
    stats.record_confirmation(4000, 400); // should evict (1000, 100)

    // The histogram should only contain 3 entries
    let buckets = stats.histogram(10);
    let total: usize = buckets.iter().map(|b| b.count).sum();
    assert_eq!(total, 3, "history should cap at max_history");

    // Fee 1000 should no longer be present — recommended_fee for wait <= 100
    // should not find 1000 anymore. 200 is the lowest wait now.
    let fee = stats.recommended_fee(100);
    assert!(fee.is_none(), "evicted entry should not affect results");
}
