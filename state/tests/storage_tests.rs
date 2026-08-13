use blake2::Digest;
use bytes::Bytes;
use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage, SnapshotReader};
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{AVLTree, Blake2b256, Node, NodeId};
use ergo_avltree_rust::operation::{Digest32, KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use std::rc::Rc;
use tempfile::tempdir;

const KEY_LEN: usize = 32;

fn params() -> AVLTreeParams {
    AVLTreeParams {
        key_length: KEY_LEN,
        value_length: None,
    }
}

fn make_key(seed: u8) -> Bytes {
    // Keys must be strictly between negative_infinity [0;32] and
    // positive_infinity [0xFF;32].  Byte 0 = 0x01 guarantees that.
    let mut key = vec![0u8; KEY_LEN];
    key[0] = 0x01;
    key[1] = seed;
    Bytes::from(key)
}

fn make_value(seed: u8, len: usize) -> Bytes {
    Bytes::from(vec![seed; len])
}

/// Create a fresh storage + prover pair with an initial empty-tree commit.
fn setup(keep_versions: u32) -> (RedbAVLStorage, BatchAVLProver, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let mut storage =
        RedbAVLStorage::open(&path, params(), keep_versions, CacheSize::default()).unwrap();

    let resolver = storage.resolver();
    let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // Initial commit with one key — can't have an empty-tree commit because
    // digest() is None on a truly empty tree.
    let key = make_key(0);
    let value = make_value(0, 64);
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: key.clone(),
            value: value.clone(),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();

    // Reset the prover's changed-node tracking for the next batch.
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();

    (storage, prover, dir)
}

// ── Basic CRUD ────────────────────────────────────────────────────────

#[test]
fn insert_updates_version() {
    let (storage, _, _dir) = setup(10);
    assert!(storage.version().is_some());
}

#[test]
fn insert_and_lookup() {
    let (mut storage, mut prover, _dir) = setup(10);

    let key = make_key(1);
    let value = make_value(1, 128);
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: key.clone(),
            value: value.clone(),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();

    let found = prover.unauthenticated_lookup(&key);
    assert_eq!(found, Some(value));
}

#[test]
fn remove_key() {
    let (mut storage, mut prover, _dir) = setup(10);

    // Insert
    let key = make_key(2);
    let value = make_value(2, 64);
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: key.clone(),
            value: value.clone(),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();

    // Remove
    prover
        .perform_one_operation(&Operation::Remove(key.clone()))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();

    let found = prover.unauthenticated_lookup(&key);
    assert_eq!(found, None);
}

#[test]
fn digest_changes_on_update() {
    let (mut storage, mut prover, _dir) = setup(10);
    let d1 = storage.version().unwrap();

    let key = make_key(3);
    let value = make_value(3, 64);
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: key.clone(),
            value: value.clone(),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();

    let d2 = storage.version().unwrap();
    assert_ne!(d1, d2);
}

// ── Rollback ──────────────────────────────────────────────────────────

#[test]
fn rollback_restores_previous_digest() {
    let (mut storage, mut prover, _dir) = setup(10);
    let d1 = storage.version().unwrap();

    // Insert another key → version D2.
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(10),
            value: make_value(10, 64),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();
    let d2 = storage.version().unwrap();
    assert_ne!(d1, d2);

    // Rollback to D1.
    let (root, height) = storage.rollback(&d1).unwrap();
    assert_eq!(storage.version().unwrap(), d1);

    // Reconstruct the prover with the rolled-back root.
    prover.base.tree.root = Some(root);
    prover.base.tree.height = height;

    // The digest from the prover should match.
    assert_eq!(prover.digest().unwrap(), d1);
}

#[test]
fn rollback_multi_step() {
    let (mut storage, mut prover, _dir) = setup(10);
    let d1 = storage.version().unwrap();

    // Apply several versions.
    let mut digests = vec![d1.clone()];
    for i in 20..25u8 {
        prover.base.tree.reset();
        prover.base.changed_nodes_buffer.clear();
        prover.base.changed_nodes_buffer_to_check.clear();

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(i),
                value: make_value(i, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
        digests.push(storage.version().unwrap());
    }

    // Rollback to the second version (d1 + one insert).
    let target = &digests[1];
    let (root, height) = storage.rollback(target).unwrap();
    assert_eq!(storage.version().unwrap(), *target);

    prover.base.tree.root = Some(root);
    prover.base.tree.height = height;
    assert_eq!(prover.digest().unwrap(), *target);
}

#[test]
fn rollback_versions_lists_targets() {
    let (mut storage, mut prover, _dir) = setup(10);

    // One version already (from setup).  Add two more.
    for i in 30..32u8 {
        prover.base.tree.reset();
        prover.base.changed_nodes_buffer.clear();
        prover.base.changed_nodes_buffer_to_check.clear();

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(i),
                value: make_value(i, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
    }

    // rollback_versions should list 2 targets (everything except current).
    let targets: Vec<_> = storage.rollback_versions().collect();
    assert_eq!(targets.len(), 2);
}

// ── keep_versions ─────────────────────────────────────────────────────

#[test]
fn keep_versions_zero_no_undo() {
    let (mut storage, mut prover, _dir) = setup(0);

    // Add a version.
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();

    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(40),
            value: make_value(40, 64),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();

    // No rollback targets.
    let targets: Vec<_> = storage.rollback_versions().collect();
    assert!(targets.is_empty());
}

#[test]
fn keep_versions_prunes_old() {
    let (mut storage, mut prover, _dir) = setup(2);

    let mut digests = vec![storage.version().unwrap()];
    for i in 50..55u8 {
        prover.base.tree.reset();
        prover.base.changed_nodes_buffer.clear();
        prover.base.changed_nodes_buffer_to_check.clear();

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(i),
                value: make_value(i, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
        digests.push(storage.version().unwrap());
    }

    // With keep_versions=2, only 2 rollback targets available.
    let targets: Vec<_> = storage.rollback_versions().collect();
    assert_eq!(targets.len(), 2);

    // Current = digests[5].  keep_versions=2 retains digests[4] and digests[3].
    assert_eq!(targets[0], digests[4]);
    assert_eq!(targets[1], digests[3]);
}

// ── Snapshot loading ──────────────────────────────────────────────────

#[test]
fn load_snapshot_sets_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();

    // Build a small tree to extract its nodes.
    let resolver = storage.resolver();
    let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
    let mut prover = BatchAVLProver::new(tree, true);

    let key = make_key(60);
    let value = make_value(60, 64);
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: key.clone(),
            value: value.clone(),
        }))
        .unwrap();

    let digest = prover.digest().unwrap();
    let root_label = prover.base.tree.label(&prover.top_node());
    let height = prover.base.tree.height;

    // Pack the root node.
    let packed = prover.base.tree.pack(prover.top_node());
    let nodes = vec![(root_label, packed)];

    // Load via snapshot.
    storage
        .load_snapshot(nodes.into_iter(), root_label, height, digest.clone(), 0)
        .unwrap();

    assert_eq!(storage.version().unwrap(), digest);
    assert!(storage.get_node(&root_label).unwrap().is_some());
}

