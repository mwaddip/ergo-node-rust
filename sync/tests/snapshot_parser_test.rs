use ergo_avltree_rust::batch_node::LeafNode;
use ergo_sync::snapshot::parser::{
    compute_internal_label, compute_leaf_label, parse_dfs_stream, parse_node, ParsedNode,
    PACKED_INTERNAL_PREFIX, PACKED_LEAF_PREFIX,
};
use bytes::Bytes;

/// Verify our leaf label computation matches ergo_avltree_rust.
#[test]
fn leaf_label_matches_library() {
    let key = [0xAAu8; 32];
    let value = b"hello world";
    let next_key = [0xBBu8; 32];

    // Compute via library
    let leaf = LeafNode::new(
        &Bytes::copy_from_slice(&key),
        &Bytes::copy_from_slice(value),
        &Bytes::copy_from_slice(&next_key),
    );
    let lib_label = leaf.borrow_mut().label();

    // Compute via our function
    let our_label = compute_leaf_label(&key, value, &next_key);

    assert_eq!(our_label, lib_label, "leaf label mismatch with ergo_avltree_rust");
}

/// Verify our internal label computation matches ergo_avltree_rust.
#[test]
fn internal_label_matches_library() {
    use ergo_avltree_rust::batch_node::InternalNode;

    // Create two leaves to get real labels for children
    let leaf_a = LeafNode::new(
        &Bytes::copy_from_slice(&[0x01u8; 32]),
        &Bytes::copy_from_slice(b"value_a"),
        &Bytes::copy_from_slice(&[0x02u8; 32]),
    );
    let leaf_b = LeafNode::new(
        &Bytes::copy_from_slice(&[0x02u8; 32]),
        &Bytes::copy_from_slice(b"value_b"),
        &Bytes::copy_from_slice(&[0xFFu8; 32]),
    );

    let left_label = leaf_a.borrow_mut().label();
    let right_label = leaf_b.borrow_mut().label();

    let internal = InternalNode::new(
        Some(Bytes::copy_from_slice(&[0x01u8; 32])),
        &leaf_a,
        &leaf_b,
        0i8,
    );
    let lib_label = internal.borrow_mut().label();

    let our_label = compute_internal_label(0i8, &left_label, &right_label);

    assert_eq!(our_label, lib_label, "internal label mismatch with ergo_avltree_rust");
}

/// Verify internal label with non-zero balance.
#[test]
fn internal_label_nonzero_balance() {
    use ergo_avltree_rust::batch_node::InternalNode;

    let leaf_a = LeafNode::new(
        &Bytes::copy_from_slice(&[0x10u8; 32]),
        &Bytes::copy_from_slice(b"a"),
        &Bytes::copy_from_slice(&[0x20u8; 32]),
    );
    let leaf_b = LeafNode::new(
        &Bytes::copy_from_slice(&[0x20u8; 32]),
        &Bytes::copy_from_slice(b"b"),
        &Bytes::copy_from_slice(&[0xFFu8; 32]),
    );

    let left_label = leaf_a.borrow_mut().label();
    let right_label = leaf_b.borrow_mut().label();

    // Balance = -1
    let internal = InternalNode::new(
        Some(Bytes::copy_from_slice(&[0x10u8; 32])),
        &leaf_a,
        &leaf_b,
        -1i8,
    );
    let lib_label = internal.borrow_mut().label();
    let our_label = compute_internal_label(-1i8, &left_label, &right_label);
    assert_eq!(our_label, lib_label, "balance=-1 label mismatch");

    // Balance = 1
    let internal = InternalNode::new(
        Some(Bytes::copy_from_slice(&[0x10u8; 32])),
        &leaf_a,
        &leaf_b,
        1i8,
    );
    let lib_label = internal.borrow_mut().label();
    let our_label = compute_internal_label(1i8, &left_label, &right_label);
    assert_eq!(our_label, lib_label, "balance=1 label mismatch");
}

