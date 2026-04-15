use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use crate::types::*;
use crate::ApiState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn err<T>(status: StatusCode, reason: impl Into<String>) -> ApiResult<T> {
    Err((status, Json(ApiError { error: status.as_u16(), reason: reason.into(), detail: None })))
}

fn mining_err() -> (StatusCode, Json<ApiError>) {
    (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError {
        error: 503, reason: "mining not configured".into(), detail: None,
    }))
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
        (StatusCode::BAD_REQUEST, Json(ApiError { error: 400, reason: "invalid hex ID".into(), detail: None }))
    })?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(ApiError { error: 400, reason: "ID must be 32 bytes".into(), detail: None }))
    })?;
    Ok(arr)
}

// ---------------------------------------------------------------------------
// GET /info
// ---------------------------------------------------------------------------

pub async fn get_info(State(state): State<ApiState>) -> Json<NodeInfo> {
    let headers_height = state.chain.height();
    let full_height = state.validated_height.load(std::sync::atomic::Ordering::Relaxed);
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
        downloaded_height: state.downloaded_height.load(std::sync::atomic::Ordering::Relaxed),
        best_full_header_id: tip_id.clone(),
        best_header_id: tip_id,
        state_root,
        state_type: state.node_info.state_type.clone(),
        peers_count: peers.connected,
        unconfirmed_count: pool_size,
        is_mining: false,
        current_time: now,
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
) -> ApiResult<serde_json::Value> {
    let id = hex_to_id(&header_id)?;
    // Block transactions are keyed by modifier ID = blake2b256(type_id || header_id || tx_root),
    // not by header ID. Compute the modifier ID from the header.
    let header = state.chain.header_by_id(&id)
        .ok_or_else(|| err::<()>(StatusCode::NOT_FOUND, "header not found").unwrap_err())?;
    let modifier_id = section_modifier_id(BLOCK_TRANSACTIONS_TYPE, &id, header.transaction_root.0.as_ref());
    match state.store.get(BLOCK_TRANSACTIONS_TYPE, &modifier_id) {
        Some(data) => {
            match ergo_validation::parse_block_transactions(&data) {
                Ok(parsed) => Ok(Json(serde_json::json!({
                    "headerId": header_id,
                    "transactions": parsed.transactions,
                }))),
                Err(e) => err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to parse stored transactions: {e}"),
                ),
            }
        }
        None => err(StatusCode::NOT_FOUND, "transactions not found"),
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
        None => return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "node is syncing, cannot validate transactions yet",
        ),
    };

    // Compute tx_id hex string
    let tx_id_hex = String::from(tx.id());

    // Sigma-serialize for mempool storage (P2P wire format)
    let tx_bytes = match tx.sigma_serialize_bytes() {
        Ok(b) => b,
        Err(e) => return err(
            StatusCode::BAD_REQUEST,
            format!("transaction serialization failed: {e}"),
        ),
    };

    if !add_to_pool {
        // Check-only: validate inputs exist and scripts pass, but don't add to pool.
        let mut input_boxes = Vec::with_capacity(tx.inputs.len());
        for input in tx.inputs.iter() {
            let box_id_bytes: [u8; 32] = input.box_id.as_ref().try_into().map_err(|_| {
                (StatusCode::BAD_REQUEST, Json(ApiError {
                    error: 400, reason: "input box_id must be 32 bytes".into(), detail: None,
                }))
            })?;
            match state.utxo_reader.box_by_id(&box_id_bytes) {
                Some(b) => input_boxes.push(b),
                None => return err(
                    StatusCode::BAD_REQUEST,
                    format!("input box not found: {}", hex::encode(box_id_bytes)),
                ),
            }
        }
        let mut data_boxes = Vec::new();
        if let Some(ref data_inputs) = tx.data_inputs {
            for di in data_inputs.iter() {
                let box_id_bytes: [u8; 32] = di.box_id.as_ref().try_into().map_err(|_| {
                    (StatusCode::BAD_REQUEST, Json(ApiError {
                        error: 400, reason: "data-input box_id must be 32 bytes".into(), detail: None,
                    }))
                })?;
                match state.utxo_reader.box_by_id(&box_id_bytes) {
                    Some(b) => data_boxes.push(b),
                    None => return err(
                        StatusCode::BAD_REQUEST,
                        format!("data-input box not found: {}", hex::encode(box_id_bytes)),
                    ),
                }
            }
        }

        if let Err(e) = ergo_validation::validate_single_transaction(&tx, input_boxes, data_boxes, ctx) {
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
        &UtxoReaderAdapter { utxo_reader: &*state.utxo_reader },
        ctx,
        None, // local submission — bypass rate limiting
    );
    drop(ctx_guard);

    match outcome {
        ergo_mempool::types::ProcessingOutcome::Accepted { .. }
        | ergo_mempool::types::ProcessingOutcome::Replaced { .. }
        | ergo_mempool::types::ProcessingOutcome::AlreadyInPool => {
            Ok(Json(tx_id_hex))
        }
        ergo_mempool::types::ProcessingOutcome::Invalidated { reason } => {
            err(StatusCode::BAD_REQUEST, reason)
        }
        ergo_mempool::types::ProcessingOutcome::Declined { reason } => {
            err(StatusCode::BAD_REQUEST, reason)
        }
        ergo_mempool::types::ProcessingOutcome::DoubleSpendLoser { .. } => {
            err(StatusCode::BAD_REQUEST, "double-spend conflict with higher-fee transaction")
        }
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

fn default_limit() -> usize { 50 }

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
        .filter_map(|utx| serde_json::to_value(&utx.tx).ok())
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
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("serialization failed: {e}")),
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

fn default_wait_time() -> u64 { 1 }
fn default_tx_size() -> usize { 100 }

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

fn default_bins() -> usize { 10 }

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

pub async fn get_connected_peers(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let counts = (state.peer_count)();
    Json(serde_json::json!({
        "connectedPeers": counts.connected,
    }))
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
    let Some(after_scheme) = url.split("://").nth(1) else { return false };
    let host_part = after_scheme.split('/').next().unwrap_or(after_scheme);

    // Handle IPv6 bracket notation: [::1]:9053
    let host = if host_part.starts_with('[') {
        host_part.split(']').next().unwrap_or("").trim_start_matches('[')
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
        return err(StatusCode::SERVICE_UNAVAILABLE, "modifier pipeline not available");
    };

    // Parse body: sequence of (type_id: u8, modifier_id: [u8; 32], data_len: u32 BE, data: [u8])
    let mut cursor = 0;
    let mut count = 0u32;
    while cursor < body.len() {
        // type_id (1) + modifier_id (32) + data_len (4) = 37 bytes minimum
        if cursor + 37 > body.len() {
            return err(StatusCode::BAD_REQUEST, format!(
                "truncated modifier header at offset {cursor}"
            ));
        }

        let type_id = body[cursor];
        cursor += 1;

        let mut id = [0u8; 32];
        id.copy_from_slice(&body[cursor..cursor + 32]);
        cursor += 32;

        let data_len = u32::from_be_bytes(
            body[cursor..cursor + 4].try_into().unwrap()
        ) as usize;
        cursor += 4;

        if cursor + data_len > body.len() {
            return err(StatusCode::BAD_REQUEST, format!(
                "truncated modifier data at offset {cursor}: need {data_len} bytes"
            ));
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
        return err(StatusCode::BAD_REQUEST, format!("height exceeds max ({MAX_EMISSION_HEIGHT})"));
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
// GET /mining/candidate
// ---------------------------------------------------------------------------

pub async fn get_mining_candidate(
    State(state): State<ApiState>,
) -> ApiResult<ergo_mining::WorkMessage> {
    let mining = state.mining.as_ref().ok_or_else(mining_err)?;

    let tip_height = state.chain.height();
    match mining.cached_work(tip_height) {
        Some(work) => Ok(Json(work)),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "no candidate available — node may still be syncing"),
    }
}

// ---------------------------------------------------------------------------
// GET /mining/rewardAddress
// ---------------------------------------------------------------------------

pub async fn get_mining_reward_address(
    State(state): State<ApiState>,
) -> ApiResult<serde_json::Value> {
    let mining = state.mining.as_ref().ok_or_else(mining_err)?;

    // Derive P2S address from miner PK
    let pk_hex: String = (*mining.config.miner_pk.h).clone().into();
    Ok(Json(serde_json::json!({
        "rewardAddress": pk_hex,
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

    // Parse nonce
    let nonce = hex::decode(&submission.n).map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(ApiError {
            error: 400,
            reason: "invalid nonce hex".into(),
            detail: None,
        }))
    })?;
    if nonce.len() != 8 {
        return err(StatusCode::BAD_REQUEST, "nonce must be exactly 8 bytes");
    }

    // Collect candidates to try: current first, then previous (so a GPU
    // miner solving the old candidate while a regen occurred still works).
    let mut candidates = Vec::with_capacity(2);
    if let Some(c) = mining.cached_block() {
        candidates.push(c);
    }
    if let Some(p) = mining.previous_block() {
        candidates.push(p);
    }
    if candidates.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no current candidate — fetch /mining/candidate first");
    }

    // Build Autolykos v2 solution (miner_pk not used in PoW calc, use configured pk)
    let solution = ergo_chain_types::AutolykosSolution {
        miner_pk: Box::new((*mining.config.miner_pk.h).clone()),
        pow_onetime_pk: None,
        nonce,
        pow_distance: None,
    };

    // Try each candidate — the solution may match the current or previous one.
    let mut last_err = None;
    let mut matched = None;
    for candidate in candidates {
        match ergo_mining::solution::validate_solution(&candidate, solution.clone()) {
            Ok(h) => { matched = Some((h, candidate)); break; }
            Err(e) => { last_err = Some(e); }
        }
    }
    let (header, candidate) = matched.ok_or_else(|| {
        let e = last_err.unwrap();
        match e {
            ergo_mining::MiningError::InvalidSolution(msg) => {
                (StatusCode::BAD_REQUEST, Json(ApiError {
                    error: 400,
                    reason: format!("invalid solution: {msg}"),
                    detail: None,
                }))
            }
            other => {
                tracing::error!("solution validation error: {other}");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError {
                    error: 500,
                    reason: "solution validation failed".into(),
                    detail: Some(other.to_string()),
                }))
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
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError {
        error: 500,
        reason: format!("block transactions serialize: {e}"),
        detail: None,
    })))?;

    let ad_proofs_bytes = ergo_validation::serialize_ad_proofs(
        &header_id,
        &candidate.ad_proof_bytes,
    );

    let extension_bytes = ergo_validation::serialize_extension(
        &header_id,
        &candidate.extension.fields,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError {
        error: 500,
        reason: format!("extension serialize: {e}"),
        detail: None,
    })))?;

    // Submit to the local pipeline + P2P broadcast (if submitter configured)
    if let Some(ref submitter) = state.block_submitter {
        submitter
            .submit(header, block_txs_bytes, ad_proofs_bytes, extension_bytes)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError {
                error: 500,
                reason: format!("block submission failed: {e}"),
                detail: None,
            })))?;
    } else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "block submitter not configured");
    }

    // Invalidate the cached candidate so the next poll generates a fresh one
    mining.invalidate();

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
    let current = state.validated_height.load(std::sync::atomic::Ordering::Relaxed);
    if current > params.after {
        return Ok(get_info(State(state)).await);
    }

    let mut rx = state.height_watch.clone();
    tokio::select! {
        _ = rx.changed() => Ok(get_info(State(state)).await),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            Err(StatusCode::NO_CONTENT)
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

    Json(DebugMemory { process, jemalloc, components })
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
        let kb = rest.trim().split_whitespace().next()
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
}
