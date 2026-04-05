# UTXO Snapshot Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bootstrap UTXO state from a peer-served snapshot instead of replaying 267k+ blocks from genesis.

**Architecture:** Six new P2P message codes (76-81) carry snapshot discovery, manifest, and chunk data. The sync machine gains a snapshot phase between header sync and block download. Manifest/chunk parsing and label computation live in sync/; the parsed (label, packed_bytes) pairs feed into state/'s existing `load_snapshot()`. A temporary redb file persists chunk downloads across crashes. The validator is created after snapshot loading via a oneshot channel handoff between the sync task and main.

**Tech Stack:** blake2 (label hashing), redb (chunk download storage), tokio oneshot channels (validator handoff), ergo_avltree_rust types (Digest32, ADDigest, Bytes)

**Contract:** `facts/snapshot.md`

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `sync/src/snapshot/mod.rs` | Module entry, `SnapshotData` type, `run_snapshot_sync()` orchestrator |
| `sync/src/snapshot/parser.rs` | AVL+ node DFS parser, label computation (Blake2b256) |
| `sync/src/snapshot/manifest.rs` | Manifest shallow parse: extract subtree IDs without label computation |
| `sync/src/snapshot/protocol.rs` | Serialize/deserialize P2P messages 76-81 from `Unknown { code, body }` |
| `sync/src/snapshot/download.rs` | Temporary redb file for chunk storage + crash recovery |
| `sync/tests/snapshot_parser_test.rs` | Tests for DFS parsing and label computation |
| `sync/tests/snapshot_manifest_test.rs` | Tests for subtree ID extraction |
| `sync/tests/snapshot_protocol_test.rs` | Tests for P2P message codec |
| `sync/tests/snapshot_download_test.rs` | Tests for temporary redb storage |
| `sync/tests/snapshot_roundtrip_test.rs` | End-to-end: build tree, serialize, parse, verify labels |

### Modified files

| File | Changes |
|------|---------|
| `sync/src/lib.rs` | Add `pub mod snapshot;` export |
| `sync/src/state.rs` | Add snapshot phase in `run()` after `SyncOutcome::Synced`, make validator `Option<V>`, add `snapshot_sync()` integration |
| `sync/src/traits.rs` | Add `header_state_root()` to `SyncChain` |
| `sync/Cargo.toml` | Add `blake2`, `redb`, `tempfile` (dev) dependencies |
| `validation/src/lib.rs` | No changes needed (validator created externally) |
| `src/main.rs` | Config additions (`utxo_bootstrap`, `min_snapshot_peers`), optional validator, oneshot channel handoff, snapshot loading into storage |
| `src/bridge.rs` | Implement `header_state_root()` on `SharedChain` |

---

## Task 1: AVL+ Node Label Computation

Foundation for everything else. The label formula MUST match ergo_avltree_rust exactly.

**Files:**
- Create: `sync/src/snapshot/parser.rs`
- Create: `sync/src/snapshot/mod.rs`
- Modify: `sync/src/lib.rs`
- Modify: `sync/Cargo.toml`
- Test: `sync/tests/snapshot_parser_test.rs`

- [ ] **Step 1: Add dependencies to sync/Cargo.toml**

```toml
# Under [dependencies], add:
blake2 = "0.10"
redb = "2"
bytes = "1"

# Under [dev-dependencies], add:
tempfile = "3"
ergo_avltree_rust = { git = "https://github.com/mwaddip/ergo_avltree_rust.git", rev = "28862a1" }
```

Note: `ergo_avltree_rust` is dev-only — used in tests to verify our label computation matches the library. Production code computes labels directly with `blake2`.

- [ ] **Step 2: Create module skeleton**

`sync/src/snapshot/mod.rs`:
```rust
pub mod parser;
pub mod manifest;
pub mod protocol;
pub mod download;

/// Parsed snapshot data ready for loading into state storage.
pub struct SnapshotData {
    /// (node_label, packed_node_bytes) pairs for all nodes in the snapshot.
    pub nodes: Vec<([u8; 32], Vec<u8>)>,
    /// Root hash of the AVL+ tree (first 32 bytes of stateRoot).
    pub root_hash: [u8; 32],
    /// Height of the AVL+ tree (last byte of stateRoot).
    pub tree_height: u8,
    /// Block height at which this snapshot was taken.
    pub snapshot_height: u32,
}

/// Configuration for snapshot sync.
pub struct SnapshotConfig {
    /// Minimum peers that must announce the same manifest before downloading.
    pub min_snapshot_peers: u32,
    /// Delivery timeout multiplier for chunks (applied to base delivery_timeout).
    pub chunk_timeout_multiplier: u32,
    /// Directory for temporary download storage.
    pub data_dir: std::path::PathBuf,
}
```

`sync/src/lib.rs` — add after existing module declarations:
```rust
pub mod snapshot;
```

- [ ] **Step 3: Write failing test for leaf label computation**

`sync/tests/snapshot_parser_test.rs`:
```rust
use ergo_avltree_rust::batch_node::{AVLTree, LeafNode, Blake2b256};
use ergo_avltree_rust::operation::Digest32;
use blake2::Digest;
use bytes::Bytes;
use std::sync::Arc;

/// Verify our label computation matches ergo_avltree_rust for a leaf node.
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
    let lib_label = {
        let node = leaf.borrow();
        *node.label().expect("leaf should have label")
    };

    // Compute via our function
    let our_label = ergo_sync::snapshot::parser::compute_leaf_label(&key, value, &next_key);

    assert_eq!(our_label, lib_label);
}

/// Verify our label computation matches ergo_avltree_rust for an internal node.
#[test]
fn internal_label_matches_library() {
    // Create two leaves first to get real labels for children
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

    let left_label = *leaf_a.borrow().label().unwrap();
    let right_label = *leaf_b.borrow().label().unwrap();

    use ergo_avltree_rust::batch_node::{InternalNode, Node};
    let internal = InternalNode::new(
        Some(Bytes::copy_from_slice(&[0x01u8; 32])),
        &leaf_a,
        &leaf_b,
        0i8, // balance
    );
    let lib_label = *internal.borrow().label().unwrap();

    // Compute via our function
    let our_label = ergo_sync::snapshot::parser::compute_internal_label(
        0i8,
        &left_label,
        &right_label,
    );

    assert_eq!(our_label, lib_label);
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_parser_test -- --nocapture 2>&1 | head -30`

Expected: compile error — `compute_leaf_label` and `compute_internal_label` not found.

- [ ] **Step 5: Implement label computation**

`sync/src/snapshot/parser.rs`:
```rust
use blake2::digest::{FixedOutput, Update};

type Blake2b256 = blake2::Blake2b<blake2::digest::consts::U32>;

const LEAF_PREFIX: u8 = 0x00;
const INTERNAL_PREFIX: u8 = 0x01;

/// Packed node prefix bytes (used in serialization format).
pub const PACKED_LEAF_PREFIX: u8 = 0x01;
pub const PACKED_INTERNAL_PREFIX: u8 = 0x00;

/// Compute the Blake2b256 label for a leaf node.
///
/// Formula (matching ergo_avltree_rust): H(0x00 || key || value || next_key)
pub fn compute_leaf_label(key: &[u8], value: &[u8], next_key: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::default();
    hasher.update(&[LEAF_PREFIX]);
    hasher.update(key);
    hasher.update(value);
    hasher.update(next_key);
    let mut label = [0u8; 32];
    label.copy_from_slice(&hasher.finalize_fixed());
    label
}

/// Compute the Blake2b256 label for an internal node.
///
/// Formula (matching ergo_avltree_rust): H(0x01 || balance_byte || left_label || right_label)
/// Note: the key is NOT included in the label (matches JVM scorex behavior).
pub fn compute_internal_label(
    balance: i8,
    left_label: &[u8; 32],
    right_label: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Blake2b256::default();
    hasher.update(&[INTERNAL_PREFIX]);
    hasher.update(&[balance as u8]);
    hasher.update(left_label);
    hasher.update(right_label);
    let mut label = [0u8; 32];
    label.copy_from_slice(&hasher.finalize_fixed());
    label
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_parser_test -- --nocapture`

Expected: both tests PASS. If they fail, the label formula doesn't match ergo_avltree_rust — investigate before proceeding. This is consensus-critical; do not continue with mismatched labels.

- [ ] **Step 7: Commit**

```bash
git add sync/src/snapshot/ sync/src/lib.rs sync/Cargo.toml sync/tests/snapshot_parser_test.rs
git commit -m "feat(sync): AVL+ node label computation for snapshot sync

Blake2b256 label computation matching ergo_avltree_rust formulas:
- Leaf: H(0x00 || key || value || next_key)
- Internal: H(0x01 || balance || left_label || right_label)
Verified against library output in tests."
```

---

## Task 2: DFS Byte Stream Parser

Parse the packed node DFS byte stream used in both manifests and subtree chunks. Each call reads one node, advances the offset, returns the label and packed bytes.

**Files:**
- Modify: `sync/src/snapshot/parser.rs`
- Modify: `sync/tests/snapshot_parser_test.rs`

- [ ] **Step 1: Write failing test for single node parsing**

Add to `sync/tests/snapshot_parser_test.rs`:
```rust
use ergo_sync::snapshot::parser::{ParsedNode, parse_node};

#[test]
fn parse_internal_node() {
    let balance: i8 = -1;
    let key = [0x11u8; 32];
    let left_label = [0x22u8; 32];
    let right_label = [0x33u8; 32];

    // Build packed bytes: 0x00 + balance + key + left_label + right_label
    let mut packed = Vec::with_capacity(98);
    packed.push(0x00); // internal prefix
    packed.push(balance as u8);
    packed.extend_from_slice(&key);
    packed.extend_from_slice(&left_label);
    packed.extend_from_slice(&right_label);

    let (node, consumed) = parse_node(&packed, 32).unwrap();
    assert_eq!(consumed, 98);
    match node {
        ParsedNode::Internal { label, balance: b, key: k, left_label: ll, right_label: rl, packed_bytes } => {
            assert_eq!(b, -1);
            assert_eq!(k, key);
            assert_eq!(ll, left_label);
            assert_eq!(rl, right_label);
            assert_eq!(packed_bytes, packed);
            // Verify label
            let expected = ergo_sync::snapshot::parser::compute_internal_label(
                balance, &left_label, &right_label,
            );
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
    packed.push(0x01); // leaf prefix
    packed.extend_from_slice(&key);
    packed.extend_from_slice(&(value.len() as u32).to_be_bytes());
    packed.extend_from_slice(value);
    packed.extend_from_slice(&next_key);

    let (node, consumed) = parse_node(&packed, 32).unwrap();
    assert_eq!(consumed, 1 + 32 + 4 + value.len() + 32);
    match node {
        ParsedNode::Leaf { label, key: k, value: v, next_key: nk, packed_bytes } => {
            assert_eq!(k, key);
            assert_eq!(v.as_slice(), value);
            assert_eq!(nk, next_key);
            let expected = ergo_sync::snapshot::parser::compute_leaf_label(&key, value, &next_key);
            assert_eq!(label, expected);
        }
        _ => panic!("expected leaf node"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_parser_test parse_internal_node parse_leaf_node -- --nocapture 2>&1 | head -20`

