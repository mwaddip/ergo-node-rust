use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;

use super::{ApiContext, Pagination};
use crate::types::*;

#[utoipa::path(get, path = "/api/v1/boxes/{box_id}", params(
    ("box_id" = String, Path, description = "Box ID (hex)"),
), responses((status = 200, body = BoxRow), (status = 404)))]
pub async fn get_box_by_id(
    State(ctx): State<ApiContext>,
    Path(box_id): Path<String>,
) -> Result<Json<BoxRow>, StatusCode> {
    let id = hex::decode(&box_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    ctx.db
        .get_box(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[utoipa::path(get, path = "/api/v1/addresses/{address}/balance", params(
    ("address" = String, Path, description = "Ergo address"),
), responses((status = 200, body = Balance)))]
pub async fn get_address_balance(
    State(ctx): State<ApiContext>,
    Path(address): Path<String>,
) -> Result<Json<Balance>, StatusCode> {
    ctx.db
        .get_balance(&address)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/addresses/{address}/unspent", params(
    ("address" = String, Path, description = "Ergo address"),
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<BoxRow>)))]
pub async fn get_address_unspent(
    State(ctx): State<ApiContext>,
    Path(address): Path<String>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<BoxRow>>, StatusCode> {
    let (offset, limit) = page.clamped();
    ctx.db
        .get_unspent_by_address(&address, offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/addresses/{address}/boxes", params(
    ("address" = String, Path, description = "Ergo address"),
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<BoxRow>)))]
pub async fn get_address_boxes(
    State(ctx): State<ApiContext>,
    Path(address): Path<String>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<BoxRow>>, StatusCode> {
    let (offset, limit) = page.clamped();
    ctx.db
        .get_boxes_by_address(&address, offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/ergo-tree/{hash}/unspent", params(
    ("hash" = String, Path, description = "ErgoTree hash (hex, blake2b256)"),
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<BoxRow>)))]
pub async fn get_ergo_tree_unspent(
    State(ctx): State<ApiContext>,
    Path(hash): Path<String>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<BoxRow>>, StatusCode> {
    let hash_bytes = hex::decode(&hash).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (offset, limit) = page.clamped();
    ctx.db
        .get_unspent_by_ergo_tree(&hash_bytes, offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
