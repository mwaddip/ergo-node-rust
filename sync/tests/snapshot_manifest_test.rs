use ergo_sync::snapshot::manifest::{
    extract_subtree_ids, parse_manifest_header, verify_manifest, ManifestError,
};
use ergo_sync::snapshot::parser::{
    compute_internal_label, compute_leaf_label, ParseError, PACKED_INTERNAL_PREFIX,
    PACKED_LEAF_PREFIX,
};
use ergo_sync::snapshot::tree::{Side, TreeError};

/// Helper: serialize an internal node into packed bytes.
fn pack_internal(
    balance: i8,
    key: &[u8; 32],
    left_label: &[u8; 32],
    right_label: &[u8; 32],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(98);
    buf.push(PACKED_INTERNAL_PREFIX);
    buf.push(balance as u8);
    buf.extend_from_slice(key);
    buf.extend_from_slice(left_label);
    buf.extend_from_slice(right_label);
    buf
}

/// Helper: serialize a leaf node into packed bytes.
fn pack_leaf(key: &[u8; 32], value: &[u8], next_key: &[u8; 32]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(PACKED_LEAF_PREFIX);
    buf.extend_from_slice(key);
    buf.extend_from_slice(&(value.len() as u32).to_be_bytes());
    buf.extend_from_slice(value);
    buf.extend_from_slice(next_key);
    buf
}

#[test]
fn header_parsing() {
    let data = [7u8, 3u8, 0xFF];
    let (root_height, manifest_depth) = parse_manifest_header(&data).unwrap();
    assert_eq!(root_height, 7);
    assert_eq!(manifest_depth, 3);

    let data = [12u8, 1u8];
    let (rh, md) = parse_manifest_header(&data).unwrap();
    assert_eq!(rh, 12);
    assert_eq!(md, 1);

    assert!(parse_manifest_header(&[42u8]).is_err());
    assert!(parse_manifest_header(&[]).is_err());
}

/// Root at JVM level 1, two internal children at level 2 = boundary.
/// manifest_depth = 2. Each boundary node contributes 2 subtree IDs. Total: 4.
///
///         root (level 1, internal)
///        /                        \
///   child_L (level 2, boundary)   child_R (level 2, boundary)
///
/// DFS: root, child_L, child_R
#[test]
fn depth_2_boundary_extracts_4_ids() {
    let root_height = 5u8;
    let manifest_depth = 2u8; // JVM: root=1, children=2=boundary

    let subtree_a = [0xA0u8; 32];
    let subtree_b = [0xB0u8; 32];
    let subtree_c = [0xC0u8; 32];
    let subtree_d = [0xD0u8; 32];

    let key_l = [0x10u8; 32];
    let child_l_label = compute_internal_label(0, &subtree_a, &subtree_b);
    let key_r = [0x20u8; 32];
    let child_r_label = compute_internal_label(0, &subtree_c, &subtree_d);

    let root_key = [0x10u8; 32];

    let mut manifest = Vec::new();
    manifest.push(root_height);
    manifest.push(manifest_depth);
    manifest.extend_from_slice(&pack_internal(0, &root_key, &child_l_label, &child_r_label));
    manifest.extend_from_slice(&pack_internal(0, &key_l, &subtree_a, &subtree_b));
    manifest.extend_from_slice(&pack_internal(0, &key_r, &subtree_c, &subtree_d));

    let ids = extract_subtree_ids(&manifest, 32).unwrap();
    assert_eq!(ids.len(), 4, "expected 4 subtree IDs, got {}", ids.len());
    assert_eq!(ids[0], subtree_a);
    assert_eq!(ids[1], subtree_b);
    assert_eq!(ids[2], subtree_c);
    assert_eq!(ids[3], subtree_d);
}

