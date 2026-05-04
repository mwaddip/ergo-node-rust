use ergo_chain_types::blake2b256_hash;
use ergo_chain_types::Header;

use crate::state_type::StateType;

/// Modifier type IDs (Ergo `NetworkObjectTypeId`).
///
/// `HEADER_TYPE_ID` and the three section IDs identify the components of a
/// full block. `TRANSACTION_TYPE_ID` identifies an *unconfirmed* transaction
/// modifier (mempool / inv-relay), not a block section — it's not returned
/// by [`section_ids`] or [`required_section_ids`].
pub const HEADER_TYPE_ID: u8 = 101;
pub const BLOCK_TRANSACTIONS_TYPE_ID: u8 = 102;
pub const AD_PROOFS_TYPE_ID: u8 = 104;
pub const EXTENSION_TYPE_ID: u8 = 108;

/// Modifier type ID for unconfirmed transactions (mempool / inv-relay).
///
/// Distinct from `BLOCK_TRANSACTIONS_TYPE_ID` (102), which is the block
/// section carrying confirmed transactions. This constant is the modifier
/// type used when relaying or requesting individual unconfirmed transactions.
pub const TRANSACTION_TYPE_ID: u8 = 2;

/// Compute the modifier IDs for the three non-header block sections.
///
/// Returns `[(type_id, modifier_id); 3]` for BlockTransactions, ADProofs,
/// and Extension. Each modifier ID is `Blake2b256(type_id || header.id || section_root)`.
///
/// Matches JVM `Header.sectionIds`. For mode-filtered sections, use
/// [`required_section_ids`] instead.
pub fn section_ids(header: &Header) -> [(u8, [u8; 32]); 3] {
    [
        (BLOCK_TRANSACTIONS_TYPE_ID, prefixed_hash(BLOCK_TRANSACTIONS_TYPE_ID, &header.id.0 .0, &header.transaction_root.0)),
        (AD_PROOFS_TYPE_ID, prefixed_hash(AD_PROOFS_TYPE_ID, &header.id.0 .0, &header.ad_proofs_root.0)),
        (EXTENSION_TYPE_ID, prefixed_hash(EXTENSION_TYPE_ID, &header.id.0 .0, &header.extension_root.0)),
    ]
}

/// Block sections required for a given header and node state type.
///
/// Mirrors JVM's `ToDownloadProcessor.requiredModifiersForHeader`:
/// - UTXO mode → `sectionIdsWithNoProof` (BlockTransactions + Extension)
/// - Digest mode → `sectionIds` (all three including ADProofs)
/// - Light mode → empty Vec; light clients download no block sections.
///   Returning empty here lets sync's section-queue construction handle
///   `Light` without a special case at the call site.
pub fn required_section_ids(header: &Header, state_type: StateType) -> Vec<(u8, [u8; 32])> {
    match state_type {
        StateType::Light => Vec::new(),
        StateType::Digest => section_ids(header).to_vec(),
        StateType::Utxo => section_ids(header)
            .iter()
            .filter(|(type_id, _)| *type_id != AD_PROOFS_TYPE_ID)
            .copied()
            .collect(),
    }
}

/// `Blake2b256(prefix_byte || data1 || data2)` — mirrors Scorex `Algos.hash.prefixedHash`.
fn prefixed_hash(prefix: u8, data1: &[u8; 32], data2: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + 32 + 32);
    buf.push(prefix);
    buf.extend_from_slice(data1);
    buf.extend_from_slice(data2);
    blake2b256_hash(&buf).0
}