// ── Persistence across reopen ─────────────────────────────────────────

#[test]
fn reopen_preserves_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");

    let original_version;
    {
        let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(70),
                value: make_value(70, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
        original_version = storage.version().unwrap();
    }

    // Reopen.
    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert_eq!(storage.version().unwrap(), original_version);
}

#[test]
fn reopen_preserves_rollback_chain() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");

    let d1;
    {
        let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);

        // Version 1.
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(80),
                value: make_value(80, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
        d1 = storage.version().unwrap();

        prover.base.tree.reset();
        prover.base.changed_nodes_buffer.clear();
        prover.base.changed_nodes_buffer_to_check.clear();

        // Version 2.
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(81),
                value: make_value(81, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
    }

    // Reopen and verify we can still rollback.
    let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    let targets: Vec<_> = storage.rollback_versions().collect();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0], d1);

    // Actually rollback.
    let (root, height) = storage.rollback(&d1).unwrap();
    assert_eq!(storage.version().unwrap(), d1);

    // Verify the root unpacks correctly.
    let resolver = storage.resolver();
    let mut tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
    tree.root = Some(root);
    tree.height = height;
    let prover = BatchAVLProver::new(tree, false);
    assert_eq!(prover.digest().unwrap(), d1);
}

// ── flush() ──────────────────────────────────────────────────────────

#[test]
fn flush_persists_state_across_reopen() {
    // After an update() (Durability::None) followed by flush() and drop,
    // the state must be recoverable on reopen.  This is the main node's
    // guarantee on graceful shutdown.
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");

    let expected_version;
    {
        let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(90),
                value: make_value(90, 64),
            }))
            .unwrap();
        storage.update(&mut prover, vec![]).unwrap();
        expected_version = storage.version().unwrap();

        // Force a durable commit before the storage goes out of scope.
        storage.flush().unwrap();
    }

    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert_eq!(storage.version().unwrap(), expected_version);
}

#[test]
fn flush_between_updates_is_transparent() {
    // Calling flush() between updates must not interfere with subsequent
    // writes or the in-memory version chain.
    let (mut storage, mut prover, _dir) = setup(10);
    let v1 = storage.version().unwrap();

    storage.flush().unwrap();
    assert_eq!(storage.version().unwrap(), v1);

    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();

    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(91),
            value: make_value(91, 64),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();

    let v2 = storage.version().unwrap();
    assert_ne!(v1, v2);

    storage.flush().unwrap();
    assert_eq!(storage.version().unwrap(), v2);
}

#[test]
fn flush_is_idempotent() {
    // Back-to-back flushes must all succeed — no-op after the first.
    let (storage, _, _dir) = setup(10);
    storage.flush().unwrap();
    storage.flush().unwrap();
    storage.flush().unwrap();
}

#[test]
fn flush_on_empty_storage_succeeds() {
    // Flush on a freshly opened storage with no updates must not error.
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert!(storage.version().is_none());
    storage.flush().unwrap();
}

// ── Snapshot dump ────────────────────────────────────────────────────

/// Compute a node label from its packed bytes, matching ergo_avltree_rust convention.
/// Internal: Blake2b256(0x01 || balance || left_label || right_label)
/// Leaf:     Blake2b256(0x00 || key || value || next_key)
fn label_from_packed(packed: &[u8], key_length: usize) -> Digest32 {
    let node_type = packed[0];
    let mut hasher = Blake2b256::new();
    if node_type == 0x00 {
        // Internal: pack uses 0x00, but label hash uses prefix 0x01
        hasher.update([1u8]);
        let balance = packed[1];
        hasher.update([balance]);
        let left_offset = 2 + key_length;
        hasher.update(&packed[left_offset..left_offset + 32]);
        hasher.update(&packed[left_offset + 32..left_offset + 64]);
    } else {
        // Leaf: pack uses 0x01, but label hash uses prefix 0x00
        hasher.update([0u8]);
        // key
        hasher.update(&packed[1..1 + key_length]);
        // value_len (BE u32) + value
        let vlen_offset = 1 + key_length;
        let value_len =
            u32::from_be_bytes(packed[vlen_offset..vlen_offset + 4].try_into().unwrap()) as usize;
        let value_start = vlen_offset + 4;
        hasher.update(&packed[value_start..value_start + value_len]);
        // next_leaf_key
        let next_key_offset = value_start + value_len;
        hasher.update(&packed[next_key_offset..next_key_offset + key_length]);
    }
    let mut label: Digest32 = [0u8; 32];
    label.copy_from_slice(&hasher.finalize());
    label
}