/// Root level 1, left child is a leaf (terminates), right child is internal at
/// level 2 (boundary). Only the boundary node produces subtree IDs.
///
/// manifest_depth = 2:
///         root (level 1)
///        /               \
///   leaf_L (level 2)    child_R (level 2, boundary)
#[test]
fn mixed_tree_leaf_before_boundary() {
    let root_height = 3u8;
    let manifest_depth = 2u8;

    let leaf_key = [0x05u8; 32];
    let leaf_value = b"some_box_data";
    let leaf_next = [0x10u8; 32];
    let leaf_label = compute_leaf_label(&leaf_key, leaf_value, &leaf_next);

    let subtree_c = [0xC0u8; 32];
    let subtree_d = [0xD0u8; 32];
    let key_r = [0x10u8; 32];
    let child_r_label = compute_internal_label(1, &subtree_c, &subtree_d);

    let root_key = [0x05u8; 32];

    let mut manifest = Vec::new();
    manifest.push(root_height);
    manifest.push(manifest_depth);
    manifest.extend_from_slice(&pack_internal(0, &root_key, &leaf_label, &child_r_label));
    manifest.extend_from_slice(&pack_leaf(&leaf_key, leaf_value, &leaf_next));
    manifest.extend_from_slice(&pack_internal(1, &key_r, &subtree_c, &subtree_d));

    let ids = extract_subtree_ids(&manifest, 32).unwrap();
    assert_eq!(ids.len(), 2, "expected 2 subtree IDs, got {}", ids.len());
    assert_eq!(ids[0], subtree_c);
    assert_eq!(ids[1], subtree_d);
}

/// Deeper tree: root level 1, two internals at level 2, four boundary internals
/// at level 3. manifest_depth = 3. Should produce 8 subtree IDs.
///
///                root (level 1)
///               /              \
///         mid_L (level 2)    mid_R (level 2)
///        /        \          /        \
///    b_LL (lv3)  b_LR (3)  b_RL (3)  b_RR (3)
///
/// DFS: root, mid_L, b_LL, b_LR, mid_R, b_RL, b_RR
#[test]
fn depth_3_full_tree_extracts_8_ids() {
    let root_height = 10u8;
    let manifest_depth = 3u8;

    let st: Vec<[u8; 32]> = (0..8u8).map(|i| [0x50 + i; 32]).collect();

    let key_ll = [0x01u8; 32];
    let b_ll_label = compute_internal_label(0, &st[0], &st[1]);
    let key_lr = [0x02u8; 32];
    let b_lr_label = compute_internal_label(0, &st[2], &st[3]);
    let key_rl = [0x03u8; 32];
    let b_rl_label = compute_internal_label(0, &st[4], &st[5]);
    let key_rr = [0x04u8; 32];
    let b_rr_label = compute_internal_label(0, &st[6], &st[7]);

    let key_ml = [0x01u8; 32];
    let mid_l_label = compute_internal_label(0, &b_ll_label, &b_lr_label);
    let key_mr = [0x03u8; 32];
    let mid_r_label = compute_internal_label(0, &b_rl_label, &b_rr_label);

    let root_key = [0x01u8; 32];

    let mut manifest = Vec::new();
    manifest.push(root_height);
    manifest.push(manifest_depth);
    manifest.extend_from_slice(&pack_internal(0, &root_key, &mid_l_label, &mid_r_label));
    manifest.extend_from_slice(&pack_internal(0, &key_ml, &b_ll_label, &b_lr_label));
    manifest.extend_from_slice(&pack_internal(0, &key_ll, &st[0], &st[1]));
    manifest.extend_from_slice(&pack_internal(0, &key_lr, &st[2], &st[3]));
    manifest.extend_from_slice(&pack_internal(0, &key_mr, &b_rl_label, &b_rr_label));
    manifest.extend_from_slice(&pack_internal(0, &key_rl, &st[4], &st[5]));
    manifest.extend_from_slice(&pack_internal(0, &key_rr, &st[6], &st[7]));

    let ids = extract_subtree_ids(&manifest, 32).unwrap();
    assert_eq!(ids.len(), 8, "expected 8 subtree IDs, got {}", ids.len());
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, st[i], "subtree ID mismatch at index {i}");
    }
}

// ── Byte accounting and verification ────────────────────────────────────────

