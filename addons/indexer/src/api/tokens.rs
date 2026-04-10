use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;

use super::{ApiContext, Pagination};
use crate::types::*;

#[utoipa::path(get, path = "/api/v1/tokens", params(
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<TokenRow>)))]
pub async fn get_tokens(
    State(ctx): State<ApiContext>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<TokenRow>>, StatusCode> {
    let (offset, limit) = page.clamped();
    ctx.db
        .get_tokens(offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/tokens/{token_id}", params(
    ("token_id" = String, Path, description = "Token ID (hex)"),
), responses((status = 200, body = TokenRow), (status = 404)))]
pub async fn get_token(
    State(ctx): State<ApiContext>,
    Path(token_id): Path<String>,
) -> Result<Json<TokenRow>, StatusCode> {
    let id = hex::decode(&token_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    ctx.db
        .get_token(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[utoipa::path(get, path = "/api/v1/tokens/{token_id}/holders", params(
    ("token_id" = String, Path, description = "Token ID (hex)"),
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<HolderRow>)))]
pub async fn get_token_holders(
    State(ctx): State<ApiContext>,
    Path(token_id): Path<String>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<HolderRow>>, StatusCode> {
    let id = hex::decode(&token_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (offset, limit) = page.clamped();
    ctx.db
        .get_token_holders(&id, offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[utoipa::path(get, path = "/api/v1/tokens/{token_id}/boxes", params(
    ("token_id" = String, Path, description = "Token ID (hex)"),
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<BoxRow>)))]
pub async fn get_token_boxes(
    State(ctx): State<ApiContext>,
    Path(token_id): Path<String>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<BoxRow>>, StatusCode> {
    let id = hex::decode(&token_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (offset, limit) = page.clamped();
    ctx.db
        .get_token_boxes(&id, offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