/// Parse a DFS byte stream into (label, packed_bytes) pairs.
fn parse_dfs_nodes(data: &[u8], key_length: usize) -> Vec<(Digest32, Vec<u8>)> {
    let mut nodes = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let node_type = data[pos];
        let node_len = if node_type == 0x00 {
            // Internal: type(1) + balance(1) + key(key_length) + left(32) + right(32)
            1 + 1 + key_length + 32 + 32
        } else {
            // Leaf: type(1) + key(key_length) + value_len(4) + value + next_key(key_length)
            let vlen_offset = pos + 1 + key_length;
            let value_len =
                u32::from_be_bytes(data[vlen_offset..vlen_offset + 4].try_into().unwrap()) as usize;
            1 + key_length + 4 + value_len + key_length
        };

        let packed = &data[pos..pos + node_len];
        let label = label_from_packed(packed, key_length);
        nodes.push((label, packed.to_vec()));
        pos += node_len;
    }
    nodes
}

#[test]
fn dump_snapshot_round_trip() {
    // 1. Create storage + reader + prover.
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();

    let reader: SnapshotReader = storage.snapshot_reader();

    let resolver = storage.resolver();
    let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // 2. Insert 100 keys.
    for i in 0u8..100 {
        let mut key = vec![0u8; KEY_LEN];
        key[0] = 0x01;
        key[1] = i / 10 + 1;
        key[2] = i % 10;
        let value = vec![i; 64];
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: Bytes::from(key),
                value: Bytes::from(value),
            }))
            .unwrap();
    }

    // 3. Commit.
    storage.update(&mut prover, vec![]).unwrap();
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();

    let expected_root = prover.base.tree.label(&prover.top_node());
    let expected_height = prover.base.tree.height;

    // 4. Dump snapshot at depth 3.
    let snap = reader.dump_snapshot(3).unwrap().expect("tree not empty");

    // 5. Verify metadata.
    assert_eq!(snap.root_hash, expected_root, "root hash mismatch");
    assert_eq!(
        snap.tree_height, expected_height as u8,
        "tree height mismatch"
    );

    // 6. Verify manifest header.
    assert_eq!(snap.manifest[0], expected_height as u8);
    assert_eq!(snap.manifest[1], 3); // manifest_depth

    // 7. Verify manifest bytes are parseable (skip 2-byte header).
    let manifest_nodes = parse_dfs_nodes(&snap.manifest[2..], KEY_LEN);
    assert!(!manifest_nodes.is_empty(), "manifest has no nodes");

    // First node's label should be the root hash.
    assert_eq!(
        manifest_nodes[0].0, expected_root,
        "first manifest node is not root"
    );

    // 8. Verify chunks are non-empty.
    assert!(!snap.chunks.is_empty(), "no chunks produced");

    // 9. Collect boundary subtree labels from the manifest.
    let mut boundary_labels: Vec<[u8; 32]> = Vec::new();
    for (_, packed) in &manifest_nodes {
        if packed[0] == 0x00 {
            // Check if this internal node's children are NOT in the manifest.
            // Boundary nodes are those whose children appear as chunk roots.
            let offset = 2 + KEY_LEN;
            let mut left = [0u8; 32];
            let mut right = [0u8; 32];
            left.copy_from_slice(&packed[offset..offset + 32]);
            right.copy_from_slice(&packed[offset + 32..offset + 64]);

            let left_in_manifest = manifest_nodes.iter().any(|(l, _)| *l == left);
            let right_in_manifest = manifest_nodes.iter().any(|(l, _)| *l == right);

            if !left_in_manifest {
                boundary_labels.push(left);
            }
            if !right_in_manifest {
                boundary_labels.push(right);
            }
        }
    }

    // Every chunk root label should be in boundary_labels.
    for (chunk_label, _) in &snap.chunks {
        assert!(
            boundary_labels.contains(chunk_label),
            "chunk root not found in manifest boundary"
        );
    }

    // 10. Round-trip: load into a second storage and verify root_state matches.
    let dir2 = tempdir().unwrap();
    let path2 = dir2.path().join("state2.redb");
    let mut storage2 = RedbAVLStorage::open(&path2, params(), 10, CacheSize::default()).unwrap();

    // Collect all nodes from manifest + chunks.
    let mut all_nodes: Vec<(Digest32, Bytes)> = manifest_nodes
        .iter()
        .map(|(l, p)| (*l, Bytes::from(p.clone())))
        .collect();

    for (_, chunk_bytes) in &snap.chunks {
        let chunk_nodes = parse_dfs_nodes(chunk_bytes, KEY_LEN);
        for (l, p) in chunk_nodes {
            all_nodes.push((l, Bytes::from(p)));
        }
    }

    let version = prover.digest().unwrap();
    storage2
        .load_snapshot(
            all_nodes.into_iter(),
            snap.root_hash,
            snap.tree_height as usize,
            version,
            0,
        )
        .unwrap();

    // Verify the second storage has the same root state.
    let (root2, height2) = storage2.root_state().expect("no root state after load");
    assert_eq!(root2, snap.root_hash, "loaded root hash mismatch");
    assert_eq!(height2, snap.tree_height as usize, "loaded height mismatch");
}