Expected: compile error — `ParsedNode` and `parse_node` not defined.

- [ ] **Step 3: Implement DFS node parser**

Add to `sync/src/snapshot/parser.rs`:
```rust
/// A parsed AVL+ node with its computed label and raw packed bytes.
#[derive(Debug, Clone)]
pub enum ParsedNode {
    Internal {
        label: [u8; 32],
        balance: i8,
        key: [u8; 32],
        left_label: [u8; 32],
        right_label: [u8; 32],
        packed_bytes: Vec<u8>,
    },
    Leaf {
        label: [u8; 32],
        key: [u8; 32],
        value: Vec<u8>,
        next_key: [u8; 32],
        packed_bytes: Vec<u8>,
    },
}

impl ParsedNode {
    pub fn label(&self) -> &[u8; 32] {
        match self {
            ParsedNode::Internal { label, .. } => label,
            ParsedNode::Leaf { label, .. } => label,
        }
    }

    pub fn packed_bytes(&self) -> &[u8] {
        match self {
            ParsedNode::Internal { packed_bytes, .. } => packed_bytes,
            ParsedNode::Leaf { packed_bytes, .. } => packed_bytes,
        }
    }
}

/// Parse a single AVL+ node from a byte slice. Returns the parsed node and
/// number of bytes consumed.
///
/// The key_length is always 32 for Ergo (ADKey = box ID).
/// Value length is variable (no fixed length).
pub fn parse_node(data: &[u8], key_length: usize) -> Result<(ParsedNode, usize), ParseError> {
    if data.is_empty() {
        return Err(ParseError::UnexpectedEof);
    }

    match data[0] {
        PACKED_INTERNAL_PREFIX => {
            // 0x00 + balance(1) + key(key_length) + left_label(32) + right_label(32)
            let expected = 1 + 1 + key_length + 32 + 32;
            if data.len() < expected {
                return Err(ParseError::UnexpectedEof);
            }

            let balance = data[1] as i8;
            let mut key = [0u8; 32];
            key.copy_from_slice(&data[2..2 + key_length]);
            let mut left_label = [0u8; 32];
            left_label.copy_from_slice(&data[2 + key_length..2 + key_length + 32]);
            let mut right_label = [0u8; 32];
            right_label.copy_from_slice(&data[2 + key_length + 32..expected]);

            let label = compute_internal_label(balance, &left_label, &right_label);
            let packed_bytes = data[..expected].to_vec();

            Ok((
                ParsedNode::Internal {
                    label,
                    balance,
                    key,
                    left_label,
                    right_label,
                    packed_bytes,
                },
                expected,
            ))
        }
        PACKED_LEAF_PREFIX => {
            // 0x01 + key(key_length) + value_len(4 BE) + value + next_key(key_length)
            let header_len = 1 + key_length + 4;
            if data.len() < header_len {
                return Err(ParseError::UnexpectedEof);
            }

            let mut key = [0u8; 32];
            key.copy_from_slice(&data[1..1 + key_length]);

            let value_len = u32::from_be_bytes([
                data[1 + key_length],
                data[2 + key_length],
                data[3 + key_length],
                data[4 + key_length],
            ]) as usize;

            let total = header_len + value_len + key_length;
            if data.len() < total {
                return Err(ParseError::UnexpectedEof);
            }

            let value = data[header_len..header_len + value_len].to_vec();
            let mut next_key = [0u8; 32];
            next_key.copy_from_slice(&data[header_len + value_len..total]);

            let label = compute_leaf_label(&key, &value, &next_key);
            let packed_bytes = data[..total].to_vec();

            Ok((
                ParsedNode::Leaf {
                    label,
                    key,
                    value,
                    next_key,
                    packed_bytes,
                },
                total,
            ))
        }
        other => Err(ParseError::InvalidPrefix(other)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unexpected end of data")]
    UnexpectedEof,
    #[error("invalid node prefix byte: 0x{0:02x}")]
    InvalidPrefix(u8),
}
```

Add `thiserror = "2"` to sync/Cargo.toml `[dependencies]`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_parser_test -- --nocapture`

Expected: all 4 tests PASS (2 label tests + 2 parse tests).

- [ ] **Step 5: Add DFS stream parser**

Add to `sync/src/snapshot/parser.rs`:
```rust
/// Parse a complete DFS byte stream into a sequence of (label, packed_bytes) pairs.
/// Used for both manifest and chunk reconstruction.
pub fn parse_dfs_stream(
    data: &[u8],
    key_length: usize,
) -> Result<Vec<ParsedNode>, ParseError> {
    let mut nodes = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let (node, consumed) = parse_node(&data[offset..], key_length)?;
        nodes.push(node);
        offset += consumed;
    }

    Ok(nodes)
}
```

- [ ] **Step 6: Test DFS stream parsing with a 3-node tree**

Add to `sync/tests/snapshot_parser_test.rs`:
```rust
use ergo_sync::snapshot::parser::parse_dfs_stream;

#[test]
fn parse_dfs_three_node_tree() {
    // Tree: internal(leaf_a, leaf_b)
    // DFS order: internal, leaf_a, leaf_b

    let key_a = [0x01u8; 32];
    let value_a = b"box_a";
    let next_a = [0x02u8; 32];

    let key_b = [0x02u8; 32];
    let value_b = b"box_b";
    let next_b = [0xFFu8; 32];

    // Compute leaf labels first (needed for internal node's child labels)
    let label_a = ergo_sync::snapshot::parser::compute_leaf_label(&key_a, value_a, &next_a);
    let label_b = ergo_sync::snapshot::parser::compute_leaf_label(&key_b, value_b, &next_b);

    // Build DFS byte stream
    let mut stream = Vec::new();

    // Internal node: prefix + balance + key + left_label + right_label
    stream.push(0x00);
    stream.push(0x00); // balance = 0
    stream.extend_from_slice(&key_a); // key (same as left child for BST)
    stream.extend_from_slice(&label_a);
    stream.extend_from_slice(&label_b);

    // Leaf A
    stream.push(0x01);
    stream.extend_from_slice(&key_a);
    stream.extend_from_slice(&(value_a.len() as u32).to_be_bytes());
    stream.extend_from_slice(value_a);
    stream.extend_from_slice(&next_a);

    // Leaf B
    stream.push(0x01);
    stream.extend_from_slice(&key_b);
    stream.extend_from_slice(&(value_b.len() as u32).to_be_bytes());
    stream.extend_from_slice(value_b);
    stream.extend_from_slice(&next_b);

    let nodes = parse_dfs_stream(&stream, 32).unwrap();
    assert_eq!(nodes.len(), 3);

    // First node is internal
    assert!(matches!(nodes[0], ergo_sync::snapshot::parser::ParsedNode::Internal { .. }));
    // Second and third are leaves
    assert!(matches!(nodes[1], ergo_sync::snapshot::parser::ParsedNode::Leaf { .. }));
    assert!(matches!(nodes[2], ergo_sync::snapshot::parser::ParsedNode::Leaf { .. }));

    // Internal node's label should be computable from children
    let expected_root = ergo_sync::snapshot::parser::compute_internal_label(
        0, &label_a, &label_b,
    );
    assert_eq!(*nodes[0].label(), expected_root);
}
```

- [ ] **Step 7: Run and verify, then commit**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_parser_test -- --nocapture`

Expected: all tests PASS.

```bash
git add sync/src/snapshot/parser.rs sync/tests/snapshot_parser_test.rs sync/Cargo.toml
git commit -m "feat(sync): DFS byte stream parser for AVL+ snapshot nodes

Parses internal and leaf nodes from packed DFS byte streams.
Computes labels during parsing. Used for both manifest and chunk
reconstruction."
```

---

## Task 3: Manifest Subtree ID Extraction

Shallow parse of manifest bytes to extract the subtree IDs (chunk IDs to download). This does NOT compute labels for all nodes — it only walks the DFS structure to find boundary internal nodes at `manifest_depth` and extracts their `left_label` and `right_label` fields.

**Files:**
- Create: `sync/src/snapshot/manifest.rs`
- Create: `sync/tests/snapshot_manifest_test.rs`

- [ ] **Step 1: Write failing test**

