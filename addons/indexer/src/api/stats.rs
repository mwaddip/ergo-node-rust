use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use super::ApiContext;
use crate::types::*;

#[utoipa::path(get, path = "/api/v1/stats", responses((status = 200, body = NetworkStats)))]
pub async fn get_stats(State(ctx): State<ApiContext>) -> Result<Json<NetworkStats>, StatusCode> {
    ctx.db
        .get_stats()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
pub struct DailyQuery {
    #[serde(default = "default_days")]
    pub days: u32,
}

fn default_days() -> u32 {
    30
}

#[utoipa::path(get, path = "/api/v1/stats/daily", params(
    ("days" = Option<u32>, Query, description = "Number of days (default 30)"),
), responses((status = 200, body = Vec<DailyStats>)))]
pub async fn get_daily_stats(
    State(ctx): State<ApiContext>,
    Query(params): Query<DailyQuery>,
) -> Result<Json<Vec<DailyStats>>, StatusCode> {
    ctx.db
        .get_daily_stats(params.days.min(365))
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/info", responses((status = 200, body = IndexerInfo)))]
pub async fn get_info(State(ctx): State<ApiContext>) -> Result<Json<IndexerInfo>, StatusCode> {
    let indexed_height = ctx
        .db
        .get_stats()
        .await
        .map(|s| s.indexed_height)
        .unwrap_or(0);

    // Try to get node height — best effort
    let node_height = reqwest::get(format!("{}/info", ctx.node_url))
        .await
        .ok()
        .and_then(|r| {
            if r.status().is_success() {
                Some(r)
            } else {
                None
            }
        });
    let node_h = match node_height {
        Some(resp) => resp
            .json::<NodeInfo>()
            .await
            .map(|i| i.full_height)
            .unwrap_or(0),
        None => 0,
    };

    let backend = if ctx.node_url.contains("postgres") {
        "postgresql"
    } else {
        "sqlite"
    };

    Ok(Json(IndexerInfo {
        indexed_height,
        node_height: node_h,
        backend: backend.to_string(),
        uptime_secs: ctx.start_time.elapsed().as_secs(),
    }))
}

/// Cheap liveness + sync-progress probe. Reads ONLY the in-memory
/// [`crate::health::HealthState`] the sync loop maintains — no DB query, no
/// node call, no lock the sync write-path can hold — so it answers in
/// single-digit milliseconds even mid-rollback and is safe to poll as a
/// heartbeat. Always `200` while the server is alive; lag is reported in the
/// numeric fields, never as a `503`. See `facts/indexer.md`.
#[utoipa::path(get, path = "/api/v1/health", responses((status = 200, body = HealthResponse)))]
pub async fn get_health(State(ctx): State<ApiContext>) -> Json<HealthResponse> {
    let indexed_height = ctx.health.indexed_height();
    let node_height = ctx.health.node_height();
    Json(HealthResponse {
        status: ctx.health.status().to_string(),
        indexed_height,
        node_height,
        behind_by: node_height.saturating_sub(indexed_height),
        last_advance_secs_ago: ctx.health.last_advance_secs_ago(),
        node: ctx.health.node_str().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use crate::api::block_tx_cache::BlockTxCache;
    use crate::api::ApiContext;
    use crate::db::sqlite::SqliteDb;
    use crate::db::IndexerDb;
    use crate::health::HealthState;
    use crate::node_client::NodeClient;
    use crate::types::IndexedBlock;
    use axum::Router;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn temp_db_path() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        std::env::temp_dir()
            .join(format!("ergo-indexer-health-test-{pid}-{nanos}.db"))
            .to_string_lossy()
            .into_owned()
    }

    /// Minimal sequential block with a height-unique header — enough to drive
    /// `insert_block` write load. Carries no transactions.
    fn empty_block(height: u64) -> IndexedBlock {
        let mut header_id = [0u8; 32];
        header_id[..8].copy_from_slice(&height.to_le_bytes());
        IndexedBlock {
            height,
            header_id,
            timestamp: 0,
            difficulty: 1,
            miner_pk: vec![0],
            block_size: 0,
            transactions: vec![],
        }
    }

    /// Build the real router (the same `nest("/api/v1", api_routes())` shape
    /// `serve()` uses) so the test proves `/health` is actually wired, not just
    /// that the handler function exists. Returns the base URL + the DB handle.
    async fn serve_router(health: Arc<HealthState>) -> (String, Arc<dyn IndexerDb>, String) {
        let db_path = temp_db_path();
        let db: Arc<dyn IndexerDb> = Arc::new(SqliteDb::open(&db_path).unwrap());
        let ctx = ApiContext {
            db: db.clone(),
            start_time: Instant::now(),
            node_url: "http://127.0.0.1:1".to_string(),
            node_client: Arc::new(NodeClient::new("http://127.0.0.1:1").unwrap()),
            block_tx_cache: Arc::new(BlockTxCache::new()),
            health,
        };
        let app = Router::new()
            .nest("/api/v1", crate::api::api_routes())
            .with_state(ctx);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), db, db_path)
    }

    #[tokio::test]
    async fn health_endpoint_serves_200_with_contract_json() {
        let health = Arc::new(HealthState::new(true)); // combined mode
        health.record_node_height(1_796_007);
        health.record_indexed_height(1_796_000); // behind by 7
        health.set_node_reachable(true);

        let (base, _db, db_path) = serve_router(health).await;
        let resp = reqwest::get(format!("{base}/api/v1/health")).await.unwrap();
        assert_eq!(resp.status(), 200);

        let raw: serde_json::Value = resp.json().await.unwrap();
        // Exact camelCase keys + values per facts/indexer.md.
        assert_eq!(raw["status"], "ok");
        assert_eq!(raw["indexedHeight"], 1_796_000);
        assert_eq!(raw["nodeHeight"], 1_796_007);
        assert_eq!(raw["behindBy"], 7);
        assert_eq!(raw["node"], "reachable");
        assert_eq!(raw["version"], env!("CARGO_PKG_VERSION"));
        assert!(raw["lastAdvanceSecsAgo"].is_u64());

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn health_endpoint_node_unknown_before_poll_then_resolves() {
        // Combined mode, no poll yet: node must read "unknown" — never assert
        // an "unreachable" we have not observed.
        let health = Arc::new(HealthState::new(true));
        let (base, _db, db_path) = serve_router(health.clone()).await;
        let url = format!("{base}/api/v1/health");

        let raw: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(raw["status"], "ok");
        assert_eq!(raw["node"], "unknown");
        assert_eq!(raw["behindBy"], 0);

        // A failed poll resolves it to "unreachable".
        health.set_node_reachable(false);
        let raw: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(raw["node"], "unreachable");

        // A successful poll resolves it to "reachable".
        health.set_node_reachable(true);
        let raw: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(raw["node"], "reachable");

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn health_endpoint_serve_only_mode_reports_serve_only_status() {
        // sync_present = false → serve-only: status signals the progress fields
        // are not driven by a local sync loop. Still always 200.
        let (base, _db, db_path) = serve_router(Arc::new(HealthState::new(false))).await;
        let resp = reqwest::get(format!("{base}/api/v1/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let raw: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(raw["status"], "serve-only");
        assert_eq!(raw["node"], "unknown"); // no local poll in serve-only
        let _ = std::fs::remove_file(&db_path);
    }

    /// The reason `/health` exists: `/info` hit ~780 ms under reorg-rollback
    /// write transactions. `/health` shares nothing with the DB path, so a
    /// sustained write storm must not slow it. The structural proof (no
    /// `.await`, no DB handle in the handler) is the real guarantee; this is
    /// the empirical guardrail that catches a regression that reintroduces a DB
    /// call (which would spike latency the way `/info` did). Bound is loose on
    /// purpose — it sits well below the unhealthy regime and far above healthy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn health_endpoint_stays_fast_under_db_write_load() {
        let health = Arc::new(HealthState::new(true));
        health.set_node_reachable(true);
        let (base, db, db_path) = serve_router(health).await;
        let url = format!("{base}/api/v1/health");

        // Sustained DB write load on a background task (single writer, as the
        // contract mandates). Capped so a stuck probe can't run it unbounded.
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let db = db.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut h = 1u64;
                while !stop.load(Ordering::Relaxed) && h <= 20_000 {
                    let _ = db.insert_block(&empty_block(h)).await;
                    h += 1;
                }
            })
        };

        let mut worst = Duration::ZERO;
        for _ in 0..50 {
            let t = Instant::now();
            let resp = reqwest::get(&url).await.unwrap();
            let dt = t.elapsed();
            assert_eq!(resp.status(), 200);
            worst = worst.max(dt);
        }
        stop.store(true, Ordering::Relaxed);
        let _ = writer.await;

        println!("worst /health latency under DB write load: {worst:?}");
        assert!(
            worst < Duration::from_millis(250),
            "/health slowed to {worst:?} under DB write load — it must not touch the DB path"
        );

        let _ = std::fs::remove_file(&db_path);
    }
}
