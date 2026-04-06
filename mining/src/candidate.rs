//! Header construction and WorkMessage derivation for mining candidates.

use blake2::Digest as Blake2Digest;
use ergo_chain_types::autolykos_pow_scheme::decode_compact_bits;
use ergo_chain_types::{
    AutolykosSolution, BlockId, Digest, Digest32, EcPoint, Header, Votes,
};
use ergo_lib::chain::transaction::Transaction;
use ergo_merkle_tree::{MerkleNode, MerkleTree};

use crate::extension::extension_digest;
use crate::types::*;
use crate::MiningError;

type Blake2b256 = blake2::Blake2b<blake2::digest::typenum::U32>;

/// Compute the Merkle root of transaction IDs for the header's transaction_root.
pub fn transactions_root(txs: &[Transaction]) -> Result<Digest32, MiningError> {
    if txs.is_empty() {
        return Err(MiningError::AssemblyFailed("no transactions".into()));
    }

    let nodes: Vec<MerkleNode> = txs
        .iter()
        .map(|tx| {
            let tx_id = tx.id();
            MerkleNode::from_bytes(tx_id.as_ref().to_vec())
        })
        .collect();

    let tree = MerkleTree::new(nodes);
    Ok(tree.root_hash())
}

/// Build the candidate header (without PoW) and derive the WorkMessage.
///
/// The header is constructed with a placeholder PoW solution — miners fill
/// in the real solution. `serialize_without_pow()` produces the bytes that
/// the miner hashes to find a valid nonce.
///
/// Consensus-critical: the serialization must match the JVM's
/// `HeaderSerializer.bytesWithoutPow()` byte-for-byte.
pub fn build_work_message(
    candidate: &CandidateBlock,
    miner_pk: &EcPoint,
) -> Result<(Vec<u8>, WorkMessage), MiningError> {
    let height = candidate.parent.height + 1;

    // AD proofs root = Blake2b256(ad_proof_bytes)
    let ad_proofs_root = {
        let mut hasher = Blake2b256::new();
        hasher.update(&candidate.ad_proof_bytes);
        let hash: [u8; 32] = hasher.finalize().into();
        Digest32::from(hash)
    };

    // Transaction root = Merkle root of tx IDs
    let tx_root = transactions_root(&candidate.transactions)?;

    // Extension root
    let ext_root_bytes = extension_digest(&candidate.extension)?;

    // Build header with placeholder solution (excluded from serialization)
    let header = Header {
        version: candidate.version,
        id: BlockId(Digest::from([0u8; 32])), // computed after PoW
        parent_id: candidate.parent.id,
        ad_proofs_root,
        state_root: candidate.state_root,
        transaction_root: tx_root,
        timestamp: candidate.timestamp,
        n_bits: candidate.n_bits,
        height,
        extension_root: Digest32::from(ext_root_bytes),
        autolykos_solution: AutolykosSolution {
            miner_pk: Box::new(miner_pk.clone()),
            pow_onetime_pk: None,
            nonce: vec![0u8; 8],
            pow_distance: None,
        },
        votes: Votes(candidate.votes),
        unparsed_bytes: Box::new([]),
    };

    // Serialize header without PoW fields
    let header_bytes = header
        .serialize_without_pow()
        .map_err(|e| MiningError::AssemblyFailed(format!("header serialize: {e}")))?;

    // msg = Blake2b256(header_bytes)
    let msg = {
        let mut hasher = Blake2b256::new();
        hasher.update(&header_bytes);
        let hash: [u8; 32] = hasher.finalize().into();
        hash
    };

    // b = target from nBits
    let target = decode_compact_bits(candidate.n_bits);

    // pk = compressed EcPoint hex
    let pk_hex: String = miner_pk.clone().into();

    let work = WorkMessage {
        msg: hex::encode(msg),
        b: target.to_string(),
        h: height,
        pk: pk_hex,
        proof: ProofOfUpcomingTransactions {
            msg_preimage: hex::encode(&header_bytes),
            tx_proofs: vec![],
        },
    };

    Ok((header_bytes, work))
}
