//! The snapshot manifest: its 2-byte header, the verified walk of its node
//! stream down to the boundary, and the binding of the whole to a header's
//! `state_root`.

use super::parser::{ParseError, ParsedNode};
use super::split_state_root;
use super::tree::{walk_tree, TreeError, VerifiedTree, KEY_LENGTH};

/// Manifest header size: root_height (1 byte) + manifest_depth (1 byte).
const HEADER_SIZE: usize = 2;

/// Why a manifest was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    /// The node stream does not hold together: a parse fault, a broken
    /// parent-child link, a root that is not the header's, bytes missing or
    /// left over.
    #[error("{0}")]
    Tree(#[from] TreeError),
    /// A `manifest_depth` of zero would put the root below the boundary.
    #[error("manifest depth is zero")]
    ZeroDepth,
    /// The manifest's root height is not the header's tree height.
    #[error("manifest root height {got} does not match the header's tree height {expected}")]
    RootHeightMismatch { expected: u8, got: u8 },
}

impl From<ParseError> for ManifestError {
    fn from(e: ParseError) -> Self {
        ManifestError::Tree(TreeError::Parse(e))
    }
}

/// Parse the 2-byte manifest header.
///
/// Returns `(root_height, manifest_depth)`.
pub fn parse_manifest_header(data: &[u8]) -> Result<(u8, u8), ParseError> {
    if data.len() < HEADER_SIZE {
        return Err(ParseError::UnexpectedEof);
    }
    Ok((data[0], data[1]))
}

/// Walk a manifest: the header, then the node stream down to the boundary
/// with every parent-child link and every byte checked (`tree::walk_tree`).
/// Returns the root height with the verified tree.
fn walk_manifest(
    manifest_bytes: &[u8],
    key_length: usize,
    expected_root: Option<[u8; 32]>,
) -> Result<(u8, VerifiedTree), ManifestError> {
    let (root_height, manifest_depth) = parse_manifest_header(manifest_bytes)?;
    if manifest_depth == 0 {
        return Err(ManifestError::ZeroDepth);
    }
    let tree = walk_tree(
        &manifest_bytes[HEADER_SIZE..],
        key_length,
        Some(manifest_depth as usize),
        expected_root,
    )?;
    Ok((root_height, tree))
}

/// Walk the manifest DFS stream and extract subtree IDs.
///
/// The manifest bytes must include the 2-byte header. Every parent-child
/// link above the boundary must hold; a structural fault is an error, never
/// a partial list.
pub fn extract_subtree_ids(
    manifest_bytes: &[u8],
    key_length: usize,
) -> Result<Vec<[u8; 32]>, ManifestError> {
    Ok(walk_manifest(manifest_bytes, key_length, None)?
        .1
        .subtree_ids)
}

/// Manifest bytes bound to a header's `state_root`.
///
/// [`verify_manifest`] is the only constructor. Holding one means the root
/// label recomputed from these bytes is the header's root hash, every
/// parent-child link above the boundary holds, and the manifest's root
/// height is the header's tree height — the contract's `verify`
/// (`facts/snapshot.md` § Verification). Subtree ids come from here and
/// nowhere else, so nothing downstream descends from unverified bytes.
#[derive(Debug, Clone)]
pub struct VerifiedManifest {
    bytes: Vec<u8>,
    root_hash: [u8; 32],
    root_height: u8,
    manifest_depth: u8,
    subtree_ids: Vec<[u8; 32]>,
}

impl VerifiedManifest {
    /// The raw manifest, header included — what the download store keeps.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The node stream: everything after the 2-byte header.
    pub fn node_bytes(&self) -> &[u8] {
        &self.bytes[HEADER_SIZE..]
    }

    /// Root label recomputed from the first node; equal to the header's root hash.
    pub fn root_hash(&self) -> [u8; 32] {
        self.root_hash
    }

    /// Root height from the manifest header; equal to the header's tree height.
    pub fn root_height(&self) -> u8 {
        self.root_height
    }

    /// Ids of the subtree chunks below the boundary nodes, in DFS order.
    pub fn subtree_ids(&self) -> &[[u8; 32]] {
        &self.subtree_ids
    }

    /// Walk the node stream once more, against the verified root, and hand
    /// back every node in DFS order. Assembly uses this so the nodes it
    /// emits are the nodes that verified.
    pub fn nodes(&self) -> Result<Vec<ParsedNode>, ManifestError> {
        let tree = walk_tree(
            self.node_bytes(),
            KEY_LENGTH,
            Some(self.manifest_depth as usize),
            Some(self.root_hash),
        )?;
        Ok(tree.nodes)
    }
}

/// Bind manifest bytes to the header's `state_root` at the snapshot height.
///
/// Walks the whole manifest with the header's root hash as what the first
/// node must hash to — any structural fault or broken link rejects it — then
/// checks the manifest's root height against the header's tree height. The
/// expectation is the header's: the manifest id agreed by quorum is never a
/// substitute for hashing the bytes that arrived (`facts/receive-path.md`).
pub fn verify_manifest(
    bytes: Vec<u8>,
    expected_state_root: &[u8; 33],
) -> Result<VerifiedManifest, ManifestError> {
    let (expected_root, expected_height) = split_state_root(expected_state_root);
    let (root_height, tree) = walk_manifest(&bytes, KEY_LENGTH, Some(expected_root))?;
    if root_height != expected_height {
        return Err(ManifestError::RootHeightMismatch {
            expected: expected_height,
            got: root_height,
        });
    }
    let manifest_depth = bytes[1];
    Ok(VerifiedManifest {
        bytes,
        root_hash: tree.root_label,
        root_height,
        manifest_depth,
        subtree_ids: tree.subtree_ids,
    })
}
