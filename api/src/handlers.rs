use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use std::sync::Arc;

use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use sigma_ser::ScorexSerializable;

use crate::types::*;
use crate::ApiState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn err<T>(status: StatusCode, reason: impl Into<String>) -> ApiResult<T> {
    Err(api_error(status, reason, None))
}

/// Build an `(StatusCode, Json<ApiError>)` tuple ready for `Err(...)`, `ok_or_else`,
/// or `map_err`. The `error` field mirrors the HTTP status code, matching the JVM
/// node's `ApiError` shape.
fn api_error(
    status: StatusCode,
    reason: impl Into<String>,
    detail: Option<String>,
) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: status.as_u16(),
            reason: reason.into(),
            detail,
        }),
    )
}

fn mining_err() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: 503,
            reason: "mining not configured".into(),
            detail: None,
        }),
    )
}

/// Parse the inner proof_bytes out of a raw AD proofs section.
///
/// Stored format: `[header_id: 32B] [proof_size: VLQ u32] [proof_bytes: proof_size B]`.
/// Returns the proof_bytes slice. None on malformed input.
fn inline_ad_proof_bytes(data: &[u8]) -> Option<&[u8]> {
    use sigma_ser::vlq_encode::ReadSigmaVlqExt;
    if data.len() < 33 {
        return None;
    }
    let mut cursor = std::io::Cursor::new(&data[32..]);
    let proof_size = cursor.get_u32().ok()? as usize;
    let pos = 32 + cursor.position() as usize;
    let end = pos.checked_add(proof_size)?;
    if end > data.len() {
        return None;
    }
    Some(&data[pos..end])
}

/// Compute modifier ID = blake2b256(type_id || header_id || section_root).
/// Matches JVM's `Algos.hash.prefixedHash`.
fn section_modifier_id(type_id: u8, header_id: &[u8; 32], root: &[u8]) -> [u8; 32] {
    use blake2::digest::consts::U32;
    use blake2::{Blake2b, Digest};
    type Blake2b256 = Blake2b<U32>;
    let mut hasher = Blake2b256::new();
    hasher.update([type_id]);
    hasher.update(header_id);
    hasher.update(root);
    hasher.finalize().into()
}

fn hex_to_id(hex_str: &str) -> Result<[u8; 32], (StatusCode, Json<ApiError>)> {
    let bytes = hex::decode(hex_str).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: 400,
                reason: "invalid hex ID".into(),
                detail: None,
            }),
        )
    })?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: 400,
                reason: "ID must be 32 bytes".into(),
                detail: None,
            }),
        )
    })?;
    Ok(arr)
}

/// Constant-time check of the `api_key` request header against the configured hash.
/// When `state.api_key_hash` is `None`, every request passes (unauth mode).
/// When set, requests must supply a header whose Blake2b256 matches.
fn check_api_key(
    state: &ApiState,
    headers: &axum::http::HeaderMap,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let Some(expected) = state.api_key_hash else {
        return Ok(());
    };
    let provided = headers
        .get("api_key")
        .or_else(|| headers.get("api-key"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: 403,
                reason: "api_key required".into(),
                detail: None,
            }),
        ));
    }
    use blake2::digest::consts::U32;
    use blake2::{Blake2b, Digest};
    type Blake2b256 = Blake2b<U32>;
    let mut hasher = Blake2b256::new();
    hasher.update(provided.as_bytes());
    let provided_hash: [u8; 32] = hasher.finalize().into();
    // Constant-time compare to defeat timing oracles.
    if subtle_constant_eq(&provided_hash, &expected) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: 403,
                reason: "invalid api_key".into(),
                detail: None,
            }),
        ))
    }
}

/// Constant-time byte comparison. Bit-OR all diffs, return early-exit-free `==`.
fn subtle_constant_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// GET /info
// ---------------------------------------------------------------------------

pub async fn get_info(State(state): State<ApiState>) -> Json<NodeInfo> {
    let headers_height = state.chain.height();
    let full_height = state
        .validated_height
        .load(std::sync::atomic::Ordering::Relaxed);
    let (tip_id, state_root) = match state.chain.tip() {
        Some(tip) => (
            hex::encode(tip.id.0.as_ref()),
            hex::encode(<[u8; 33]>::from(tip.state_root)),
        ),
        None => (String::new(), String::new()),
    };

    let pool_size = state.mempool.lock().await.len();
    let peers = (state.peer_count)();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Json(NodeInfo {
        name: state.node_info.name.clone(),
        app_version: state.node_info.version.clone(),
        network: state.node_info.network.clone(),
        full_height,
        headers_height,
        downloaded_height: state
            .downloaded_height
            .load(std::sync::atomic::Ordering::Relaxed),
        best_full_header_id: tip_id.clone(),
        best_header_id: tip_id,
        state_root,
        state_type: state.node_info.state_type.clone(),
        peers_count: peers.connected,
        unconfirmed_count: pool_size,
        is_mining: false,
        current_time: now,
        journal_events_version: crate::JOURNAL_EVENTS_VERSION.to_string(),
        stats_version: state
            .stats_enabled
            .then(|| crate::STATS_VERSION.to_string()),
    })
}

// ---------------------------------------------------------------------------
// GET /blocks/at/{height}
// ---------------------------------------------------------------------------

pub async fn get_block_ids_at_height(
    State(state): State<ApiState>,
    Path(height): Path<u32>,
) -> ApiResult<Vec<String>> {
    match state.chain.header_at(height) {
        Some(header) => Ok(Json(vec![hex::encode(header.id.0.as_ref())])),
        None => err(StatusCode::NOT_FOUND, "no block at this height"),
    }
}

// ---------------------------------------------------------------------------
// GET /blocks/{header_id}/header
// ---------------------------------------------------------------------------

pub async fn get_block_header(
    State(state): State<ApiState>,
    Path(header_id): Path<String>,
) -> ApiResult<ergo_chain_types::Header> {
    let id = hex_to_id(&header_id)?;
    match state.chain.header_by_id(&id) {
        Some(header) => Ok(Json(header)),
        None => err(StatusCode::NOT_FOUND, "header not found"),
    }
}

// ---------------------------------------------------------------------------
// GET /blocks/{header_id}/transactions
// ---------------------------------------------------------------------------

/// Block transactions section type ID (Ergo modifier type 102).
const BLOCK_TRANSACTIONS_TYPE: u8 = 102;

pub async fn get_block_transactions(
    State(state): State<ApiState>,
    Path(header_id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let id = match hex_to_id(&header_id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    // Block transactions are keyed by modifier ID = blake2b256(type_id || header_id || tx_root),
    // not by header ID. Compute the modifier ID from the header.
    let Some(header) = state.chain.header_by_id(&id) else {
        return api_error(StatusCode::NOT_FOUND, "header not found", None).into_response();
    };
    let modifier_id = section_modifier_id(
        BLOCK_TRANSACTIONS_TYPE,
        &id,
        header.transaction_root.0.as_ref(),
    );
    let Some(data) = state.store.get(BLOCK_TRANSACTIONS_TYPE, &modifier_id) else {
        return api_error(StatusCode::NOT_FOUND, "transactions not found", None).into_response();
    };

    // A fat block costs ~0.5s of CPU to render, ~75% of it sigma-rust's
    // Transaction::sigma_parse. Running that on the async reactor starved block
    // application under concurrent load (the 2026-05 reactor-starvation
    // incident), so parse+serialize go on the blocking pool. Keep rendering via
    // serde_json::Value so object keys stay byte-for-byte sorted as before.
    let rendered = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let parsed = ergo_validation::parse_block_transactions(&data)
            .map_err(|e| format!("failed to parse stored transactions: {e}"))?;
        let value = serde_json::json!({
            "headerId": header_id,
            "transactions": parsed.transactions,
        });
        serde_json::to_vec(&value).map_err(|e| format!("failed to serialize transactions: {e}"))
    })
    .await;

    match rendered {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Ok(Err(reason)) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, reason, None).into_response()
        }
        Err(join_err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("transaction rendering task failed: {join_err}"),
            None,
        )
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// GET /blocks/lastHeaders/{count}
// ---------------------------------------------------------------------------

pub async fn get_last_headers(
    State(state): State<ApiState>,
    Path(count): Path<u32>,
) -> Json<Vec<ergo_chain_types::Header>> {
    let count = count.min(100);
    let height = state.chain.height();
    let mut headers = Vec::with_capacity(count as usize);
    for h in (1..=height).rev().take(count as usize) {
        if let Some(header) = state.chain.header_at(h) {
            headers.push(header);
        }
    }
    Json(headers)
}

// ---------------------------------------------------------------------------
// POST /transactions
// ---------------------------------------------------------------------------

pub async fn post_transaction(
    State(state): State<ApiState>,
    Json(tx): Json<ergo_validation::Transaction>,
) -> ApiResult<String> {
    process_transaction(state, tx, true).await
}

// ---------------------------------------------------------------------------
// POST /transactions/check
// ---------------------------------------------------------------------------

pub async fn check_transaction(
    State(state): State<ApiState>,
    Json(tx): Json<ergo_validation::Transaction>,
) -> ApiResult<String> {
    process_transaction(state, tx, false).await
}

async fn process_transaction(
    state: ApiState,
    tx: ergo_validation::Transaction,
    add_to_pool: bool,
) -> ApiResult<String> {
    let ctx_guard = state.state_context.read().await;
    let ctx = match ctx_guard.as_ref() {
        Some(c) => c,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "node is syncing, cannot validate transactions yet",
            )
        }
    };

    // Compute tx_id hex string
    let tx_id_hex = String::from(tx.id());

    // Sigma-serialize for mempool storage (P2P wire format)
    let tx_bytes = match tx.sigma_serialize_bytes() {
        Ok(b) => b,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("transaction serialization failed: {e}"),
            )
        }
    };

    if !add_to_pool {
        // Check-only: validate inputs exist and scripts pass, but don't add to pool.
        let mut input_boxes = Vec::with_capacity(tx.inputs.len());
        for input in tx.inputs.iter() {
            let box_id_bytes: [u8; 32] = input.box_id.as_ref().try_into().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: 400,
                        reason: "input box_id must be 32 bytes".into(),
                        detail: None,
                    }),
                )
            })?;
            match state.utxo_reader.box_by_id(&box_id_bytes) {
                Some(b) => input_boxes.push(b),
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        format!("input box not found: {}", hex::encode(box_id_bytes)),
                    )
                }
            }
        }
        let mut data_boxes = Vec::new();
        if let Some(ref data_inputs) = tx.data_inputs {
            for di in data_inputs.iter() {
                let box_id_bytes: [u8; 32] = di.box_id.as_ref().try_into().map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ApiError {
                            error: 400,
                            reason: "data-input box_id must be 32 bytes".into(),
                            detail: None,
                        }),
                    )
                })?;
                match state.utxo_reader.box_by_id(&box_id_bytes) {
                    Some(b) => data_boxes.push(b),
                    None => {
                        return err(
                            StatusCode::BAD_REQUEST,
                            format!("data-input box not found: {}", hex::encode(box_id_bytes)),
                        )
                    }
                }
            }
        }

        if let Err(e) =
            ergo_validation::validate_single_transaction(&tx, input_boxes, data_boxes, ctx)
        {
            return err(StatusCode::BAD_REQUEST, format!("{e}"));
        }
        drop(ctx_guard);
        return Ok(Json(tx_id_hex));
    }

    // Full submission: validate + add to mempool
    let mut pool = state.mempool.lock().await;
    let outcome = pool.process(
        tx,
        tx_bytes,
        &UtxoReaderAdapter {
            utxo_reader: &*state.utxo_reader,
        },
        ctx,
        None, // local submission — bypass rate limiting
    );
    drop(ctx_guard);

    match outcome {
        ergo_mempool::types::ProcessingOutcome::Accepted { .. }
        | ergo_mempool::types::ProcessingOutcome::Replaced { .. }
        | ergo_mempool::types::ProcessingOutcome::AlreadyInPool => Ok(Json(tx_id_hex)),
        ergo_mempool::types::ProcessingOutcome::Invalidated { reason } => {
            err(StatusCode::BAD_REQUEST, reason)
        }
        ergo_mempool::types::ProcessingOutcome::Declined { reason } => {
            err(StatusCode::BAD_REQUEST, reason)
        }
        ergo_mempool::types::ProcessingOutcome::DoubleSpendLoser { .. } => err(
            StatusCode::BAD_REQUEST,
            "double-spend conflict with higher-fee transaction",
        ),
    }
}

/// UtxoReader adapter for mempool validation from API context.
struct UtxoReaderAdapter<'a> {
    utxo_reader: &'a dyn crate::UtxoAccess,
}

impl ergo_mempool::types::UtxoReader for UtxoReaderAdapter<'_> {
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ergo_validation::ErgoBox> {
        self.utxo_reader.box_by_id(box_id)
    }
}

// ---------------------------------------------------------------------------
// GET /transactions/unconfirmed
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

pub async fn get_unconfirmed(
    State(state): State<ApiState>,
    Query(params): Query<PaginationParams>,
) -> Json<Vec<serde_json::Value>> {
    let limit = params.limit.min(100);
    let offset = params.offset.min(100_000);
    let pool = state.mempool.lock().await;
    let txs: Vec<_> = pool
        .all_prioritized()
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|utx| match serde_json::to_value(&utx.tx) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, tx_id = %hex::encode(utx.tx.id().0.0), "unconfirmed_transactions: serde failed; tx omitted");
                None
            }
        })
        .collect();
    Json(txs)
}

// ---------------------------------------------------------------------------
// GET /transactions/unconfirmed/transactionIds
// ---------------------------------------------------------------------------

pub async fn get_unconfirmed_ids(State(state): State<ApiState>) -> Json<Vec<String>> {
    let pool = state.mempool.lock().await;
    let ids: Vec<String> = pool.tx_ids().into_iter().map(hex::encode).collect();
    Json(ids)
}

// ---------------------------------------------------------------------------
// GET /transactions/unconfirmed/byTransactionId/{tx_id}
// ---------------------------------------------------------------------------

pub async fn get_unconfirmed_by_id(
    State(state): State<ApiState>,
    Path(tx_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let id = hex_to_id(&tx_id)?;
    let pool = state.mempool.lock().await;
    match pool.get(&id) {
        Some(utx) => match serde_json::to_value(&utx.tx) {
            Ok(v) => Ok(Json(v)),
            Err(e) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialization failed: {e}"),
            ),
        },
        None => err(StatusCode::NOT_FOUND, "transaction not in mempool"),
    }
}

