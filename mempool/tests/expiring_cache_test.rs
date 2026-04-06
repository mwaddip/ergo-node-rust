use std::time::Duration;
use std::thread;

use ergo_mempool::expiring_cache::ExpiringCache;

#[test]
fn insert_and_contains() {
    let mut cache: ExpiringCache<[u8; 32]> = ExpiringCache::new(
        Duration::from_secs(60),
        100,
    );

    let key_a = [1u8; 32];
    let key_b = [2u8; 32];
    let key_absent = [99u8; 32];

    cache.insert(key_a);
    cache.insert(key_b);

    assert!(cache.contains(&key_a));
    assert!(cache.contains(&key_b));
    assert!(!cache.contains(&key_absent));
    assert_eq!(cache.len(), 2);
}

#[test]
fn insert_is_idempotent() {
    let mut cache: ExpiringCache<[u8; 32]> = ExpiringCache::new(
        Duration::from_secs(60),
        100,
    );

    let key = [1u8; 32];
    cache.insert(key);
    cache.insert(key);

    assert_eq!(cache.len(), 1);
}

#[test]
fn capacity_evicts_oldest() {
    let mut cache: ExpiringCache<[u8; 32]> = ExpiringCache::new(
        Duration::from_secs(60),
        3, // capacity of 3
    );

    let key_a = [1u8; 32];
    let key_b = [2u8; 32];
    let key_c = [3u8; 32];
    let key_d = [4u8; 32];

    cache.insert(key_a);
    cache.insert(key_b);
    cache.insert(key_c);
    assert_eq!(cache.len(), 3);

    // Inserting a 4th should evict the oldest (key_a)
    cache.insert(key_d);
    assert_eq!(cache.len(), 3);
    assert!(!cache.contains(&key_a), "oldest entry should have been evicted");
    assert!(cache.contains(&key_b));
    assert!(cache.contains(&key_c));
    assert!(cache.contains(&key_d));
}

#[test]
fn expired_entries_not_found() {
    let mut cache: ExpiringCache<[u8; 32]> = ExpiringCache::new(
        Duration::from_millis(1), // 1ms TTL
        100,
    );

    let key = [1u8; 32];
    cache.insert(key);
    assert!(cache.contains(&key), "freshly inserted key should be found");

    // Sleep long enough for the entry to expire
    thread::sleep(Duration::from_millis(5));

    assert!(!cache.contains(&key), "expired entry should not be found via contains()");
    // Note: len() still counts expired entries until prune() is called
    assert_eq!(cache.len(), 1, "expired entry is still stored until pruned");
}

#[test]
fn prune_removes_expired() {
    let mut cache: ExpiringCache<[u8; 32]> = ExpiringCache::new(
        Duration::from_millis(1), // 1ms TTL
        100,
    );

    let key_a = [1u8; 32];
    let key_b = [2u8; 32];

    cache.insert(key_a);
    cache.insert(key_b);
    assert_eq!(cache.len(), 2);

    thread::sleep(Duration::from_millis(5));

    // Insert a fresh key after expiration period
    let key_c = [3u8; 32];
    cache.insert(key_c);

    cache.prune();

    // Expired entries should be removed
    assert!(!cache.contains(&key_a));
    assert!(!cache.contains(&key_b));
    assert!(cache.contains(&key_c));
    assert_eq!(cache.len(), 1, "only the fresh entry should remain after prune");
}

#[test]
fn clear_empties_cache() {
    let mut cache: ExpiringCache<[u8; 32]> = ExpiringCache::new(
        Duration::from_secs(60),
        100,
    );

    cache.insert([1u8; 32]);
    cache.insert([2u8; 32]);
    assert_eq!(cache.len(), 2);

    cache.clear();
    assert_eq!(cache.len(), 0);
    assert!(!cache.contains(&[1u8; 32]));
}
