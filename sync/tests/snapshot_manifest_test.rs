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
