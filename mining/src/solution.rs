//! PoW solution validation and block assembly.

use blake2::Digest as Blake2Digest;
use ergo_chain_types::autolykos_pow_scheme::{decode_compact_bits, AutolykosPowScheme};
use ergo_chain_types::{AutolykosSolution, BlockId, Digest, Digest32, Header, Votes};

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

    // Build the full header with the submitted solution
    let header = Header {
        version: candidate.version,
        id: BlockId(Digest::from([0u8; 32])), // TODO: compute from full serialization
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

    // Verify PoW: hit < target
    let pow = AutolykosPowScheme::default();
    let hit = pow
        .pow_hit(&header)
        .map_err(|e| MiningError::InvalidSolution(format!("pow_hit: {e}")))?;

    let target = decode_compact_bits(candidate.n_bits);
    let target_uint = target
        .to_biguint()
        .ok_or(MiningError::InvalidSolution("negative target".into()))?;

    if hit >= target_uint {
        return Err(MiningError::InvalidSolution(format!(
            "hit {hit} >= target {target_uint}"
        )));
    }

    Ok(header)
}