// ---------------------------------------------------------------------------
// GET /transactions/getFee
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FeeParams {
    #[serde(default = "default_wait_time")]
    #[serde(rename = "waitTime")]
    wait_time: u64,
    #[serde(default = "default_tx_size")]
    #[serde(rename = "txSize")]
    tx_size: usize,
}

fn default_wait_time() -> u64 {
    1
}
fn default_tx_size() -> usize {
    100
}

pub async fn get_recommended_fee(
    State(state): State<ApiState>,
    Query(params): Query<FeeParams>,
) -> ApiResult<FeeResponse> {
    let pool = state.mempool.lock().await;
    // Convert wait_time from blocks to approximate milliseconds
    // (Ergo target block time ~2 minutes = 120_000ms)
    let wait_time = params.wait_time.min(100);
    let tx_size = params.tx_size.min(1_000_000);
    let wait_ms = wait_time.saturating_mul(120_000);
    match pool.recommended_fee(wait_ms, tx_size) {
        Some(fee) => Ok(Json(FeeResponse { fee })),
        None => err(StatusCode::BAD_REQUEST, "insufficient fee history"),
    }
}

// ---------------------------------------------------------------------------
// GET /transactions/poolHistogram
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct HistogramParams {
    #[serde(default = "default_bins")]
    bins: usize,
}

fn default_bins() -> usize {
    10
}

pub async fn get_pool_histogram(
    State(state): State<ApiState>,
    Query(params): Query<HistogramParams>,
) -> Json<serde_json::Value> {
    let bins = params.bins.min(50);
    let pool = state.mempool.lock().await;
    let histogram = pool.fee_histogram(bins);
    Json(serde_json::to_value(histogram).unwrap_or(serde_json::Value::Array(vec![])))
}

// ---------------------------------------------------------------------------
// GET /utxo/byId/{box_id}
// ---------------------------------------------------------------------------

pub async fn get_utxo_by_id(
    State(state): State<ApiState>,
    Path(box_id): Path<String>,
) -> ApiResult<ergo_validation::ErgoBox> {
    let id = hex_to_id(&box_id)?;
    match state.utxo_reader.box_by_id(&id) {
        Some(ergo_box) => Ok(Json(ergo_box)),
        None => err(StatusCode::NOT_FOUND, "box not found in UTXO set"),
    }
}

// ---------------------------------------------------------------------------
// GET /utxo/withPool/byId/{box_id}
// ---------------------------------------------------------------------------

pub async fn get_utxo_with_pool(
    State(state): State<ApiState>,
    Path(box_id): Path<String>,
) -> ApiResult<ergo_validation::ErgoBox> {
    let id = hex_to_id(&box_id)?;
    // Check confirmed UTXO set first
    if let Some(ergo_box) = state.utxo_reader.box_by_id(&id) {
        return Ok(Json(ergo_box));
    }
    // Check mempool outputs
    let pool = state.mempool.lock().await;
    match pool.unconfirmed_box(&id) {
        Some(ergo_box) => Ok(Json(ergo_box.clone())),
        None => err(StatusCode::NOT_FOUND, "box not found"),
    }
}

// ---------------------------------------------------------------------------
// GET /peers/connected
// ---------------------------------------------------------------------------

pub async fn get_connected_peers(State(state): State<ApiState>) -> Json<Vec<PeerInfoEntry>> {
    let peers = (state.peer_all)();
    Json(
        peers
            .into_iter()
            .filter(|p| p.connection_type.is_some())
            .map(peer_info_to_entry)
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// GET /peers/api-urls
// ---------------------------------------------------------------------------

pub async fn get_peer_api_urls(State(state): State<ApiState>) -> Json<Vec<PeerApiUrl>> {
    let peers = (state.peer_api_urls)();
    let mut urls = Vec::new();
    for info in peers {
        if let Some(ref url_str) = info.rest_url {
            // Security: only expose URLs whose hostname matches the peer's socket IP.
            if url_host_matches_addr(url_str, &info.addr) {
                urls.push(PeerApiUrl {
                    peer_id: info.peer_id,
                    url: url_str.clone(),
                });
            }
        }
    }
    Json(urls)
}

/// Check that a URL's host component matches a socket address IP.
/// Rejects peers that advertise URLs pointing to a different host.
fn url_host_matches_addr(url: &str, addr: &std::net::SocketAddr) -> bool {
    // Parse "http://1.2.3.4:9053" or "http://[::1]:9053" — extract host between :// and next : or /
    let Some(after_scheme) = url.split("://").nth(1) else {
        return false;
    };
    let host_part = after_scheme.split('/').next().unwrap_or(after_scheme);

    // Handle IPv6 bracket notation: [::1]:9053
    let host = if host_part.starts_with('[') {
        host_part
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        // IPv4: strip port
        host_part.split(':').next().unwrap_or(host_part)
    };

    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip == addr.ip(),
        Err(_) => false, // DNS names rejected — IP only
    }
}

// ---------------------------------------------------------------------------
// POST /ingest/modifiers
// ---------------------------------------------------------------------------

pub async fn post_ingest_modifiers(
    axum::extract::ConnectInfo(remote): axum::extract::ConnectInfo<std::net::SocketAddr>,
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> ApiResult<serde_json::Value> {
    // Localhost-only: reject requests from non-loopback addresses
    if !remote.ip().is_loopback() {
        return err(StatusCode::FORBIDDEN, "ingest endpoint is localhost-only");
    }

    let Some(ref tx) = state.modifier_tx else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "modifier pipeline not available",
        );
    };

    // Parse body: sequence of (type_id: u8, modifier_id: [u8; 32], data_len: u32 BE, data: [u8])
    let mut cursor = 0;
    let mut count = 0u32;
    while cursor < body.len() {
        // type_id (1) + modifier_id (32) + data_len (4) = 37 bytes minimum
        if cursor + 37 > body.len() {
            return err(
                StatusCode::BAD_REQUEST,
                format!("truncated modifier header at offset {cursor}"),
            );
        }

        let type_id = body[cursor];
        cursor += 1;

        let mut id = [0u8; 32];
        id.copy_from_slice(&body[cursor..cursor + 32]);
        cursor += 32;

        let data_len = u32::from_be_bytes(body[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;

        if cursor + data_len > body.len() {
            return err(
                StatusCode::BAD_REQUEST,
                format!("truncated modifier data at offset {cursor}: need {data_len} bytes"),
            );
        }

        let data = body[cursor..cursor + data_len].to_vec();
        cursor += data_len;

        if tx.try_send((type_id, id, data, None)).is_err() {
            return err(StatusCode::SERVICE_UNAVAILABLE, "pipeline channel full");
        }
        count += 1;
    }

    tracing::info!(count, bytes = body.len(), "ingested modifiers via REST");
    Ok(Json(serde_json::json!({ "accepted": count })))
}

// ---------------------------------------------------------------------------
// GET /emission/at/{height}
// ---------------------------------------------------------------------------

/// Ergo emission ends around height 2,080,800. Cap to prevent DoS via
/// unbounded iteration (the loop is O(height)).
const MAX_EMISSION_HEIGHT: u32 = 2_100_000;

pub async fn get_emission_at(Path(height): Path<u32>) -> ApiResult<EmissionInfo> {
    use ergo_lib::chain::emission::{EmissionRules, MonetarySettings};

    if height > MAX_EMISSION_HEIGHT {
        return err(
            StatusCode::BAD_REQUEST,
            format!("height exceeds max ({MAX_EMISSION_HEIGHT})"),
        );
    }

    let settings = MonetarySettings::default();
    let rules = EmissionRules::new(settings);

    let h = height as i64;
    let reward = rules.emission_at_height(h);
    let mut total_issued: i64 = 0;
    for i in 0..=h {
        total_issued += rules.emission_at_height(i);
    }
    let total_supply = rules.coins_total();

    Ok(Json(EmissionInfo {
        miner_reward: reward.max(0) as u64,
        total_coins_issued: total_issued.max(0) as u64,
        total_remain_coins: (total_supply - total_issued).max(0) as u64,
    }))
}

// ---------------------------------------------------------------------------
// GET /nipopow/proof/{m}/{k}
// GET /nipopow/proof/{m}/{k}/{header_id}
// ---------------------------------------------------------------------------

pub async fn get_nipopow_proof(
    State(state): State<ApiState>,
    Path((m, k)): Path<(u32, u32)>,
) -> ApiResult<serde_json::Value> {
    nipopow_proof_response(Arc::clone(&state.chain), m, k, None).await
}

pub async fn get_nipopow_proof_by_header(
    State(state): State<ApiState>,
    Path((m, k, header_id)): Path<(u32, u32, String)>,
) -> ApiResult<serde_json::Value> {
    let id = hex_to_id(&header_id)?;
    // Surface "unknown header_id" as 404 before kicking off the (potentially
    // expensive) proof construction. The chain-side `build_nipopow_proof`
    // returns `MissingPopowHeader` for this case but doesn't distinguish it
    // from other reader failures, so we filter it out here at the boundary.
    if state.chain.header_by_id(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "header not found");
    }
    nipopow_proof_response(Arc::clone(&state.chain), m, k, Some(id)).await
}

async fn nipopow_proof_response(
    chain: Arc<dyn crate::ChainAccess>,
    m: u32,
    k: u32,
    header_id: Option<[u8; 32]>,
) -> ApiResult<serde_json::Value> {
    // Proof construction walks the interlink hierarchy and can take tens of
    // ms on a long chain. Move it off the async runtime.
    let bytes =
        match tokio::task::spawn_blocking(move || chain.build_nipopow_proof(m, k, header_id)).await
        {
            Ok(Ok(b)) => b,
            Ok(Err(reason)) => return err(StatusCode::BAD_REQUEST, reason),
            Err(join_err) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("nipopow build task panicked: {join_err}"),
                )
            }
        };

    let proof = ergo_nipopow::NipopowProof::scorex_parse_bytes(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: "failed to parse just-built proof".into(),
                detail: Some(format!("{e:?}")),
            }),
        )
    })?;

    // sigma-rust's NipopowProof serde shape matches the JVM encoder for every
    // field EXCEPT `continuous`, which the JVM emits but the Rust struct
    // doesn't carry. We flatten the derived serialization and add the
    // missing field. `continuous` is always `false` for this release — the
    // chain-side proof builder never produces continuous-mode proofs (see
    // `facts/nipopow.md`).
    #[derive(serde::Serialize)]
    struct JvmCompatProof<'a> {
        #[serde(flatten)]
        proof: &'a ergo_nipopow::NipopowProof,
        continuous: bool,
    }

    let value = serde_json::to_value(JvmCompatProof {
        proof: &proof,
        continuous: false,
    })
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: "failed to serialize nipopow proof".into(),
                detail: Some(format!("{e}")),
            }),
        )
    })?;

    Ok(Json(value))
}

// ---------------------------------------------------------------------------
// GET /mining/candidate
// ---------------------------------------------------------------------------

pub async fn get_mining_candidate(
    State(state): State<ApiState>,
) -> ApiResult<ergo_mining::WorkMessage> {
    let mining = state.mining.as_ref().ok_or_else(mining_err)?;

    // Query by VALIDATED height, NOT the header-chain tip. The mining task
    // builds the candidate on the validated tip's state root and keys the
    // cache to `validated_height` (the same Arc<AtomicU32> ApiState holds).
    // The header chain leads validation (`full < headers`) every time a new
    // header arrives before its body validates — querying `chain.height()`
    // there fails `cached.tip_height == current_tip_height` and returns 503
    // even though a valid candidate for the validated tip is cached.
    // (facts/mining.md — "GET /mining/candidate", height-source paragraph.)
    let tip_height = state.validated_height.load(std::sync::atomic::Ordering::Relaxed);
    match mining.cached_work(tip_height) {
        Some(work) => Ok(Json(work)),
        None => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no candidate available — node may still be syncing",
        ),
    }
}

// ---------------------------------------------------------------------------
// GET /mining/rewardAddress
// ---------------------------------------------------------------------------

pub async fn get_mining_reward_address(
    State(state): State<ApiState>,
) -> ApiResult<serde_json::Value> {
    use ergo_lib::chain::ergo_tree_predef;
    use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};

    let mining = state.mining.as_ref().ok_or_else(mining_err)?;

    // JVM Pay2SAddress(rewardOutputScript(minerRewardDelay, pk)).
    // The miner reward is locked behind a time-delay script, not a bare P2PK —
    // so the surfaced address is P2S over that script, not P2PK over the PK.
    let tree = ergo_tree_predef::reward_output_script(
        mining.config.reward_delay,
        mining.config.miner_pk.clone(),
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: format!("failed to build reward output script: {e}"),
                detail: None,
            }),
        )
    })?;
    let tree_bytes = tree.sigma_serialize_bytes().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: format!("failed to serialize reward output script: {e}"),
                detail: None,
            }),
        )
    })?;
    let prefix = match state.node_info.network.as_str() {
        "testnet" => NetworkPrefix::Testnet,
        _ => NetworkPrefix::Mainnet,
    };
    let address_str = AddressEncoder::new(prefix).address_to_str(&Address::P2S(tree_bytes));

    Ok(Json(serde_json::json!({
        "rewardAddress": address_str,
    })))
}

// ---------------------------------------------------------------------------
// POST /mining/solution
// ---------------------------------------------------------------------------

/// Solution submission from miners. For Autolykos v2, only the 8-byte nonce
/// is required. The miner_pk and other fields are optional (defaults apply).
#[derive(Deserialize)]
pub struct SolutionSubmission {
    /// 8-byte nonce as hex string.
    pub n: String,
}