// ── CacheSize ────────────────────────────────────────────────────────

#[test]
fn cache_size_bytes_returns_exact_value() {
    let cs = CacheSize::Bytes(512 * 1024 * 1024);
    assert_eq!(cs.resolve(), 512 * 1024 * 1024);
}

#[test]
fn cache_size_default_is_256mb() {
    assert_eq!(CacheSize::default().resolve(), 256 * 1024 * 1024);
}

#[test]
fn cache_size_percent_returns_fraction_of_ram() {
    let half = CacheSize::Percent(0.5);
    let resolved = half.resolve();
    // On any machine running these tests, half of RAM should be >128MB.
    assert!(
        resolved > 128 * 1024 * 1024,
        "half of RAM unexpectedly small: {resolved}"
    );
    // And less than 1TB, just to catch parse failures returning garbage.
    assert!(
        resolved < 1024 * 1024 * 1024 * 1024,
        "half of RAM unexpectedly large: {resolved}"
    );
}

#[test]
fn cache_occupancy_is_reported() {
    let dir = tempdir().unwrap();
    let storage = RedbAVLStorage::open(
        &dir.path().join("s.redb"),
        params(),
        32,
        CacheSize::Bytes(8 * 1024 * 1024),
    )
    .unwrap();
    assert!(
        storage.cache_bytes_used() > 0,
        "cache_metrics disabled? used_bytes was 0"
    );
}

#[test]
fn snapshot_reader_reports_the_same_cache_occupancy() {
    // The API's only handle on state is a SnapshotReader, so the figure has to
    // be reachable from there — and it must be the same database, not a
    // second one that happens to also report a number.
    let dir = tempdir().unwrap();
    let storage = RedbAVLStorage::open(
        &dir.path().join("s.redb"),
        params(),
        32,
        CacheSize::Bytes(8 * 1024 * 1024),
    )
    .unwrap();
    let reader = storage.snapshot_reader();

    assert!(
        reader.cache_bytes_used() > 0,
        "cache_metrics disabled? used_bytes was 0"
    );
    assert_eq!(
        reader.cache_bytes_used(),
        storage.cache_bytes_used(),
        "reader and storage disagree — not the same Arc<Database>?"
    );
}

// ── block_height persistence ─────────────────────────────────────────

#[test]
fn update_persists_block_height() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    {
        let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);

        // Apply 5 updates with ascending block heights.
        let heights = [1_000_000u32, 1_000_001, 1_000_002, 1_000_003, 1_000_004];
        for (i, h) in heights.iter().enumerate() {
            prover.base.tree.reset();
            prover.base.changed_nodes_buffer.clear();
            prover.base.changed_nodes_buffer_to_check.clear();

            let seed = 100 + i as u8;
            prover
                .perform_one_operation(&Operation::Insert(KeyValue {
                    key: make_key(seed),
                    value: make_value(seed, 64),
                }))
                .unwrap();
            storage.update_with_height(&mut prover, vec![], *h).unwrap();
            assert_eq!(storage.block_height(), Some(*h));
        }
        storage.flush().unwrap();
    }

    // Reopen — the last value must survive.
    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert_eq!(storage.block_height(), Some(1_000_004));
}

#[test]
fn rollback_restores_block_height() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    let resolver = storage.resolver();
    let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // First update at height 100.
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(1),
            value: make_value(1, 64),
        }))
        .unwrap();
    storage
        .update_with_height(&mut prover, vec![], 100)
        .unwrap();
    let digest_at_100 = storage.version().unwrap();

    // Update at height 101.
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(2),
            value: make_value(2, 64),
        }))
        .unwrap();
    storage
        .update_with_height(&mut prover, vec![], 101)
        .unwrap();

    // Update at height 102.
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(3),
            value: make_value(3, 64),
        }))
        .unwrap();
    storage
        .update_with_height(&mut prover, vec![], 102)
        .unwrap();

    assert_eq!(storage.block_height(), Some(102));

    // Rollback to the version at height 100.
    storage.rollback(&digest_at_100).unwrap();
    assert_eq!(storage.version().unwrap(), digest_at_100);
    assert_eq!(storage.block_height(), Some(100));
}

#[test]
fn load_snapshot_sets_block_height() {
    // Build a tiny tree with a known root, then bulk-load it with
    // block_height = 500_000.
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    {
        let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();

        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(77),
                value: make_value(77, 64),
            }))
            .unwrap();

        let digest = prover.digest().unwrap();
        let root_label = prover.base.tree.label(&prover.top_node());
        let height = prover.base.tree.height;
        let packed = prover.base.tree.pack(prover.top_node());

        storage
            .load_snapshot(
                vec![(root_label, packed)].into_iter(),
                root_label,
                height,
                digest,
                500_000,
            )
            .unwrap();

        assert_eq!(storage.block_height(), Some(500_000));
        storage.flush().unwrap();
    }

    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert_eq!(storage.block_height(), Some(500_000));
}