/// The depth-2 manifest from `depth_2_boundary_extracts_4_ids`, with the
/// 33-byte state root a header would carry for it.
fn depth_2_manifest() -> (Vec<u8>, [u8; 33], Vec<[u8; 32]>) {
    let root_height = 5u8;
    let subtrees = [[0xA0u8; 32], [0xB0; 32], [0xC0; 32], [0xD0; 32]];
    let child_l = compute_internal_label(0, &subtrees[0], &subtrees[1]);
    let child_r = compute_internal_label(0, &subtrees[2], &subtrees[3]);
    let root = compute_internal_label(0, &child_l, &child_r);

    let mut manifest = vec![root_height, 2];
    manifest.extend_from_slice(&pack_internal(0, &[0x10; 32], &child_l, &child_r));
    manifest.extend_from_slice(&pack_internal(0, &[0x10; 32], &subtrees[0], &subtrees[1]));
    manifest.extend_from_slice(&pack_internal(0, &[0x20; 32], &subtrees[2], &subtrees[3]));

    let mut state_root = [0u8; 33];
    state_root[..32].copy_from_slice(&root);
    state_root[32] = root_height;
    (manifest, state_root, subtrees.to_vec())
}

#[test]
fn verify_binds_root_label_and_height_to_the_state_root() {
    let (manifest, state_root, subtrees) = depth_2_manifest();
    let verified = verify_manifest(manifest.clone(), &state_root).unwrap();
    assert_eq!(&verified.root_hash()[..], &state_root[..32]);
    assert_eq!(verified.root_height(), 5);
    assert_eq!(verified.subtree_ids(), subtrees.as_slice());
    assert_eq!(verified.bytes(), manifest.as_slice());
    assert_eq!(verified.node_bytes(), &manifest[2..]);
}

#[test]
fn verify_accepts_a_single_leaf_manifest() {
    let key = [1u8; 32];
    let value = b"box";
    let next = [2u8; 32];
    let label = compute_leaf_label(&key, value, &next);
    let mut manifest = vec![1u8, 14];
    manifest.extend_from_slice(&pack_leaf(&key, value, &next));
    let mut state_root = [0u8; 33];
    state_root[..32].copy_from_slice(&label);
    state_root[32] = 1;

    let verified = verify_manifest(manifest, &state_root).unwrap();
    assert_eq!(verified.root_hash(), label);
    assert!(verified.subtree_ids().is_empty());
}

#[test]
fn verify_rejects_a_root_label_that_is_not_the_headers() {
    let (mut manifest, state_root, _) = depth_2_manifest();
    // Flip the root node's balance byte: its label changes, the rest of the
    // manifest stays well-formed.
    manifest[3] ^= 0x01;
    assert_eq!(
        verify_manifest(manifest, &state_root).unwrap_err(),
        ManifestError::Tree(TreeError::RootMismatch)
    );
}

#[test]
fn verify_rejects_a_root_height_that_is_not_the_headers() {
    let (mut manifest, state_root, _) = depth_2_manifest();
    manifest[0] = 6;
    assert_eq!(
        verify_manifest(manifest, &state_root).unwrap_err(),
        ManifestError::RootHeightMismatch {
            expected: 5,
            got: 6
        }
    );
}

#[test]
fn walk_rejects_a_manifest_cut_at_a_node_boundary() {
    let (mut manifest, state_root, _) = depth_2_manifest();
    // Drop the last boundary node whole: every remaining node parses, and a
    // walk that stopped at end-of-bytes would report two subtree ids of four.
    manifest.truncate(manifest.len() - 98);
    assert_eq!(
        extract_subtree_ids(&manifest, 32).unwrap_err(),
        ManifestError::Tree(TreeError::Truncated)
    );
    assert_eq!(
        verify_manifest(manifest, &state_root).unwrap_err(),
        ManifestError::Tree(TreeError::Truncated)
    );
}

#[test]
fn walk_rejects_a_manifest_cut_inside_a_node() {
    let (mut manifest, _, _) = depth_2_manifest();
    manifest.truncate(manifest.len() - 10);
    assert_eq!(
        extract_subtree_ids(&manifest, 32).unwrap_err(),
        ManifestError::Tree(TreeError::Parse(ParseError::UnexpectedEof))
    );
}

#[test]
fn walk_rejects_trailing_bytes_after_the_tree() {
    let (mut manifest, state_root, _) = depth_2_manifest();
    manifest.extend_from_slice(&[0u8; 3]);
    assert_eq!(
        extract_subtree_ids(&manifest, 32).unwrap_err(),
        ManifestError::Tree(TreeError::TrailingBytes(3))
    );
    assert_eq!(
        verify_manifest(manifest, &state_root).unwrap_err(),
        ManifestError::Tree(TreeError::TrailingBytes(3))
    );
}

