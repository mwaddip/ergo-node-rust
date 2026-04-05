//! Integration test: create a tree, dump a snapshot, store it, serve it,
//! parse it with the receiver's code, and verify the root hash matches.

use std::sync::Arc;

use bytes::Bytes;
use enr_state::{AVLTreeParams, RedbAVLStorage};
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{AVLTree, Node, NodeHeader, Resolver};
use ergo_avltree_rust::operation::{KeyValue, Operation};
use ergo_avltree_rust::persistent_batch_avl_prover::PersistentBatchAVLProver;
use ergo_node_rust::snapshot_store::SnapshotStore;
use ergo_sync::snapshot::manifest::extract_subtree_ids;
use ergo_sync::snapshot::parser::parse_dfs_stream;
use tempfile::TempDir;

#[test]
fn dump_serve_receive_round_trip() {
    let dir = TempDir::new().unwrap();

    // ── Build a tree with content ────────────────────────────────────────
    let state_path = dir.path().join("state.redb");
    let params = AVLTreeParams {
        key_length: 32,
        value_length: None,
    };
    let storage = RedbAVLStorage::open(&state_path, params, 0).unwrap();

    // Create snapshot reader BEFORE prover takes ownership of storage
    let snapshot_reader = storage.snapshot_reader();

    let resolver: Resolver =
        Arc::new(|digest: &[u8; 32]| Node::LabelOnly(NodeHeader::new(Some(*digest), None)));
    let tree = AVLTree::new(resolver, 32, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // Insert enough entries to have a non-trivial tree.
    // Keys must be strictly between negative_infinity [0;32] and
    // positive_infinity [0xFF;32], and sorted ascending for batch ops.
    let mut keys: Vec<[u8; 32]> = (0u32..200)
        .map(|i| {
            let mut key = [0u8; 32];
            key[0] = 0x01; // avoid negative infinity sentinel
            key[1..5].copy_from_slice(&i.to_be_bytes());
            key
        })
        .collect();
    keys.sort();

    for (i, key) in keys.iter().enumerate() {
        let value = vec![i as u8; 50];
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: Bytes::copy_from_slice(key),
                value: Bytes::copy_from_slice(&value),
            }))
            .unwrap();
    }

    let _persistent =
        PersistentBatchAVLProver::new(prover, Box::new(storage), vec![]).unwrap();
    let digest = _persistent.digest();
    let original_root: [u8; 32] = digest[..32].try_into().unwrap();
    let original_height = digest[32];

    // ── Dump snapshot (using reader that shares Arc<Database>) ───────────
    let dump = snapshot_reader
        .dump_snapshot(3)
        .unwrap()
        .expect("tree is not empty");

    assert_eq!(dump.root_hash, original_root);
    assert_eq!(dump.tree_height, original_height);
    assert!(
        dump.manifest.len() > 2,
        "manifest should have content beyond header"
    );
    assert!(!dump.chunks.is_empty(), "should have chunks");

    // ── Store in SnapshotStore ───────────────────────────────────────────
    let snap_path = dir.path().join("snapshots.redb");
    let snap_store = SnapshotStore::open(&snap_path).unwrap();
    snap_store
        .write_snapshot(1000, dump.root_hash, &dump.manifest, &dump.chunks, 2)
        .unwrap();

    // ── "Serve" — read back from store ───────────────────────────────────
    let served_manifest = snap_store.get_manifest(&dump.root_hash).unwrap().unwrap();
    let mut served_chunks: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    for (id, _) in &dump.chunks {
        let data = snap_store.get_chunk(id).unwrap().unwrap();
        served_chunks.push((*id, data));
    }

    // ── "Receive" — parse with sync crate's receiver code ────────────────
    let _subtree_ids = extract_subtree_ids(&served_manifest, 32).unwrap();
    let manifest_nodes = parse_dfs_stream(&served_manifest[2..], 32).unwrap();

    let mut all_nodes: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    for node in &manifest_nodes {
        all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
    }
    for (_, chunk_data) in &served_chunks {
        let chunk_nodes = parse_dfs_stream(chunk_data, 32).unwrap();
        for node in &chunk_nodes {
            all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
        }
    }

    // ── Load into fresh state ────────────────────────────────────────────
    let state2_path = dir.path().join("state2.redb");
    let params2 = AVLTreeParams {
        key_length: 32,
        value_length: None,
    };
    let mut storage2 = RedbAVLStorage::open(&state2_path, params2, 0).unwrap();

    let mut version_bytes = Vec::with_capacity(33);
    version_bytes.extend_from_slice(&dump.root_hash);
    version_bytes.push(dump.tree_height);
    let version = Bytes::from(version_bytes);

    storage2
        .load_snapshot(
            all_nodes
                .into_iter()
                .map(|(label, packed)| (label, Bytes::from(packed))),
            dump.root_hash,
            dump.tree_height as usize,
            version,
        )
        .unwrap();

    // ── Verify ───────────────────────────────────────────────────────────
    let (loaded_root, loaded_height) = storage2.root_state().unwrap();
    assert_eq!(
        loaded_root, original_root,
        "root hash must match after round-trip"
    );
    assert_eq!(
        loaded_height, original_height as usize,
        "tree height must match"
    );
}