pub async fn post_mining_solution(
    State(state): State<ApiState>,
    Json(submission): Json<SolutionSubmission>,
) -> ApiResult<serde_json::Value> {
    let mining = state.mining.as_ref().ok_or_else(mining_err)?;

    // Solved latch (contract step 2): an accepted solution is still awaiting
    // block application. Both candidate slots are by construction at the
    // latched height — accepting another solution would submit a
    // self-competing block. Checked before any other work: cheapest rejection.
    if mining.solved_pending() {
        return err(
            StatusCode::BAD_REQUEST,
            "solution already accepted, awaiting block application",
        );
    }

    // Parse nonce
    let nonce = hex::decode(&submission.n).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: 400,
                reason: "invalid nonce hex".into(),
                detail: None,
            }),
        )
    })?;
    if nonce.len() != 8 {
        return err(StatusCode::BAD_REQUEST, "nonce must be exactly 8 bytes");
    }

    // Collect candidates to try: current first, then previous (so a GPU
    // miner solving the old candidate while a regen occurred still works).
    // Skip any that no longer build on the chain tip (contract step 3) —
    // `on_block_applied` drops those, but a peer block can land in the
    // window between application and the hook running.
    let tip_id = state.chain.tip().map(|h| h.id);
    let mut candidates = Vec::with_capacity(2);
    let mut skipped_stale = false;
    for slot in [mining.cached_block(), mining.previous_block()] {
        let Some(candidate) = slot else { continue };
        if tip_id == Some(candidate.parent.id) {
            candidates.push(candidate);
        } else {
            skipped_stale = true;
        }
    }
    if candidates.is_empty() {
        // Distinct messages: "stale" tells the miner the node moved on
        // (a new block applied since the work was fetched); "no current"
        // means no candidate was ever generated. Same remedy — refetch —
        // but the diagnostic difference matters when debugging a miner.
        return err(
            StatusCode::BAD_REQUEST,
            if skipped_stale {
                "stale candidate — chain tip advanced; refetch /mining/candidate"
            } else {
                "no current candidate — fetch /mining/candidate first"
            },
        );
    }

    // Build Autolykos v2 solution (miner_pk not used in PoW calc, use configured pk)
    let solution = ergo_chain_types::AutolykosSolution {
        miner_pk: Box::new(*mining.config.miner_pk.h),
        pow_onetime_pk: None,
        nonce,
        pow_distance: None,
    };

    // Try each candidate — the solution may match the current or previous one.
    let mut last_err = None;
    let mut matched = None;
    for candidate in candidates {
        match ergo_mining::solution::validate_solution(&candidate, solution.clone()) {
            Ok(h) => {
                matched = Some((h, candidate));
                break;
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    let (header, candidate) = matched.ok_or_else(|| {
        let e = last_err.unwrap();
        match e {
            ergo_mining::MiningError::InvalidSolution(msg) => (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: 400,
                    reason: format!("invalid solution: {msg}"),
                    detail: None,
                }),
            ),
            other => {
                tracing::error!("solution validation error: {other}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError {
                        error: 500,
                        reason: "solution validation failed".into(),
                        detail: Some(other.to_string()),
                    }),
                )
            }
        }
    })?;

    // Serialize block sections to wire format for submission
    let mut header_id = [0u8; 32];
    header_id.copy_from_slice(header.id.0.as_ref());

    let block_txs_bytes = ergo_validation::serialize_block_transactions(
        &header_id,
        candidate.version as u32,
        &candidate.transactions,
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: format!("block transactions serialize: {e}"),
                detail: None,
            }),
        )
    })?;

    let ad_proofs_bytes =
        ergo_validation::serialize_ad_proofs(&header_id, &candidate.ad_proof_bytes);

    let extension_bytes =
        ergo_validation::serialize_extension(&header_id, &candidate.extension.fields).map_err(
            |e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError {
                        error: 500,
                        reason: format!("extension serialize: {e}"),
                        detail: None,
                    }),
                )
            },
        )?;

    // Claim the latch (contract step 7) BEFORE the submitter hand-off —
    // after submission, rejecting the race loser is too late: its block has
    // already left the node. The `solved_pending()` check at the top is the
    // cheap fast path; this atomic claim is the gate that counts (handlers
    // run concurrently; the JVM actor serializes, axum does not). The
    // candidate slots stay as-is — block application runs `on_block_applied`,
    // which drops the stale slots and clears the latch in one place. An
    // `invalidate()` here would move the just-solved candidate to
    // `previous`, leaving it re-solvable during the accept→apply window.
    if !mining.try_mark_solved(header.id, header.height) {
        return err(
            StatusCode::BAD_REQUEST,
            "solution already accepted, awaiting block application",
        );
    }

    // Submit to the local pipeline + P2P broadcast. Both failure paths
    // release the just-claimed latch (contract steps 8-9): the block never
    // left the node, and a held latch would wedge solution acceptance until
    // the next applied block.
    if let Some(ref submitter) = state.block_submitter {
        submitter
            .submit(header, block_txs_bytes, ad_proofs_bytes, extension_bytes)
            .map_err(|e| {
                mining.clear_solved();
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError {
                        error: 500,
                        reason: format!("block submission failed: {e}"),
                        detail: None,
                    }),
                )
            })?;
    } else {
        mining.clear_solved();
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "block submitter not configured",
        );
    }

    tracing::info!("valid mining solution accepted, block submitted");
    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

// ---------------------------------------------------------------------------
// GET /info/wait?after={height}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct WaitQuery {
    after: u32,
}

pub async fn info_wait(
    State(state): State<ApiState>,
    Query(params): Query<WaitQuery>,
) -> Result<Json<crate::types::NodeInfo>, StatusCode> {
    let current = state
        .validated_height
        .load(std::sync::atomic::Ordering::Relaxed);
    if current > params.after {
        return Ok(get_info(State(state)).await);
    }

    // Loop until the requested height is actually exceeded, or the deadline
    // fires. `rx.changed()` wakes on every validated block, which during a
    // long resync fires ~10 Hz — returning on every wake would hot-loop the
    // caller while our current height is still far below `after`.
    let mut rx = state.height_watch.clone();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = rx.changed() => {
                if *rx.borrow() > params.after {
                    return Ok(get_info(State(state)).await);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(StatusCode::NO_CONTENT);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GET /debug/memory
// ---------------------------------------------------------------------------

/// Average bytes per header in the in-memory chain. Real headers vary with
/// interlink vector size; 800 is a coarse working estimate. Good enough to
/// know whether chain is 0.5 GB or 2 GB of the total — don't use for anything
/// that requires precision.
const AVG_HEADER_BYTES: u64 = 800;

pub async fn get_debug_memory(State(state): State<ApiState>) -> Json<DebugMemory> {
    // Process memory from /proc/self/status. Fall back to zeros if any field
    // is missing — the endpoint is diagnostic, not mission-critical.
    let process = read_proc_memory();

    // Jemalloc stats: only when the main crate wired a probe.
    let jemalloc = state.jemalloc_probe.as_ref().map(|p| {
        let s = p();
        JemallocMemory {
            allocated_bytes: s.allocated,
            active_bytes: s.active,
            resident_bytes: s.resident,
            retained_bytes: s.retained,
            metadata_bytes: s.metadata,
        }
    });

    let chain_header_count = state.chain.height();
    let mempool_tx_count = state.mempool.lock().await.len() as u32;

    let components = ComponentMemory {
        chain_header_estimate_bytes: chain_header_count as u64 * AVG_HEADER_BYTES,
        chain_header_count,
        mempool_tx_count,
    };

    Json(DebugMemory {
        process,
        jemalloc,
        components,
    })
}

// ---------------------------------------------------------------------------
// GET /debug/p2p-capture/info
// ---------------------------------------------------------------------------

/// Inspect the p2p capture ring without dumping it.
///
/// Returns the full `CaptureInfo` JSON when capture is enabled; returns
/// `{"enabled": false}` (200 OK) when the operator has not configured
/// `[debug.p2p_capture]`. The 200-when-disabled shape is deliberate:
/// `/info` is a status probe, so an operator running it on a node that
/// happens to have the feature off should get a clear "off" answer, not
/// a 404 that suggests the endpoint doesn't exist.
pub async fn get_capture_info(State(state): State<ApiState>) -> Json<serde_json::Value> {
    match &state.capture {
        Some(c) => {
            Json(serde_json::to_value(c.info()).expect("CaptureInfo Serialize is infallible"))
        }
        None => Json(serde_json::json!({ "enabled": false })),
    }
}

// ---------------------------------------------------------------------------
// GET /debug/p2p-capture/dump
// ---------------------------------------------------------------------------

/// Query params for `/debug/p2p-capture/dump`. All optional, combine
/// with AND semantics in the underlying `DumpFilter`.
#[derive(Debug, Default, Deserialize)]
pub struct CaptureDumpQuery {
    /// IPv4 or IPv6 literal. Bad parse → 400.
    pub peer: Option<String>,
    /// Keep records with `ts_sec` within the last N seconds.
    pub since_secs: Option<u64>,
    /// "inbound" or "outbound"; anything else → 400.
    pub direction: Option<String>,
}

/// Stream the capture ring as a pcap file (chronological, full snapshot).
///
/// 404 when capture is disabled — `/dump` is an operation that requires the
/// subsystem to be on, in contrast to `/info`'s probe-friendly 200.
pub async fn get_capture_dump(
    State(state): State<ApiState>,
    Query(query): Query<CaptureDumpQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let capture = match &state.capture {
        Some(c) => c,
        None => return capture_error_response(StatusCode::NOT_FOUND, "capture-disabled", None),
    };

    let mut filter = enr_p2p::capture::DumpFilter::default();
    if let Some(peer_str) = &query.peer {
        match peer_str.parse::<std::net::IpAddr>() {
            Ok(ip) => filter.peer = Some(ip),
            Err(_) => {
                return capture_error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid-peer",
                    Some("peer must be a valid IP address".into()),
                );
            }
        }
    }
    filter.since_secs = query.since_secs;
    filter.direction = match query.direction.as_deref() {
        None => None,
        Some("inbound") => Some(enr_p2p::capture::Direction::Inbound),
        Some("outbound") => Some(enr_p2p::capture::Direction::Outbound),
        Some(other) => {
            return capture_error_response(
                StatusCode::BAD_REQUEST,
                "invalid-direction",
                Some(format!("direction must be inbound|outbound (got {other})")),
            );
        }
    };

    let pcap_bytes = capture.dump(&filter);
    let filename = capture_dump_filename(std::time::SystemTime::now());

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/vnd.tcpdump.pcap".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        pcap_bytes,
    )
        .into_response()
}

fn capture_error_response(
    status: StatusCode,
    reason: &str,
    detail: Option<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    api_error(status, reason, detail).into_response()
}

/// Build `p2p-capture-YYYYMMDDTHHMMSSZ.pcap` from `now`.
///
/// Inline Y-M-D math to avoid pulling chrono just for a filename
/// stamp; same Howard Hinnant calendar algorithm `enr_p2p::capture::handle`
/// uses for its ISO formatting.
fn capture_dump_filename(now: std::time::SystemTime) -> String {
    let dur = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let tod = secs % 86400;
    let h = tod / 3600;
    let m = (tod / 60) % 60;
    let s = tod % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!(
        "p2p-capture-{:04}{:02}{:02}T{:02}{:02}{:02}Z.pcap",
        y, mo, d, h, m, s
    )
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// POST /debug/p2p-capture/reset
// ---------------------------------------------------------------------------

/// Reset the capture ring's write head and bump generation.
///
/// 404 when capture is disabled. Returns `{reset, previous_generation,
/// current_generation}`. We snapshot `generation` via `info()` both
/// before and after the call so the response shows the exact pair the
/// trait observed — cheap because post-reset the ring is empty and the
/// chronological walk inside `info()` exits immediately.
pub async fn post_capture_reset(State(state): State<ApiState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let capture = match &state.capture {
        Some(c) => c,
        None => return capture_error_response(StatusCode::NOT_FOUND, "capture-disabled", None),
    };
    let previous_generation = capture.info().generation;
    capture.reset();
    let current_generation = capture.info().generation;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "reset": true,
            "previous_generation": previous_generation,
            "current_generation": current_generation,
        })),
    )
        .into_response()
}

/// Read anon/file/peak RSS and VmSize from `/proc/self/status`, PSS from
/// `/proc/self/smaps_rollup`. All fields return 0 on parse failure.
fn read_proc_memory() -> ProcessMemory {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut rss_anon = 0u64;
    let mut rss_file = 0u64;
    let mut rss_shmem = 0u64;
    let mut rss_peak = 0u64;
    let mut vm_size = 0u64;
    for line in status.lines() {
        let (key, rest) = match line.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        let kb = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let bytes = kb * 1024;
        match key {
            "RssAnon" => rss_anon = bytes,
            "RssFile" => rss_file = bytes,
            "RssShmem" => rss_shmem = bytes,
            "VmHWM" => rss_peak = bytes,
            "VmSize" => vm_size = bytes,
            _ => {}
        }
    }
    let rss_total = rss_anon + rss_file + rss_shmem;

    // smaps_rollup is one line of aggregate PSS — not always readable (requires
    // CAP_SYS_PTRACE or ownership). Best-effort.
    let pss_bytes = std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Pss:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        });

    ProcessMemory {
        rss_anon_bytes: rss_anon,
        rss_file_bytes: rss_file,
        rss_total_bytes: rss_total,
        rss_peak_bytes: rss_peak,
        vm_size_bytes: vm_size,
        pss_bytes,
    }
}

// ---------------------------------------------------------------------------
// GET /blocks?offset=0&limit=50
// ---------------------------------------------------------------------------

pub async fn get_blocks(
    State(state): State<ApiState>,
    Query(params): Query<PaginationParams>,
) -> Json<Vec<String>> {
    let limit = params.limit.min(100) as u32;
    let offset = params.offset.min(u32::MAX as usize) as u32;
    let ids = state.chain.header_ids(offset, limit);
    Json(ids.iter().map(hex::encode).collect())
}

// ---------------------------------------------------------------------------
// GET /blocks/{header_id}  — full block (header + transactions + adProofs + extension)
// ---------------------------------------------------------------------------

/// Block sections modifier type IDs.
const AD_PROOFS_TYPE: u8 = 104;
const EXTENSION_TYPE: u8 = 108;
const HEADER_TYPE: u8 = 101;

