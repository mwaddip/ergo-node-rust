pub mod emission;
pub mod types;

pub use types::*;

/// Errors from mining operations.
#[derive(Debug, thiserror::Error)]
pub enum MiningError {
    #[error("mining not available: {0}")]
    Unavailable(String),

    #[error("no cached candidate")]
    NoCachedCandidate,

    #[error("candidate is stale (tip changed)")]
    StaleCandidate,

    #[error("invalid PoW solution: {0}")]
    InvalidSolution(String),

    #[error("block assembly failed: {0}")]
    AssemblyFailed(String),

    #[error("validation error: {0}")]
    Validation(#[from] ergo_validation::ValidationError),

    #[error("emission error: {0}")]
    Emission(String),

    #[error("state root computation failed: {0}")]
    StateRoot(String),
}