`sync/tests/snapshot_manifest_test.rs`:
```rust
use ergo_sync::snapshot::manifest::{parse_manifest_header, extract_subtree_ids};

#[test]
fn extract_subtree_ids_depth_1() {
    // Manifest with depth=1: root is an internal node at depth 0.
    // At depth 1 (== manifest_depth), we DON'T recurse — children are subtree roots.
    // But depth 1 means the root IS the boundary: its children are the subtree IDs.
    //
    // Wait — the JVM traversal rules:
    //   depth 1 to manifest_depth-1: serialize + recurse
    //   depth == manifest_depth: serialize + DON'T recurse
    //
    // With manifest_depth=1, the root is at depth 0 (not boundary).
    // Actually the root starts at depth 0. For depth=1:
    //   depth 0: serialize root (internal), recurse into children
    //   depth 1: serialize children, DON'T recurse
    //
    // So we need a tree of depth >= 2 for depth=1 to have boundary nodes.
    // Let's use depth=1 meaning: root (depth 0) recurses, children at depth 1 are boundary.

    // Actually, re-reading the contract: at depth == manifest_depth, serialize but
    // DON'T recurse. The children's labels are the subtree IDs.
    //
    // For manifest_depth=1:
    //   Root at depth 0: internal node, recurse
    //   Children at depth 1: if internal, DON'T recurse → extract left/right labels
    //   Children at depth 1: if leaf, no subtrees
    //
    // Simplest test: manifest_depth=1, root has two leaf children.
    // No subtree IDs (leaves have no children to become subtrees).

    // Better test: manifest_depth=1, root at depth 0 has two internal children at depth 1.
    // Each child's left_label and right_label are subtree IDs. Total: 4 subtree IDs.

    let left_label_a = [0x11u8; 32];
    let right_label_a = [0x22u8; 32];
    let left_label_b = [0x33u8; 32];
    let right_label_b = [0x44u8; 32];

    // Child labels (computed from their children)
    let child_a_label = ergo_sync::snapshot::parser::compute_internal_label(
        0, &left_label_a, &right_label_a,
    );
    let child_b_label = ergo_sync::snapshot::parser::compute_internal_label(
        0, &left_label_b, &right_label_b,
    );

    let mut manifest = Vec::new();
    // Header
    manifest.push(5);  // root_height (arbitrary)
    manifest.push(1);  // manifest_depth

    // Root (depth 0): internal node
    manifest.push(0x00);
    manifest.push(0x00); // balance
    manifest.extend_from_slice(&[0x01u8; 32]); // key
    manifest.extend_from_slice(&child_a_label);
    manifest.extend_from_slice(&child_b_label);

    // Left child (depth 1, boundary): internal node — NOT recursed
    manifest.push(0x00);
    manifest.push(0x00);
    manifest.extend_from_slice(&[0x01u8; 32]);
    manifest.extend_from_slice(&left_label_a);
    manifest.extend_from_slice(&right_label_a);

    // Right child (depth 1, boundary): internal node — NOT recursed
    manifest.push(0x00);
    manifest.push(0x00);
    manifest.extend_from_slice(&[0x02u8; 32]);
    manifest.extend_from_slice(&left_label_b);
    manifest.extend_from_slice(&right_label_b);

    let (root_height, manifest_depth) = parse_manifest_header(&manifest).unwrap();
    assert_eq!(root_height, 5);
    assert_eq!(manifest_depth, 1);

    let subtree_ids = extract_subtree_ids(&manifest, 32).unwrap();
    assert_eq!(subtree_ids.len(), 4);
    assert!(subtree_ids.contains(&left_label_a));
    assert!(subtree_ids.contains(&right_label_a));
    assert!(subtree_ids.contains(&left_label_b));
    assert!(subtree_ids.contains(&right_label_b));
}

#[test]
fn extract_subtree_ids_leaf_at_boundary() {
    // If a leaf appears before manifest_depth, it terminates — no subtree IDs from it.
    let mut manifest = Vec::new();
    manifest.push(3);  // root_height
    manifest.push(2);  // manifest_depth

    // Root (depth 0): internal
    let leaf_label = ergo_sync::snapshot::parser::compute_leaf_label(
        &[0x01u8; 32], b"val", &[0x02u8; 32],
    );
    let child_label = ergo_sync::snapshot::parser::compute_internal_label(
        0, &[0xAAu8; 32], &[0xBBu8; 32],
    );
    manifest.push(0x00); // internal prefix
    manifest.push(0x00); // balance
    manifest.extend_from_slice(&[0x01u8; 32]); // key
    manifest.extend_from_slice(&leaf_label);    // left = leaf
    manifest.extend_from_slice(&child_label);   // right = internal

    // Left child (depth 1): leaf — terminates, no subtrees
    manifest.push(0x01);
    manifest.extend_from_slice(&[0x01u8; 32]); // key
    manifest.extend_from_slice(&3u32.to_be_bytes()); // value_len
    manifest.extend_from_slice(b"val");
    manifest.extend_from_slice(&[0x02u8; 32]); // next_key

    // Right child (depth 1): internal — recurse to depth 2
    manifest.push(0x00);
    manifest.push(0x00);
    manifest.extend_from_slice(&[0x02u8; 32]);
    manifest.extend_from_slice(&[0xAAu8; 32]); // left_label = subtree ID
    manifest.extend_from_slice(&[0xBBu8; 32]); // right_label = subtree ID

    // Right-left grandchild (depth 2, boundary): internal — NOT recursed
    manifest.push(0x00);
    manifest.push(0x00);
    manifest.extend_from_slice(&[0x02u8; 32]);
    manifest.extend_from_slice(&[0xCCu8; 32]);
    manifest.extend_from_slice(&[0xDDu8; 32]);

    // Right-right grandchild (depth 2, boundary): internal — NOT recursed
    manifest.push(0x00);
    manifest.push(0x00);
    manifest.extend_from_slice(&[0x03u8; 32]);
    manifest.extend_from_slice(&[0xEEu8; 32]);
    manifest.extend_from_slice(&[0xFFu8; 32]);

    let ids = extract_subtree_ids(&manifest, 32).unwrap();
    // Right child at depth 1 → recurse → its children at depth 2 are boundary
    // Right-left child: left_label=0xCC, right_label=0xDD
    // Right-right child: left_label=0xEE, right_label=0xFF
    assert_eq!(ids.len(), 4);
    assert!(ids.contains(&[0xCCu8; 32]));
    assert!(ids.contains(&[0xDDu8; 32]));
    assert!(ids.contains(&[0xEEu8; 32]));
    assert!(ids.contains(&[0xFFu8; 32]));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_manifest_test -- --nocapture 2>&1 | head -20`

Expected: compile error.

- [ ] **Step 3: Implement manifest parser**

`sync/src/snapshot/manifest.rs`:
```rust
use crate::snapshot::parser::{parse_node, ParsedNode, ParseError, PACKED_INTERNAL_PREFIX};

/// Parse the 2-byte manifest header. Returns (root_height, manifest_depth).
pub fn parse_manifest_header(data: &[u8]) -> Result<(u8, u8), ParseError> {
    if data.len() < 2 {
        return Err(ParseError::UnexpectedEof);
    }
    Ok((data[0], data[1]))
}

/// Extract subtree IDs from manifest bytes.
///
/// Walks the DFS byte stream, tracking depth. At `depth == manifest_depth`,
/// internal nodes' `left_label` and `right_label` are subtree IDs (chunks to
/// download). Leaves at any depth terminate traversal — no subtree IDs.
pub fn extract_subtree_ids(
    manifest_bytes: &[u8],
    key_length: usize,
) -> Result<Vec<[u8; 32]>, ParseError> {
    if manifest_bytes.len() < 2 {
        return Err(ParseError::UnexpectedEof);
    }

    let manifest_depth = manifest_bytes[1] as usize;
    let mut subtree_ids = Vec::new();
    let mut offset = 2; // skip header

    fn walk(
        data: &[u8],
        offset: &mut usize,
        depth: usize,
        manifest_depth: usize,
        key_length: usize,
        subtree_ids: &mut Vec<[u8; 32]>,
    ) -> Result<(), ParseError> {
        if *offset >= data.len() {
            return Err(ParseError::UnexpectedEof);
        }

        let (node, consumed) = parse_node(&data[*offset..], key_length)?;
        *offset += consumed;

        match node {
            ParsedNode::Internal { left_label, right_label, .. } => {
                if depth == manifest_depth {
                    // Boundary: children are subtree roots, don't recurse
                    subtree_ids.push(left_label);
                    subtree_ids.push(right_label);
                } else {
                    // Above boundary: recurse into children
                    walk(data, offset, depth + 1, manifest_depth, key_length, subtree_ids)?;
                    walk(data, offset, depth + 1, manifest_depth, key_length, subtree_ids)?;
                }
            }
            ParsedNode::Leaf { .. } => {
                // Leaf terminates regardless of depth — no subtrees
            }
        }

        Ok(())
    }

    walk(manifest_bytes, &mut offset, 0, manifest_depth, key_length, &mut subtree_ids)?;
    Ok(subtree_ids)
}
```

- [ ] **Step 4: Run tests to verify they pass, then commit**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_manifest_test -- --nocapture`

Expected: PASS.

```bash
git add sync/src/snapshot/manifest.rs sync/tests/snapshot_manifest_test.rs
git commit -m "feat(sync): manifest parser extracts subtree IDs for chunk download

Walks DFS byte stream tracking depth. At manifest_depth boundary,
extracts left/right labels from internal nodes as chunk download targets."
```

---

## Task 4: Snapshot P2P Message Codec

Parse and build the 6 snapshot-related P2P messages from/to `ProtocolMessage::Unknown { code, body }` bytes.

**Files:**
- Create: `sync/src/snapshot/protocol.rs`
- Create: `sync/tests/snapshot_protocol_test.rs`

- [ ] **Step 1: Write failing test for SnapshotsInfo parsing**

`sync/tests/snapshot_protocol_test.rs`:
```rust
use ergo_sync::snapshot::protocol::*;

#[test]
fn parse_snapshots_info() {
    let mut body = Vec::new();
    // count = 2
    body.extend_from_slice(&2u32.to_be_bytes());
    // entry 1: height=52223, manifest_id
    body.extend_from_slice(&52223i32.to_be_bytes());
    body.extend_from_slice(&[0xAAu8; 32]);
    // entry 2: height=104447, manifest_id
    body.extend_from_slice(&104447i32.to_be_bytes());
    body.extend_from_slice(&[0xBBu8; 32]);

    let info = SnapshotMessage::parse(MSG_SNAPSHOTS_INFO, &body).unwrap();
    match info {
        SnapshotMessage::SnapshotsInfo(entries) => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].height, 52223);
            assert_eq!(entries[0].manifest_id, [0xAAu8; 32]);
            assert_eq!(entries[1].height, 104447);
            assert_eq!(entries[1].manifest_id, [0xBBu8; 32]);
        }
        _ => panic!("expected SnapshotsInfo"),
    }
}

#[test]
fn build_get_snapshots_info() {
    let (code, body) = SnapshotMessage::GetSnapshotsInfo.encode();
    assert_eq!(code, MSG_GET_SNAPSHOTS_INFO);
    assert!(body.is_empty());
}

#[test]
fn build_get_manifest() {
    let manifest_id = [0xCCu8; 32];
    let (code, body) = SnapshotMessage::GetManifest(manifest_id).encode();
    assert_eq!(code, MSG_GET_MANIFEST);
    assert_eq!(body, manifest_id);
}

#[test]
fn build_get_chunk() {
    let subtree_id = [0xDDu8; 32];
    let (code, body) = SnapshotMessage::GetUtxoSnapshotChunk(subtree_id).encode();
    assert_eq!(code, MSG_GET_UTXO_SNAPSHOT_CHUNK);
    assert_eq!(body, subtree_id);
}

