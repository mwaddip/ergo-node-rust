//! Extension section building — NiPoPoW interlinks and voting parameters.

use ergo_chain_types::{BlockId, ExtensionCandidate as ErgoExtensionCandidate, Header};
use ergo_lib::chain::parameters::Parameters;
use ergo_merkle_tree::{MerkleNode, MerkleTree};
use ergo_nipopow::NipopowAlgos;

use crate::types::ExtensionCandidate;
use crate::MiningError;

/// Unpack a parent block's interlinks from its raw extension bytes.
///
/// Bridges the in-repo modifier store (which stores raw extension bytes)
/// to sigma-rust's `NipopowAlgos::unpack_interlinks` (which takes a parsed
/// `ExtensionCandidate`). Used by the mining task to obtain interlinks for
/// the next candidate's `build_extension` call.
///
/// Returns an empty Vec if the extension contains no interlink fields,
/// or if parsing fails. Errors are logged inside, never propagated —
/// genesis (no parent extension) is a normal case.
pub fn unpack_parent_interlinks(parent_extension_bytes: &[u8]) -> Vec<BlockId> {
    let parsed = match ergo_validation::parse_extension(parent_extension_bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("mining: parent extension parse failed: {e}; using empty interlinks");
            return vec![];
        }
    };

    let fields: Vec<([u8; 2], Vec<u8>)> = parsed
        .fields
        .into_iter()
        .map(|f| (f.key, f.value))
        .collect();

    let ec = match ErgoExtensionCandidate::new(fields) {
        Ok(ec) => ec,
        Err(e) => {
            tracing::warn!(
                "mining: ExtensionCandidate::new failed: {e}; using empty interlinks"
            );
            return vec![];
        }
    };

    match NipopowAlgos::unpack_interlinks(&ec) {
        Ok(links) => links,
        Err(e) => {
            tracing::warn!("mining: unpack_interlinks failed: {e}; using empty interlinks");
            vec![]
        }
    }
}

/// Build the extension section for a new block.
///
/// Contains NiPoPoW interlinks updated from the parent header. When
/// `boundary_params` is `Some`, also includes the packed parameter fields
/// (`[0x00, param_id]` keys with 4-byte BE i32 values) — required at every
/// epoch-boundary block per the consensus rules. When `None`, only
/// interlinks are emitted (within-epoch blocks).
///
/// `proposed_update_bytes` is the chain's active proposed-update payload
/// (typically `chain.active_proposed_update_bytes()`). When non-empty,
/// emitted as field `[0x00, 124]` (`SoftForkDisablingRules`). Required for
/// the boundary block where v6 activates: `compute_expected_parameters`
/// gates the `SubblocksPerBlock` auto-insert on the activated proposed
/// update containing rule 409, and the block's own extension must carry
/// that same payload so other peers compute identical expected params.
/// Empty payload at non-activation boundaries is a no-op.
///
/// Voting decisions (which parameters to vote for) are encoded in the
/// header's `votes` field, NOT in the extension. The extension at an
/// epoch-boundary block carries the RESULT of the just-ended voting epoch.
pub fn build_extension(
    parent: &Header,
    parent_interlinks: &[BlockId],
    boundary_params: Option<&Parameters>,
    proposed_update_bytes: &[u8],
) -> Result<ExtensionCandidate, MiningError> {
    // Update interlinks from parent
    let updated_interlinks =
        NipopowAlgos::update_interlinks(parent.clone(), parent_interlinks.to_vec())
            .map_err(|e| MiningError::AssemblyFailed(format!("interlinks: {e}")))?;

    // Pack interlinks into extension key-value format
    let mut fields = NipopowAlgos::pack_interlinks(updated_interlinks);

    // At epoch boundary, append packed parameters
    if let Some(params) = boundary_params {
        fields.extend(ergo_validation::pack_parameters(params));
    }

    // Carry the active proposed-update payload at key [0x00, 124] when set.
    if !proposed_update_bytes.is_empty() {
        fields.push(([0x00, 124], proposed_update_bytes.to_vec()));
    }

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
