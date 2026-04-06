//! PoW solution validation and block assembly.

use blake2::Digest as Blake2Digest;
use ergo_chain_types::{AutolykosSolution, BlockId, Digest, Digest32, Header, Votes};
use sigma_ser::ScorexSerializable;

use crate::candidate::transactions_root;
use crate::types::CandidateBlock;
use crate::MiningError;

type Blake2b256 = blake2::Blake2b<blake2::digest::typenum::U32>;

/// Validate a PoW solution against a cached candidate and assemble the full header.
///
/// Recomputes the header from the candidate (same fields used during
/// `build_work_message`), injects the submitted solution, verifies
/// `pow_hit < target`, and returns the assembled header on success.
pub fn validate_solution(
    candidate: &CandidateBlock,
    solution: AutolykosSolution,
) -> Result<Header, MiningError> {
    let height = candidate.parent.height + 1;

    // Recompute header fields (must match build_work_message exactly)
    let ad_proofs_root = {
        let mut hasher = Blake2b256::new();
        hasher.update(&candidate.ad_proof_bytes);
        let hash: [u8; 32] = hasher.finalize().into();
        Digest32::from(hash)
    };

    let tx_root = transactions_root(&candidate.transactions)?;
    let ext_root_bytes = crate::extension::extension_digest(&candidate.extension)?;

    // Build the full header with the submitted solution.
    // The id field is initially zero — we compute the proper hash after assembly.
    let mut header = Header {
        version: candidate.version,
        id: BlockId(Digest::from([0u8; 32])),
        parent_id: candidate.parent.id,
        ad_proofs_root,
        state_root: candidate.state_root,
        transaction_root: tx_root,
        timestamp: candidate.timestamp,
        n_bits: candidate.n_bits,
        height,
        extension_root: Digest32::from(ext_root_bytes),
        autolykos_solution: solution,
        votes: Votes(candidate.votes),
        unparsed_bytes: Box::new([]),
    };

    // Compute the header ID = Blake2b256(scorex_serialize_bytes(header))
    // This must happen BEFORE check_pow because pow_hit uses serialize_without_pow
    // which doesn't include the id, so the order doesn't actually matter — but
    // setting id correctly is needed for downstream consumers.
    let header_bytes = header
        .scorex_serialize_bytes()
        .map_err(|e| MiningError::AssemblyFailed(format!("header serialize: {e}")))?;
    let id_hash: [u8; 32] = {
        let mut hasher = Blake2b256::new();
        hasher.update(&header_bytes);
        hasher.finalize().into()
    };
    header.id = BlockId(Digest32::from(id_hash));

    // Verify PoW using Header::check_pow() which computes:
    //   target = order / decode_compact_bits(n_bits)
    //   valid = pow_hit(header) < target
    let valid = header
        .check_pow()
        .map_err(|e| MiningError::InvalidSolution(format!("check_pow: {e}")))?;

    if !valid {
        return Err(MiningError::InvalidSolution("pow_hit >= target".into()));
    }

    Ok(header)
}
