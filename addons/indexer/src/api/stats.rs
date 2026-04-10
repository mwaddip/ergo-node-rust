use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use super::ApiContext;
use crate::types::*;

#[utoipa::path(get, path = "/api/v1/stats", responses((status = 200, body = NetworkStats)))]
pub async fn get_stats(
    State(ctx): State<ApiContext>,
) -> Result<Json<NetworkStats>, StatusCode> {
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
pub async fn get_info(
    State(ctx): State<ApiContext>,
) -> Result<Json<IndexerInfo>, StatusCode> {
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
