//! Prover memory attribution — measured from the live AVL structures.
//!
//! `/debug/memory` could not name most of a catching-up node's heap: a v0.8.0
//! node that applied ~27k blocks held 1356 MB of live heap (87%) that no crate
//! claimed, while a node at the same tip that did not sync held 214 MB
//! unattributed. The caches were not the difference — the catch-up node's were
//! already down to 34 MB state / 18 MB store. Something that grows with block
//! *application* and is never released is holding it, and the AVL prover was
//! one of two suspects that report nothing (`facts/api.md`,
//! `facts/validation.md`).
//!
//! ## Why the prover's tree is a candidate at all
//!
//! `AVLTree::resolve` replaces a `Node::LabelOnly` child **in place** with the
//! node unpacked from storage (`batch_node.rs`), and nothing ever converts a
//! resolved node back to a label. `RedbAVLStorage::update_internal` does not
//! reinstall the root either — it commits, resets the dirty flags, and clears
//! the changed-node buffers, leaving `tree.root` exactly as the block left it
//! (`state/src/storage.rs`). So every node touched since the last
//! `restore_root` stays materialised in RAM, and the resident tree grows
//! monotonically — with reads as much as with writes, since a lookup resolves
//! too. That is the shape of the reported symptom; this module measures it
//! rather than asserting it.
//!
//! ## Everything here is counted, never multiplied
//!
//! `AVG_HEADER_BYTES` reported 1.48 GB for a `Vec` that had been deleted four
//! months earlier because it multiplied a count by a constant nobody rechecked
//! (`facts/api.md`). Nothing here is a constant: the per-node figure is
//! `size_of` of the real types, so it moves if the fork's `Node` changes, and
//! every payload figure is the `len()` of the actual buffer.

use std::cell::RefCell;

use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOpsBase;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::batch_node::{Node, NodeHeader, NodeId};

/// Resident bytes held by the AVL prover, best-effort.
///
/// The two byte figures are **disjoint** and may be summed. Neither is derived
/// from the other, and neither is a count times a constant.
///
/// ⚠ **Producing this costs O(`node_count`)** — a full traversal of the
/// materialised tree, which on a synced mainnet node is the same order as
/// applying a block. It is a diagnostic, not an accessor: sample it on a flush
/// cadence or every N blocks, never per block and never on an HTTP path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProverMemoryEstimate {
    /// Retained allocation of the prover's three per-cycle bookkeeping
    /// containers: `modified_nodes` (the address-keyed map proof generation
    /// gates on), `changed_nodes_buffer`, and `changed_nodes_buffer_to_check`.
    /// All three are populated as nodes are visited and cleared at the end of
    /// the proof cycle, so between blocks this is small and stable.
    ///
    /// It counts the **containers, not the `Node`s they reference** — those
    /// are the same allocations `resident_nodes_bytes` already counted, and
    /// adding them here would double-count the tree against itself.
    ///
    /// Stated blind spot: a copy-on-write predecessor displaced from the tree
    /// but still pinned by `changed_nodes_buffer` is counted by neither field.
    /// It is bounded by one block's working set and freed when the buffers
    /// clear, so it cannot be a source of unbounded growth — but it is not
    /// zero, and this is where it went.
    ///
    /// `Vec` capacity is exact retained heap and is used as such, so a buffer
    /// that was cleared but kept a large allocation still shows up. `BTreeMap`
    /// exposes only `len()`, so its slot payload is a lower bound on the
    /// B-tree's internal arrays.
    pub modified_nodes_bytes: u64,
    /// Tree nodes held resident between blocks: every `Rc<RefCell<Node>>`
    /// allocation reachable from the root **without touching storage**, plus
    /// the key/value bytes each node owns. This is the figure that grows with
    /// blocks applied.
    ///
    /// Accuracy, in both directions:
    /// - **Under** by the allocator's per-allocation overhead and size-class
    ///   rounding, so real RSS is at least this.
    /// - **Under** by the `bytes::Bytes` shared-control-block allocation (tens
    ///   of bytes per storage-unpacked node).
    /// - **Over** where several `Bytes` handles share one allocation and each
    ///   contributes its `len()`. On the dominant path — `AVLTree::unpack`,
    ///   which splits one buffer into key, value and next-key — those three
    ///   lengths sum to very nearly that one allocation, so this term is
    ///   small.
    ///
    /// Not counted: the tree's `Resolver`, an `Arc<dyn Fn>` capturing the redb
    /// handle already attributed as `stateCacheBytes`.
    pub resident_nodes_bytes: u64,
    /// Number of resident nodes behind `resident_nodes_bytes`, so a
    /// bytes-per-node that drifts is visible rather than inferred. Includes
    /// the unresolved `LabelOnly` frontier, which is a real allocation with no
    /// payload. `modified_nodes_bytes` counts no nodes and is not in this
    /// ratio.
    pub node_count: u64,
}