pub async fn get_full_block(
    State(state): State<ApiState>,
    Path(header_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let id = hex_to_id(&header_id)?;
    let header = match state.chain.header_by_id(&id) {
        Some(h) => h,
        None => return err(StatusCode::NOT_FOUND, "header not found"),
    };
    let header_id_hex = hex::encode(id);

    // Block transactions: keyed by blake2b256(102 || header_id || transaction_root).
    let txs_modifier_id = section_modifier_id(
        BLOCK_TRANSACTIONS_TYPE,
        &id,
        header.transaction_root.0.as_ref(),
    );
    let txs_value = match state.store.get(BLOCK_TRANSACTIONS_TYPE, &txs_modifier_id) {
        Some(data) => {
            let parsed = ergo_validation::parse_block_transactions(&data).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError {
                        error: 500,
                        reason: format!("failed to parse stored transactions: {e}"),
                        detail: None,
                    }),
                )
            })?;
            serde_json::json!({
                "headerId": header_id_hex,
                "transactions": parsed.transactions,
                "blockVersion": parsed.block_version,
                "size": data.len(),
            })
        }
        None => return err(StatusCode::NOT_FOUND, "transactions not found"),
    };

    // AD proofs (optional in JVM): keyed by blake2b256(104 || header_id || ad_proofs_root).
    // Stored format: [header_id: 32B] [proof_size: VLQ u32] [proof_bytes: proof_size B].
    let ad_modifier_id = section_modifier_id(AD_PROOFS_TYPE, &id, header.ad_proofs_root.0.as_ref());
    let ad_proofs_value = state
        .store
        .get(AD_PROOFS_TYPE, &ad_modifier_id)
        .and_then(|data| {
            let proof_bytes = inline_ad_proof_bytes(&data)?;
            Some(serde_json::json!({
                "headerId": header_id_hex,
                "proofBytes": hex::encode(proof_bytes),
                "digest": hex::encode(header.ad_proofs_root.0.as_ref()),
                "size": data.len(),
            }))
        });

    // Extension: keyed by blake2b256(108 || header_id || extension_root).
    let ext_modifier_id =
        section_modifier_id(EXTENSION_TYPE, &id, header.extension_root.0.as_ref());
    let extension_value = match state.store.get(EXTENSION_TYPE, &ext_modifier_id) {
        Some(data) => {
            let parsed = ergo_validation::parse_extension(&data).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError {
                        error: 500,
                        reason: format!("failed to parse stored extension: {e}"),
                        detail: None,
                    }),
                )
            })?;
            let fields: Vec<[String; 2]> = parsed
                .fields
                .iter()
                .map(|f| [hex::encode(f.key), hex::encode(&f.value)])
                .collect();
            serde_json::json!({
                "headerId": header_id_hex,
                "digest": hex::encode(header.extension_root.0.as_ref()),
                "fields": fields,
            })
        }
        None => return err(StatusCode::NOT_FOUND, "extension not found"),
    };

    let mut full = serde_json::Map::new();
    full.insert(
        "header".into(),
        serde_json::to_value(&header).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: 500,
                    reason: format!("header serialization failed: {e}"),
                    detail: None,
                }),
            )
        })?,
    );
    full.insert("blockTransactions".into(), txs_value);
    full.insert("extension".into(), extension_value);
    full.insert(
        "adProofs".into(),
        ad_proofs_value.unwrap_or(serde_json::Value::Null),
    );
    Ok(Json(serde_json::Value::Object(full)))
}

// ---------------------------------------------------------------------------
// GET /blocks/modifier/{modifier_id}  — fetch by ID across types 101/102/104/108
// ---------------------------------------------------------------------------

pub async fn get_block_modifier(
    State(state): State<ApiState>,
    Path(modifier_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let id = hex_to_id(&modifier_id)?;
    for &type_id in &[
        HEADER_TYPE,
        BLOCK_TRANSACTIONS_TYPE,
        AD_PROOFS_TYPE,
        EXTENSION_TYPE,
    ] {
        let Some(data) = state.store.get(type_id, &id) else {
            continue;
        };
        let id_hex = hex::encode(id);
        let value = match type_id {
            HEADER_TYPE => {
                // Headers stored by header ID, so id IS the header ID.
                match state.chain.header_by_id(&id) {
                    Some(h) => serde_json::to_value(&h).map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ApiError {
                                error: 500,
                                reason: format!("header serialization failed: {e}"),
                                detail: None,
                            }),
                        )
                    })?,
                    None => {
                        serde_json::json!({ "type": "header", "id": id_hex, "size": data.len() })
                    }
                }
            }
            BLOCK_TRANSACTIONS_TYPE => {
                let parsed = ergo_validation::parse_block_transactions(&data).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiError {
                            error: 500,
                            reason: format!("parse failed: {e}"),
                            detail: None,
                        }),
                    )
                })?;
                serde_json::json!({
                    "headerId": hex::encode(parsed.header_id),
                    "transactions": parsed.transactions,
                    "blockVersion": parsed.block_version,
                    "size": data.len(),
                })
            }
            AD_PROOFS_TYPE => {
                // Stored: [header_id: 32B] [proof_size: VLQ] [proof_bytes].
                if data.len() < 32 {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "ad_proofs too short");
                }
                let inner_header_id = hex::encode(&data[..32]);
                let proof_bytes = match inline_ad_proof_bytes(&data) {
                    Some(b) => b,
                    None => return err(StatusCode::INTERNAL_SERVER_ERROR, "malformed ad_proofs"),
                };
                serde_json::json!({
                    "headerId": inner_header_id,
                    "proofBytes": hex::encode(proof_bytes),
                    "size": data.len(),
                })
            }
            EXTENSION_TYPE => {
                let parsed = ergo_validation::parse_extension(&data).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiError {
                            error: 500,
                            reason: format!("parse failed: {e}"),
                            detail: None,
                        }),
                    )
                })?;
                let fields: Vec<[String; 2]> = parsed
                    .fields
                    .iter()
                    .map(|f| [hex::encode(f.key), hex::encode(&f.value)])
                    .collect();
                serde_json::json!({
                    "headerId": hex::encode(parsed.header_id),
                    "fields": fields,
                })
            }
            _ => unreachable!(),
        };
        return Ok(Json(value));
    }
    err(StatusCode::NOT_FOUND, "modifier not found")
}

// ---------------------------------------------------------------------------
// HEAD /transactions/unconfirmed/{tx_id}  — status only, empty body
// ---------------------------------------------------------------------------

pub async fn head_unconfirmed(
    State(state): State<ApiState>,
    Path(tx_id): Path<String>,
) -> StatusCode {
    // Malformed hex → 404 (HEAD has no body to put a reason in, so we map to 404
    // for any input the mempool can't possibly contain).
    let Ok(id) = hex_to_id_status(&tx_id) else {
        return StatusCode::NOT_FOUND;
    };
    let pool = state.mempool.lock().await;
    if pool.contains(&id) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

fn hex_to_id_status(hex_str: &str) -> Result<[u8; 32], ()> {
    let bytes = hex::decode(hex_str).map_err(|_| ())?;
    bytes.try_into().map_err(|_| ())
}

// ---------------------------------------------------------------------------
// GET /transactions/waitTime?fee=...&txSize=...
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct WaitTimeParams {
    fee: u64,
    #[serde(rename = "txSize", default = "default_tx_size")]
    tx_size: usize,
}

#[derive(serde::Serialize)]
pub struct WaitTimeResponse {
    #[serde(rename = "waitTime")]
    wait_time: u64,
}

pub async fn get_wait_time(
    State(state): State<ApiState>,
    Query(params): Query<WaitTimeParams>,
) -> ApiResult<WaitTimeResponse> {
    let tx_size = params.tx_size.min(1_000_000);
    let pool = state.mempool.lock().await;
    match pool.expected_wait_time(params.fee, tx_size) {
        Some(wait_ms) => {
            // Convert milliseconds back to blocks (target block time ~2 min).
            // recommended_fee/expected_wait_time work in ms; the endpoint
            // returns blocks per the JVM contract.
            let blocks = wait_ms / 120_000;
            Ok(Json(WaitTimeResponse { wait_time: blocks }))
        }
        None => err(StatusCode::BAD_REQUEST, "insufficient fee history"),
    }
}

// ---------------------------------------------------------------------------
// POST /utxo/withPool/byIds  — batch lookup, max 100, positional null for missing
// ---------------------------------------------------------------------------

pub async fn post_utxo_with_pool_by_ids(
    State(state): State<ApiState>,
    Json(ids): Json<Vec<String>>,
) -> ApiResult<Vec<Option<ergo_validation::ErgoBox>>> {
    if ids.len() > 100 {
        return err(StatusCode::BAD_REQUEST, "max 100 box IDs per request");
    }
    // Parse and lookup. Malformed IDs are treated as "not found" (return null),
    // not as a 400 — the contract is positional, and rejecting the whole batch
    // for one bad ID would be operationally hostile.
    let mut results: Vec<Option<ergo_validation::ErgoBox>> = Vec::with_capacity(ids.len());
    let pool = state.mempool.lock().await;
    for hex_id in &ids {
        let parsed = hex::decode(hex_id)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok());
        match parsed {
            Some(id) => {
                let found = state
                    .utxo_reader
                    .box_by_id(&id)
                    .or_else(|| pool.unconfirmed_box(&id).cloned());
                results.push(found);
            }
            None => results.push(None),
        }
    }
    Ok(Json(results))
}

// ---------------------------------------------------------------------------
// GET /utxo/getSnapshotsInfo  — { "availableManifests": [ {height, digest} ] }
// ---------------------------------------------------------------------------

pub async fn get_snapshots_info(State(state): State<ApiState>) -> Json<SnapshotsInfo> {
    let inventory = (state.snapshots_info)();
    let available_manifests = inventory
        .into_iter()
        .map(|e| SnapshotManifestEntry {
            height: e.height,
            digest: hex::encode(e.digest),
        })
        .collect();
    Json(SnapshotsInfo {
        available_manifests,
    })
}

// ---------------------------------------------------------------------------
// GET /peers/all  — connected + disconnected
// ---------------------------------------------------------------------------

pub async fn get_all_peers(State(state): State<ApiState>) -> Json<Vec<PeerInfoEntry>> {
    let peers = (state.peer_all)();
    Json(peers.into_iter().map(peer_info_to_entry).collect())
}

fn peer_info_to_entry(p: crate::PeerInfo) -> PeerInfoEntry {
    PeerInfoEntry {
        address: p.address.to_string(),
        name: p.name,
        last_seen: p.last_seen,
        connection_type: p.connection_type,
    }
}

// ---------------------------------------------------------------------------
// GET /peers/status  — { lastIncomingMessage, currentNetworkTime }
// ---------------------------------------------------------------------------

pub async fn get_peers_status(State(state): State<ApiState>) -> Json<PeerStatus> {
    let s = (state.peer_status)();
    Json(PeerStatus {
        last_incoming_message: s.last_incoming_message,
        current_network_time: s.current_network_time,
    })
}

// ---------------------------------------------------------------------------
// GET /peers/blacklisted  — { "addresses": [ "host:port", ... ] }
// ---------------------------------------------------------------------------

pub async fn get_blacklisted_peers(State(state): State<ApiState>) -> Json<PeersBlacklisted> {
    let addrs = (state.peer_blacklisted)();
    Json(PeersBlacklisted {
        addresses: addrs.into_iter().map(|a| a.to_string()).collect(),
    })
}

// ---------------------------------------------------------------------------
// POST /peers/connect  — body: "host:port" string, auth required
// ---------------------------------------------------------------------------

pub async fn post_peers_connect(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult<serde_json::Value> {
    check_api_key(&state, &headers)?;
    // Body is a JSON string like "1.2.3.4:9030". Strip surrounding quotes.
    let trimmed = body.trim();
    let addr_str = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed);
    let addr: std::net::SocketAddr = addr_str.parse().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: 400,
                reason: format!("malformed socket address: {e}"),
                detail: None,
            }),
        )
    })?;
    match (state.peer_connect)(addr) {
        Ok(()) => Ok(Json(serde_json::json!({ "status": "queued" }))),
        Err(reason) => err(StatusCode::BAD_REQUEST, reason),
    }
}

// ---------------------------------------------------------------------------
// GET /nipopow/popowHeader/{header_id}
// GET /nipopow/popowHeader/last
// ---------------------------------------------------------------------------

pub async fn get_popow_header_by_id(
    State(state): State<ApiState>,
    Path(header_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let id = hex_to_id(&header_id)?;
    popow_header_response(&state, id).await
}

pub async fn get_popow_header_last(State(state): State<ApiState>) -> ApiResult<serde_json::Value> {
    let tip = match state.chain.tip() {
        Some(t) => t,
        None => return err(StatusCode::NOT_FOUND, "chain is empty"),
    };
    let mut tip_id = [0u8; 32];
    tip_id.copy_from_slice(tip.id.0.as_ref());
    popow_header_response(&state, tip_id).await
}

async fn popow_header_response(
    state: &ApiState,
    header_id: [u8; 32],
) -> ApiResult<serde_json::Value> {
    let chain = Arc::clone(&state.chain);
    let result = tokio::task::spawn_blocking(move || chain.popow_header_by_id(&header_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: 500,
                    reason: format!("popow_header task panicked: {e}"),
                    detail: None,
                }),
            )
        })?;
    let bytes = match result {
        Ok(Some(b)) => b,
        Ok(None) => return err(StatusCode::NOT_FOUND, "header not found"),
        Err(reason) => return err(StatusCode::INTERNAL_SERVER_ERROR, reason),
    };
    let header = ergo_nipopow::PoPowHeader::scorex_parse_bytes(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: "failed to parse popow header bytes".into(),
                detail: Some(format!("{e:?}")),
            }),
        )
    })?;
    serde_json::to_value(&header).map(Json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: 500,
                reason: format!("popow header serialization failed: {e}"),
                detail: None,
            }),
        )
    })
}

// ---------------------------------------------------------------------------
// GET /blocks/{header_id}/validation-fragments
// ---------------------------------------------------------------------------

