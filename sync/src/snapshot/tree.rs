//! One verifying walk over a pre-order DFS node stream, shared by the
//! manifest (cut at a boundary depth) and the chunks (walked to the leaves).
//!
//! Every node's label is recomputed from its own bytes when it is parsed
//! (`parser::parse_node`). What makes those labels a tree is the links: an
//! internal node carries the labels of its two children, and the walk checks
//! that the subtree which follows it in DFS order hashes to what it carries.
//! With the root checked against an expectation from outside — the header's
//! state root for a manifest, the requested subtree id for a chunk — every
//! node in the stream is bound to that expectation.

use std::fmt;

use super::parser::{parse_node, ParseError, ParsedNode};

/// Ergo's AVL+ key length (a box id).
pub(crate) const KEY_LENGTH: usize = 32;

/// Which child of an internal node a link refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Left => "left",
            Side::Right => "right",
        })
    }
}

/// Why a node stream was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TreeError {
    #[error("{0}")]
    Parse(#[from] ParseError),
    /// The bytes ran out while the walk still owed nodes.
    #[error("stream ends before the tree is complete")]
    Truncated,
    /// Bytes remain after the walk consumed the whole tree.
    #[error("{0} trailing bytes after the tree")]
    TrailingBytes(usize),
    /// The root's recomputed label is not what was expected of it.
    #[error("root label does not match the expected label")]
    RootMismatch,
    /// An internal node's stored child label is not the recomputed label of
    /// the subtree that follows it. `parent` is the node's index in DFS order.
    #[error("node {parent}'s {side} child does not hash to the label it carries")]
    BrokenLink { parent: usize, side: Side },
}

/// What a completed walk established, from the bytes alone.
#[derive(Debug)]
pub struct VerifiedTree {
    /// Recomputed label of the first node.
    pub root_label: [u8; 32],
    /// Child labels of the boundary nodes, in DFS order — a manifest's
    /// subtree ids. Empty when the walk had no boundary.
    pub subtree_ids: Vec<[u8; 32]>,
    /// Every node in the stream, in DFS order.
    pub nodes: Vec<ParsedNode>,
}

/// What the walk owes the next node it parses.
struct Owed {
    /// The label the node must hash to; `None` only for a root with no
    /// expectation.
    label: Option<[u8; 32]>,
    /// JVM level: the root is 1.
    depth: usize,
    /// The internal node (DFS index) that carries `label`, and on which side.
    parent: Option<(usize, Side)>,
}

/// Walk `data` as a pre-order DFS node stream and verify every link.
///
/// `boundary_depth` is the level (root = 1) at which a manifest's internal
/// nodes stop carrying inline children — their child labels are subtree ids
/// instead; `None` walks to the leaves, as a chunk is serialized.
/// `expected_root` is what the first node must hash to, if anything.
///
/// Every byte is accounted for: a stream that ends with nodes still owed is
/// `Truncated`, bytes left over once the tree is complete are `TrailingBytes`.
pub fn walk_tree(
    data: &[u8],
    key_length: usize,
    boundary_depth: Option<usize>,
    expected_root: Option<[u8; 32]>,
) -> Result<VerifiedTree, TreeError> {
    let mut nodes: Vec<ParsedNode> = Vec::new();
    let mut subtree_ids = Vec::new();
    let mut offset = 0;
    // Nodes owed, next one on top. A parent pushes its right child before
    // its left, so the left is parsed first.
    let mut owed = vec![Owed {
        label: expected_root,
        depth: 1,
        parent: None,
    }];

    while let Some(Owed {
        label,
        depth,
        parent,
    }) = owed.pop()
    {
        if offset >= data.len() {
            return Err(TreeError::Truncated);
        }
        let (node, consumed) = parse_node(&data[offset..], key_length)?;
        offset += consumed;

        if let Some(expected) = label {
            if *node.label() != expected {
                return Err(match parent {
                    None => TreeError::RootMismatch,
                    Some((parent, side)) => TreeError::BrokenLink { parent, side },
                });
            }
        }

        let index = nodes.len();
        if let ParsedNode::Internal {
            left_label,
            right_label,
            ..
        } = &node
        {
            if boundary_depth == Some(depth) {
                // Boundary node: its children are subtree chunks, not
                // serialized here. Those links are checked when the chunks
                // arrive, against these ids.
                subtree_ids.push(*left_label);
                subtree_ids.push(*right_label);
            } else {
                owed.push(Owed {
                    label: Some(*right_label),
                    depth: depth + 1,
                    parent: Some((index, Side::Right)),
                });
                owed.push(Owed {
                    label: Some(*left_label),
                    depth: depth + 1,
                    parent: Some((index, Side::Left)),
                });
            }
        }
        nodes.push(node);
    }

    if offset != data.len() {
        return Err(TreeError::TrailingBytes(data.len() - offset));
    }
    let root_label = nodes
        .first()
        .map(|node| *node.label())
        .ok_or(TreeError::Truncated)?;
    Ok(VerifiedTree {
        root_label,
        subtree_ids,
        nodes,
    })
}

/// Verify a chunk against the subtree id it was requested, or is stored,
/// under: root label, every link down to the leaves, every byte.
pub fn verify_chunk(data: &[u8], subtree_id: &[u8; 32]) -> Result<VerifiedTree, TreeError> {
    walk_tree(data, KEY_LENGTH, None, Some(*subtree_id))
}