#[test]
fn crash_simulation_preserves_pre_update_block_height() {
    // An uncommitted write transaction against the metadata table must
    // leave block_height at its prior committed value.  This is the
    // atomicity guarantee — an in-flight block_height write that never
    // commits is invisible on reopen.
    use redb::{Database, TableDefinition};

    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");

    // 1. Commit block_height = 42 via the normal update path.
    let committed_version;
    {
        let mut storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
        let mut prover = BatchAVLProver::new(tree, true);

        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(1),
                value: make_value(1, 64),
            }))
            .unwrap();
        storage.update_with_height(&mut prover, vec![], 42).unwrap();
        storage.flush().unwrap();
        committed_version = storage.version().unwrap();
    }

    // 2. Start a raw redb write transaction that would overwrite
    //    block_height to a bogus value, then drop WITHOUT committing.
    //    redb must discard the uncommitted write.
    {
        let db = Database::builder().create(&path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let meta_def: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
            let mut meta = write_txn.open_table(meta_def).unwrap();
            meta.insert("block_height", 999_999u32.to_be_bytes().as_slice())
                .unwrap();
        }
        drop(write_txn);
    }

    // 3. Reopen and verify block_height is the pre-"crash" value.
    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert_eq!(storage.version().unwrap(), committed_version);
    assert_eq!(storage.block_height(), Some(42));
}

#[test]
fn block_height_is_none_on_empty_storage() {
    // Invariant: block_height() returns None iff version() is None.
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let storage = RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert!(storage.version().is_none());
    assert!(storage.block_height().is_none());
}

// ── Offline compaction ───────────────────────────────────────────────

use enr_state::{CompactionProgress, CompactionStats};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::cell::RefCell;
use std::path::Path;

const NODES_DEF: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
const UNDO_DEF: TableDefinition<u64, &[u8]> = TableDefinition::new("undo");
const META_DEF: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

/// Open storage and rebuild a prover on top of whatever root is stored —
/// the resume flow from the contract's initialization section.  Used to
/// churn a tree across several open/close cycles.
fn open_with_prover(path: &Path, keep_versions: u32) -> (RedbAVLStorage, BatchAVLProver) {
    let mut storage =
        RedbAVLStorage::open(path, params(), keep_versions, CacheSize::default()).unwrap();
    let resolver = storage.resolver();
    let mut tree = AVLTree::with_resolver(resolver, KEY_LEN, None);
    if let Some(version) = storage.version() {
        let (root, height) = storage.rollback(&version).unwrap();
        tree.root = Some(root);
        tree.height = height;
    }
    let prover = BatchAVLProver::new(tree, true);
    (storage, prover)
}

/// Overwrite every key with a round-specific value and commit at `height`.
fn churn_round(path: &Path, keys: u8, round: u8, value_len: usize, height: u32) {
    let (mut storage, mut prover) = open_with_prover(path, 10);
    for k in 0..keys {
        prover
            .perform_one_operation(&Operation::InsertOrUpdate(KeyValue {
                key: make_key(k),
                value: make_value(k.wrapping_add(round), value_len),
            }))
            .unwrap();
    }
    storage
        .update_with_height(&mut prover, vec![], height)
        .unwrap();
    storage.flush().unwrap();
}

fn every_node_row(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let db = Database::builder().create(path).unwrap();
    let txn = db.begin_read().unwrap();
    let table = txn.open_table(NODES_DEF).unwrap();
    table
        .iter()
        .unwrap()
        .map(|row| {
            let (k, v) = row.unwrap();
            (k.value().to_vec(), v.value().to_vec())
        })
        .collect()
}

fn node_row_count(path: &Path) -> u64 {
    let db = Database::builder().create(path).unwrap();
    let txn = db.begin_read().unwrap();
    txn.open_table(NODES_DEF).unwrap().len().unwrap()
}

fn undo_row_count(path: &Path) -> u64 {
    let db = Database::builder().create(path).unwrap();
    let txn = db.begin_read().unwrap();
    txn.open_table(UNDO_DEF).unwrap().len().unwrap()
}

/// Independent walk of the live tree, so the test's notion of "reachable"
/// does not come from the code under test.
fn reachable_node_count(path: &Path) -> u64 {
    let db = Database::builder().create(path).unwrap();
    let txn = db.begin_read().unwrap();
    let nodes = txn.open_table(NODES_DEF).unwrap();
    let meta = txn.open_table(META_DEF).unwrap();

    let root = meta.get("top_node_hash").unwrap().unwrap().value().to_vec();

    let mut stack = vec![root];
    let mut count = 0u64;
    while let Some(label) = stack.pop() {
        let packed = nodes
            .get(label.as_slice())
            .unwrap()
            .expect("reachable node missing from source")
            .value()
            .to_vec();
        count += 1;
        // Internal node: the two child labels are the final 64 bytes.
        if packed[0] == 0x00 {
            let split = packed.len() - 64;
            stack.push(packed[split..split + 32].to_vec());
            stack.push(packed[split + 32..].to_vec());
        }
    }
    count
}

/// Re-insert the rows an update deleted.  This reproduces the v0.7.5–v0.7.9
/// leak, where `generate_proof()` cleared the changed-node buffers before
/// `update_internal` read them, so the per-block delete list came back empty
/// and every superseded node stayed in `nodes`.  Returns the rows put back.
fn reinject_removed(path: &Path, before: &[(Vec<u8>, Vec<u8>)]) -> usize {
    let db = Database::builder().create(path).unwrap();
    let mut write_txn = db.begin_write().unwrap();
    write_txn.set_quick_repair(true);
    let mut reinjected = 0;
    {
        let mut table = write_txn.open_table(NODES_DEF).unwrap();
        for (label, packed) in before {
            let gone = table.get(label.as_slice()).unwrap().is_none();
            if gone {
                table.insert(label.as_slice(), packed.as_slice()).unwrap();
                reinjected += 1;
            }
        }
    }
    write_txn.commit().unwrap();
    reinjected
}

