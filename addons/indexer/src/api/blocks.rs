use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;

use super::{ApiContext, Pagination};
use crate::types::*;

#[utoipa::path(get, path = "/api/v1/blocks", params(
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<BlockRow>)))]
pub async fn get_blocks(
    State(ctx): State<ApiContext>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<BlockRow>>, StatusCode> {
    let (offset, limit) = page.clamped();
    ctx.db
        .get_blocks(offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/blocks/height/{height}", params(
    ("height" = u64, Path, description = "Block height"),
), responses((status = 200, body = BlockRow), (status = 404)))]
pub async fn get_block_by_height(
    State(ctx): State<ApiContext>,
    Path(height): Path<u64>,
) -> Result<Json<BlockRow>, StatusCode> {
    ctx.db
        .get_block_by_height(height)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[utoipa::path(get, path = "/api/v1/blocks/{header_id}", params(
    ("header_id" = String, Path, description = "Block header ID (hex)"),
), responses((status = 200, body = BlockRow), (status = 404)))]
pub async fn get_block_by_id(
    State(ctx): State<ApiContext>,
    Path(header_id): Path<String>,
) -> Result<Json<BlockRow>, StatusCode> {
    let id = hex::decode(&header_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    ctx.db
        .get_block_by_id(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[utoipa::path(get, path = "/api/v1/blocks/{header_id}/transactions", params(
    ("header_id" = String, Path, description = "Block header ID (hex)"),
), responses((status = 200, body = Vec<TxRow>)))]
pub async fn get_block_transactions(
    State(ctx): State<ApiContext>,
    Path(header_id): Path<String>,
) -> Result<Json<Vec<TxRow>>, StatusCode> {
    let id = hex::decode(&header_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    ctx.db
        .get_transactions_for_block(&id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