#[test]
fn parse_manifest_response() {
    let manifest_bytes = vec![5u8, 14, 0x00, 0x00]; // minimal: header + truncated node
    let mut body = Vec::new();
    body.extend_from_slice(&(manifest_bytes.len() as u32).to_be_bytes());
    body.extend_from_slice(&manifest_bytes);

    let msg = SnapshotMessage::parse(MSG_MANIFEST, &body).unwrap();
    match msg {
        SnapshotMessage::Manifest(data) => {
            assert_eq!(data, manifest_bytes);
        }
        _ => panic!("expected Manifest"),
    }
}

#[test]
fn parse_chunk_response() {
    let chunk_bytes = vec![0x01, 0x02, 0x03];
    let mut body = Vec::new();
    body.extend_from_slice(&(chunk_bytes.len() as u32).to_be_bytes());
    body.extend_from_slice(&chunk_bytes);

    let msg = SnapshotMessage::parse(MSG_UTXO_SNAPSHOT_CHUNK, &body).unwrap();
    match msg {
        SnapshotMessage::UtxoSnapshotChunk(data) => {
            assert_eq!(data, chunk_bytes);
        }
        _ => panic!("expected UtxoSnapshotChunk"),
    }
}
```

- [ ] **Step 2: Implement protocol codec**

`sync/src/snapshot/protocol.rs`:
```rust
pub const MSG_GET_SNAPSHOTS_INFO: u8 = 76;
pub const MSG_SNAPSHOTS_INFO: u8 = 77;
pub const MSG_GET_MANIFEST: u8 = 78;
pub const MSG_MANIFEST: u8 = 79;
pub const MSG_GET_UTXO_SNAPSHOT_CHUNK: u8 = 80;
pub const MSG_UTXO_SNAPSHOT_CHUNK: u8 = 81;

/// Max payload sizes (from JVM BasicMessagesRepo).
pub const MAX_SNAPSHOTS_INFO_SIZE: usize = 20_000;
pub const MAX_MANIFEST_SIZE: usize = 4_000_000;
pub const MAX_CHUNK_SIZE: usize = 4_000_000;

#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    pub height: u32,
    pub manifest_id: [u8; 32],
}

#[derive(Debug, Clone)]
pub enum SnapshotMessage {
    GetSnapshotsInfo,
    SnapshotsInfo(Vec<SnapshotEntry>),
    GetManifest([u8; 32]),
    Manifest(Vec<u8>),
    GetUtxoSnapshotChunk([u8; 32]),
    UtxoSnapshotChunk(Vec<u8>),
}

impl SnapshotMessage {
    /// Parse a snapshot message from a P2P Unknown message's code and body.
    pub fn parse(code: u8, body: &[u8]) -> Result<Self, ProtocolError> {
        match code {
            MSG_GET_SNAPSHOTS_INFO => Ok(SnapshotMessage::GetSnapshotsInfo),
            MSG_SNAPSHOTS_INFO => {
                if body.len() < 4 {
                    return Err(ProtocolError::TooShort);
                }
                if body.len() > MAX_SNAPSHOTS_INFO_SIZE {
                    return Err(ProtocolError::TooLarge(body.len()));
                }
                let count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let expected = 4 + count * 36; // 4 (height) + 32 (manifest_id)
                if body.len() < expected {
                    return Err(ProtocolError::TooShort);
                }
                let mut entries = Vec::with_capacity(count);
                let mut offset = 4;
                for _ in 0..count {
                    let height = i32::from_be_bytes([
                        body[offset], body[offset + 1], body[offset + 2], body[offset + 3],
                    ]) as u32;
                    let mut manifest_id = [0u8; 32];
                    manifest_id.copy_from_slice(&body[offset + 4..offset + 36]);
                    entries.push(SnapshotEntry { height, manifest_id });
                    offset += 36;
                }
                Ok(SnapshotMessage::SnapshotsInfo(entries))
            }
            MSG_GET_MANIFEST => {
                if body.len() < 32 {
                    return Err(ProtocolError::TooShort);
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(&body[..32]);
                Ok(SnapshotMessage::GetManifest(id))
            }
            MSG_MANIFEST => {
                if body.len() < 4 {
                    return Err(ProtocolError::TooShort);
                }
                let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                if len > MAX_MANIFEST_SIZE {
                    return Err(ProtocolError::TooLarge(len));
                }
                if body.len() < 4 + len {
                    return Err(ProtocolError::TooShort);
                }
                Ok(SnapshotMessage::Manifest(body[4..4 + len].to_vec()))
            }
            MSG_GET_UTXO_SNAPSHOT_CHUNK => {
                if body.len() < 32 {
                    return Err(ProtocolError::TooShort);
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(&body[..32]);
                Ok(SnapshotMessage::GetUtxoSnapshotChunk(id))
            }
            MSG_UTXO_SNAPSHOT_CHUNK => {
                if body.len() < 4 {
                    return Err(ProtocolError::TooShort);
                }
                let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                if len > MAX_CHUNK_SIZE {
                    return Err(ProtocolError::TooLarge(len));
                }
                if body.len() < 4 + len {
                    return Err(ProtocolError::TooShort);
                }
                Ok(SnapshotMessage::UtxoSnapshotChunk(body[4..4 + len].to_vec()))
            }
            _ => Err(ProtocolError::UnknownCode(code)),
        }
    }

    /// Encode a snapshot message to (code, body) for sending via P2P.
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            SnapshotMessage::GetSnapshotsInfo => (MSG_GET_SNAPSHOTS_INFO, Vec::new()),
            SnapshotMessage::GetManifest(id) => (MSG_GET_MANIFEST, id.to_vec()),
            SnapshotMessage::GetUtxoSnapshotChunk(id) => (MSG_GET_UTXO_SNAPSHOT_CHUNK, id.to_vec()),
            // Response messages are received, not sent — encode is for requests.
            // Include for completeness (serving will need these later).
            SnapshotMessage::SnapshotsInfo(entries) => {
                let mut body = Vec::with_capacity(4 + entries.len() * 36);
                body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
                for entry in entries {
                    body.extend_from_slice(&(entry.height as i32).to_be_bytes());
                    body.extend_from_slice(&entry.manifest_id);
                }
                (MSG_SNAPSHOTS_INFO, body)
            }
            SnapshotMessage::Manifest(data) => {
                let mut body = Vec::with_capacity(4 + data.len());
                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                body.extend_from_slice(data);
                (MSG_MANIFEST, body)
            }
            SnapshotMessage::UtxoSnapshotChunk(data) => {
                let mut body = Vec::with_capacity(4 + data.len());
                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                body.extend_from_slice(data);
                (MSG_UTXO_SNAPSHOT_CHUNK, body)
            }
        }
    }

