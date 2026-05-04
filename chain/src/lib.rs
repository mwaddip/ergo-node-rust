//! Header chain validation for the Ergo Rust node.
//!
//! Phase 1: Parse headers from wire bytes, track best known height.
//! Phase 2: Verify proof of work before accepting headers.
//! Phase 3: Validate headers form a correct chain (parent, timestamp, difficulty).

pub(crate) mod cache;
mod chain;
mod config;
pub(crate) mod difficulty;
mod error;
mod nipopow_proof;
mod pow;
mod section;
mod state_type;
mod sync_info;
#[cfg(test)]
mod tests;
mod tracker;
mod voting;

pub use cache::{HeaderLoader, ScoreLoader, DEFAULT_CACHE_CAPACITY};
pub use chain::{AppendResult, HeaderChain};
pub use config::{ChainConfig, Network};
pub use ergo_chain_types::autolykos_pow_scheme::decode_compact_bits;
pub use ergo_chain_types::{BlockId, Header};
pub use error::ChainError;
pub use pow::verify_pow;
pub use section::{
    required_section_ids, section_ids,
    AD_PROOFS_TYPE_ID, BLOCK_TRANSACTIONS_TYPE_ID, EXTENSION_TYPE_ID, HEADER_TYPE_ID,
    TRANSACTION_TYPE_ID,
};
pub use state_type::StateType;
pub use sync_info::{build_sync_info, parse_sync_info, SyncInfo};
pub use num_bigint::BigUint;
pub use tracker::HeaderTracker;
pub use nipopow_proof::{
    build_nipopow_proof, compare_nipopow_proof_bytes, verify_nipopow_proof_bytes,
    NipopowVerificationResult,
};
pub use voting::{
    extract_disabling_rules_from_kv, pack_extension_bytes, pack_parameters_to_kv,
    parse_extension_bytes, parse_parameters_from_kv, VotingConfig,
    ID_BLOCK_VERSION, ID_SOFT_FORK_DISABLING_RULES, ID_SOFT_FORK_STARTING_HEIGHT,
    ID_SOFT_FORK_VOTES_COLLECTED, SOFT_FORK_VOTE,
};

use sigma_ser::ScorexSerializable;

/// Parse an `ergo-chain-types::Header` from raw Scorex-serialized bytes.
///
/// The `data` argument is the raw payload from a ModifierResponse with modifier_type = 101 (Header).
/// The header's `id` field is computed automatically (blake2b256 of the serialized header).
///
/// Never panics on malformed input — returns `Err(ChainError::Parse)` instead.
pub fn parse_header(data: &[u8]) -> Result<Header, ChainError> {
    Ok(Header::scorex_parse_bytes(data)?)
}
