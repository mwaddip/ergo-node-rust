use blake2::Digest;
use bytes::Bytes;
use enr_state::{AVLTreeParams, CacheSize, RedbAVLStorage, SnapshotReader};
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{AVLTree, Blake2b256};
use ergo_avltree_rust::operation::{Digest32, KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
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
    let mut storage = RedbAVLStorage::open(&path, params(), keep_versions, CacheSize::default()).unwrap();

    let resolver = storage.resolver();
    let tree = AVLTree::new(resolver, KEY_LEN, None);
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
    let tree = AVLTree::new(resolver, KEY_LEN, None);
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
        let tree = AVLTree::new(resolver, KEY_LEN, None);
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
        let tree = AVLTree::new(resolver, KEY_LEN, None);
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
    let mut tree = AVLTree::new(resolver, KEY_LEN, None);
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
        let mut storage =
            RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::new(resolver, KEY_LEN, None);
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
    let storage =
        RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
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
        hasher.update(&[1u8]);
        let balance = packed[1];
        hasher.update(&[balance]);
        let left_offset = 2 + key_length;
        hasher.update(&packed[left_offset..left_offset + 32]);
        hasher.update(&packed[left_offset + 32..left_offset + 64]);
    } else {
        // Leaf: pack uses 0x01, but label hash uses prefix 0x00
        hasher.update(&[0u8]);
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
                u32::from_be_bytes(data[vlen_offset..vlen_offset + 4].try_into().unwrap())
                    as usize;
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
    let tree = AVLTree::new(resolver, KEY_LEN, None);
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
    assert_eq!(snap.tree_height, expected_height as u8, "tree height mismatch");

    // 6. Verify manifest header.
    assert_eq!(snap.manifest[0], expected_height as u8);
    assert_eq!(snap.manifest[1], 3); // manifest_depth

    // 7. Verify manifest bytes are parseable (skip 2-byte header).
    let manifest_nodes = parse_dfs_nodes(&snap.manifest[2..], KEY_LEN);
    assert!(!manifest_nodes.is_empty(), "manifest has no nodes");

    // First node's label should be the root hash.
    assert_eq!(manifest_nodes[0].0, expected_root, "first manifest node is not root");

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
    assert!(resolved > 128 * 1024 * 1024, "half of RAM unexpectedly small: {resolved}");
    // And less than 1TB, just to catch parse failures returning garbage.
    assert!(resolved < 1024 * 1024 * 1024 * 1024, "half of RAM unexpectedly large: {resolved}");
}

// ── block_height persistence ─────────────────────────────────────────

#[test]
fn update_persists_block_height() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.redb");
    {
        let mut storage =
            RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::new(resolver, KEY_LEN, None);
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
    let mut storage =
        RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    let resolver = storage.resolver();
    let tree = AVLTree::new(resolver, KEY_LEN, None);
    let mut prover = BatchAVLProver::new(tree, true);

    // First update at height 100.
    prover
        .perform_one_operation(&Operation::Insert(KeyValue {
            key: make_key(1),
            value: make_value(1, 64),
        }))
        .unwrap();
    storage.update_with_height(&mut prover, vec![], 100).unwrap();
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
    storage.update_with_height(&mut prover, vec![], 101).unwrap();

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
    storage.update_with_height(&mut prover, vec![], 102).unwrap();

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
        let mut storage =
            RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();

        let resolver = storage.resolver();
        let tree = AVLTree::new(resolver, KEY_LEN, None);
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
        let mut storage =
            RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
        let resolver = storage.resolver();
        let tree = AVLTree::new(resolver, KEY_LEN, None);
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
    let storage =
        RedbAVLStorage::open(&path, params(), 10, CacheSize::default()).unwrap();
    assert!(storage.version().is_none());
    assert!(storage.block_height().is_none());
}