    /// Check if a P2P Unknown message code is a snapshot message.
    pub fn is_snapshot_code(code: u8) -> bool {
        (MSG_GET_SNAPSHOTS_INFO..=MSG_UTXO_SNAPSHOT_CHUNK).contains(&code)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message body too short")]
    TooShort,
    #[error("message body too large: {0} bytes")]
    TooLarge(usize),
    #[error("unknown snapshot message code: {0}")]
    UnknownCode(u8),
}
```

- [ ] **Step 3: Run tests to verify, then commit**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_protocol_test -- --nocapture`

Expected: all 6 tests PASS.

```bash
git add sync/src/snapshot/protocol.rs sync/tests/snapshot_protocol_test.rs
git commit -m "feat(sync): P2P message codec for snapshot protocol (codes 76-81)

Parse and encode GetSnapshotsInfo, SnapshotsInfo, GetManifest, Manifest,
GetUtxoSnapshotChunk, UtxoSnapshotChunk. Size limits match JVM."
```

---

## Task 5: Chunk Download Storage

Temporary redb file for persisting downloaded chunks across crashes.

**Files:**
- Create: `sync/src/snapshot/download.rs`
- Create: `sync/tests/snapshot_download_test.rs`

- [ ] **Step 1: Write failing test for basic storage operations**

`sync/tests/snapshot_download_test.rs`:
```rust
use ergo_sync::snapshot::download::ChunkDownloadStore;
use tempfile::tempdir;

#[test]
fn store_and_retrieve_chunks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("snapshot_download.redb");

    let manifest_id = [0xAAu8; 32];
    let manifest_bytes = vec![5u8, 14, 0x00, 0x01]; // fake manifest
    let snapshot_height = 52223u32;
    let total_chunks = 100u32;

    {
        let mut store = ChunkDownloadStore::create(
            &path, manifest_id, snapshot_height, &manifest_bytes, total_chunks,
        ).unwrap();

        let chunk_id = [0x11u8; 32];
        let chunk_data = vec![0x01, 0x02, 0x03, 0x04];
        store.store_chunk(&chunk_id, &chunk_data).unwrap();

        assert_eq!(store.chunk_count(), 1);
        assert_eq!(store.total_chunks(), 100);
        assert!(!store.is_complete());
    }

    // Reopen — simulates crash recovery
    {
        let store = ChunkDownloadStore::open(&path).unwrap();
        assert_eq!(store.manifest_id(), manifest_id);
        assert_eq!(store.snapshot_height(), 52223);
        assert_eq!(store.chunk_count(), 1);
        assert_eq!(store.total_chunks(), 100);

        let chunk_id = [0x11u8; 32];
        let data = store.get_chunk(&chunk_id).unwrap();
        assert_eq!(data, Some(vec![0x01, 0x02, 0x03, 0x04]));
    }
}

#[test]
fn download_complete_detection() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("snapshot_download.redb");

    let mut store = ChunkDownloadStore::create(
        &path, [0u8; 32], 100, &[], 2,
    ).unwrap();

    store.store_chunk(&[0x01u8; 32], &[1]).unwrap();
    assert!(!store.is_complete());

    store.store_chunk(&[0x02u8; 32], &[2]).unwrap();
    assert!(store.is_complete());
}

#[test]
fn iterate_all_chunks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("snapshot_download.redb");

    let mut store = ChunkDownloadStore::create(
        &path, [0u8; 32], 100, &[], 2,
    ).unwrap();

    store.store_chunk(&[0x01u8; 32], &[0xAA]).unwrap();
    store.store_chunk(&[0x02u8; 32], &[0xBB]).unwrap();

    let chunks: Vec<_> = store.iter_chunks().unwrap().collect();
    assert_eq!(chunks.len(), 2);
}

#[test]
fn stored_chunk_ids() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("snapshot_download.redb");

    let mut store = ChunkDownloadStore::create(
        &path, [0u8; 32], 100, &[], 3,
    ).unwrap();

    store.store_chunk(&[0x01u8; 32], &[1]).unwrap();
    store.store_chunk(&[0x02u8; 32], &[2]).unwrap();

    let ids = store.stored_chunk_ids().unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&[0x01u8; 32]));
    assert!(ids.contains(&[0x02u8; 32]));
}
```

- [ ] **Step 2: Implement download storage**

`sync/src/snapshot/download.rs`:
```rust
use redb::{Database, TableDefinition, ReadableTable};
use std::path::Path;

const CHUNKS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

const META_MANIFEST_ID: &str = "manifest_id";
const META_SNAPSHOT_HEIGHT: &str = "snapshot_height";
const META_MANIFEST_BYTES: &str = "manifest_bytes";
const META_TOTAL_CHUNKS: &str = "total_chunks";

pub struct ChunkDownloadStore {
    db: Database,
    manifest_id: [u8; 32],
    snapshot_height: u32,
    total_chunks: u32,
}

impl ChunkDownloadStore {
    /// Create a new download store. Overwrites any existing file.
    pub fn create(
        path: &Path,
        manifest_id: [u8; 32],
        snapshot_height: u32,
        manifest_bytes: &[u8],
        total_chunks: u32,
    ) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(CHUNKS_TABLE)?;
            let mut meta = write_txn.open_table(META_TABLE)?;
            meta.insert(META_MANIFEST_ID, manifest_id.as_slice())?;
            meta.insert(META_SNAPSHOT_HEIGHT, snapshot_height.to_be_bytes().as_slice())?;
            meta.insert(META_MANIFEST_BYTES, manifest_bytes)?;
            meta.insert(META_TOTAL_CHUNKS, total_chunks.to_be_bytes().as_slice())?;
        }
        write_txn.commit()?;
        Ok(Self { db, manifest_id, snapshot_height, total_chunks })
    }

    /// Open an existing download store (crash recovery).
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::open(path)?;
        let read_txn = db.begin_read()?;
        let meta = read_txn.open_table(META_TABLE)?;

        let mut manifest_id = [0u8; 32];
        manifest_id.copy_from_slice(meta.get(META_MANIFEST_ID)?.unwrap().value());

        let snapshot_height = {
            let val = meta.get(META_SNAPSHOT_HEIGHT)?.unwrap();
            let bytes: [u8; 4] = val.value().try_into().unwrap();
            u32::from_be_bytes(bytes)
        };

        let total_chunks = {
            let val = meta.get(META_TOTAL_CHUNKS)?.unwrap();
            let bytes: [u8; 4] = val.value().try_into().unwrap();
            u32::from_be_bytes(bytes)
        };

        Ok(Self { db, manifest_id, snapshot_height, total_chunks })
    }

    /// Store a downloaded chunk.
    pub fn store_chunk(&mut self, id: &[u8; 32], data: &[u8]) -> Result<(), redb::Error> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(CHUNKS_TABLE)?;
            table.insert(id.as_slice(), data)?;
        }
        write_txn.commit()
    }

    /// Retrieve a chunk's data.
    pub fn get_chunk(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CHUNKS_TABLE)?;
        Ok(table.get(id.as_slice())?.map(|v| v.value().to_vec()))
    }

    /// Number of chunks stored so far.
    pub fn chunk_count(&self) -> u32 {
        let read_txn = self.db.begin_read().unwrap();
        let table = read_txn.open_table(CHUNKS_TABLE).unwrap();
        table.len().unwrap() as u32
    }

    /// Total expected chunks.
    pub fn total_chunks(&self) -> u32 {
        self.total_chunks
    }

    pub fn manifest_id(&self) -> [u8; 32] {
        self.manifest_id
    }

    pub fn snapshot_height(&self) -> u32 {
        self.snapshot_height
    }

    /// Get the raw manifest bytes (for re-extracting subtree IDs on recovery).
    pub fn manifest_bytes(&self) -> Result<Vec<u8>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let meta = read_txn.open_table(META_TABLE)?;
        Ok(meta.get(META_MANIFEST_BYTES)?.unwrap().value().to_vec())
    }

    /// All download complete?
    pub fn is_complete(&self) -> bool {
        self.chunk_count() >= self.total_chunks
    }

    /// Get IDs of all stored chunks (for computing remaining downloads).
    pub fn stored_chunk_ids(&self) -> Result<Vec<[u8; 32]>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CHUNKS_TABLE)?;
        let mut ids = Vec::new();
        for entry in table.iter()? {
            let (key, _) = entry?;
            let mut id = [0u8; 32];
            id.copy_from_slice(key.value());
            ids.push(id);
        }
        Ok(ids)
    }

    /// Iterate all chunks: (subtree_id, chunk_bytes).
    pub fn iter_chunks(&self) -> Result<impl Iterator<Item = ([u8; 32], Vec<u8>)> + '_, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CHUNKS_TABLE)?;
        let mut chunks = Vec::new();
        for entry in table.iter()? {
            let (key, val) = entry.unwrap();
            let mut id = [0u8; 32];
            id.copy_from_slice(key.value());
            chunks.push((id, val.value().to_vec()));
        }
        Ok(chunks.into_iter())
    }

    /// Delete the download store file.
    pub fn cleanup(path: &Path) -> std::io::Result<()> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}
```

- [ ] **Step 3: Run tests to verify, then commit**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_download_test -- --nocapture`

Expected: all 4 tests PASS.

```bash
git add sync/src/snapshot/download.rs sync/tests/snapshot_download_test.rs
git commit -m "feat(sync): temporary redb storage for snapshot chunk downloads

Persists chunks across crashes. Stores manifest metadata for recovery.
Tracks completion (chunk_count >= total_chunks). Cleaned up after
successful state loading."
```

---

## Task 6: SyncChain Trait Extension

The snapshot sync needs to look up a header's `state_root` to verify manifest IDs. Add this to `SyncChain`.

**Files:**
- Modify: `sync/src/traits.rs`
- Modify: `src/bridge.rs`

- [ ] **Step 1: Add `header_state_root` to SyncChain trait**

In `sync/src/traits.rs`, add after the existing `header_at` method:
```rust
    /// Return the state_root (33 bytes: root_hash[32] + tree_height[1]) at a given height.
    /// Returns None if the header doesn't exist.
    fn header_state_root(&self, height: u32) -> impl std::future::Future<Output = Option<[u8; 33]>> + Send;
```

- [ ] **Step 2: Implement on SharedChain in bridge.rs**

In `src/bridge.rs`, add the implementation in the `SyncChain for SharedChain` impl block:
```rust
    async fn header_state_root(&self, height: u32) -> Option<[u8; 33]> {
        let chain = self.chain.lock().await;
        let header = chain.header_at(height)?;
        Some(header.state_root)
    }
```

Verify that `Header.state_root` is `[u8; 33]`. If it's a different type (e.g., `ADDigest` / `Bytes`), adjust the conversion accordingly. The header's `state_root` is the 33-byte AD digest: `[root_hash: 32B | tree_height: 1B]`.

- [ ] **Step 3: Add SyncConfig extensions**

In `sync/src/state.rs`, add fields to `SyncConfig`:
```rust
    /// Enable UTXO snapshot bootstrapping.
    pub utxo_bootstrap: bool,
    /// Minimum peers announcing the same manifest before downloading.
    pub min_snapshot_peers: u32,
```

Update the `Default` impl:
```rust
    utxo_bootstrap: false,
    min_snapshot_peers: 2,
```

- [ ] **Step 4: Verify compilation**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo check 2>&1 | head -30`

Fix any compilation errors from the trait extension (all implementors must be updated).

- [ ] **Step 5: Commit**

```bash
git add sync/src/traits.rs sync/src/state.rs src/bridge.rs
git commit -m "feat(sync): extend SyncChain with header_state_root, add snapshot config

SyncChain gains header_state_root() for manifest verification.
SyncConfig gains utxo_bootstrap and min_snapshot_peers fields."
```

---

## Task 7: Snapshot Sync Orchestrator

The main state machine: discovery, manifest download, chunk download, and assembly into `SnapshotData`. This is an async function called from the sync machine's run loop.

**Files:**
- Modify: `sync/src/snapshot/mod.rs`

- [ ] **Step 1: Define the orchestrator function signature and error type**

Add to `sync/src/snapshot/mod.rs`:
```rust
use crate::traits::{SyncTransport, SyncChain};
use crate::snapshot::protocol::*;
use crate::snapshot::manifest::extract_subtree_ids;
use crate::snapshot::download::ChunkDownloadStore;
use crate::snapshot::parser::parse_dfs_stream;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("no peers support snapshots")]
    NoPeers,
    #[error("no valid snapshot found with sufficient peers")]
    NoQuorum,
    #[error("manifest verification failed: expected {expected}, got {actual}")]
    ManifestMismatch { expected: String, actual: String },
    #[error("manifest download failed from all peers")]
    ManifestDownloadFailed,
    #[error("chunk download timed out")]
    ChunkTimeout,
    #[error("parse error: {0}")]
    Parse(#[from] crate::snapshot::parser::ParseError),
    #[error("protocol error: {0}")]
    Protocol(#[from] crate::snapshot::protocol::ProtocolError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::Error),
    #[error("download invalidated by reorg")]
    Invalidated,
}

/// Split a 33-byte state_root into (root_hash[32], tree_height[1]).
pub fn split_state_root(state_root: &[u8; 33]) -> ([u8; 32], u8) {
    let mut root_hash = [0u8; 32];
    root_hash.copy_from_slice(&state_root[..32]);
    (root_hash, state_root[32])
}
```

- [ ] **Step 2: Implement discovery phase**

Add to `sync/src/snapshot/mod.rs`:
```rust
/// Discovered snapshot: manifest ID validated against header chain.
struct ValidatedSnapshot {
    height: u32,
    manifest_id: [u8; 32],
    state_root: [u8; 33],
    peers: Vec<enr_p2p::PeerId>,
}

/// Run snapshot discovery: broadcast GetSnapshotsInfo, collect validated responses.
/// Returns the best snapshot (highest height) with sufficient peer quorum.
async fn discover_snapshot<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
    min_peers: u32,
    timeout: Duration,
) -> Result<ValidatedSnapshot, SnapshotError> {
    use enr_p2p::protocol::messages::ProtocolMessage;

    // Send GetSnapshotsInfo to all outbound peers
    let peers = transport.outbound_peers().await;
    if peers.is_empty() {
        return Err(SnapshotError::NoPeers);
    }

    let (code, body) = SnapshotMessage::GetSnapshotsInfo.encode();
    for &peer in &peers {
        let msg = ProtocolMessage::Unknown { code, body: body.clone() };
        let _ = transport.send_to(peer, msg).await;
    }

    // Collect responses: manifest_id → (height, state_root, Vec<peer>)
    let mut manifests: HashMap<[u8; 32], (u32, [u8; 33], Vec<enr_p2p::PeerId>)> = HashMap::new();
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let remaining = deadline - Instant::now();
        let event = tokio::time::timeout(remaining, transport.next_event()).await;

        let event = match event {
            Ok(Some(e)) => e,
            Ok(None) | Err(_) => break,
        };

        if let enr_p2p::protocol::peer::ProtocolEvent::Message { peer_id, message } = event {
            if let ProtocolMessage::Unknown { code: MSG_SNAPSHOTS_INFO, body } = message {
                if let Ok(SnapshotMessage::SnapshotsInfo(entries)) = SnapshotMessage::parse(MSG_SNAPSHOTS_INFO, &body) {
                    for entry in entries {
                        // Validate against header chain
                        if let Some(state_root) = chain.header_state_root(entry.height).await {
                            let (root_hash, _) = split_state_root(&state_root);
                            if root_hash == entry.manifest_id {
                                let record = manifests
                                    .entry(entry.manifest_id)
                                    .or_insert((entry.height, state_root, Vec::new()));
                                if !record.2.contains(&peer_id) {
                                    record.2.push(peer_id);
                                }
                            }
                        }
                    }

                    // Check quorum: find highest-height manifest with enough peers
                    let best = manifests.values()
                        .filter(|(_, _, peers)| peers.len() as u32 >= min_peers)
                        .max_by_key(|(height, _, _)| *height);

                    if let Some((height, state_root, peers)) = best {
                        return Ok(ValidatedSnapshot {
                            height: *height,
                            manifest_id: {
                                let (root_hash, _) = split_state_root(state_root);
                                root_hash
                            },
                            state_root: *state_root,
                            peers: peers.clone(),
                        });
                    }
                }
            }
        }
    }

    Err(SnapshotError::NoQuorum)
}
```

- [ ] **Step 3: Implement manifest download + chunk download + assembly**

Add to `sync/src/snapshot/mod.rs`:
```rust
use rand::seq::SliceRandom;

/// Download manifest from a peer, verify against expected state root.
async fn download_manifest<T: SyncTransport>(
    transport: &mut T,
    snapshot: &ValidatedSnapshot,
    timeout: Duration,
) -> Result<Vec<u8>, SnapshotError> {
    let (code, body) = SnapshotMessage::GetManifest(snapshot.manifest_id).encode();
    let mut peers = snapshot.peers.clone();
    let mut rng = rand::thread_rng();
    peers.shuffle(&mut rng);

    for peer in &peers {
        let msg = enr_p2p::protocol::messages::ProtocolMessage::Unknown { code, body: body.clone() };
        let _ = transport.send_to(*peer, msg).await;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline - Instant::now();
            let event = tokio::time::timeout(remaining, transport.next_event()).await;

            let event = match event {
                Ok(Some(e)) => e,
                Ok(None) | Err(_) => break,
            };

            if let enr_p2p::protocol::peer::ProtocolEvent::Message { message, .. } = event {
                if let enr_p2p::protocol::messages::ProtocolMessage::Unknown { code: MSG_MANIFEST, body } = message {
                    if let Ok(SnapshotMessage::Manifest(manifest_bytes)) = SnapshotMessage::parse(MSG_MANIFEST, &body) {
                        // Verify manifest root matches expected
                        if manifest_bytes.len() < 2 {
                            continue;
                        }
                        // The manifest's root label must match the manifest_id
                        // Full verification happens when we parse all nodes
                        return Ok(manifest_bytes);
                    }
                }
            }
        }
    }

    Err(SnapshotError::ManifestDownloadFailed)
}

const CHUNKS_IN_PARALLEL: usize = 16;
const CHUNKS_PER_PEER: usize = 4;

/// Download all chunks, storing them in the temporary redb file.
/// Resumes from existing progress if the store already has chunks.
async fn download_chunks<T: SyncTransport>(
    transport: &mut T,
    store: &mut ChunkDownloadStore,
    all_subtree_ids: &[[u8; 32]],
    peers: &[enr_p2p::PeerId],
    chunk_timeout: Duration,
) -> Result<(), SnapshotError> {
    // Determine which chunks still need downloading
    let stored = store.stored_chunk_ids()?;
    let stored_set: HashSet<[u8; 32]> = stored.into_iter().collect();
    let mut pending: Vec<[u8; 32]> = all_subtree_ids
        .iter()
        .filter(|id| !stored_set.contains(*id))
        .copied()
        .collect();

    let mut in_flight: HashMap<[u8; 32], Instant> = HashMap::new();
    let mut rng = rand::thread_rng();

    loop {
        if pending.is_empty() && in_flight.is_empty() {
            return Ok(());
        }

        // Fill up in-flight requests
        while in_flight.len() < CHUNKS_IN_PARALLEL && !pending.is_empty() {
            let batch_size = CHUNKS_PER_PEER.min(pending.len());
            let batch: Vec<[u8; 32]> = pending.drain(..batch_size).collect();

            if let Some(peer) = peers.choose(&mut rng) {
                for id in &batch {
                    let (code, body) = SnapshotMessage::GetUtxoSnapshotChunk(*id).encode();
                    let msg = enr_p2p::protocol::messages::ProtocolMessage::Unknown { code, body };
                    let _ = transport.send_to(*peer, msg).await;
                    in_flight.insert(*id, Instant::now());
                }
            }
        }

        // Wait for responses or timeout
        let event = tokio::time::timeout(Duration::from_secs(1), transport.next_event()).await;

        match event {
            Ok(Some(enr_p2p::protocol::peer::ProtocolEvent::Message { message, .. })) => {
                if let enr_p2p::protocol::messages::ProtocolMessage::Unknown {
                    code: MSG_UTXO_SNAPSHOT_CHUNK, body,
                } = message {
                    if let Ok(SnapshotMessage::UtxoSnapshotChunk(data)) = SnapshotMessage::parse(MSG_UTXO_SNAPSHOT_CHUNK, &body) {
                        // Parse the chunk to find its root label (first node)
                        if let Ok((node, _)) = crate::snapshot::parser::parse_node(&data, 32) {
                            let chunk_id = *node.label();
                            if in_flight.remove(&chunk_id).is_some() {
                                store.store_chunk(&chunk_id, &data)?;
                                let count = store.chunk_count();
                                let total = store.total_chunks();
                                if count % 100 == 0 || count == total {
                                    tracing::info!("snapshot chunks: {count}/{total}");
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        // Check for timed-out requests
        let now = Instant::now();
        let timed_out: Vec<[u8; 32]> = in_flight
            .iter()
            .filter(|(_, sent_at)| now.duration_since(**sent_at) > chunk_timeout)
            .map(|(id, _)| *id)
            .collect();

        for id in timed_out {
            in_flight.remove(&id);
            pending.push(id); // Re-queue for retry
        }
    }
}

/// Assemble all downloaded data into SnapshotData.
/// Parses manifest + all chunks into (label, packed_bytes) pairs.
fn assemble_snapshot(
    manifest_bytes: &[u8],
    store: &ChunkDownloadStore,
    snapshot_height: u32,
) -> Result<SnapshotData, SnapshotError> {
    let (root_height, _) = crate::snapshot::manifest::parse_manifest_header(manifest_bytes)?;

    // Parse manifest nodes (includes the tree structure down to boundary depth)
    let manifest_nodes = parse_dfs_stream(&manifest_bytes[2..], 32)?;

    let mut all_nodes: Vec<([u8; 32], Vec<u8>)> = Vec::new();

    // Add manifest nodes
    for node in &manifest_nodes {
        all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
    }

    // Parse and add all chunk nodes
    for (_, chunk_data) in store.iter_chunks()? {
        let chunk_nodes = parse_dfs_stream(&chunk_data, 32)?;
        for node in &chunk_nodes {
            all_nodes.push((*node.label(), node.packed_bytes().to_vec()));
        }
    }

    // Root hash is the first manifest node's label
    let root_hash = if let Some(first) = manifest_nodes.first() {
        *first.label()
    } else {
        return Err(SnapshotError::Parse(crate::snapshot::parser::ParseError::UnexpectedEof));
    };

    Ok(SnapshotData {
        nodes: all_nodes,
        root_hash,
        tree_height: root_height,
        snapshot_height,
    })
}
```

- [ ] **Step 4: Implement the top-level orchestrator**

Add to `sync/src/snapshot/mod.rs`:
```rust
/// Run the complete snapshot sync: discover → download manifest → download chunks → assemble.
///
/// Returns SnapshotData ready for loading into state storage.
/// Uses a temporary redb file for crash-safe chunk storage.
pub async fn run_snapshot_sync<T: SyncTransport, C: SyncChain>(
    transport: &mut T,
    chain: &C,
    config: &SnapshotConfig,
) -> Result<SnapshotData, SnapshotError> {
    let download_path = config.data_dir.join("snapshot_download.redb");

    // Check for interrupted download (crash recovery)
    if download_path.exists() {
        tracing::info!("found interrupted snapshot download, attempting resume");
        let store = ChunkDownloadStore::open(&download_path)?;

        // Verify the download is still valid (header chain hasn't reorged)
        let manifest_id = store.manifest_id();
        let height = store.snapshot_height();
        if let Some(state_root) = chain.header_state_root(height).await {
            let (root_hash, _) = split_state_root(&state_root);
            if root_hash == manifest_id {
                // Valid — resume download
                let manifest_bytes = store.manifest_bytes()?;
                let subtree_ids = extract_subtree_ids(&manifest_bytes, 32)?;

                if store.is_complete() {
                    tracing::info!("all chunks already downloaded, assembling snapshot");
                    let data = assemble_snapshot(&manifest_bytes, &store, height)?;
                    ChunkDownloadStore::cleanup(&download_path).ok();
                    return Ok(data);
                }

                // Resume chunk download
                let peers = transport.outbound_peers().await;
                let chunk_timeout = Duration::from_secs(
                    config.chunk_timeout_multiplier as u64 * 10,
                );
                let mut store = store;
                download_chunks(transport, &mut store, &subtree_ids, &peers, chunk_timeout).await?;

                let data = assemble_snapshot(&manifest_bytes, &store, height)?;
                ChunkDownloadStore::cleanup(&download_path).ok();
                return Ok(data);
            }
        }
        // Invalid — stale download, delete and start fresh
        tracing::warn!("stale snapshot download (reorg?), starting fresh");
        ChunkDownloadStore::cleanup(&download_path).ok();
    }

    // Fresh discovery
    tracing::info!("starting UTXO snapshot discovery");
    let discovery_timeout = Duration::from_secs(60);
    let snapshot = discover_snapshot(transport, chain, config.min_snapshot_peers, discovery_timeout).await?;
    tracing::info!(
        "snapshot found: height={}, manifest={}, peers={}",
        snapshot.height,
        hex::encode(&snapshot.manifest_id[..8]),
        snapshot.peers.len(),
    );

    // Download manifest
    let manifest_timeout = Duration::from_secs(30);
    let manifest_bytes = download_manifest(transport, &snapshot, manifest_timeout).await?;
    tracing::info!("manifest downloaded: {} bytes", manifest_bytes.len());

    // Extract subtree IDs
    let subtree_ids = extract_subtree_ids(&manifest_bytes, 32)?;
    tracing::info!("manifest contains {} subtree chunks", subtree_ids.len());

    // Create download store
    let mut store = ChunkDownloadStore::create(
        &download_path,
        snapshot.manifest_id,
        snapshot.height,
        &manifest_bytes,
        subtree_ids.len() as u32,
    )?;

    // Download chunks
    let chunk_timeout = Duration::from_secs(config.chunk_timeout_multiplier as u64 * 10);
    download_chunks(transport, &mut store, &subtree_ids, &snapshot.peers, chunk_timeout).await?;

    // Assemble
    tracing::info!("all chunks downloaded, assembling snapshot");
    let data = assemble_snapshot(&manifest_bytes, &store, snapshot.height)?;
    ChunkDownloadStore::cleanup(&download_path).ok();

    Ok(data)
}
```

- [ ] **Step 5: Add `hex` and `rand` dependencies to sync/Cargo.toml**

```toml
hex = "0.4"
rand = "0.8"
```

- [ ] **Step 6: Verify compilation**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo check -p ergo-sync 2>&1 | head -40`

Fix any import issues. The exact import paths for `enr_p2p::PeerId`, `enr_p2p::protocol::peer::ProtocolEvent`, and `enr_p2p::protocol::messages::ProtocolMessage` may differ — verify against the actual p2p crate exports and adjust.

- [ ] **Step 7: Commit**

```bash
git add sync/src/snapshot/mod.rs sync/Cargo.toml
git commit -m "feat(sync): snapshot sync orchestrator — discovery, download, assembly

Complete state machine: discover snapshots from peers, download manifest,
parallel chunk download with temporary redb storage, crash recovery,
assembly into SnapshotData for state loading."
```

---

## Task 8: Integrate Snapshot Sync into HeaderSync

Wire the snapshot orchestrator into the existing sync machine's run loop. Make the validator optional to support the bootstrap phase where no state exists yet.

**Files:**
- Modify: `sync/src/state.rs`

- [ ] **Step 1: Make validator optional**

In `sync/src/state.rs`, change the `HeaderSync` struct's validator field from `V` to `Option<V>`:

```rust
// In the struct definition:
    validator: Option<V>,
```

Update `HeaderSync::new()` to accept `Option<V>`:
```rust
    pub fn new(
        config: SyncConfig,
        transport: T,
        chain: C,
        store: S,
        validator: Option<V>,
        // ... rest unchanged
    ) -> Self {
```

- [ ] **Step 2: Guard all validator usages**

Find every `self.validator.` call in state.rs. Each one needs to handle the None case:

In `advance_validated_height()`: add an early return at the top:
```rust
    let validator = match self.validator.as_mut() {
        Some(v) => v,
        None => return, // No validator yet (snapshot bootstrap pending)
    };
```

In the `run()` initialization where `validated_height` is set from `validator.validated_height()`:
```rust
    let validated_height = self.validator.as_ref().map_or(0, |v| v.validated_height());
```

In any `reset_to()` calls:
```rust
    if let Some(v) = self.validator.as_mut() {
        v.reset_to(fork_point, fork_header.state_root);
    }
```

- [ ] **Step 3: Add snapshot channels**

Add fields to `HeaderSync` for the snapshot handoff:
```rust
    snapshot_tx: Option<tokio::sync::oneshot::Sender<crate::snapshot::SnapshotData>>,
    validator_rx: Option<tokio::sync::oneshot::Receiver<V>>,
```

Update `new()` to accept these:
```rust
    pub fn new(
        config: SyncConfig,
        transport: T,
        chain: C,
        store: S,
        validator: Option<V>,
        progress: tokio::sync::mpsc::Receiver<u32>,
        delivery_control: tokio::sync::mpsc::UnboundedReceiver<crate::delivery::DeliveryControl>,
        delivery_data: tokio::sync::mpsc::Receiver<crate::delivery::DeliveryData>,
        snapshot_tx: Option<tokio::sync::oneshot::Sender<crate::snapshot::SnapshotData>>,
        validator_rx: Option<tokio::sync::oneshot::Receiver<V>>,
    ) -> Self {
```

- [ ] **Step 4: Add snapshot sync phase to run()**

In the `run()` method, after the `SyncOutcome::Synced` match arm, before calling `self.synced()`:

```rust
    SyncOutcome::Synced => {
        // Check if we need snapshot bootstrap
        if self.config.utxo_bootstrap
            && self.validator.is_none()
            && self.snapshot_tx.is_some()
        {
            tracing::info!("headers synced, starting UTXO snapshot sync");
            let snapshot_config = crate::snapshot::SnapshotConfig {
                min_snapshot_peers: self.config.min_snapshot_peers,
                chunk_timeout_multiplier: 4,
                data_dir: self.config.data_dir.clone(),
            };

            match crate::snapshot::run_snapshot_sync(
                &mut self.transport,
                &self.chain,
                &snapshot_config,
            ).await {
                Ok(snapshot_data) => {
                    let height = snapshot_data.snapshot_height;
                    // Send snapshot to main for state loading
                    if let Some(tx) = self.snapshot_tx.take() {
                        let _ = tx.send(snapshot_data);
                    }
                    // Wait for validator back from main
                    if let Some(rx) = self.validator_rx.take() {
                        match rx.await {
                            Ok(validator) => {
                                self.validator = Some(validator);
                                self.downloaded_height = height;
                                self.validated_height = height;
                                tracing::info!("snapshot loaded, resuming block sync from height {}", height + 1);
                            }
                            Err(_) => {
                                tracing::error!("validator channel closed during snapshot bootstrap");
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("snapshot sync failed: {e}, retrying next cycle");
                    continue; // Will re-enter pick_sync_peer → sync_from_peer cycle
                }
            }
        }

        self.synced().await;
    }
```

- [ ] **Step 5: Add `data_dir` to SyncConfig**

In `sync/src/state.rs`, add to `SyncConfig`:
```rust
    /// Data directory for temporary snapshot storage.
    pub data_dir: std::path::PathBuf,
```

Update the Default impl (use a reasonable default or require it):
```rust
    data_dir: std::path::PathBuf::from("."),
```

- [ ] **Step 6: Verify compilation**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo check 2>&1 | head -40`

Callers of `HeaderSync::new()` will need updating (Task 9). For now, verify sync crate compiles.

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo check -p ergo-sync 2>&1 | head -40`

- [ ] **Step 7: Commit**

```bash
git add sync/src/state.rs
git commit -m "feat(sync): integrate snapshot sync into HeaderSync run loop

Validator is now Option<V> to support snapshot bootstrap phase.
After headers sync, if utxo_bootstrap is enabled and no validator exists,
runs snapshot download, sends data to main via oneshot channel, waits
for validator back, then continues to block sync."
```

---

## Task 9: Main Crate Integration

Wire everything together in main.rs: config additions, oneshot channels, snapshot handling, validator creation from loaded state.

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add config fields**

In the `NodeConfig` struct in `src/main.rs`:
```rust
    utxo_bootstrap: bool,        // default: false
    min_snapshot_peers: u32,      // default: 2
```

Update the TOML parsing section to read these (with defaults).

- [ ] **Step 2: Create snapshot channels when utxo_bootstrap is enabled**

After the validator creation section, before creating the sync machine:
```rust
    // Snapshot bootstrap channels
    let (snapshot_tx, snapshot_rx, validator_tx, validator_rx) = if node_config.utxo_bootstrap
        && matches!(&validator, None)
    {
        let (stx, srx) = tokio::sync::oneshot::channel();
        let (vtx, vrx) = tokio::sync::oneshot::channel();
        (Some(stx), Some(srx), Some(vtx), Some(vrx))
    } else {
        (None, None, None, None)
    };
```

- [ ] **Step 3: Modify validator creation for snapshot bootstrap**

Change the UTXO validator creation to return `None` when `utxo_bootstrap` is enabled and state is empty:
```rust
    let validator = if state_type == StateType::Utxo {
        let mut storage = RedbAVLStorage::open(&data_dir.join("state.redb"), params, keep_versions)?;
        if storage.version().is_some() {
            // Resume from existing state
            // ... (existing code to create prover and UtxoValidator)
            Some(Validator::Utxo(utxo_validator))
        } else if node_config.utxo_bootstrap {
            // Snapshot bootstrap — validator will be created after snapshot download
            tracing::info!("UTXO state empty, will bootstrap from snapshot");
            None
        } else {
            // Genesis bootstrap
            // ... (existing genesis code)
            Some(Validator::Utxo(utxo_validator))
        }
    } else {
        // Digest mode — existing code
        Some(Validator::Digest(digest_validator))
    };
```

- [ ] **Step 4: Pass channels to HeaderSync and handle snapshot result**

Update the sync machine creation:
```rust
    let sync_config = SyncConfig {
        // ... existing fields ...
        utxo_bootstrap: node_config.utxo_bootstrap,
        min_snapshot_peers: node_config.min_snapshot_peers,
        data_dir: data_dir.clone(),
    };

    let sync = HeaderSync::new(
        sync_config,
        transport,
        sync_chain,
        sync_store,
        validator,
        progress_rx,
        delivery_control_rx,
        delivery_data_rx,
        snapshot_tx,
        validator_rx,
    );

    let sync_handle = tokio::spawn(async move {
        sync.run().await;
    });
```

Add the snapshot handler (runs concurrently, waiting for snapshot data):
```rust
    if let Some(snapshot_rx) = snapshot_rx {
        let storage_path = data_dir.join("state.redb");
        let validator_tx = validator_tx.unwrap();
        tokio::spawn(async move {
            match snapshot_rx.await {
                Ok(snapshot_data) => {
                    tracing::info!(
                        "received snapshot data: {} nodes, height {}",
                        snapshot_data.nodes.len(),
                        snapshot_data.snapshot_height,
                    );

                    // Load into state storage
                    let params = AVLTreeParams { key_length: 32, value_length: None };
                    let mut storage = RedbAVLStorage::open(&storage_path, params, 200)
                        .expect("failed to open state storage for snapshot");

                    // Convert to (Digest32, Bytes) iterator
                    let nodes_iter = snapshot_data.nodes.into_iter().map(|(label, packed)| {
                        let mut digest = [0u8; 32];
                        digest.copy_from_slice(&label);
                        (digest, bytes::Bytes::from(packed))
                    });

                    // Build ADDigest (33 bytes: root_hash[32] + tree_height[1])
                    let mut version = Vec::with_capacity(33);
                    version.extend_from_slice(&snapshot_data.root_hash);
                    version.push(snapshot_data.tree_height);
                    let version = bytes::Bytes::from(version);

                    storage.load_snapshot(
                        nodes_iter,
                        snapshot_data.root_hash,
                        snapshot_data.tree_height as usize,
                        version,
                    ).expect("failed to load snapshot into state");

                    tracing::info!("snapshot loaded into state.redb");

                    // Create validator from loaded state
                    let resolver = storage.resolver();
                    let tree = ergo_avltree_rust::batch_node::AVLTree::new(
                        resolver, 32, None,
                    );
                    let prover = ergo_avltree_rust::batch_avl_prover::BatchAVLProver::new(tree, true);
                    let persistent = ergo_avltree_rust::persistent_batch_avl_prover::PersistentBatchAVLProver::new(
                        prover, Box::new(storage), vec![],
                    );

                    let validator = UtxoValidator::new(
                        persistent,
                        snapshot_data.snapshot_height,
                        node_config.checkpoint_height.unwrap_or(0),
                    );

                    let _ = validator_tx.send(Validator::Utxo(validator));
                    tracing::info!("validator created from snapshot, sent to sync machine");
                }
                Err(_) => {
                    tracing::warn!("snapshot channel closed without data");
                }
            }
        });
    }
```

- [ ] **Step 5: Verify compilation**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo check 2>&1 | head -40`

Fix any type mismatches. The exact types for `Digest32`, `ADDigest`, `Bytes`, PeerId, and validator construction may need adjustment based on the actual crate exports. Use the existing genesis bootstrap code as reference for the correct constructor calls.

- [ ] **Step 6: Run existing tests to verify no regressions**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test 2>&1 | tail -20`

All existing tests should still pass. The changes are additive — new code paths are only triggered when `utxo_bootstrap = true` and state is empty.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire UTXO snapshot sync into node startup

When utxo_bootstrap=true and state is empty, the sync machine downloads
a UTXO snapshot after header sync, sends it to main via oneshot channel,
main loads it into state.redb and creates the validator. Normal block
sync resumes from snapshot_height + 1."
```

---

## Task 10: Round-Trip Integration Test

Verify the full pipeline: build a tree, serialize as manifest + chunks, parse, load into state, verify root digest.

**Files:**
- Create: `sync/tests/snapshot_roundtrip_test.rs`

- [ ] **Step 1: Write the integration test**

`sync/tests/snapshot_roundtrip_test.rs`:
```rust
//! Round-trip test: build a small AVL+ tree using ergo_avltree_rust,
//! serialize it as manifest + chunks (manually), parse with our code,
//! verify all labels match.

use ergo_avltree_rust::batch_node::{AVLTree, LeafNode, InternalNode, Node};
use ergo_avltree_rust::operation::Digest32;
use ergo_sync::snapshot::parser::{
    compute_leaf_label, compute_internal_label, parse_dfs_stream, ParsedNode,
    PACKED_INTERNAL_PREFIX, PACKED_LEAF_PREFIX,
};
use ergo_sync::snapshot::manifest::extract_subtree_ids;
use bytes::Bytes;
use std::sync::Arc;

/// Build a simple 3-node tree (root + 2 leaves), serialize as manifest
/// with depth=1, parse back, verify labels match library.
#[test]
fn roundtrip_simple_tree() {
    // Create two leaves via the library
    let key_a = [0x01u8; 32];
    let val_a = b"box_aaa";
    let next_a = [0x02u8; 32];

    let key_b = [0x02u8; 32];
    let val_b = b"box_bbb";
    let next_b = [0xFFu8; 32];

    let leaf_a = LeafNode::new(
        &Bytes::copy_from_slice(&key_a),
        &Bytes::copy_from_slice(val_a),
        &Bytes::copy_from_slice(&next_a),
    );
    let leaf_b = LeafNode::new(
        &Bytes::copy_from_slice(&key_b),
        &Bytes::copy_from_slice(val_b),
        &Bytes::copy_from_slice(&next_b),
    );

    let label_a = *leaf_a.borrow().label().unwrap();
    let label_b = *leaf_b.borrow().label().unwrap();

    let internal = InternalNode::new(
        Some(Bytes::copy_from_slice(&key_a)),
        &leaf_a,
        &leaf_b,
        0i8,
    );
    let root_label = *internal.borrow().label().unwrap();

    // Verify our label functions match
    assert_eq!(compute_leaf_label(&key_a, val_a, &next_a), label_a);
    assert_eq!(compute_leaf_label(&key_b, val_b, &next_b), label_b);
    assert_eq!(compute_internal_label(0, &label_a, &label_b), root_label);

    // Build manifest bytes: header(root_height=1, depth=0) + root only
    // depth=0 means the root is the boundary — its children are subtree IDs
    let mut manifest = Vec::new();
    manifest.push(1); // root_height
    manifest.push(0); // manifest_depth = 0 (root is boundary)

    // Root internal node
    manifest.push(PACKED_INTERNAL_PREFIX);
    manifest.push(0x00); // balance
    manifest.extend_from_slice(&key_a);
    manifest.extend_from_slice(&label_a);
    manifest.extend_from_slice(&label_b);

    // Extract subtree IDs
    let ids = extract_subtree_ids(&manifest, 32).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&label_a));
    assert!(ids.contains(&label_b));

    // Build chunk A (leaf A)
    let mut chunk_a = Vec::new();
    chunk_a.push(PACKED_LEAF_PREFIX);
    chunk_a.extend_from_slice(&key_a);
    chunk_a.extend_from_slice(&(val_a.len() as u32).to_be_bytes());
    chunk_a.extend_from_slice(val_a);
    chunk_a.extend_from_slice(&next_a);

    // Build chunk B (leaf B)
    let mut chunk_b = Vec::new();
    chunk_b.push(PACKED_LEAF_PREFIX);
    chunk_b.extend_from_slice(&key_b);
    chunk_b.extend_from_slice(&(val_b.len() as u32).to_be_bytes());
    chunk_b.extend_from_slice(val_b);
    chunk_b.extend_from_slice(&next_b);

    // Parse manifest nodes (skip 2-byte header)
    let manifest_nodes = parse_dfs_stream(&manifest[2..], 32).unwrap();
    assert_eq!(manifest_nodes.len(), 1);
    assert_eq!(*manifest_nodes[0].label(), root_label);

    // Parse chunk nodes
    let chunk_a_nodes = parse_dfs_stream(&chunk_a, 32).unwrap();
    assert_eq!(chunk_a_nodes.len(), 1);
    assert_eq!(*chunk_a_nodes[0].label(), label_a);

    let chunk_b_nodes = parse_dfs_stream(&chunk_b, 32).unwrap();
    assert_eq!(chunk_b_nodes.len(), 1);
    assert_eq!(*chunk_b_nodes[0].label(), label_b);

    // Collect all (label, packed_bytes) — this is what load_snapshot() receives
    let all_nodes: Vec<([u8; 32], Vec<u8>)> = manifest_nodes.iter()
        .chain(chunk_a_nodes.iter())
        .chain(chunk_b_nodes.iter())
        .map(|n| (*n.label(), n.packed_bytes().to_vec()))
        .collect();

    assert_eq!(all_nodes.len(), 3);
    // Root label should match what the library computed
    assert_eq!(all_nodes[0].0, root_label);
}
```

- [ ] **Step 2: Run the test**

Run: `cd /home/mwaddip/projects/ergo-node-rust && cargo test -p ergo-sync --test snapshot_roundtrip_test -- --nocapture`

Expected: PASS. If it fails, the label computation or DFS parsing has a bug — fix before proceeding.

- [ ] **Step 3: Commit**

```bash
git add sync/tests/snapshot_roundtrip_test.rs
git commit -m "test(sync): round-trip integration test for snapshot parsing

Builds AVL+ tree via ergo_avltree_rust, serializes as manifest+chunks,
parses with our code, verifies all labels match library output."
```

---

## Post-Implementation Notes

### What this plan does NOT cover (future work)

1. **Snapshot serving** — creating snapshots and responding to `GetSnapshotsInfo`/`GetManifest`/`GetUtxoSnapshotChunk` requests. This requires a synced UTXO node first.

2. **Peer version filtering** — the discovery phase should only query peers with version >= 5.0.12. This requires exposing peer version from the P2P layer. For now, the `Unknown` message forwarding means old peers simply won't respond.

3. **ModePeerFeature advertising** — after snapshot bootstrap, the node should advertise `blocksToKeep = -2` (`UTXOSetBootstrapped`) to peers so they don't request historical block sections we don't have.

4. **Concurrent event handling during snapshot sync** — the current implementation blocks the transport during snapshot sync. Other P2P events (header announces, sync requests from peers) are not processed. This is acceptable for initial implementation but should be improved.

### Critical verification before deploying

1. **Label computation must match ergo_avltree_rust exactly.** Task 1 Step 6 is the gate — if the tests fail, stop and investigate.

2. **The JVM node must actually serve snapshots.** Not all peers have `storingUtxoSnapshots > 0` enabled. On testnet, you may need to configure a JVM peer to store snapshots.

3. **The manifest depth (14) must match.** Our parser handles any depth, but if the JVM sends depth 14, verify our parser handles trees that deep correctly.

### Config for testing

```toml
[node]
state_type = "utxo"
utxo_bootstrap = true
min_snapshot_peers = 1    # testnet: lower quorum for testing
checkpoint_height = 267000
```