/// Build a bloated source: populate, then churn, leaking every superseded
/// node the way the shipped bug did.  Returns the tempdir, the source path
/// and the number of leaked rows.
fn bloated_source(rounds: u8) -> (tempfile::TempDir, std::path::PathBuf, usize) {
    const KEYS: u8 = 200;
    const VALUE_LEN: usize = 512;

    let dir = tempdir().unwrap();
    let src = dir.path().join("state.redb");

    churn_round(&src, KEYS, 0, VALUE_LEN, 1);

    let mut leaked = 0;
    for round in 1..=rounds {
        let before = every_node_row(&src);
        churn_round(&src, KEYS, round, VALUE_LEN, 1 + u32::from(round));
        leaked += reinject_removed(&src, &before);
    }

    (dir, src, leaked)
}

#[test]
fn compact_to_reclaims_unreachable_rows() {
    let (dir, src, leaked) = bloated_source(12);
    assert!(leaked > 0, "the churn loop leaked nothing to reclaim");

    let bloated_rows = node_row_count(&src);
    let reachable = reachable_node_count(&src);
    assert!(
        bloated_rows > reachable,
        "expected unreachable rows: {bloated_rows} total vs {reachable} reachable"
    );

    let (version, block_height) = {
        let storage = RedbAVLStorage::open(&src, params(), 10, CacheSize::default()).unwrap();
        (storage.version().unwrap(), storage.block_height().unwrap())
    };

    // Byte-level baseline — the contract promises source is untouched.
    let source_before = std::fs::read(&src).unwrap();

    let dest = dir.path().join("state.redb.compacted");
    let ticks = RefCell::new(Vec::new());
    let stats = RedbAVLStorage::compact_to(
        &src,
        &dest,
        Some(&|p: CompactionProgress| ticks.borrow_mut().push(p.nodes_written)),
    )
    .unwrap();

    assert_eq!(
        std::fs::read(&src).unwrap(),
        source_before,
        "source must be byte-identical after compaction"
    );

    assert_eq!(stats.digest, version, "compaction must preserve the digest");
    assert_eq!(
        stats.block_height, block_height,
        "block_height must carry over"
    );
    assert_eq!(stats.nodes_written, reachable);
    assert_eq!(stats.source_bytes, source_before.len() as u64);
    assert_eq!(
        node_row_count(&dest),
        reachable,
        "dest must hold exactly the reachable nodes"
    );
    assert_eq!(
        undo_row_count(&dest),
        0,
        "dest must have an empty undo table"
    );
    assert!(
        stats.dest_bytes < stats.source_bytes,
        "dest ({}) is not smaller than source ({})",
        stats.dest_bytes,
        stats.source_bytes
    );

    // The final tick always fires, so the caller sees the true total.
    let ticks = ticks.into_inner();
    assert_eq!(ticks.last().copied(), Some(reachable));
}

#[test]
fn compacted_output_resumes_and_applies_the_next_block() {
    // The swap protocol's acceptance test: the compacted file opens as
    // storage, reports the same version and height, and takes another block.
    let (dir, src, _) = bloated_source(6);

    let (version, block_height) = {
        let storage = RedbAVLStorage::open(&src, params(), 10, CacheSize::default()).unwrap();
        (storage.version().unwrap(), storage.block_height().unwrap())
    };

    let dest = dir.path().join("state.redb.compacted");
    let stats = RedbAVLStorage::compact_to(&src, &dest, None).unwrap();
    assert_eq!(stats.digest, version);

    {
        let storage = RedbAVLStorage::open(&dest, params(), 10, CacheSize::default()).unwrap();
        assert_eq!(storage.version().unwrap(), version);
        assert_eq!(storage.block_height().unwrap(), block_height);
        // The rollback window is the documented trade — a single version
        // survives, so there is nothing to roll back to.
        assert_eq!(storage.rollback_versions().count(), 0);
    }

    // Resume a prover from the compacted root and apply one more block.
    let (mut storage, mut prover) = open_with_prover(&dest, 10);
    assert_eq!(prover.digest().unwrap(), version);

    prover
        .perform_one_operation(&Operation::InsertOrUpdate(KeyValue {
            key: make_key(250),
            value: make_value(250, 64),
        }))
        .unwrap();
    storage
        .update_with_height(&mut prover, vec![], block_height + 1)
        .unwrap();

    assert_ne!(storage.version().unwrap(), version);
    assert_eq!(storage.block_height().unwrap(), block_height + 1);
    assert_eq!(prover.digest().unwrap(), storage.version().unwrap());
}

#[test]
fn compact_to_drops_rollback_orphans() {
    // Mechanism 1 from the contract: rollback deliberately skips deleting an
    // undo record's inserted labels, because they may still be referenced.
    // The ones that are not become unreachable rows.
    let dir = tempdir().unwrap();
    let src = dir.path().join("state.redb");

    churn_round(&src, 120, 0, 256, 1);
    let mut versions = Vec::new();
    for round in 1..=6u8 {
        churn_round(&src, 120, round, 256, 1 + u32::from(round));
        let storage = RedbAVLStorage::open(&src, params(), 20, CacheSize::default()).unwrap();
        versions.push(storage.version().unwrap());
    }

    let target = versions[1].clone();
    {
        let mut storage = RedbAVLStorage::open(&src, params(), 20, CacheSize::default()).unwrap();
        storage.rollback(&target).unwrap();
        storage.flush().unwrap();
    }

    let reachable = reachable_node_count(&src);
    assert!(
        node_row_count(&src) > reachable,
        "rollback should have left orphaned nodes behind"
    );

    let dest = dir.path().join("state.redb.compacted");
    let stats = RedbAVLStorage::compact_to(&src, &dest, None).unwrap();

    assert_eq!(stats.digest, target);
    assert_eq!(stats.nodes_written, reachable);
    assert_eq!(node_row_count(&dest), reachable);
}