#[test]
fn walk_rejects_a_zero_manifest_depth() {
    let (mut manifest, _, _) = depth_2_manifest();
    manifest[1] = 0;
    assert_eq!(
        extract_subtree_ids(&manifest, 32).unwrap_err(),
        ManifestError::ZeroDepth
    );
}

#[test]
fn walk_rejects_an_unknown_node_prefix() {
    let (mut manifest, _, _) = depth_2_manifest();
    // The prefix byte of the second node.
    manifest[2 + 98] = 0x07;
    assert_eq!(
        extract_subtree_ids(&manifest, 32).unwrap_err(),
        ManifestError::Tree(TreeError::Parse(ParseError::InvalidPrefix(0x07)))
    );
}

// ── Parent-child link verification ──────────────────────────────────────────

/// Build a depth-2 manifest where the root is correct (its label commits to
/// the real left/right labels) but the left boundary node is replaced by a
/// *different* internal node. The root's stored left_label no longer matches
/// the recomputed label of the node that follows it.
#[test]
fn verify_rejects_an_interior_child_replaced_by_a_different_node() {
    // Real children:
    let subtrees = [[0xA0u8; 32], [0xB0; 32], [0xC0; 32], [0xD0; 32]];
    let child_l = compute_internal_label(0, &subtrees[0], &subtrees[1]);
    let child_r = compute_internal_label(0, &subtrees[2], &subtrees[3]);
    let root = compute_internal_label(0, &child_l, &child_r);

    // A replacement with different content → different recomputed label.
    let rogue_l = compute_internal_label(0, &[0xEE; 32], &[0xFF; 32]);

    let root_height = 5u8;
    let mut manifest = vec![root_height, 2];
    manifest.extend_from_slice(&pack_internal(0, &[0x10; 32], &child_l, &child_r));
    // Slot in the replacement: root's stored left_label stays child_l, but the
    // node hashes to rogue_l.
    manifest.extend_from_slice(&pack_internal(0, &[0x10; 32], &[0xEE; 32], &[0xFF; 32]));
    manifest.extend_from_slice(&pack_internal(0, &[0x20; 32], &subtrees[2], &subtrees[3]));

    let mut state_root = [0u8; 33];
    state_root[..32].copy_from_slice(&root);
    state_root[32] = root_height;

    // The root node's left_label ≠ the rogue node's recomputed label.
    assert_ne!(child_l, rogue_l, "test setup: the labels should differ");

    let err = verify_manifest(manifest, &state_root).unwrap_err();
    assert_eq!(
        err,
        ManifestError::Tree(TreeError::BrokenLink {
            parent: 0,
            side: Side::Left,
        })
    );
}

/// Same idea, but the two children are swapped: left node where right should
/// be and vice versa. Each child is well-formed, but the root's stored labels
/// don't match what is at each position.
#[test]
fn verify_rejects_two_children_swapped() {
    let subtrees = [[0xA0u8; 32], [0xB0; 32], [0xC0; 32], [0xD0; 32]];
    let child_l = compute_internal_label(0, &subtrees[0], &subtrees[1]);
    let child_r = compute_internal_label(0, &subtrees[2], &subtrees[3]);
    let root = compute_internal_label(0, &child_l, &child_r);

    let root_height = 5u8;
    let mut manifest = vec![root_height, 2];
    manifest.extend_from_slice(&pack_internal(0, &[0x10; 32], &child_l, &child_r));
    // DFS: left position should carry child_l, but we put child_r there.
    manifest.extend_from_slice(&pack_internal(0, &[0x20; 32], &subtrees[2], &subtrees[3]));
    manifest.extend_from_slice(&pack_internal(0, &[0x10; 32], &subtrees[0], &subtrees[1]));

    let mut state_root = [0u8; 33];
    state_root[..32].copy_from_slice(&root);
    state_root[32] = root_height;

    let err = verify_manifest(manifest, &state_root).unwrap_err();
    // The walk sees the first child (child_r) where it expected child_l.
    assert_eq!(
        err,
        ManifestError::Tree(TreeError::BrokenLink {
            parent: 0,
            side: Side::Left,
        })
    );
}
