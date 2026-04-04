use ergo_sync::snapshot::download::ChunkDownloadStore;
use std::collections::HashSet;

fn make_chunk_id(n: u8) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[0] = n;
    id
}

fn make_chunk_data(n: u8) -> Vec<u8> {
    vec![n; 64]
}

/// Store and retrieve chunks, verify data integrity.
#[test]
fn store_and_retrieve_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let manifest_id = [0x11; 32];

    let store =
        ChunkDownloadStore::create(&path, manifest_id, 100_000, b"manifest", 5).unwrap();

    // Store three chunks.
    for i in 0..3u8 {
        store.store_chunk(&make_chunk_id(i), &make_chunk_data(i)).unwrap();
    }

    // Retrieve each and verify data.
    for i in 0..3u8 {
        let data = store.get_chunk(&make_chunk_id(i)).unwrap();
        assert_eq!(data, Some(make_chunk_data(i)), "chunk {i} data mismatch");
    }

    // Non-existent chunk returns None.
    assert_eq!(store.get_chunk(&make_chunk_id(99)).unwrap(), None);

    // Count matches.
    assert_eq!(store.chunk_count().unwrap(), 3);
}

/// Reopen after close (crash recovery) — verify metadata and chunks survive.
#[test]
fn crash_recovery_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.redb");
    let manifest_id = [0x22; 32];
    let manifest_bytes = b"the real manifest bytes here";
    let height = 250_000u32;
    let total = 10u32;

    // Create and store some chunks, then drop the store.
    {
        let store =
            ChunkDownloadStore::create(&path, manifest_id, height, manifest_bytes, total).unwrap();
        store.store_chunk(&make_chunk_id(0), &make_chunk_data(0)).unwrap();
        store.store_chunk(&make_chunk_id(1), &make_chunk_data(1)).unwrap();
        // store dropped here — simulates crash
    }

    // Reopen and verify everything survived.
    let store = ChunkDownloadStore::open(&path).unwrap();
    assert_eq!(store.manifest_id(), manifest_id);
    assert_eq!(store.snapshot_height(), height);
    assert_eq!(store.total_chunks(), total);
    assert_eq!(store.manifest_bytes().unwrap(), manifest_bytes);
    assert_eq!(store.chunk_count().unwrap(), 2);

    // Chunks are still readable.
    assert_eq!(
        store.get_chunk(&make_chunk_id(0)).unwrap(),
        Some(make_chunk_data(0))
    );
    assert_eq!(
        store.get_chunk(&make_chunk_id(1)).unwrap(),
        Some(make_chunk_data(1))
    );
}

/// is_complete returns false until chunk_count >= total_chunks.
#[test]
fn is_complete_detection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("complete.redb");
    let total = 3u32;

    let store =
        ChunkDownloadStore::create(&path, [0x33; 32], 300_000, b"m", total).unwrap();

    assert!(!store.is_complete().unwrap(), "should not be complete with 0 chunks");

    store.store_chunk(&make_chunk_id(0), &make_chunk_data(0)).unwrap();
    assert!(!store.is_complete().unwrap(), "should not be complete with 1/3 chunks");

    store.store_chunk(&make_chunk_id(1), &make_chunk_data(1)).unwrap();
    assert!(!store.is_complete().unwrap(), "should not be complete with 2/3 chunks");

    store.store_chunk(&make_chunk_id(2), &make_chunk_data(2)).unwrap();
    assert!(store.is_complete().unwrap(), "should be complete with 3/3 chunks");
}

/// stored_chunk_ids returns the correct set of IDs.
#[test]
fn stored_chunk_ids_returns_correct_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ids.redb");

    let store =
        ChunkDownloadStore::create(&path, [0x44; 32], 400_000, b"m", 5).unwrap();

    let expected: HashSet<[u8; 32]> = (0..3u8).map(make_chunk_id).collect();

    for i in 0..3u8 {
        store.store_chunk(&make_chunk_id(i), &make_chunk_data(i)).unwrap();
    }

    let ids: HashSet<[u8; 32]> = store.stored_chunk_ids().unwrap().into_iter().collect();
    assert_eq!(ids, expected);
}

/// iter_chunks returns all stored chunks with correct data.
#[test]
fn iter_chunks_returns_all() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("iter.redb");

    let store =
        ChunkDownloadStore::create(&path, [0x55; 32], 500_000, b"m", 4).unwrap();

    for i in 0..4u8 {
        store.store_chunk(&make_chunk_id(i), &make_chunk_data(i)).unwrap();
    }

    let chunks = store.iter_chunks().unwrap();
    assert_eq!(chunks.len(), 4);

    // Build a map for order-independent comparison.
    let map: std::collections::HashMap<[u8; 32], Vec<u8>> = chunks.into_iter().collect();
    for i in 0..4u8 {
        let id = make_chunk_id(i);
        assert_eq!(
            map.get(&id),
            Some(&make_chunk_data(i)),
            "iter_chunks data mismatch for chunk {i}"
        );
    }
}
