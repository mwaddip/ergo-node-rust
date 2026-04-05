use ergo_node_rust::snapshot_store::SnapshotStore;
use tempfile::TempDir;

#[test]
fn store_and_retrieve_snapshot() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let manifest_id = [0xAA; 32];
    let manifest_bytes = vec![0x10, 0x0E, 0x00, 0x01, 0x02];
    let chunk_id_1 = [0xBB; 32];
    let chunk_data_1 = vec![0x01, 0x03, 0x04];
    let chunk_id_2 = [0xCC; 32];
    let chunk_data_2 = vec![0x01, 0x05, 0x06];

    store
        .write_snapshot(
            1000,
            manifest_id,
            &manifest_bytes,
            &[(chunk_id_1, chunk_data_1.clone()), (chunk_id_2, chunk_data_2.clone())],
            2,
        )
        .unwrap();

    // Verify snapshots_info
    let info = store.snapshots_info().unwrap();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].0, 1000);
    assert_eq!(info[0].1, manifest_id);

    // Verify manifest lookup
    assert_eq!(
        store.get_manifest(&manifest_id).unwrap(),
        Some(manifest_bytes)
    );
    assert_eq!(store.get_manifest(&[0xFF; 32]).unwrap(), None);

    // Verify chunk lookup
    assert_eq!(
        store.get_chunk(&chunk_id_1).unwrap(),
        Some(chunk_data_1)
    );
    assert_eq!(
        store.get_chunk(&chunk_id_2).unwrap(),
        Some(chunk_data_2)
    );
    assert_eq!(store.get_chunk(&[0xFF; 32]).unwrap(), None);
}

#[test]
fn prune_oldest_snapshot() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    // Write 3 snapshots with storing_snapshots = 2
    for i in 0..3u8 {
        let manifest_id = [i + 1; 32];
        let chunk_id = [i + 100; 32];
        store
            .write_snapshot(
                (i as u32 + 1) * 1000,
                manifest_id,
                &[0x10, 0x0E],
                &[(chunk_id, vec![0x01, i])],
                2,
            )
            .unwrap();
    }

    let info = store.snapshots_info().unwrap();
    assert_eq!(info.len(), 2, "should have pruned to 2 snapshots");
    assert_eq!(info[0].0, 2000, "oldest remaining should be height 2000");
    assert_eq!(info[1].0, 3000, "newest should be height 3000");

    // Verify pruned snapshot's data is gone
    assert_eq!(store.get_manifest(&[1; 32]).unwrap(), None);
    assert_eq!(store.get_chunk(&[100; 32]).unwrap(), None);

    // Verify surviving snapshots' data is still there
    assert!(store.get_manifest(&[2; 32]).unwrap().is_some());
    assert!(store.get_manifest(&[3; 32]).unwrap().is_some());
    assert!(store.get_chunk(&[101; 32]).unwrap().is_some());
    assert!(store.get_chunk(&[102; 32]).unwrap().is_some());
}
