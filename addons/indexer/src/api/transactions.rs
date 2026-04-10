use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;

use super::{ApiContext, Pagination};
use crate::types::*;

#[utoipa::path(get, path = "/api/v1/transactions/{tx_id}", params(
    ("tx_id" = String, Path, description = "Transaction ID (hex)"),
), responses((status = 200, body = TxRow), (status = 404)))]
pub async fn get_transaction(
    State(ctx): State<ApiContext>,
    Path(tx_id): Path<String>,
) -> Result<Json<TxRow>, StatusCode> {
    let id = hex::decode(&tx_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    ctx.db
        .get_transaction(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[utoipa::path(get, path = "/api/v1/addresses/{address}/transactions", params(
    ("address" = String, Path, description = "Ergo address"),
    ("offset" = Option<u64>, Query, description = "Offset"),
    ("limit" = Option<u64>, Query, description = "Limit (max 100)"),
), responses((status = 200, body = Page<TxRow>)))]
pub async fn get_address_transactions(
    State(ctx): State<ApiContext>,
    Path(address): Path<String>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<TxRow>>, StatusCode> {
    let (offset, limit) = page.clamped();
    ctx.db
        .get_txs_by_address(&address, offset, limit)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
