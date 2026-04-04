use ergo_sync::snapshot::manifest::{extract_subtree_ids, parse_manifest_header};
use ergo_sync::snapshot::parser::{
    compute_internal_label, compute_leaf_label, PACKED_INTERNAL_PREFIX, PACKED_LEAF_PREFIX,
};

/// Helper: serialize an internal node into packed bytes.
fn pack_internal(balance: i8, key: &[u8; 32], left_label: &[u8; 32], right_label: &[u8; 32]) -> Vec<u8> {
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
    // Valid header
    let data = [7u8, 3u8, 0xFF];
    let (root_height, manifest_depth) = parse_manifest_header(&data).unwrap();
    assert_eq!(root_height, 7);
    assert_eq!(manifest_depth, 3);

    // Exactly 2 bytes
    let data = [12u8, 1u8];
    let (rh, md) = parse_manifest_header(&data).unwrap();
    assert_eq!(rh, 12);
    assert_eq!(md, 1);

    // Too short
    assert!(parse_manifest_header(&[42u8]).is_err());
    assert!(parse_manifest_header(&[]).is_err());
}

/// Depth-1 manifest: root is an internal node at depth 0, with two internal
/// children at depth 1 (the boundary). Each boundary internal node contributes
/// its left_label and right_label as subtree IDs. Total: 4 subtree IDs.
///
/// Tree structure (manifest_depth = 1):
///
///         root (depth 0, internal)
///        /                        \
///   child_L (depth 1, boundary)   child_R (depth 1, boundary)
///   -> left_label_L, right_label_L   -> left_label_R, right_label_R
///
/// DFS pre-order: root, child_L, child_R
#[test]
fn depth_1_all_internal_boundary_extracts_4_ids() {
    let root_height = 5u8;
    let manifest_depth = 1u8;

    // The subtree labels that the boundary nodes point to (not serialized).
    let subtree_a = [0xA0u8; 32];
    let subtree_b = [0xB0u8; 32];
    let subtree_c = [0xC0u8; 32];
    let subtree_d = [0xD0u8; 32];

    // Boundary internal nodes (depth 1).
    let key_l = [0x10u8; 32];
    let child_l_label = compute_internal_label(0, &subtree_a, &subtree_b);
    let key_r = [0x20u8; 32];
    let child_r_label = compute_internal_label(0, &subtree_c, &subtree_d);

    // Root internal node (depth 0).
    let root_key = [0x10u8; 32];

    // Build manifest bytes: header + DFS(root, child_L, child_R)
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

/// Mixed tree: root is internal at depth 0, left child is a leaf (terminates
/// before boundary), right child is an internal at the boundary depth.
/// Only the boundary internal node produces subtree IDs.
///
/// Tree structure (manifest_depth = 1):
///
///         root (depth 0, internal)
///        /                        \
///   leaf_L (depth 1, leaf)     child_R (depth 1, boundary internal)
///                              -> subtree_c, subtree_d
///
/// DFS pre-order: root, leaf_L, child_R
#[test]
fn mixed_tree_leaf_before_boundary() {
    let root_height = 3u8;
    let manifest_depth = 1u8;

    // Left child: leaf at depth 1 (terminates, no subtree IDs).
    let leaf_key = [0x05u8; 32];
    let leaf_value = b"some_box_data";
    let leaf_next = [0x10u8; 32];
    let leaf_label = compute_leaf_label(&leaf_key, leaf_value, &leaf_next);

    // Right child: internal at depth 1 (boundary).
    let subtree_c = [0xC0u8; 32];
    let subtree_d = [0xD0u8; 32];
    let key_r = [0x10u8; 32];
    let child_r_label = compute_internal_label(1, &subtree_c, &subtree_d);

    // Root internal node.
    let root_key = [0x05u8; 32];

    let mut manifest = Vec::new();
    manifest.push(root_height);
    manifest.push(manifest_depth);
    manifest.extend_from_slice(&pack_internal(0, &root_key, &leaf_label, &child_r_label));
    manifest.extend_from_slice(&pack_leaf(&leaf_key, leaf_value, &leaf_next));
    manifest.extend_from_slice(&pack_internal(1, &key_r, &subtree_c, &subtree_d));

    let ids = extract_subtree_ids(&manifest, 32).unwrap();
    assert_eq!(ids.len(), 2, "expected 2 subtree IDs from right boundary node, got {}", ids.len());
    assert_eq!(ids[0], subtree_c);
    assert_eq!(ids[1], subtree_d);
}

/// Depth 2 manifest with a deeper tree: root at depth 0, two internals at depth 1,
/// four boundary internals at depth 2. Should produce 8 subtree IDs.
///
///                root (depth 0)
///               /              \
///         mid_L (depth 1)    mid_R (depth 1)
///        /        \          /        \
///    b_LL (d2)  b_LR (d2)  b_RL (d2)  b_RR (d2)
///
/// DFS: root, mid_L, b_LL, b_LR, mid_R, b_RL, b_RR
#[test]
fn depth_2_full_tree_extracts_8_ids() {
    let root_height = 10u8;
    let manifest_depth = 2u8;

    // 8 subtree labels
    let st: Vec<[u8; 32]> = (0..8u8).map(|i| [0x50 + i; 32]).collect();

    // Boundary nodes (depth 2)
    let key_ll = [0x01u8; 32];
    let b_ll_label = compute_internal_label(0, &st[0], &st[1]);
    let key_lr = [0x02u8; 32];
    let b_lr_label = compute_internal_label(0, &st[2], &st[3]);
    let key_rl = [0x03u8; 32];
    let b_rl_label = compute_internal_label(0, &st[4], &st[5]);
    let key_rr = [0x04u8; 32];
    let b_rr_label = compute_internal_label(0, &st[6], &st[7]);

    // Mid-level nodes (depth 1)
    let key_ml = [0x01u8; 32];
    let mid_l_label = compute_internal_label(0, &b_ll_label, &b_lr_label);
    let key_mr = [0x03u8; 32];
    let mid_r_label = compute_internal_label(0, &b_rl_label, &b_rr_label);

    // Root
    let root_key = [0x01u8; 32];

    let mut manifest = Vec::new();
    manifest.push(root_height);
    manifest.push(manifest_depth);
    // DFS pre-order
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
