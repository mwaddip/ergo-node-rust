use ergo_chain_types::autolykos_pow_scheme::AutolykosPowSchemeError;
use ergo_chain_types::BlockId;
use sigma_ser::ScorexParsingError;

/// Errors that can occur in header chain operations.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// Header deserialization failed.
    #[error("header parse failed: {0}")]
    Parse(#[from] ScorexParsingError),

    /// Proof-of-work verification failed.
    #[error("PoW verification failed: hit {hit} >= target {target}")]
    PowInvalid { hit: String, target: String },

    /// Error computing proof-of-work hit.
    #[error("PoW computation error: {0}")]
    PowCompute(#[from] AutolykosPowSchemeError),

    /// Header's parent_id doesn't match any header in the chain.
    #[error("parent not found: {parent_id}")]
    ParentNotFound { parent_id: BlockId },

    /// Header height is not parent height + 1.
    #[error("non-sequential height: expected {expected}, got {got}")]
    NonSequentialHeight { expected: u32, got: u32 },

    /// Header timestamp is not strictly greater than parent's.
    #[error("timestamp not increasing: parent {parent_ts}, got {got}")]
    TimestampNotIncreasing { parent_ts: u64, got: u64 },

    /// Header timestamp is too far in the future.
    #[error("timestamp too far in future: {timestamp} > max {max_allowed}")]
    TimestampTooFarInFuture { timestamp: u64, max_allowed: u64 },

    /// Header nBits doesn't match the expected difficulty for this height.
    #[error("wrong difficulty at height {height}: expected {expected}, got {got}")]
    WrongDifficulty {
        height: u32,
        expected: u32,
        got: u32,
    },

    /// Genesis header has wrong parent ID (must be all zeros).
    #[error("invalid genesis parent: {got}")]
    InvalidGenesisParent { got: BlockId },

    /// Genesis header has wrong height (must be 1).
    #[error("invalid genesis height: expected 1, got {got}")]
    InvalidGenesisHeight { got: u32 },

    /// Genesis header ID doesn't match the expected value for this network.
    #[error("genesis ID mismatch: expected {expected}, got {got}")]
    GenesisIdMismatch { expected: BlockId, got: BlockId },

    /// Difficulty calculation failed.
    #[error("difficulty calculation error: {0}")]
    DifficultyCalc(String),

    /// Reorg precondition violated.
    #[error("reorg error: {0}")]
    Reorg(String),

    /// SyncInfo message is malformed or violates constraints.
    #[error("SyncInfo error: {0}")]
    SyncInfo(String),

    /// Voting / soft-fork state error.
    #[error("voting error: {0}")]
    Voting(String),

    /// Extension parsing error.
    #[error("extension parse error: {0}")]
    ExtensionParse(String),

    /// NiPoPoW proof error.
    #[error("nipopow error: {0}")]
    Nipopow(String),

    /// `install_from_nipopow_proof` was called on a chain that already
    /// contains headers. The install API only accepts an empty chain.
    #[error("chain not empty: install_from_nipopow_proof requires is_empty()")]
    ChainNotEmpty,
}