pub async fn get_block_validation_fragments(
    State(state): State<ApiState>,
    Path(header_id): Path<String>,
) -> ApiResult<ValidationFragments> {
    // Echo whatever the client sent (preserving case) when surfacing it in
    // the `detail` field — clients dispatch on `reason`, not on the echo.
    let id = hex::decode(&header_id)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid-header-id",
                Some(format!("headerId={header_id}")),
            )
        })?;

    let header = state.chain.header_by_id(&id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "block-not-found",
            Some(format!("headerId={header_id}")),
        )
    })?;

    // Canonical header bytes. Header impls ScorexSerializable, not
    // SigmaSerializable — these are the bytes whose blake2b256 IS the
    // header id.
    let header_bytes = header.scorex_serialize_bytes().map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "header-serialize-failed",
            Some(format!("{e}")),
        )
    })?;

    // Block transactions section. Stored under blake2b256(102 || header_id || transaction_root).
    // Absent here = pruned (or never had — but at a known header, "never had" implies prune).
    let txs_modifier_id = section_modifier_id(
        BLOCK_TRANSACTIONS_TYPE,
        &id,
        header.transaction_root.0.as_ref(),
    );
    let txs_data = state
        .store
        .get(BLOCK_TRANSACTIONS_TYPE, &txs_modifier_id)
        .ok_or_else(|| {
            api_error(
                StatusCode::GONE,
                "block-pruned",
                Some(format!("headerId={header_id}")),
            )
        })?;
    let parsed_txs = ergo_validation::parse_block_transactions(&txs_data).map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "block-transactions-parse-failed",
            Some(format!("{e}")),
        )
    })?;

    // Parameters: from the block's extension, NOT current chain parameters.
    // Null on any failure along the path (extension missing, parse error,
    // no parameter fields, etc.). Per contract pitfall #2.
    let ext_modifier_id =
        section_modifier_id(EXTENSION_TYPE, &id, header.extension_root.0.as_ref());
    let parameters = state
        .store
        .get(EXTENSION_TYPE, &ext_modifier_id)
        .and_then(|data| ergo_validation::parse_extension(&data).ok())
        .and_then(|ext| ergo_validation::parse_parameters_from_extension(&ext).ok())
        .map(|p| ValidationFragmentsParameters {
            max_block_cost: p.max_block_cost(),
        });

    // Per-tx fragments. `signing_message` is bytes_to_sign() (Pitfall #1:
    // strips proofs AND ContextExtensions). `bytes` is the full canonical
    // sigma_serialize_bytes() — every input's spending proof + ContextExtension
    // in on-chain wire order — whose blake2b256 IS the tx id.
    let mut tx_fragments = Vec::with_capacity(parsed_txs.transactions.len());
    for tx in &parsed_txs.transactions {
        let signing_message = tx.bytes_to_sign().map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tx-bytes-to-sign-failed",
                Some(format!("{e}")),
            )
        })?;
        let canonical_bytes = tx.sigma_serialize_bytes().map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tx-serialize-failed",
                Some(format!("{e}")),
            )
        })?;
        tx_fragments.push(ValidationFragmentsTx {
            signing_message: hex::encode(&signing_message),
            bytes: hex::encode(&canonical_bytes),
        });
    }

    Ok(Json(ValidationFragments {
        header_bytes: hex::encode(&header_bytes),
        parameters,
        transactions: tx_fragments,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn url_host_matches_ipv4() {
        let addr: SocketAddr = "213.239.193.208:9030".parse().unwrap();
        assert!(url_host_matches_addr("http://213.239.193.208:9053", &addr));
        assert!(!url_host_matches_addr("http://1.2.3.4:9053", &addr));
    }

    #[test]
    fn url_host_matches_ipv6() {
        let addr: SocketAddr = "[::1]:9030".parse().unwrap();
        assert!(url_host_matches_addr("http://[::1]:9053", &addr));
        assert!(!url_host_matches_addr("http://[::2]:9053", &addr));
    }

    #[test]
    fn url_host_rejects_dns() {
        let addr: SocketAddr = "213.239.193.208:9030".parse().unwrap();
        assert!(!url_host_matches_addr("http://example.com:9053", &addr));
    }

    #[test]
    fn url_host_rejects_garbage() {
        let addr: SocketAddr = "127.0.0.1:9030".parse().unwrap();
        assert!(!url_host_matches_addr("not-a-url", &addr));
        assert!(!url_host_matches_addr("", &addr));
    }

    // -----------------------------------------------------------------------
    // NiPoPoW handler tests
    // -----------------------------------------------------------------------

    use crate::ChainAccess;
    use ergo_chain_types::{
        ADDigest, AutolykosSolution, BlockId, Digest32, EcPoint, Header, Votes,
    };
    use ergo_merkle_tree::BatchMerkleProof;
    use ergo_nipopow::{NipopowProof, PoPowHeader};
    use sigma_ser::ScorexSerializable;
    use std::sync::Arc;

    /// Mock `ChainAccess` for handler tests. `header_by_id` returns the
    /// header iff its id matches `known_header_id`. `build_nipopow_proof`
    /// returns whatever the caller stashed in `proof_result`.
    struct MockChain {
        known_header_id: Option<[u8; 32]>,
        header_for_known_id: Option<Header>,
        proof_result: Result<Vec<u8>, String>,
    }

    impl ChainAccess for MockChain {
        fn height(&self) -> u32 {
            0
        }
        fn header_at(&self, _h: u32) -> Option<Header> {
            None
        }
        fn header_by_id(&self, id: &[u8; 32]) -> Option<Header> {
            if Some(*id) == self.known_header_id {
                self.header_for_known_id.clone()
            } else {
                None
            }
        }
        fn tip(&self) -> Option<Header> {
            None
        }
        fn build_nipopow_proof(
            &self,
            _m: u32,
            _k: u32,
            _header_id: Option<[u8; 32]>,
        ) -> Result<Vec<u8>, String> {
            self.proof_result.clone()
        }
        fn header_ids(&self, _offset: u32, _limit: u32) -> Vec<[u8; 32]> {
            vec![]
        }
        fn popow_header_by_id(&self, _id: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }

    fn make_minimal_header(height: u32) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id: BlockId(Digest32::zero()),
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp: 1_000_000 + height as u64,
            n_bits: 100_000,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        // Reparse to recompute the id field.
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    /// Construct a minimal `NipopowProof` (empty interlinks/proof) and
    /// serialize to wire bytes. The proof would fail
    /// `has_valid_connections` against a real chain, but it round-trips
    /// through `scorex_parse_bytes` cleanly — that's all the handler does.
    fn make_test_proof_bytes() -> Vec<u8> {
        let suffix_head = PoPowHeader {
            header: make_minimal_header(2),
            interlinks: vec![],
            interlinks_proof: BatchMerkleProof::new(vec![], vec![]),
        };
        let proof = NipopowProof::new(1, 1, vec![], suffix_head, vec![]).unwrap();
        proof.scorex_serialize_bytes().unwrap()
    }

    fn build_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn nipopow_proof_invalid_m_returns_400() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("m and k must be >= 1".into()),
        });
        let rt = build_runtime();
        let result = rt.block_on(nipopow_proof_response(
            chain as Arc<dyn ChainAccess>,
            0,
            2,
            None,
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(body.reason.contains("m and k"));
            }
            Ok(_) => panic!("expected 400, got 200"),
        }
    }

    #[test]
    fn nipopow_proof_invalid_k_returns_400() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("m and k must be >= 1".into()),
        });
        let rt = build_runtime();
        let result = rt.block_on(nipopow_proof_response(
            chain as Arc<dyn ChainAccess>,
            2,
            0,
            None,
        ));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("expected 400, got 200"),
        }
    }

    #[test]
    fn nipopow_proof_chain_too_short_returns_400() {
        // Simulates `ChainError::Nipopow("chain too short...")`.
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("prove_with_reader failed: ChainTooShort".into()),
        });
        let rt = build_runtime();
        let result = rt.block_on(nipopow_proof_response(
            chain as Arc<dyn ChainAccess>,
            6,
            10,
            None,
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(body.reason.contains("ChainTooShort") || body.reason.contains("too short"));
            }
            Ok(_) => panic!("expected 400, got 200"),
        }
    }

    #[test]
    fn nipopow_proof_happy_path_returns_jvm_compat_json() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Ok(make_test_proof_bytes()),
        });
        let rt = build_runtime();
        let result = rt.block_on(nipopow_proof_response(
            chain as Arc<dyn ChainAccess>,
            1,
            1,
            None,
        ));
        let Json(value) = match result {
            Ok(v) => v,
            Err((status, body)) => panic!("expected 200, got {status} / {}", body.reason),
        };

        // JVM-compat field shape: m, k, prefix, suffixHead, suffixTail, continuous.
        let obj = value.as_object().expect("response must be a JSON object");
        assert_eq!(obj.get("m").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(obj.get("k").and_then(|v| v.as_u64()), Some(1));
        assert!(obj.get("prefix").is_some_and(|v| v.is_array()));
        assert!(obj.get("suffixHead").is_some_and(|v| v.is_object()));
        assert!(obj.get("suffixTail").is_some_and(|v| v.is_array()));
        // `continuous` is the JVM-only field we inject — must be present and false.
        assert_eq!(obj.get("continuous"), Some(&serde_json::Value::Bool(false)));

        // suffixHead must carry the JVM-compat sub-shape: header + interlinks + interlinksProof
        let suffix_head = obj["suffixHead"].as_object().unwrap();
        assert!(suffix_head.contains_key("header"));
        assert!(suffix_head.contains_key("interlinks"));
        assert!(suffix_head.contains_key("interlinksProof"));
    }

    // Note on 404 coverage: `header_by_id` is the pre-check that turns an
    // unknown header_id into 404 *before* `nipopow_proof_response` is called.
    // The pre-check happens in `get_nipopow_proof_by_header`. Testing that
    // wrapper through the full axum extractors requires constructing a
    // complete `ApiState`, which is heavy for a one-call assertion — the
    // pre-check is a single `is_none()` branch and is covered by integration
    // testing against a real chain.

    // -----------------------------------------------------------------------
    // Helper / inline-parser tests
    // -----------------------------------------------------------------------

    #[test]
    fn subtle_constant_eq_matches() {
        let a = [0x11u8; 32];
        let b = [0x11u8; 32];
        assert!(subtle_constant_eq(&a, &b));
    }

    #[test]
    fn subtle_constant_eq_differs() {
        let a = [0x11u8; 32];
        let mut b = [0x11u8; 32];
        b[17] = 0x12;
        assert!(!subtle_constant_eq(&a, &b));
    }

    #[test]
    fn hex_to_id_status_ok() {
        let hex = "00".repeat(32);
        assert!(hex_to_id_status(&hex).is_ok());
    }

    #[test]
    fn hex_to_id_status_bad_hex() {
        assert!(hex_to_id_status("zzz").is_err());
    }

    #[test]
    fn hex_to_id_status_wrong_length() {
        assert!(hex_to_id_status("00").is_err()); // too short
        assert!(hex_to_id_status(&"00".repeat(40)).is_err()); // too long
    }

    #[test]
    fn inline_ad_proof_bytes_round_trip() {
        // [header_id: 32] [VLQ-encoded size=3] [3 bytes payload]
        let mut data = vec![0u8; 32];
        // VLQ encoding of 3 is just the single byte 0x03 (no continuation).
        data.push(0x03);
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let bytes = inline_ad_proof_bytes(&data).expect("ok");
        assert_eq!(bytes, &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn inline_ad_proof_bytes_too_short() {
        assert!(inline_ad_proof_bytes(&[]).is_none());
        assert!(inline_ad_proof_bytes(&[0u8; 32]).is_none()); // missing size
    }

    #[test]
    fn inline_ad_proof_bytes_truncated_payload() {
        let mut data = vec![0u8; 32];
        data.push(0x05); // claims 5 bytes
        data.extend_from_slice(&[0xAA, 0xBB]); // only 2
        assert!(inline_ad_proof_bytes(&data).is_none());
    }

    // -----------------------------------------------------------------------
    // ApiState builder + handler tests for the new endpoints
    // -----------------------------------------------------------------------

    use crate::{
        BlockSubmitter, NodeMeta, PeerCounts, PeerInfo, PeerRestInfo, PeerStatusSummary,
        SnapshotInfoEntry, StoreAccess, UtxoAccess,
    };
    use std::sync::atomic::AtomicU32;

    /// Empty UTXO reader — every lookup returns None.
    struct EmptyUtxoReader;
    impl UtxoAccess for EmptyUtxoReader {
        fn box_by_id(&self, _id: &[u8; 32]) -> Option<ergo_validation::ErgoBox> {
            None
        }
    }

    /// Empty store — every lookup returns None.
    struct EmptyStore;
    impl StoreAccess for EmptyStore {
        fn get(&self, _type_id: u8, _id: &[u8; 32]) -> Option<Vec<u8>> {
            None
        }
        fn get_at_height(&self, _type_id: u8, _height: u32) -> Option<Vec<u8>> {
            None
        }
    }

    struct UnusedSubmitter;
    impl BlockSubmitter for UnusedSubmitter {
        fn submit(&self, _h: Header, _b: Vec<u8>, _a: Vec<u8>, _e: Vec<u8>) -> Result<(), String> {
            Err("unused".into())
        }
    }

    /// Build a minimal `ApiState` suitable for unit-testing handlers.
    /// Callers override the relevant fields before invoking handlers.
    fn test_state(chain: Arc<dyn ChainAccess>) -> ApiState {
        let (_tx, rx) = tokio::sync::watch::channel(0u32);
        ApiState {
            chain,
            store: Arc::new(EmptyStore),
            mempool: Arc::new(tokio::sync::Mutex::new(ergo_mempool::Mempool::new(
                ergo_mempool::types::MempoolConfig::default(),
            ))),
            utxo_reader: Arc::new(EmptyUtxoReader),
            state_context: Arc::new(tokio::sync::RwLock::new(None)),
            peer_count: Arc::new(|| PeerCounts { connected: 0 }),
            node_info: Arc::new(NodeMeta {
                name: "test".into(),
                version: "0.0.0".into(),
                network: "testnet".into(),
                state_type: "utxo".into(),
            }),
            mining: None,
            block_submitter: Some(Arc::new(UnusedSubmitter)),
            validated_height: Arc::new(AtomicU32::new(0)),
            downloaded_height: Arc::new(AtomicU32::new(0)),
            peer_api_urls: Arc::new(Vec::<PeerRestInfo>::new) as _,
            peer_all: Arc::new(Vec::<PeerInfo>::new) as _,
            peer_status: Arc::new(|| PeerStatusSummary {
                last_incoming_message: None,
                current_network_time: 0,
            }),
            peer_blacklisted: Arc::new(Vec::<std::net::SocketAddr>::new) as _,
            peer_connect: Arc::new(|_| Err("unused".into())) as _,
            snapshots_info: Arc::new(Vec::<SnapshotInfoEntry>::new) as _,
            api_key_hash: None,
            modifier_tx: None,
            height_watch: rx,
            jemalloc_probe: None,
            stats_enabled: false,
            capture: None,
        }
    }

    /// Mock chain that returns a configurable list of header IDs.
    struct PaginatingChain {
        ids: Vec<[u8; 32]>,
    }
    impl ChainAccess for PaginatingChain {
        fn height(&self) -> u32 {
            self.ids.len() as u32
        }
        fn header_at(&self, _h: u32) -> Option<Header> {
            None
        }
        fn header_by_id(&self, _id: &[u8; 32]) -> Option<Header> {
            None
        }
        fn tip(&self) -> Option<Header> {
            None
        }
        fn build_nipopow_proof(
            &self,
            _m: u32,
            _k: u32,
            _id: Option<[u8; 32]>,
        ) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn header_ids(&self, offset: u32, limit: u32) -> Vec<[u8; 32]> {
            self.ids
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .copied()
                .collect()
        }
        fn popow_header_by_id(&self, _id: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }

    #[test]
    fn get_blocks_paginates() {
        let ids = vec![[1u8; 32], [2u8; 32], [3u8; 32]];
        let chain = Arc::new(PaginatingChain { ids: ids.clone() });
        let state = test_state(chain);
        let rt = build_runtime();
        let Json(result) = rt.block_on(get_blocks(
            State(state),
            Query(PaginationParams {
                offset: 0,
                limit: 2,
            }),
        ));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], hex::encode(ids[0]));
    }

    #[test]
    fn get_blocks_limit_caps_at_100() {
        let ids: Vec<_> = (0..200u8).map(|i| [i; 32]).collect();
        let chain = Arc::new(PaginatingChain { ids });
        let state = test_state(chain);
        let rt = build_runtime();
        let Json(result) = rt.block_on(get_blocks(
            State(state),
            // Request 500, hard cap is 100.
            Query(PaginationParams {
                offset: 0,
                limit: 500,
            }),
        ));
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn head_unconfirmed_not_in_pool_returns_404() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let status = rt.block_on(head_unconfirmed(State(state), Path("00".repeat(32))));
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn head_unconfirmed_malformed_returns_404() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let status = rt.block_on(head_unconfirmed(State(state), Path("zzz".into())));
        // Malformed id → can't possibly be in mempool → 404, not 400.
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn get_wait_time_no_history_returns_400() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_wait_time(
            State(state),
            Query(WaitTimeParams {
                fee: 1_000_000,
                tx_size: 100,
            }),
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(body.reason.contains("fee history"));
            }
            Ok(_) => panic!("expected 400 with empty mempool, got 200"),
        }
    }

    #[test]
    fn post_utxo_with_pool_by_ids_caps_at_100() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let ids: Vec<String> = (0..101).map(|_| "00".repeat(32)).collect();
        let result = rt.block_on(post_utxo_with_pool_by_ids(State(state), Json(ids)));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("expected 400 for >100 IDs"),
        }
    }

    #[test]
    fn post_utxo_with_pool_by_ids_returns_positional_null() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let ids = vec!["00".repeat(32), "zz".repeat(32), "11".repeat(32)];
        let Json(results) = match rt.block_on(post_utxo_with_pool_by_ids(State(state), Json(ids))) {
            Ok(v) => v,
            Err((status, body)) => panic!("expected 200, got {status} / {}", body.reason),
        };
        // All three positions present, all null (empty UTXO + empty mempool + bad hex).
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Option::is_none));
    }

    #[test]
    fn get_snapshots_info_empty() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let Json(info) = rt.block_on(get_snapshots_info(State(state)));
        assert!(info.available_manifests.is_empty());
    }

    #[test]
    fn get_snapshots_info_returns_inventory() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        state.snapshots_info = Arc::new(|| {
            vec![SnapshotInfoEntry {
                height: 1_700_000,
                digest: [0xAB; 32],
            }]
        });
        let rt = build_runtime();
        let Json(info) = rt.block_on(get_snapshots_info(State(state)));
        assert_eq!(info.available_manifests.len(), 1);
        assert_eq!(info.available_manifests[0].height, 1_700_000);
        assert_eq!(info.available_manifests[0].digest, "ab".repeat(32));
    }

    #[test]
    fn get_peers_status_returns_summary() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        state.peer_status = Arc::new(|| PeerStatusSummary {
            last_incoming_message: Some(1_712_400_000_000),
            current_network_time: 1_712_400_001_000,
        });
        let rt = build_runtime();
        let Json(s) = rt.block_on(get_peers_status(State(state)));
        assert_eq!(s.last_incoming_message, Some(1_712_400_000_000));
        assert_eq!(s.current_network_time, 1_712_400_001_000);
    }

    #[test]
    fn get_blacklisted_returns_addresses() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        state.peer_blacklisted = Arc::new(|| {
            vec![
                "1.2.3.4:9030".parse().unwrap(),
                "[::1]:9030".parse().unwrap(),
            ]
        });
        let rt = build_runtime();
        let Json(result) = rt.block_on(get_blacklisted_peers(State(state)));
        assert_eq!(result.addresses.len(), 2);
        assert_eq!(result.addresses[0], "1.2.3.4:9030");
    }

    #[test]
    fn peers_connect_rejects_malformed_addr() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(post_peers_connect(
            State(state),
            axum::http::HeaderMap::new(),
            "\"not-an-address\"".into(),
        ));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("expected 400 for malformed addr"),
        }
    }

    #[test]
    fn peers_connect_calls_callback_on_valid_addr() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        let called = Arc::new(std::sync::Mutex::new(None));
        let called_clone = Arc::clone(&called);
        state.peer_connect = Arc::new(move |addr| {
            *called_clone.lock().unwrap() = Some(addr);
            Ok(())
        });
        let rt = build_runtime();
        let result = rt.block_on(post_peers_connect(
            State(state),
            axum::http::HeaderMap::new(),
            "\"1.2.3.4:9030\"".into(),
        ));
        assert!(result.is_ok());
        let received = called.lock().unwrap().unwrap();
        assert_eq!(received, "1.2.3.4:9030".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn peers_connect_propagates_callback_error() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        state.peer_connect = Arc::new(|_| Err("address is banned".into()));
        let rt = build_runtime();
        let result = rt.block_on(post_peers_connect(
            State(state),
            axum::http::HeaderMap::new(),
            "\"1.2.3.4:9030\"".into(),
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(body.reason.contains("banned"));
            }
            Ok(_) => panic!("expected 400, got 200"),
        }
    }

    #[test]
    fn peers_connect_rejects_without_api_key() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        // Pin a known hash that no missing-header request can match.
        state.api_key_hash = Some([0x11u8; 32]);
        let rt = build_runtime();
        let result = rt.block_on(post_peers_connect(
            State(state),
            axum::http::HeaderMap::new(),
            "\"1.2.3.4:9030\"".into(),
        ));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
            Ok(_) => panic!("expected 403 without api_key"),
        }
    }

    #[test]
    fn peers_connect_rejects_wrong_api_key() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        state.api_key_hash = Some([0x11u8; 32]);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("api_key", "wrong".parse().unwrap());
        let rt = build_runtime();
        let result = rt.block_on(post_peers_connect(
            State(state),
            headers,
            "\"1.2.3.4:9030\"".into(),
        ));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::FORBIDDEN),
            Ok(_) => panic!("expected 403 for wrong key"),
        }
    }

    #[test]
    fn peers_connect_accepts_correct_api_key() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        // Hash of "secret"
        use blake2::digest::consts::U32;
        use blake2::{Blake2b, Digest};
        type Blake2b256 = Blake2b<U32>;
        let mut hasher = Blake2b256::new();
        hasher.update(b"secret");
        let expected: [u8; 32] = hasher.finalize().into();
        state.api_key_hash = Some(expected);
        state.peer_connect = Arc::new(|_| Ok(()));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("api_key", "secret".parse().unwrap());
        let rt = build_runtime();
        let result = rt.block_on(post_peers_connect(
            State(state),
            headers,
            "\"1.2.3.4:9030\"".into(),
        ));
        assert!(result.is_ok());
    }

    #[test]
    fn block_modifier_not_found_returns_404() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_block_modifier(State(state), Path("aa".repeat(32))));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::NOT_FOUND),
            Ok(_) => panic!("expected 404"),
        }
    }

    #[test]
    fn block_modifier_bad_hex_returns_400() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_block_modifier(State(state), Path("zzz".into())));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("expected 400"),
        }
    }

    #[test]
    fn popow_header_unknown_returns_404() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_popow_header_by_id(State(state), Path("aa".repeat(32))));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::NOT_FOUND),
            Ok(_) => panic!("expected 404"),
        }
    }

    #[test]
    fn popow_header_last_empty_chain_returns_404() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_popow_header_last(State(state)));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::NOT_FOUND),
            Ok(_) => panic!("expected 404"),
        }
    }

    #[test]
    fn full_block_unknown_header_returns_404() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_full_block(State(state), Path("aa".repeat(32))));
        match result {
            Err((status, _)) => assert_eq!(status, StatusCode::NOT_FOUND),
            Ok(_) => panic!("expected 404"),
        }
    }

    #[test]
    fn get_all_peers_returns_entries() {
        let chain = Arc::new(MockChain {
            known_header_id: None,
            header_for_known_id: None,
            proof_result: Err("unused".into()),
        });
        let mut state = test_state(chain);
        state.peer_all = Arc::new(|| {
            vec![PeerInfo {
                address: "1.2.3.4:9030".parse().unwrap(),
                name: Some("ergo-mainnet-6.0.3".into()),
                last_seen: Some(1_712_400_000_000),
                connection_type: Some("Outgoing".into()),
            }]
        });
        let rt = build_runtime();
        let Json(entries) = rt.block_on(get_all_peers(State(state)));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].address, "1.2.3.4:9030");
        assert_eq!(entries[0].connection_type.as_deref(), Some("Outgoing"));
    }

    // -----------------------------------------------------------------------
    // GET /blocks/{id}/validation-fragments — tests
    //
    // Fixture: a synthetic height-685 block with one P2PK tx (single input).
    // The endpoint is stateless w.r.t. UTXO state, so the fixture only wires
    // up chain (target + parent) and a stored block-transactions section.
    // -----------------------------------------------------------------------

    use std::collections::HashMap;

    /// Bare ProveDlog ErgoTree — same script used by the validation helper's
    /// inline tests.
    const VF_P2PK_TREE_HEX: &str =
        "0008cd02d84a11191f434daa5bed70e0e4db4e1563910622ee269f3dc219e0e854e108a5";
    const VF_SRC_TX_HEX: &str = "9302a2983d9cc3f2b9e271097aa3128581c6cad8b59f7b6bc3e08fa6cb63ad3f";

    /// Chain mock that resolves multiple headers by id (target + ancestors).
    struct MultiHeaderChain {
        by_id: HashMap<[u8; 32], Header>,
    }
    impl ChainAccess for MultiHeaderChain {
        fn height(&self) -> u32 {
            0
        }
        fn header_at(&self, _h: u32) -> Option<Header> {
            None
        }
        fn header_by_id(&self, id: &[u8; 32]) -> Option<Header> {
            self.by_id.get(id).cloned()
        }
        fn tip(&self) -> Option<Header> {
            None
        }
        fn build_nipopow_proof(
            &self,
            _m: u32,
            _k: u32,
            _h: Option<[u8; 32]>,
        ) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn header_ids(&self, _o: u32, _l: u32) -> Vec<[u8; 32]> {
            vec![]
        }
        fn popow_header_by_id(&self, _id: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }

    /// Store mock: returns pre-loaded bytes keyed by `(type_id, modifier_id)`.
    struct KeyedStore {
        by_key: HashMap<(u8, [u8; 32]), Vec<u8>>,
    }
    impl StoreAccess for KeyedStore {
        fn get(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>> {
            self.by_key.get(&(type_id, *id)).cloned()
        }
        fn get_at_height(&self, _t: u8, _h: u32) -> Option<Vec<u8>> {
            None
        }
    }

    /// Build a synthetic header at `height` with the given `parent_id`.
    /// Reparses to compute the real `id` from scorex bytes.
    fn make_vf_header(height: u32, parent_id: BlockId) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp: 1_000_000 + height as u64,
            n_bits: 100_000,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    /// Build a P2PK input box at a stable box id derivable from
    /// `(src_tx_id, output_index=0)`. Used to populate the input field of
    /// the synthetic tx — the box itself doesn't need to live in any UTXO
    /// store for this endpoint.
    fn make_vf_p2pk_box(value: u64) -> ergo_validation::ErgoBox {
        use ergo_lib::ergotree_ir::chain::ergo_box::{box_value::BoxValue, NonMandatoryRegisters};
        use ergo_lib::ergotree_ir::chain::tx_id::TxId;
        use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
        use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

        let tx_id_bytes: [u8; 32] = hex::decode(VF_SRC_TX_HEX).unwrap().try_into().unwrap();
        let tx_id = TxId::from(Digest32::from(tx_id_bytes));
        let tree = ErgoTree::sigma_parse_bytes(&hex::decode(VF_P2PK_TREE_HEX).unwrap()).unwrap();
        ergo_validation::ErgoBox::new(
            BoxValue::try_from(value).unwrap(),
            tree,
            None,
            NonMandatoryRegisters::empty(),
            684,
            tx_id,
            0,
        )
        .unwrap()
    }

    /// A single-input, single-output P2PK transaction.
    fn make_vf_p2pk_tx(input_box: &ergo_validation::ErgoBox) -> ergo_validation::Transaction {
        use ergo_lib::chain::transaction::input::prover_result::ProverResult as ChainProverResult;
        use ergo_lib::chain::transaction::input::Input;
        use ergo_lib::ergotree_interpreter::sigma_protocol::prover::ProofBytes;
        use ergo_lib::ergotree_ir::chain::context_extension::ContextExtension;
        use ergo_lib::ergotree_ir::chain::ergo_box::{
            box_value::BoxValue, ErgoBoxCandidate, NonMandatoryRegisters,
        };

        let inp = Input::new(
            input_box.box_id(),
            ChainProverResult {
                proof: ProofBytes::Empty,
                extension: ContextExtension::empty(),
            },
        );
        let out = ErgoBoxCandidate {
            value: BoxValue::try_from(900_000u64).unwrap(),
            ergo_tree: input_box.ergo_tree.clone(),
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 685,
        };
        ergo_validation::Transaction::new_from_vec(vec![inp], vec![], vec![out]).unwrap()
    }

    /// Bundle: returns (state, target_header_id_hex, tx_count).
    /// `state` is wired with a target block + parent and a stored
    /// block-transactions section containing one P2PK tx.
    fn build_vf_fixture() -> (ApiState, String, usize) {
        // Parent at h=684, target at h=685 parent_id=parent.id.
        let parent = make_vf_header(684, BlockId(Digest32::zero()));
        let target = make_vf_header(685, parent.id);

        let input_box = make_vf_p2pk_box(1_000_000);
        let tx = make_vf_p2pk_tx(&input_box);

        let txs_bytes = ergo_validation::serialize_block_transactions(
            &target.id.0 .0,
            2,
            std::slice::from_ref(&tx),
        )
        .unwrap();

        // Modifier id the handler will look up.
        let txs_modifier_id = section_modifier_id(
            BLOCK_TRANSACTIONS_TYPE,
            &target.id.0 .0,
            target.transaction_root.0.as_ref(),
        );

        let mut store_map = HashMap::new();
        store_map.insert((BLOCK_TRANSACTIONS_TYPE, txs_modifier_id), txs_bytes);

        let mut chain_map = HashMap::new();
        chain_map.insert(target.id.0 .0, target.clone());
        chain_map.insert(parent.id.0 .0, parent);

        let chain = Arc::new(MultiHeaderChain { by_id: chain_map });
        let mut state = test_state(chain);
        state.store = Arc::new(KeyedStore { by_key: store_map });

        let target_id_hex = hex::encode(target.id.0 .0);
        (state, target_id_hex, 1)
    }

    #[test]
    fn validation_fragments_h685() {
        let (state, target_id_hex, expected_tx_count) = build_vf_fixture();
        let rt = build_runtime();
        let result = rt.block_on(get_block_validation_fragments(
            State(state),
            Path(target_id_hex.clone()),
        ));
        let Json(body) = match result {
            Ok(v) => v,
            Err((status, body)) => panic!("expected 200, got {status} / {body:?}"),
        };

        assert_eq!(body.transactions.len(), expected_tx_count, "tx count");
        // Header bytes hex-encoded — must round-trip back to the same id.
        let header_bytes = hex::decode(&body.header_bytes).expect("hex");
        let reparsed = Header::scorex_parse_bytes(&header_bytes).expect("reparse");
        assert_eq!(hex::encode(reparsed.id.0 .0), target_id_hex);

        // signingMessage is hex.
        assert!(!body.transactions[0].signing_message.is_empty());
        hex::decode(&body.transactions[0].signing_message).expect("signingMessage is hex");

        // bytes (full canonical tx bytes) is non-empty hex too.
        assert!(!body.transactions[0].bytes.is_empty());
        hex::decode(&body.transactions[0].bytes).expect("bytes is hex");

        // parameters is None for this fixture — we never stored an extension
        // section. Contract pitfall #2 says null on parse failure.
        assert!(body.parameters.is_none());
    }

    #[test]
    fn validation_fragments_unknown_id() {
        let chain = Arc::new(MultiHeaderChain {
            by_id: HashMap::new(),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let result = rt.block_on(get_block_validation_fragments(
            State(state),
            Path("aa".repeat(32)),
        ));
        match result {
            Err((status, Json(body))) => {
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(body.error, 404);
                assert_eq!(body.reason, "block-not-found");
                assert_eq!(body.detail.as_deref(), Some(format!("headerId={}", "aa".repeat(32)).as_str()));
            }
            Ok(_) => panic!("expected 404 for unknown headerId"),
        }
    }

    /// Index alignment: the response's transactions arity must match what
    /// would come back from `/blocks/{id}` (= what's actually stored in the
    /// section). We assert directly against the section we wrote rather
    /// than spinning a second handler call — the contract is "same length
    /// as /blocks/{id}'s transactions", which is parse_block_transactions's
    /// output.
    #[test]
    fn validation_fragments_index_alignment() {
        let (state, target_id_hex, _) = build_vf_fixture();
        let stored_txs_bytes = state
            .store
            .get(
                BLOCK_TRANSACTIONS_TYPE,
                &section_modifier_id(
                    BLOCK_TRANSACTIONS_TYPE,
                    &hex::decode(&target_id_hex).unwrap().try_into().unwrap(),
                    state
                        .chain
                        .header_by_id(&hex::decode(&target_id_hex).unwrap().try_into().unwrap())
                        .unwrap()
                        .transaction_root
                        .0
                        .as_ref(),
                ),
            )
            .expect("section was preloaded");
        let parsed = ergo_validation::parse_block_transactions(&stored_txs_bytes).unwrap();

        let rt = build_runtime();
        let result = rt.block_on(get_block_validation_fragments(
            State(state),
            Path(target_id_hex),
        ));
        let Json(body) = result.expect("200");
        assert_eq!(
            body.transactions.len(),
            parsed.transactions.len(),
            "tx count must match /blocks/{{id}}",
        );

        // Round-trip / order-preservation proof: each tx's `bytes` is the full
        // canonical sigma_serialize_bytes(), so blake2b256(bytes) must reproduce
        // the tx id (itself defined as that hash). A regression that emitted
        // bytes_to_sign() — or re-sorted ContextExtension keys — into `bytes`
        // would break this.
        use blake2::digest::consts::U32;
        use blake2::{Blake2b, Digest};
        type Blake2b256 = Blake2b<U32>;
        for (i, frag) in body.transactions.iter().enumerate() {
            assert!(!frag.bytes.is_empty(), "tx[{i}] bytes must be non-empty");
            let raw = hex::decode(&frag.bytes).expect("bytes is hex");
            let digest: [u8; 32] = Blake2b256::digest(&raw).into();
            assert_eq!(
                digest,
                parsed.transactions[i].id().0 .0,
                "blake2b256(bytes) must equal transactions[{i}].id",
            );
        }
    }

    // -----------------------------------------------------------------------
    // GET /blocks/{id}/transactions
    // -----------------------------------------------------------------------

    /// Guards the byte-shape of the response. The handler renders through
    /// `serde_json::Value`, so every object's keys come out alphabetically
    /// sorted. Re-canonicalizing the body must therefore be a no-op. A future
    /// "optimization" to a typed/`derive(Serialize)` struct would emit fields in
    /// impl order (e.g. `id` before `dataInputs`) and silently change the bytes
    /// every external consumer sees — this test fails loudly if that happens.
    #[test]
    fn block_transactions_response_is_canonical_sorted_json() {
        let (state, target_id_hex, expected_tx_count) = build_vf_fixture();
        let rt = build_runtime();
        let (status, content_type, body_bytes) = rt.block_on(async {
            let resp = get_block_transactions(State(state), Path(target_id_hex.clone())).await;
            let status = resp.status();
            let content_type = resp
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, content_type, bytes)
        });

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));

        let value: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(value["headerId"], serde_json::Value::String(target_id_hex));
        assert_eq!(
            value["transactions"].as_array().map(Vec::len),
            Some(expected_tx_count)
        );

        let recanonical = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            body_bytes.as_ref(),
            recanonical.as_slice(),
            "response must already be canonical sorted-key JSON; a direct serializer would reorder keys"
        );
    }

    #[test]
    fn block_transactions_unknown_id_is_404() {
        let chain = Arc::new(MultiHeaderChain {
            by_id: HashMap::new(),
        });
        let state = test_state(chain);
        let rt = build_runtime();
        let status = rt.block_on(async {
            get_block_transactions(State(state), Path("aa".repeat(32)))
                .await
                .status()
        });
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // P2P capture handler tests
    // -----------------------------------------------------------------------

    use enr_p2p::capture::{CaptureAccess, CaptureInfo, Direction, DumpFilter};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    /// Test double for `CaptureAccess`. The template carries the constant
    /// fields (path, size, etc.); `generation` is held in an atomic so
    /// `reset()` can actually advance it — the reset handler verifies
    /// previous → current and a mock that froze generation would hide
    /// bugs in the delta logic.
    struct MockCapture {
        info_template: CaptureInfo,
        generation: AtomicU64,
        dump_bytes: Vec<u8>,
        reset_count: AtomicUsize,
        last_filter: StdMutex<Option<DumpFilter>>,
    }

    impl CaptureAccess for MockCapture {
        fn info(&self) -> CaptureInfo {
            CaptureInfo {
                enabled: self.info_template.enabled,
                path: self.info_template.path.clone(),
                size_mb: self.info_template.size_mb,
                write_head: self.info_template.write_head,
                generation: self.generation.load(Ordering::SeqCst),
                oldest_ts: self.info_template.oldest_ts.clone(),
                newest_ts: self.info_template.newest_ts.clone(),
                filter_mode: self.info_template.filter_mode,
                filter_count: self.info_template.filter_count,
            }
        }
        fn dump(&self, filter: &DumpFilter) -> Vec<u8> {
            *self.last_filter.lock().unwrap() = Some(filter.clone());
            self.dump_bytes.clone()
        }
        fn reset(&self) {
            self.reset_count.fetch_add(1, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn sample_info() -> CaptureInfo {
        CaptureInfo {
            enabled: true,
            path: "/tmp/cap.ring".to_string(),
            size_mb: 1024,
            write_head: 12_345,
            generation: 2,
            oldest_ts: Some("2026-05-25T10:00:00.000000Z".to_string()),
            newest_ts: Some("2026-05-25T11:00:00.000000Z".to_string()),
            filter_mode: "none",
            filter_count: 0,
        }
    }

    fn mock_with_info(info: CaptureInfo) -> Arc<MockCapture> {
        let gen = info.generation;
        Arc::new(MockCapture {
            info_template: info,
            generation: AtomicU64::new(gen),
            dump_bytes: Vec::new(),
            reset_count: AtomicUsize::new(0),
            last_filter: StdMutex::new(None),
        })
    }

    fn mock_with_dump(dump_bytes: Vec<u8>) -> Arc<MockCapture> {
        let info = sample_info();
        let gen = info.generation;
        Arc::new(MockCapture {
            info_template: info,
            generation: AtomicU64::new(gen),
            dump_bytes,
            reset_count: AtomicUsize::new(0),
            last_filter: StdMutex::new(None),
        })
    }

    fn empty_state() -> ApiState {
        let chain = Arc::new(MultiHeaderChain {
            by_id: HashMap::new(),
        });
        test_state(chain)
    }

    #[test]
    fn capture_info_returns_disabled_when_no_capture() {
        let state = empty_state();
        let rt = build_runtime();
        let Json(body) = rt.block_on(get_capture_info(State(state)));
        assert_eq!(body, serde_json::json!({ "enabled": false }));
    }

    #[test]
    fn capture_info_returns_full_info_when_enabled() {
        let mut state = empty_state();
        state.capture = Some(mock_with_info(sample_info()));
        let rt = build_runtime();
        let Json(body) = rt.block_on(get_capture_info(State(state)));
        assert_eq!(body["enabled"], true);
        assert_eq!(body["path"], "/tmp/cap.ring");
        assert_eq!(body["size_mb"], 1024);
        assert_eq!(body["write_head"], 12_345);
        assert_eq!(body["generation"], 2);
        assert_eq!(body["filter_mode"], "none");
        assert_eq!(body["filter_count"], 0);
        assert_eq!(body["oldest_ts"], "2026-05-25T10:00:00.000000Z");
        assert_eq!(body["newest_ts"], "2026-05-25T11:00:00.000000Z");
    }

    fn read_body_bytes(
        rt: &tokio::runtime::Runtime,
        response: axum::response::Response,
    ) -> Vec<u8> {
        let bytes = rt
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        bytes.to_vec()
    }

    #[test]
    fn capture_dump_404_when_disabled() {
        let state = empty_state();
        let rt = build_runtime();
        let response = rt.block_on(get_capture_dump(
            State(state),
            Query(CaptureDumpQuery::default()),
        ));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn capture_dump_returns_pcap_bytes_with_headers() {
        let mut state = empty_state();
        let canned = vec![0xA1, 0xB2, 0xC3, 0xD4, 0xDE, 0xAD, 0xBE, 0xEF];
        state.capture = Some(mock_with_dump(canned.clone()));
        let rt = build_runtime();
        let response = rt.block_on(get_capture_dump(
            State(state),
            Query(CaptureDumpQuery::default()),
        ));
        assert_eq!(response.status(), StatusCode::OK);

        let headers = response.headers().clone();
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/vnd.tcpdump.pcap"
        );
        let disposition = headers
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            disposition.starts_with("attachment; filename=\"p2p-capture-"),
            "got: {disposition}"
        );
        assert!(disposition.ends_with(".pcap\""), "got: {disposition}");

        let body = read_body_bytes(&rt, response);
        assert_eq!(body, canned);
    }

    #[test]
    fn capture_dump_passes_filters_through() {
        let mut state = empty_state();
        let mock = mock_with_dump(vec![]);
        state.capture = Some(mock.clone());
        let rt = build_runtime();
        let response = rt.block_on(get_capture_dump(
            State(state),
            Query(CaptureDumpQuery {
                peer: Some("10.0.0.7".to_string()),
                since_secs: Some(120),
                direction: Some("outbound".to_string()),
            }),
        ));
        assert_eq!(response.status(), StatusCode::OK);

        let filter = mock
            .last_filter
            .lock()
            .unwrap()
            .clone()
            .expect("dump called");
        assert_eq!(filter.peer, Some("10.0.0.7".parse().unwrap()));
        assert_eq!(filter.since_secs, Some(120));
        assert_eq!(filter.direction, Some(Direction::Outbound));
    }

    #[test]
    fn capture_dump_400_on_invalid_direction() {
        let mut state = empty_state();
        state.capture = Some(mock_with_dump(vec![]));
        let rt = build_runtime();
        let response = rt.block_on(get_capture_dump(
            State(state),
            Query(CaptureDumpQuery {
                peer: None,
                since_secs: None,
                direction: Some("sideways".to_string()),
            }),
        ));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn capture_dump_400_on_invalid_peer() {
        let mut state = empty_state();
        state.capture = Some(mock_with_dump(vec![]));
        let rt = build_runtime();
        let response = rt.block_on(get_capture_dump(
            State(state),
            Query(CaptureDumpQuery {
                peer: Some("not-an-ip".to_string()),
                since_secs: None,
                direction: None,
            }),
        ));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn capture_dump_filename_known_epoch() {
        let ts = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        // 1_700_000_000 → 2023-11-14T22:13:20 UTC
        assert_eq!(
            capture_dump_filename(ts),
            "p2p-capture-20231114T221320Z.pcap"
        );
    }

    #[test]
    fn capture_reset_404_when_disabled() {
        let state = empty_state();
        let rt = build_runtime();
        let response = rt.block_on(post_capture_reset(State(state)));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn capture_reset_calls_reset_and_returns_generations() {
        let mut state = empty_state();
        let mock = mock_with_info(CaptureInfo {
            enabled: true,
            path: "/tmp/cap.ring".to_string(),
            size_mb: 1,
            write_head: 100,
            generation: 7,
            oldest_ts: None,
            newest_ts: None,
            filter_mode: "none",
            filter_count: 0,
        });
        state.capture = Some(mock.clone());
        let rt = build_runtime();
        let response = rt.block_on(post_capture_reset(State(state)));
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(mock.reset_count.load(Ordering::SeqCst), 1);

        let body = read_body_bytes(&rt, response);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["reset"], true);
        assert_eq!(json["previous_generation"], 7);
        assert_eq!(json["current_generation"], 8);
    }

    // -----------------------------------------------------------------------
    // POST /mining/solution — candidate lifecycle (latch + parent-vs-tip)
    // -----------------------------------------------------------------------

    use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;
    use ergo_mining::CandidateGenerator;

    /// Initial difficulty (testnet/mainnet genesis nBits) — decodes to 1, so
    /// target = order / 1 ≈ 2^256 and nearly any nonce is a valid solution.
    const TRIVIAL_N_BITS: u32 = 16_842_752;

    /// Chain whose tip is a fixed header.
    struct TipChain {
        tip: Header,
    }
    impl ChainAccess for TipChain {
        fn height(&self) -> u32 {
            self.tip.height
        }
        fn header_at(&self, _h: u32) -> Option<Header> {
            None
        }
        fn header_by_id(&self, _id: &[u8; 32]) -> Option<Header> {
            None
        }
        fn tip(&self) -> Option<Header> {
            Some(self.tip.clone())
        }
        fn build_nipopow_proof(
            &self,
            _m: u32,
            _k: u32,
            _id: Option<[u8; 32]>,
        ) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn header_ids(&self, _offset: u32, _limit: u32) -> Vec<[u8; 32]> {
            vec![]
        }
        fn popow_header_by_id(&self, _id: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }

    /// Submitter that accepts everything — the happy-path stand-in.
    struct OkSubmitter;
    impl BlockSubmitter for OkSubmitter {
        fn submit(&self, _h: Header, _b: Vec<u8>, _a: Vec<u8>, _e: Vec<u8>) -> Result<(), String> {
            Ok(())
        }
    }

    fn test_miner_config() -> ergo_mining::MinerConfig {
        ergo_mining::MinerConfig {
            miner_pk: ProveDlog::new(EcPoint::default()),
            reward_delay: 720,
            votes: [0, 0, 0],
            candidate_ttl: std::time::Duration::from_secs(60),
            reemission_rules: ergo_mining::emission::ReemissionRules::mainnet(),
        }
    }

    /// Minimal solvable candidate on `parent`: one P2PK tx (the merkle root
    /// builder rejects empty tx lists) and trivial difficulty.
    fn make_test_candidate(parent: &Header) -> ergo_mining::CandidateBlock {
        ergo_mining::CandidateBlock {
            parent: parent.clone(),
            version: 2,
            n_bits: TRIVIAL_N_BITS,
            state_root: ADDigest::zero(),
            ad_proof_bytes: vec![],
            transactions: vec![make_vf_p2pk_tx(&make_vf_p2pk_box(1_000_000))],
            timestamp: parent.timestamp + 1,
            extension: ergo_mining::ExtensionCandidate { fields: vec![] },
            votes: [0, 0, 0],
            header_bytes: vec![],
        }
    }

    /// WorkMessage stub — `cache_candidate` requires one, but the solution
    /// handler never reads it.
    fn stub_work() -> ergo_mining::WorkMessage {
        ergo_mining::WorkMessage {
            msg: String::new(),
            b: String::new(),
            h: 0,
            pk: String::new(),
            proof: None,
        }
    }

    /// CPU-grind a nonce for `candidate`, constructing the solution exactly
    /// as the handler does from the configured miner PK. With trivial
    /// difficulty the first attempt almost always wins.
    fn grind_nonce(candidate: &ergo_mining::CandidateBlock, pk: &ProveDlog, max: u64) -> String {
        for nonce in 0u64..max {
            let solution = AutolykosSolution {
                miner_pk: Box::new(*pk.h),
                pow_onetime_pk: None,
                nonce: nonce.to_be_bytes().to_vec(),
                pow_distance: None,
            };
            if ergo_mining::solution::validate_solution(candidate, solution).is_ok() {
                return hex::encode(nonce.to_be_bytes());
            }
        }
        panic!("no nonce found in {max} attempts at trivial difficulty");
    }

    fn mining_state(tip: Header, generator: Arc<CandidateGenerator>) -> ApiState {
        let mut state = test_state(Arc::new(TipChain { tip }));
        state.mining = Some(generator);
        state.block_submitter = Some(Arc::new(OkSubmitter));
        state
    }

    #[test]
    fn mining_solution_latch_set_returns_400() {
        let generator = Arc::new(CandidateGenerator::new(test_miner_config()));
        assert!(
            generator.try_mark_solved(BlockId(Digest32::zero()), 5),
            "claim on a fresh generator must succeed"
        );
        let state = mining_state(make_minimal_header(4), generator);

        let rt = build_runtime();
        let result = rt.block_on(post_mining_solution(
            State(state),
            Json(SolutionSubmission {
                n: "0000000000000001".into(),
            }),
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(
                    body.reason.contains("already accepted"),
                    "unexpected reason: {}",
                    body.reason
                );
            }
            Ok(_) => panic!("expected 400, got 200"),
        }
    }

    #[test]
    fn mining_solution_stale_parent_candidate_skipped() {
        // Candidate builds on header(4); the chain tip is header(7) — the
        // candidate must be skipped without attempting PoW, and the error
        // must be the stale-specific message, not "no current candidate".
        let candidate_parent = make_minimal_header(4);
        let generator = Arc::new(CandidateGenerator::new(test_miner_config()));
        generator.cache_candidate(make_test_candidate(&candidate_parent), stub_work(), 4);
        let state = mining_state(make_minimal_header(7), generator.clone());

        let rt = build_runtime();
        let result = rt.block_on(post_mining_solution(
            State(state),
            Json(SolutionSubmission {
                n: "0000000000000001".into(),
            }),
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(
                    body.reason.contains("stale candidate"),
                    "unexpected reason: {}",
                    body.reason
                );
            }
            Ok(_) => panic!("expected 400, got 200"),
        }
        assert!(
            !generator.solved_pending(),
            "rejection must not set the latch"
        );
    }

    #[test]
    fn mining_solution_accept_sets_latch() {
        let parent = make_minimal_header(4);
        let config = test_miner_config();
        let candidate = make_test_candidate(&parent);
        let nonce_hex = grind_nonce(&candidate, &config.miner_pk, 1000);

        let generator = Arc::new(CandidateGenerator::new(config));
        generator.cache_candidate(candidate, stub_work(), 4);
        let state = mining_state(parent, generator.clone());

        let rt = build_runtime();
        let result = rt.block_on(post_mining_solution(
            State(state),
            Json(SolutionSubmission { n: nonce_hex }),
        ));
        match result {
            Ok(Json(v)) => assert_eq!(v["status"], "accepted"),
            Err((status, body)) => panic!("expected 200, got {status} / {}", body.reason),
        }
        assert!(
            generator.solved_pending(),
            "latch must be set after an accepted solution"
        );

        // And the latch now gates the endpoint: an immediate re-submit of the
        // same valid solution must bounce with 400 (the accept→apply window).
        let generator2 = generator.clone();
        let state2 = mining_state(make_minimal_header(4), generator2);
        let result2 = rt.block_on(post_mining_solution(
            State(state2),
            Json(SolutionSubmission {
                n: "0000000000000001".into(),
            }),
        ));
        match result2 {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(body.reason.contains("already accepted"));
            }
            Ok(_) => panic!("expected 400 while latch is set, got 200"),
        }
    }

    /// Chain that claims the solved latch when `tip()` is read — simulating
    /// a concurrent solution winning the race in the window between the
    /// handler's `solved_pending()` fast path and its `try_mark_solved`
    /// claim (the tip read sits between the two).
    struct RaceTipChain {
        tip: Header,
        generator: Arc<CandidateGenerator>,
    }
    impl ChainAccess for RaceTipChain {
        fn height(&self) -> u32 {
            self.tip.height
        }
        fn header_at(&self, _h: u32) -> Option<Header> {
            None
        }
        fn header_by_id(&self, _id: &[u8; 32]) -> Option<Header> {
            None
        }
        fn tip(&self) -> Option<Header> {
            let claimed = self
                .generator
                .try_mark_solved(BlockId(Digest32::zero()), self.tip.height + 1);
            assert!(claimed, "race injection expects an unclaimed latch");
            Some(self.tip.clone())
        }
        fn build_nipopow_proof(
            &self,
            _m: u32,
            _k: u32,
            _id: Option<[u8; 32]>,
        ) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn header_ids(&self, _offset: u32, _limit: u32) -> Vec<[u8; 32]> {
            vec![]
        }
        fn popow_header_by_id(&self, _id: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }

    /// Submitter that counts invocations — for asserting a block never
    /// reached the hand-off.
    struct CountingSubmitter(std::sync::atomic::AtomicUsize);
    impl BlockSubmitter for CountingSubmitter {
        fn submit(&self, _h: Header, _b: Vec<u8>, _a: Vec<u8>, _e: Vec<u8>) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn mining_solution_claim_loser_after_early_check_gets_400() {
        // A concurrent winner claims the latch AFTER the cheap
        // solved_pending() check but BEFORE try_mark_solved. The loser must
        // get 400, its block must never reach the submitter, and the
        // winner's latch must survive.
        let parent = make_minimal_header(4);
        let config = test_miner_config();
        let candidate = make_test_candidate(&parent);
        let nonce_hex = grind_nonce(&candidate, &config.miner_pk, 1000);

        let generator = Arc::new(CandidateGenerator::new(config));
        generator.cache_candidate(candidate, stub_work(), 4);

        let submitted = Arc::new(CountingSubmitter(std::sync::atomic::AtomicUsize::new(0)));
        let mut state = test_state(Arc::new(RaceTipChain {
            tip: parent,
            generator: generator.clone(),
        }));
        state.mining = Some(generator.clone());
        state.block_submitter = Some(submitted.clone());

        let rt = build_runtime();
        let result = rt.block_on(post_mining_solution(
            State(state),
            Json(SolutionSubmission { n: nonce_hex }),
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(
                    body.reason.contains("already accepted"),
                    "unexpected reason: {}",
                    body.reason
                );
            }
            Ok(_) => panic!("expected 400 for the claim loser, got 200"),
        }
        assert_eq!(
            submitted.0.load(Ordering::SeqCst),
            0,
            "loser's block must never reach the submitter"
        );
        assert!(
            generator.solved_pending(),
            "winner's latch must survive the loser's rejection"
        );
    }

    #[test]
    fn mining_solution_no_submitter_releases_latch() {
        // 503 path (contract step 9): claim, find no submitter, release.
        // A second identical submission must reach 503 again — not bounce
        // off a stuck latch with 400.
        let parent = make_minimal_header(4);
        let config = test_miner_config();
        let candidate = make_test_candidate(&parent);
        let nonce_hex = grind_nonce(&candidate, &config.miner_pk, 1000);

        let generator = Arc::new(CandidateGenerator::new(config));
        generator.cache_candidate(candidate, stub_work(), 4);
        let mut state = mining_state(parent, generator.clone());
        state.block_submitter = None;

        let rt = build_runtime();
        for attempt in 0..2 {
            let result = rt.block_on(post_mining_solution(
                State(state.clone()),
                Json(SolutionSubmission {
                    n: nonce_hex.clone(),
                }),
            ));
            match result {
                Err((status, body)) => {
                    assert_eq!(
                        status,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "attempt {attempt}: expected 503, got {status} / {}",
                        body.reason
                    );
                    assert!(
                        body.reason.contains("submitter"),
                        "attempt {attempt}: unexpected reason: {}",
                        body.reason
                    );
                }
                Ok(_) => panic!("attempt {attempt}: expected 503, got 200"),
            }
            assert!(
                !generator.solved_pending(),
                "attempt {attempt}: latch must be released on the 503 path"
            );
        }
    }

    #[test]
    fn mining_solution_submit_error_releases_latch() {
        // 500 path (contract step 8): claim, submit fails, release. The
        // default test_state submitter (UnusedSubmitter) always errors.
        let parent = make_minimal_header(4);
        let config = test_miner_config();
        let candidate = make_test_candidate(&parent);
        let nonce_hex = grind_nonce(&candidate, &config.miner_pk, 1000);

        let generator = Arc::new(CandidateGenerator::new(config));
        generator.cache_candidate(candidate, stub_work(), 4);
        let mut state = mining_state(parent, generator.clone());
        state.block_submitter = Some(Arc::new(UnusedSubmitter));

        let rt = build_runtime();
        let result = rt.block_on(post_mining_solution(
            State(state),
            Json(SolutionSubmission { n: nonce_hex }),
        ));
        match result {
            Err((status, body)) => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert!(
                    body.reason.contains("block submission failed"),
                    "unexpected reason: {}",
                    body.reason
                );
            }
            Ok(_) => panic!("expected 500, got 200"),
        }
        assert!(
            !generator.solved_pending(),
            "latch must be released on the 500 path"
        );
    }

    // -----------------------------------------------------------------------
    // GET /mining/candidate — height-source (bug #3)
    // -----------------------------------------------------------------------

    #[test]
    fn candidate_served_when_header_leads_validation() {
        // The candidate is built and cached on the VALIDATED tip's state root,
        // keyed to the validated height. The header chain leads validation
        // (`full < headers`) every time a new header arrives before its body
        // validates — and persistently behind a slow peer. Querying the cache
        // by header height (chain.height()) misses and returns 503 even though
        // a valid candidate for the validated tip is cached, making the node
        // unservable as a mining source unless perfectly synced. The handler
        // must query by validated height. (facts/mining.md, GET
        // /mining/candidate.)
        const H: u32 = 100;

        // Candidate cached at the VALIDATED tip height H, carrying a sentinel
        // `h` so the served work is unmistakably this one.
        let generator = Arc::new(CandidateGenerator::new(test_miner_config()));
        let mut work = stub_work();
        work.h = H;
        generator.cache_candidate(make_test_candidate(&make_minimal_header(H)), work, H);

        // Header chain leads validation by one block: the tip reports H+1 ...
        let mut state = mining_state(make_minimal_header(H + 1), generator);
        // ... while the validator's last fully-applied height is still H.
        state.validated_height = Arc::new(AtomicU32::new(H));

        let rt = build_runtime();
        match rt.block_on(get_mining_candidate(State(state))) {
            Ok(Json(served)) => assert_eq!(
                served.h, H,
                "must serve the cached candidate for the validated tip, not 503"
            ),
            Err((status, body)) => panic!(
                "expected 200 with the cached candidate, got {status} / {}",
                body.reason
            ),
        }
    }
}
