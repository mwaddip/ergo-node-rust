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

/// Compute the version-dependent Merkle root for the header's transaction_root.
///
/// JVM `BlockTransactions.scala:59-63`: for block version 1 the leaves are
/// the tx IDs; for version >= 2 the leaves are all tx IDs followed by all
/// witness IDs (`txIds ++ witnessIds` — two concatenated lists, not
/// interleaved). Mainnet and testnet are both version >= 2 today, so a
/// tx-IDs-only root is rejected by every JVM peer.
pub fn transactions_root(
    txs: &[Transaction],
    block_version: u8,
) -> Result<Digest32, MiningError> {
    if txs.is_empty() {
        return Err(MiningError::AssemblyFailed("no transactions".into()));
    }

    let mut nodes: Vec<MerkleNode> = txs
        .iter()
        .map(|tx| {
            let tx_id = tx.id();
            MerkleNode::from_bytes(tx_id.as_ref().to_vec())
        })
        .collect();

    if block_version >= 2 {
        nodes.extend(txs.iter().map(|tx| MerkleNode::from_bytes(witness_id(tx))));
    }

    let tree = MerkleTree::new(nodes);
    Ok(tree.root_hash())
}

/// Witness ID of a transaction: blake2b256 over the concatenation of every
/// input's raw spending-proof bytes, with the first byte dropped — 31 bytes
/// (JVM `ErgoTransaction.scala:77-78`). The truncation to 248 bits is
/// deliberate: it distinguishes witness leaves from 32-byte tx-ID leaves in
/// the Merkle tree. Empty proofs (e.g. storage-rent spends) contribute no
/// bytes to the concatenation.
fn witness_id(tx: &Transaction) -> Vec<u8> {
    let mut hasher = Blake2b256::new();
    for input in tx.inputs.iter() {
        hasher.update(input.spending_proof.proof.as_ref());
    }
    let hash: [u8; 32] = hasher.finalize().into();
    hash[1..].to_vec()
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

    // Transaction root = version-dependent Merkle root (tx IDs, plus
    // witness IDs for block version >= 2)
    let tx_root = transactions_root(&candidate.transactions, candidate.version)?;

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
            miner_pk: Box::new(*miner_pk),
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
    let pk_hex: String = (*miner_pk).into();

    let work = WorkMessage {
        msg: hex::encode(msg),
        b: target.to_string(),
        h: height,
        pk: pk_hex,
        // No mandatory transaction proofs today: the block carries only the
        // emission tx. The JVM `WorkMessage` encoder drops a `None` proof via
        // `.collect { case (_, Some(v)) => ... }`, so we emit `None` and the
        // `proof` field is omitted from the JSON. This keeps the basic
        // candidate ({msg, b, h, pk}) under the reference Autolykos2 miner's
        // fixed jsmn REQ_LEN=11 token buffer; a nested proof object overflows
        // it ("Jsmn failed to parse latest block"). A future candidateWithTxs
        // path will build `Some(ProofOfUpcomingTransactions { msg_preimage:
        // hex::encode(&header_bytes), tx_proofs })` — `header_bytes` is
        // returned below for exactly that.
        proof: None,
    };

    Ok((header_bytes, work))
}