#[test]
fn compact_to_fails_when_a_reachable_node_is_missing() {
    // Genuine corruption, not bloat: a root label with no row behind it.
    // Compaction must refuse rather than launder it into a "compacted" file.
    let (dir, src, _) = bloated_source(2);

    {
        let db = Database::builder().create(&src).unwrap();
        let mut write_txn = db.begin_write().unwrap();
        write_txn.set_quick_repair(true);
        {
            let mut meta = write_txn.open_table(META_DEF).unwrap();
            meta.insert("top_node_hash", [0xABu8; 32].as_slice())
                .unwrap();
        }
        write_txn.commit().unwrap();
    }

    let dest = dir.path().join("state.redb.compacted");
    let err = RedbAVLStorage::compact_to(&src, &dest, None).unwrap_err();

    assert!(
        err.to_string().contains("missing from the `nodes` table"),
        "unexpected error: {err}"
    );
    assert!(!dest.exists(), "dest must be removed on failure");
}

#[test]
fn compact_to_fails_when_a_node_is_altered() {
    // The copy pass moves bytes verbatim, so nothing before verification
    // would notice a leaf whose value had been rewritten under its original
    // label.  Verification recomputes every label from the bytes actually
    // stored, which is the only reason "provably lossless" means anything —
    // without this the check would be a tautology over the root row.
    let (dir, src, _) = bloated_source(2);

    // Flip a byte inside some leaf's value and put it back under the same key.
    let victim = every_node_row(&src)
        .into_iter()
        .find(|(_, packed)| packed[0] == 0x01)
        .expect("no leaf in the tree");
    let (label, mut packed) = victim;
    let value_start = 1 + KEY_LEN + 4;
    packed[value_start] ^= 0xFF;

    {
        let db = Database::builder().create(&src).unwrap();
        let mut write_txn = db.begin_write().unwrap();
        write_txn.set_quick_repair(true);
        {
            let mut nodes = write_txn.open_table(NODES_DEF).unwrap();
            nodes.insert(label.as_slice(), packed.as_slice()).unwrap();
        }
        write_txn.commit().unwrap();
    }

    let dest = dir.path().join("state.redb.compacted");
    let err = RedbAVLStorage::compact_to(&src, &dest, None).unwrap_err();

    // Specifically the label-mismatch branch, not some incidental failure.
    assert!(
        err.to_string().contains("the copy was not byte-exact"),
        "unexpected error: {err}"
    );
    assert!(!dest.exists(), "dest must be removed on failure");
}

#[test]
fn compact_to_refuses_an_existing_destination() {
    let (dir, src, _) = bloated_source(1);

    let dest = dir.path().join("state.redb.compacted");
    std::fs::write(&dest, b"not mine").unwrap();

    let err = RedbAVLStorage::compact_to(&src, &dest, None).unwrap_err();
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}"
    );
    // Refusing to overwrite means refusing to delete, too.
    assert_eq!(std::fs::read(&dest).unwrap(), b"not mine");
}

#[test]
fn compact_to_rejects_an_empty_source() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("state.redb");
    drop(RedbAVLStorage::open(&src, params(), 10, CacheSize::default()).unwrap());

    let dest = dir.path().join("state.redb.compacted");
    let err = RedbAVLStorage::compact_to(&src, &dest, None).unwrap_err();

    assert!(
        err.to_string().contains("nothing to compact"),
        "unexpected error: {err}"
    );
    assert!(!dest.exists());
}

#[test]
fn compact_to_reports_a_locked_source() {
    // Someone will run this against a running node.  They get told to stop
    // it, not a raw redb error string.
    let (dir, src, _) = bloated_source(1);

    let _held = RedbAVLStorage::open(&src, params(), 10, CacheSize::default()).unwrap();

    let dest = dir.path().join("state.redb.compacted");
    let err = RedbAVLStorage::compact_to(&src, &dest, None).unwrap_err();

    assert!(
        err.to_string().contains("stop the node first"),
        "unexpected error: {err}"
    );
    assert!(!dest.exists());
}

#[test]
fn compaction_stats_are_reported() {
    // CompactionStats is what the operator inspects before swapping files,
    // so every field has to be populated.
    let (dir, src, _) = bloated_source(4);
    let dest = dir.path().join("state.redb.compacted");

    let stats: CompactionStats = RedbAVLStorage::compact_to(&src, &dest, None).unwrap();

    assert!(stats.nodes_written > 0);
    assert_eq!(stats.source_bytes, std::fs::metadata(&src).unwrap().len());
    assert_eq!(stats.dest_bytes, std::fs::metadata(&dest).unwrap().len());
    assert_eq!(stats.block_height, 5);
    // The AVL digest is the 32-byte root label plus the tree height byte.
    assert_eq!(stats.digest.len(), 33);
}

// ── SnapshotReader: read-only prover access ──────────────────────────
//
// resolver() + root_state() are everything mining needs to stand up a
// read-only prover over the committed tree without reaching the validator.

/// Storage with an internal root node — a single-key tree roots on a leaf,
/// which has no child handles to compare.
fn storage_with_internal_root() -> (RedbAVLStorage, tempfile::TempDir) {
    let (mut storage, mut prover, dir) = setup(10);
    for seed in 1..=8u8 {
        prover
            .perform_one_operation(&Operation::Insert(KeyValue {
                key: make_key(seed),
                value: make_value(seed, 64),
            }))
            .unwrap();
    }
    storage.update(&mut prover, vec![]).unwrap();
    (storage, dir)
}