/// Measure the prover's resident heap.
///
/// `None` only when the prover has no root, which is the one state in which
/// there is genuinely nothing to measure. A rooted prover always yields a
/// figure, because a root is itself at least one real allocation — so `0` is
/// unreachable here and `None` never has to stand in for "small".
///
/// The walk is depth-first from `tree.root` and **never resolves**: it matches
/// on each node in place and stops at every `Node::LabelOnly`, seeing exactly
/// what is already in RAM. Going through `AVLTree::left`/`right` instead would
/// resolve each label out of redb and pull the entire UTXO set into memory —
/// the measurement would cause the exhaustion it exists to diagnose.
///
/// Per node it counts the `Rc<RefCell<Node>>` allocation and the `Bytes` the
/// node owns: the header key, and a leaf's value and next-node key. `Digest32`
/// labels are `[u8; 32]` stored inline and are already inside
/// `size_of::<Node>()`.
pub(crate) fn estimate(prover: &BatchAVLProver) -> Option<ProverMemoryEstimate> {
    let root = prover.base.tree.root.as_ref()?;

    let per_node = node_allocation_bytes();
    let mut node_count: u64 = 0;
    let mut resident_nodes_bytes: u64 = 0;

    // Explicit stack, not recursion: the fork hit a stack-exhausting spine in
    // `label_subtree` and solved it the same way. An AVL tree keeps this to
    // O(height) entries — the pending siblings along the current path.
    let mut stack: Vec<NodeId> = vec![root.clone()];
    while let Some(node) = stack.pop() {
        node_count += 1;
        resident_nodes_bytes += per_node;
        match &*node.borrow() {
            // The unresolved frontier: a real allocation holding a label, and
            // the point past which nothing is in memory.
            Node::LabelOnly(hdr) => resident_nodes_bytes += header_payload_bytes(hdr),
            Node::Internal(inner) => {
                resident_nodes_bytes += header_payload_bytes(&inner.hdr);
                stack.push(inner.left.clone());
                stack.push(inner.right.clone());
            }
            Node::Leaf(leaf) => {
                resident_nodes_bytes += header_payload_bytes(&leaf.hdr)
                    + leaf.value.len() as u64
                    + leaf.next_node_key.len() as u64;
            }
        }
    }

    Some(ProverMemoryEstimate {
        modified_nodes_bytes: cycle_buffer_bytes(&prover.base),
        resident_nodes_bytes,
        node_count,
    })
}

/// Heap bytes for one `Rc<RefCell<Node>>`: `RcBox`'s strong and weak counters
/// followed by the payload. Derived from the fork's own types, so a change to
/// `Node` moves this figure instead of silently invalidating it — 160 bytes
/// against the pinned rev.
pub(crate) fn node_allocation_bytes() -> u64 {
    (2 * std::mem::size_of::<usize>() + std::mem::size_of::<RefCell<Node>>()) as u64
}

/// The key a node owns. `label` is an inline `[u8; 32]` and is already in
/// `size_of::<Node>()`; only the `Bytes` key is out of line.
fn header_payload_bytes(hdr: &NodeHeader) -> u64 {
    hdr.key.as_ref().map_or(0, |key| key.len()) as u64
}

/// Retained allocation of the per-cycle bookkeeping containers. All three
/// share one lifecycle: populated by `on_node_visit`, cleared by
/// `generate_proof()` and `restore_root()`, and the two `Vec`s additionally by
/// `RedbAVLStorage::update_internal`.
fn cycle_buffer_bytes(base: &AuthenticatedTreeOpsBase) -> u64 {
    let map_slot = (std::mem::size_of::<usize>() + std::mem::size_of::<NodeId>()) as u64;
    let vec_slot = std::mem::size_of::<NodeId>() as u64;

    base.modified_nodes.len() as u64 * map_slot
        + base.changed_nodes_buffer.capacity() as u64 * vec_slot
        + base.changed_nodes_buffer_to_check.capacity() as u64 * vec_slot
}