#[test]
fn parse_internal_node() {
    let balance: i8 = -1;
    let key = [0x11u8; 32];
    let left_label = [0x22u8; 32];
    let right_label = [0x33u8; 32];

    let mut packed = Vec::with_capacity(98);
    packed.push(PACKED_INTERNAL_PREFIX);
    packed.push(balance as u8);
    packed.extend_from_slice(&key);
    packed.extend_from_slice(&left_label);
    packed.extend_from_slice(&right_label);

    let (node, consumed) = parse_node(&packed, 32).unwrap();
    assert_eq!(consumed, 98);
    match node {
        ParsedNode::Internal {
            label,
            balance: b,
            key: k,
            left_label: ll,
            right_label: rl,
            packed_bytes,
        } => {
            assert_eq!(b, -1);
            assert_eq!(k, key);
            assert_eq!(ll, left_label);
            assert_eq!(rl, right_label);
            assert_eq!(packed_bytes, packed);
            let expected = compute_internal_label(balance, &left_label, &right_label);
            assert_eq!(label, expected);
        }
        _ => panic!("expected internal node"),
    }
}

#[test]
fn parse_leaf_node() {
    let key = [0xAAu8; 32];
    let value = b"test_value";
    let next_key = [0xBBu8; 32];

    let mut packed = Vec::new();
    packed.push(PACKED_LEAF_PREFIX);
    packed.extend_from_slice(&key);
    packed.extend_from_slice(&(value.len() as u32).to_be_bytes());
    packed.extend_from_slice(value);
    packed.extend_from_slice(&next_key);

    let (node, consumed) = parse_node(&packed, 32).unwrap();
    assert_eq!(consumed, 1 + 32 + 4 + value.len() + 32);
    match node {
        ParsedNode::Leaf {
            label,
            key: k,
            value: v,
            next_key: nk,
            ..
        } => {
            assert_eq!(k, key);
            assert_eq!(v.as_slice(), value.as_slice());
            assert_eq!(nk, next_key);
            let expected = compute_leaf_label(&key, value, &next_key);
            assert_eq!(label, expected);
        }
        _ => panic!("expected leaf node"),
    }
}

#[test]
fn parse_dfs_three_node_tree() {
    let key_a = [0x01u8; 32];
    let value_a = b"box_a";
    let next_a = [0x02u8; 32];

    let key_b = [0x02u8; 32];
    let value_b = b"box_b";
    let next_b = [0xFFu8; 32];

    let label_a = compute_leaf_label(&key_a, value_a, &next_a);
    let label_b = compute_leaf_label(&key_b, value_b, &next_b);

    let mut stream = Vec::new();

    // Internal node
    stream.push(PACKED_INTERNAL_PREFIX);
    stream.push(0x00); // balance = 0
    stream.extend_from_slice(&key_a);
    stream.extend_from_slice(&label_a);
    stream.extend_from_slice(&label_b);

    // Leaf A
    stream.push(PACKED_LEAF_PREFIX);
    stream.extend_from_slice(&key_a);
    stream.extend_from_slice(&(value_a.len() as u32).to_be_bytes());
    stream.extend_from_slice(value_a);
    stream.extend_from_slice(&next_a);

    // Leaf B
    stream.push(PACKED_LEAF_PREFIX);
    stream.extend_from_slice(&key_b);
    stream.extend_from_slice(&(value_b.len() as u32).to_be_bytes());
    stream.extend_from_slice(value_b);
    stream.extend_from_slice(&next_b);

    let nodes = parse_dfs_stream(&stream, 32).unwrap();
    assert_eq!(nodes.len(), 3);

    assert!(matches!(nodes[0], ParsedNode::Internal { .. }));
    assert!(matches!(nodes[1], ParsedNode::Leaf { .. }));
    assert!(matches!(nodes[2], ParsedNode::Leaf { .. }));

    let expected_root = compute_internal_label(0, &label_a, &label_b);
    assert_eq!(*nodes[0].label(), expected_root);
    assert_eq!(*nodes[1].label(), label_a);
    assert_eq!(*nodes[2].label(), label_b);
}

#[test]
fn parse_truncated_data_returns_error() {
    // Truncated internal node (only 50 bytes instead of 98)
    let data = vec![PACKED_INTERNAL_PREFIX; 50];
    assert!(parse_node(&data, 32).is_err());

    // Empty data
    assert!(parse_node(&[], 32).is_err());

    // Invalid prefix
    assert!(parse_node(&[0x02], 32).is_err());
}
