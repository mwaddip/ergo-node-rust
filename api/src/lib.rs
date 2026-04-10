mod handlers;
pub mod types;

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use axum::Router;
use ergo_mempool::Mempool;
use ergo_mining::CandidateGenerator;
use ergo_validation::ErgoStateContext;
use tokio::sync::{Mutex, RwLock};

/// Shared state available to all API handlers.
///
/// Constructed by the main crate from its existing shared handles.
/// The API crate never creates these resources — it only reads them.
#[derive(Clone)]
pub struct ApiState {
    /// Header chain — trait methods handle their own locking internally.
    pub chain: Arc<dyn ChainAccess>,
    /// Block storage — full block sections by header ID.
    pub store: Arc<dyn StoreAccess>,
    /// Mempool — transaction submission and queries.
    pub mempool: Arc<Mutex<Mempool>>,
    /// UTXO state — box lookups by ID (read-only snapshot reader).
    pub utxo_reader: Arc<dyn UtxoAccess>,
    /// Current state context for transaction validation.
    /// None during initial sync before first block is validated.
    pub state_context: Arc<RwLock<Option<ErgoStateContext>>>,
    /// Peer count callback — returns (connected, known) counts.
    pub peer_count: Arc<dyn Fn() -> PeerCounts + Send + Sync>,
    /// Node metadata.
    pub node_info: NodeMeta,
    /// Mining state — None if mining is not configured or node is in digest mode.
    pub mining: Option<Arc<CandidateGenerator>>,
    /// Block submitter — handles mined block application + broadcast.
    pub block_submitter: Option<Arc<dyn BlockSubmitter>>,
    /// Last fully validated block height (updated by the validator).
    pub validated_height: Arc<AtomicU32>,
    /// Peer REST URL callback — returns connected peers with their socket addr and REST URL.
    pub peer_api_urls: Arc<dyn Fn() -> Vec<PeerRestInfo> + Send + Sync>,
    /// Modifier pipeline sender — for the /ingest/modifiers endpoint.
    pub modifier_tx: Option<tokio::sync::mpsc::Sender<(u8, [u8; 32], Vec<u8>, Option<u64>)>>,
    /// Watch channel for validated block height — used by /info/wait long-poll.
    pub height_watch: tokio::sync::watch::Receiver<u32>,
}

/// Static node metadata.
#[derive(Clone)]
pub struct NodeMeta {
    pub name: String,
    pub version: String,
    pub network: String,
    pub state_type: String,
}

/// Peer count summary.
pub struct PeerCounts {
    pub connected: usize,
}

/// Per-peer REST info returned by the peer_api_urls callback.
pub struct PeerRestInfo {
    pub peer_id: u64,
    pub addr: std::net::SocketAddr,
    pub rest_url: Option<String>,
}

/// Trait for chain access — avoids API depending on enr-chain internals.
/// Implementations handle their own locking.
pub trait ChainAccess: Send + Sync {
    fn height(&self) -> u32;
    fn header_at(&self, height: u32) -> Option<ergo_chain_types::Header>;
    fn header_by_id(&self, id: &[u8; 32]) -> Option<ergo_chain_types::Header>;
    fn tip(&self) -> Option<ergo_chain_types::Header>;
}

/// Trait for block store access.
pub trait StoreAccess: Send + Sync {
    /// Get raw modifier bytes by type ID and modifier ID.
    fn get(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>>;
    /// Get raw modifier bytes by type ID and block height.
    /// Looks up the modifier ID at that height first, then fetches the data.
    fn get_at_height(&self, type_id: u8, height: u32) -> Option<Vec<u8>>;
}

/// Trait for UTXO lookups.
pub trait UtxoAccess: Send + Sync {
    /// Look up a box by its ID in the confirmed UTXO set.
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ergo_validation::ErgoBox>;
}

/// Trait for submitting locally-mined blocks to the node's processing pipeline.
///
/// Implementations are responsible for storing the block sections, advancing
/// the chain, and broadcasting to peers. Called by the mining solution handler
/// after a valid PoW solution is received.
pub trait BlockSubmitter: Send + Sync {
    /// Submit a freshly mined block. The header is the assembled Header (with
    /// PoW solution). The byte slices are the raw wire-format sections.
    fn submit(
        &self,
        header: ergo_chain_types::Header,
        block_txs_bytes: Vec<u8>,
        ad_proofs_bytes: Vec<u8>,
        extension_bytes: Vec<u8>,
    ) -> Result<(), String>;
}

/// Build the axum Router with all API routes.
pub fn router(state: ApiState) -> Router {
    use axum::routing::get;

    Router::new()
        // Node info
        .route("/info", get(handlers::get_info))
        .route("/info/wait", get(handlers::info_wait))
        // Blocks
        .route("/blocks/at/{height}", get(handlers::get_block_ids_at_height))
        .route("/blocks/{header_id}/header", get(handlers::get_block_header))
        .route("/blocks/{header_id}/transactions", get(handlers::get_block_transactions))
        .route("/blocks/lastHeaders/{count}", get(handlers::get_last_headers))
        // Transactions
        .route("/transactions", axum::routing::post(handlers::post_transaction))
        .route("/transactions/check", axum::routing::post(handlers::check_transaction))
        .route("/transactions/unconfirmed", get(handlers::get_unconfirmed))
        .route("/transactions/unconfirmed/transactionIds", get(handlers::get_unconfirmed_ids))
        .route("/transactions/unconfirmed/byTransactionId/{tx_id}", get(handlers::get_unconfirmed_by_id))
        .route("/transactions/getFee", get(handlers::get_recommended_fee))
        .route("/transactions/poolHistogram", get(handlers::get_pool_histogram))
        // UTXO
        .route("/utxo/byId/{box_id}", get(handlers::get_utxo_by_id))
        .route("/utxo/withPool/byId/{box_id}", get(handlers::get_utxo_with_pool))
        // Peers
        .route("/peers/connected", get(handlers::get_connected_peers))
        .route("/peers/api-urls", get(handlers::get_peer_api_urls))
        // Ingest (localhost only)
        .route("/ingest/modifiers", axum::routing::post(handlers::post_ingest_modifiers))
        // Emission
        .route("/emission/at/{height}", get(handlers::get_emission_at))
        // Mining
        .route("/mining/candidate", get(handlers::get_mining_candidate))
        .route("/mining/solution", axum::routing::post(handlers::post_mining_solution))
        .route("/mining/rewardAddress", get(handlers::get_mining_reward_address))
        .with_state(state)
}

/// Start the API server on the given address.
///
/// Returns a future that runs until the server is shut down.
pub async fn serve(
    state: ApiState,
    bind_addr: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "REST API listening");
    axum::serve(listener, app).await?;
    Ok(())
}
