use ergo_node_rust::snapshot_serve::handle_snapshot_request;
use ergo_node_rust::snapshot_store::SnapshotStore;
use ergo_sync::snapshot::protocol::{
    SnapshotMessage, GET_MANIFEST, GET_SNAPSHOTS_INFO, GET_UTXO_SNAPSHOT_CHUNK,
};
use tempfile::TempDir;

#[test]
fn handle_get_snapshots_info_returns_stored_info() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let manifest_id = [0xAA; 32];
    store
        .write_snapshot(5000, manifest_id, &[0x10, 0x0E], &[], 2)
        .unwrap();

    let response = handle_snapshot_request(GET_SNAPSHOTS_INFO, &[], &store);
    assert!(response.is_some());

    let (code, body) = response.unwrap();
    let msg = SnapshotMessage::parse(code, &body).unwrap();
    match msg {
        SnapshotMessage::SnapshotsInfo(entries) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].height, 5000);
            assert_eq!(entries[0].manifest_id, manifest_id);
        }
        _ => panic!("expected SnapshotsInfo"),
    }
}

#[test]
fn handle_get_manifest_returns_stored_manifest() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let manifest_id = [0xBB; 32];
    let manifest_data = vec![0x10, 0x0E, 0x00, 0x01, 0x02];
    store
        .write_snapshot(6000, manifest_id, &manifest_data, &[], 2)
        .unwrap();

    let response = handle_snapshot_request(GET_MANIFEST, &manifest_id, &store);
    assert!(response.is_some());

    let (code, body) = response.unwrap();
    let msg = SnapshotMessage::parse(code, &body).unwrap();
    match msg {
        SnapshotMessage::Manifest(data) => {
            assert_eq!(data, manifest_data);
        }
        _ => panic!("expected Manifest"),
    }
}

#[test]
fn handle_get_manifest_unknown_id_returns_none() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let response = handle_snapshot_request(GET_MANIFEST, &[0xFF; 32], &store);
    assert!(response.is_none(), "unknown manifest should return no response");
}

#[test]
fn handle_get_chunk_returns_stored_chunk() {
    let dir = TempDir::new().unwrap();
    let store = SnapshotStore::open(&dir.path().join("snapshots.redb")).unwrap();

    let chunk_id = [0xCC; 32];
    let chunk_data = vec![0x01, 0x03, 0x04, 0x05];
    store
        .write_snapshot(7000, [0xDD; 32], &[0x10], &[(chunk_id, chunk_data.clone())], 2)
        .unwrap();

    let response = handle_snapshot_request(GET_UTXO_SNAPSHOT_CHUNK, &chunk_id, &store);
    assert!(response.is_some());

    let (code, body) = response.unwrap();
    let msg = SnapshotMessage::parse(code, &body).unwrap();
    match msg {
        SnapshotMessage::UtxoSnapshotChunk(data) => {
            assert_eq!(data, chunk_data);
        }
        _ => panic!("expected UtxoSnapshotChunk"),
    }
}