/// Left/right child handles of an internal node, or a failure naming what
/// came back instead.
fn child_handles(node: &Node) -> (NodeId, NodeId) {
    match node {
        Node::Internal(internal) => (internal.left.clone(), internal.right.clone()),
        Node::Leaf(_) => panic!("expected an internal root, got a leaf"),
        Node::LabelOnly(_) => panic!("resolver miss: node was not in storage"),
    }
}

#[test]
fn reader_resolver_returns_the_same_node_as_the_storage_resolver() {
    let (storage, _dir) = storage_with_internal_root();
    let (root, _) = storage.root_state().expect("no root state");

    let via_storage = (storage.resolver())(&root);
    let via_reader = (storage.snapshot_reader().resolver())(&root);

    let (s_left, s_right) = child_handles(&via_storage);
    let (r_left, r_right) = child_handles(&via_reader);

    assert_eq!(s_left.borrow().get_label(), r_left.borrow().get_label());
    assert_eq!(s_right.borrow().get_label(), r_right.borrow().get_label());
}

#[test]
fn reader_resolver_hands_out_independent_node_handles() {
    // The invariant that fails silently.  A reader resolver that shared node
    // handles with the validator's prover would let a restored prover walk
    // nodes still keyed in another prover's address-keyed map, and emit a
    // *different proof for identical tree state* — same digest, different
    // bytes.  Equal labels above prove it is the same node; distinct
    // allocations here prove it is not the same memory.
    let (storage, _dir) = storage_with_internal_root();
    let (root, _) = storage.root_state().expect("no root state");

    let (s_left, s_right) = child_handles(&(storage.resolver())(&root));
    let (r_left, r_right) = child_handles(&(storage.snapshot_reader().resolver())(&root));

    assert!(
        !Rc::ptr_eq(&s_left, &r_left),
        "reader and storage resolvers share a node handle"
    );
    assert!(
        !Rc::ptr_eq(&s_right, &r_right),
        "reader and storage resolvers share a node handle"
    );
}

#[test]
fn reader_resolver_allocates_fresh_handles_per_call() {
    // Same guard, one resolver.  Caching resolved nodes is the "for
    // performance" change that would break the invariant above from the
    // inside, and it would keep every existing test green.
    let (storage, _dir) = storage_with_internal_root();
    let (root, _) = storage.root_state().expect("no root state");
    let resolver = storage.snapshot_reader().resolver();

    let (first, _) = child_handles(&resolver(&root));
    let (second, _) = child_handles(&resolver(&root));

    assert_eq!(first.borrow().get_label(), second.borrow().get_label());
    assert!(
        !Rc::ptr_eq(&first, &second),
        "resolver is caching node handles between calls"
    );
}

#[test]
fn reader_resolver_survives_a_miss_with_the_label_intact() {
    // A miss must still name the node the caller asked for, or a failed
    // lookup loses the one piece of evidence that identifies it.
    let (storage, _dir) = storage_with_internal_root();
    let absent: Digest32 = [0xAB; 32];

    let node = (storage.snapshot_reader().resolver())(&absent);

    match node {
        Node::LabelOnly(hdr) => assert_eq!(hdr.label, Some(absent)),
        other => panic!("expected LabelOnly for an absent node, got {other:?}"),
    }
}

#[test]
fn reader_root_state_matches_storage_root_state() {
    let (storage, _dir) = storage_with_internal_root();

    assert_eq!(
        storage.snapshot_reader().root_state(),
        storage.root_state(),
        "reader and storage disagree on the committed root"
    );
}

#[test]
fn reader_root_state_is_none_on_an_empty_tree() {
    // A reader that exists has a database, so None means "empty", never
    // "stale handle" — there is no liveness check to confuse it with.
    let dir = tempdir().unwrap();
    let storage = RedbAVLStorage::open(
        &dir.path().join("state.redb"),
        params(),
        10,
        CacheSize::default(),
    )
    .unwrap();

    assert_eq!(storage.snapshot_reader().root_state(), None);
}

#[test]
fn reader_root_state_ignores_uncommitted_prover_state() {
    // root_state() is the committed root.  A prover mid-batch has a newer
    // tree in memory; a reader must not see it, or mining would build a
    // candidate on a root no peer has.
    let (mut storage, mut prover, _dir) = setup(10);
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(1),
            value: make_value(1, 64),
        }))
        .unwrap();
    storage.update(&mut prover, vec![]).unwrap();
    prover.base.tree.reset();
    prover.base.changed_nodes_buffer.clear();
    prover.base.changed_nodes_buffer_to_check.clear();

    let reader = storage.snapshot_reader();
    let committed = reader.root_state().expect("no root state");

    // Uncommitted: performed on the prover, never handed to update().
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(2),
            value: make_value(2, 64),
        }))
        .unwrap();
    assert_ne!(
        prover.digest().unwrap()[..32],
        committed.0[..],
        "test is not exercising anything — the prover root did not move"
    );

    assert_eq!(
        reader.root_state(),
        Some(committed),
        "reader observed uncommitted prover state"
    );
}

#[test]
fn reader_resolver_is_read_only() {
    // Resolving must not touch the tree, the version chain, or metadata.
    let (storage, _dir) = storage_with_internal_root();
    let before_root = storage.root_state();
    let before_version = storage.version();
    let before_targets: Vec<_> = storage.rollback_versions().collect();

    let resolver = storage.snapshot_reader().resolver();
    let (root, _) = before_root.expect("no root state");
    for _ in 0..16 {
        let _ = resolver(&root);
    }

    assert_eq!(storage.root_state(), before_root);
    assert_eq!(storage.version(), before_version);
    assert_eq!(
        storage.rollback_versions().collect::<Vec<_>>(),
        before_targets
    );
}
