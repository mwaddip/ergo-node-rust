//! Header construction and WorkMessage derivation for mining candidates.

use blake2::Digest as Blake2Digest;
use ergo_chain_types::{AutolykosSolution, BlockId, Digest, Digest32, EcPoint, Header, Votes};

use crate::types::*;
use crate::MiningError;

type Blake2b256 = blake2::Blake2b<blake2::digest::typenum::U32>;

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

    let ad_proofs_root = Digest32::from(enr_chain::ad_proofs_digest(&candidate.ad_proof_bytes));

    if candidate.transactions.is_empty() {
        return Err(MiningError::AssemblyFailed("no transactions".into()));
    }
    let tx_root = Digest32::from(enr_chain::transactions_root(
        &candidate.transactions,
        candidate.version,
    ));

    let ext_root_bytes = enr_chain::extension_root(&candidate.extension.fields);

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

    // b = the Autolykos TARGET: q / decode_compact_bits(n_bits), where q is the
    // secp256k1 group order. It is NOT decode_compact_bits(n_bits) itself —
    // that is the DIFFICULTY. `enr_chain::pow_target` is the one definition of
    // this value; do not re-derive the division here. See ../facts/chain.md
    // § "Phase 2".
    let target = enr_chain::pow_target(candidate.n_bits);

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
