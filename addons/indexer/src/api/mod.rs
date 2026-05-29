pub mod block_tx_cache;
pub mod blocks;
pub mod boxes;
pub mod debug;
pub mod stats;
pub mod tokens;
pub mod transactions;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use serde::Deserialize;
use tokio::sync::watch;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::block_tx_cache::BlockTxCache;
use crate::db::IndexerDb;
use crate::health::HealthState;
use crate::node_client::NodeClient;

#[derive(Clone)]
pub struct ApiContext {
    pub db: Arc<dyn IndexerDb>,
    pub start_time: Instant,
    pub node_url: String,
    pub node_client: Arc<NodeClient>,
    /// Single-flight + cache for per-block transaction fetches, shared across
    /// all `/boxes/{id}/bytes` requests. Collapses the harness's wide
    /// concurrent box-fetch burst into one node fetch per block.
    pub block_tx_cache: Arc<BlockTxCache>,
    /// In-memory liveness + sync-progress state the sync loop updates and the
    /// `/health` handler reads. Lock-free so `/health` never contends with the
    /// sync write-path.
    pub health: Arc<HealthState>,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Ergo Indexer API", version = "1.0.0"),
    paths(
        blocks::get_blocks,
        blocks::get_block_by_height,
        blocks::get_block_by_id,
        blocks::get_block_transactions,
        transactions::get_transaction,
        transactions::get_address_transactions,
        boxes::get_box_by_id,
        boxes::get_box_bytes,
        boxes::get_address_balance,
        boxes::get_address_unspent,
        boxes::get_address_boxes,
        boxes::get_ergo_tree_unspent,
        tokens::get_tokens,
        tokens::get_token,
        tokens::get_token_holders,
        tokens::get_token_boxes,
        stats::get_stats,
        stats::get_daily_stats,
        stats::get_info,
        stats::get_health,
        debug::get_debug_memory,
    ),
    components(schemas(
        crate::types::BlockRow,
        crate::types::TxRow,
        crate::types::BoxRow,
        crate::types::BoxTokenRow,
        crate::types::RegisterRow,
        crate::types::TokenRow,
        crate::types::HolderRow,
        crate::types::Balance,
        crate::types::TokenBalance,
        crate::types::NetworkStats,
        crate::types::DailyStats,
        crate::types::IndexerInfo,
        crate::types::HealthResponse,
        boxes::BoxBytesResponse,
        boxes::BoxBytesError,
        debug::DebugMemory,
        debug::ProcessMemory,
        debug::JemallocMemory,
        debug::ComponentMemory,
    ))
)]
struct ApiDoc;

pub async fn serve(
    db: Arc<dyn IndexerDb>,
    bind: SocketAddr,
    start_time: Instant,
    node_url: String,
    health: Arc<HealthState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let node_client = Arc::new(NodeClient::new(&node_url)?);
    let ctx = ApiContext {
        db,
        start_time,
        node_url,
        node_client,
        block_tx_cache: Arc::new(BlockTxCache::new()),
        health,
    };

    let app = Router::new()
        .merge(SwaggerUi::new("/swagger").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .nest("/api/v1", api_routes())
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "indexer API listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // `changed()` errors only when the sender is dropped. Either
            // branch ends the wait — fall through and let axum drain.
            let _ = shutdown_rx.changed().await;
            tracing::info!("API shutdown signal received; draining in-flight requests");
        })
        .await?;
    tracing::info!("API stopped");
    Ok(())
}

fn api_routes() -> Router<ApiContext> {
    use axum::routing::get;

    Router::new()
        // Blocks
        .route("/blocks", get(blocks::get_blocks))
        .route("/blocks/height/{height}", get(blocks::get_block_by_height))
        .route("/blocks/{header_id}", get(blocks::get_block_by_id))
        .route(
            "/blocks/{header_id}/transactions",
            get(blocks::get_block_transactions),
        )
        // Transactions
        .route("/transactions/{tx_id}", get(transactions::get_transaction))
        .route(
            "/addresses/{address}/transactions",
            get(transactions::get_address_transactions),
        )
        // Boxes
        .route("/boxes/{box_id}", get(boxes::get_box_by_id))
        .route("/boxes/{box_id}/bytes", get(boxes::get_box_bytes))
        .route(
            "/addresses/{address}/balance",
            get(boxes::get_address_balance),
        )
        .route(
            "/addresses/{address}/unspent",
            get(boxes::get_address_unspent),
        )
        .route("/addresses/{address}/boxes", get(boxes::get_address_boxes))
        .route(
            "/ergo-tree/{hash}/unspent",
            get(boxes::get_ergo_tree_unspent),
        )
        // Tokens
        .route("/tokens", get(tokens::get_tokens))
        .route("/tokens/{token_id}", get(tokens::get_token))
        .route("/tokens/{token_id}/holders", get(tokens::get_token_holders))
        .route("/tokens/{token_id}/boxes", get(tokens::get_token_boxes))
        // Stats
        .route("/stats", get(stats::get_stats))
        .route("/stats/daily", get(stats::get_daily_stats))
        .route("/info", get(stats::get_info))
        .route("/health", get(stats::get_health))
        // Debug
        .route("/debug/memory", get(debug::get_debug_memory))
}

#[derive(Deserialize)]
pub struct Pagination {
    #[serde(default = "default_offset")]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_offset() -> u64 {
    0
}
fn default_limit() -> u64 {
    50
}

impl Pagination {
    pub fn clamped(&self) -> (u64, u64) {
        (self.offset, self.limit.min(100))
    }
}
