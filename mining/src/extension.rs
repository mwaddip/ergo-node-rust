//! Extension section building — NiPoPoW interlinks and voting parameters.

use ergo_chain_types::{BlockId, Header};
use ergo_merkle_tree::{MerkleNode, MerkleTree};
use ergo_nipopow::NipopowAlgos;

use crate::types::ExtensionCandidate;
use crate::MiningError;

/// Build the extension section for a new block.
///
/// Contains NiPoPoW interlinks updated from the parent header.
/// Voting parameters are encoded in the header's votes field, not in
/// the extension (except at epoch boundaries — deferred for first release).
pub fn build_extension(
    parent: &Header,
    parent_interlinks: &[BlockId],
) -> Result<ExtensionCandidate, MiningError> {
    // Update interlinks from parent
    let updated_interlinks =
        NipopowAlgos::update_interlinks(parent.clone(), parent_interlinks.to_vec())
            .map_err(|e| MiningError::AssemblyFailed(format!("interlinks: {e}")))?;

    // Pack interlinks into extension key-value format
    let fields = NipopowAlgos::pack_interlinks(updated_interlinks);

    Ok(ExtensionCandidate { fields })
}

/// Compute the Merkle root digest of extension fields.
///
/// Uses the same leaf format as the JVM: `[2u8] ++ key ++ value`
/// where 2 is the key length prefix. Must match JVM `Extension.rootHash`
/// byte-for-byte.
pub fn extension_digest(extension: &ExtensionCandidate) -> Result<[u8; 32], MiningError> {
    if extension.fields.is_empty() {
        // Empty extension uses special hash (genesis case)
        let tree = MerkleTree::new(Vec::<MerkleNode>::new());
        return Ok(tree.root_hash_special().into());
    }

    // Each extension field becomes a Merkle leaf: [key_length=2] ++ key ++ value
    let nodes: Vec<MerkleNode> = extension
        .fields
        .iter()
        .map(|(key, value)| {
            let mut leaf_data = Vec::with_capacity(1 + 2 + value.len());
            leaf_data.push(2u8); // key length prefix
            leaf_data.extend_from_slice(key);
            leaf_data.extend_from_slice(value);
            MerkleNode::from_bytes(leaf_data)
        })
        .collect();

    let tree = MerkleTree::new(nodes);
    Ok(tree.root_hash().into())
}
