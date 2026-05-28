use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use serde::Serialize;
use utoipa::ToSchema;

use super::block_tx_cache::BlockTxCache;
use super::{ApiContext, Pagination};
use crate::db::IndexerDb;
use crate::node_client::NodeClient;
use crate::types::*;

type Blake2b256 = Blake2b<U32>;

fn blake2b256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::new();
    hasher.update(data);
    hasher.finalize().into()
}

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

// ---------------------------------------------------------------------------
// GET /boxes/{box_id}/bytes — canonical ErgoBox::sigma_serialize bytes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BoxBytesResponse {
    pub bytes: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoxBytesError {
    pub error: String,
    pub box_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub(crate) enum BoxBytesOutcome {
    Found(Vec<u8>),
    NotFound,
    Internal { code: &'static str, message: String },
}

/// Compose canonical ErgoBox bytes for an already-indexed box_id.
///
/// Steps:
/// 1. Look up the indexer row for `box_id` (404 if absent).
/// 2. Fetch the creating block's transactions from the node, via the
///    single-flight cache so that a wide concurrent burst of box requests for
///    the same block collapses to one node fetch.
/// 3. Locate the creating tx by id, extract the output at the recorded index.
/// 4. Deserialize as an `ErgoBox` and `sigma_serialize` it.
/// 5. Verify `blake2b256(bytes) == box_id` — never serve bytes whose hash
///    disagrees with the requested id (catches composition bugs).
pub(crate) async fn compose_box_bytes(
    db: &dyn IndexerDb,
    client: &NodeClient,
    block_tx_cache: &BlockTxCache,
    box_id: &[u8; 32],
) -> BoxBytesOutcome {
    let row = match db.get_box(box_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return BoxBytesOutcome::NotFound,
        Err(e) => {
            return BoxBytesOutcome::Internal {
                code: "db-error",
                message: e.to_string(),
            };
        }
    };

    let txs = match block_tx_cache
        .get_or_fetch(&row.header_id, || client.transactions(&row.header_id))
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return BoxBytesOutcome::Internal {
                code: "node-fetch-failed",
                message: e.to_string(),
            };
        }
    };

    let target_tx_id = row.tx_id.as_str();
    let tx_val = match txs.transactions.iter().find(|t| {
        t.get("id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(target_tx_id))
    }) {
        Some(t) => t,
        None => {
            return BoxBytesOutcome::Internal {
                code: "tx-not-in-block",
                message: format!("tx {} not present in block {}", target_tx_id, row.header_id),
            };
        }
    };

    let out_idx = row.output_index as usize;
    let out_val = match tx_val
        .get("outputs")
        .and_then(|v| v.as_array())
        .and_then(|a| a.get(out_idx))
    {
        Some(o) => o,
        None => {
            return BoxBytesOutcome::Internal {
                code: "output-index-out-of-range",
                message: format!(
                    "output index {} out of range for tx {}",
                    out_idx, target_tx_id
                ),
            };
        }
    };

    let ergo_box: ErgoBox = match serde_json::from_value(out_val.clone()) {
        Ok(b) => b,
        Err(e) => {
            return BoxBytesOutcome::Internal {
                code: "ergo-box-parse-failed",
                message: e.to_string(),
            };
        }
    };

    let bytes = match ergo_box.sigma_serialize_bytes() {
        Ok(b) => b,
        Err(e) => {
            return BoxBytesOutcome::Internal {
                code: "sigma-serialize-failed",
                message: e.to_string(),
            };
        }
    };

    let hash = blake2b256(&bytes);
    if hash != *box_id {
        return BoxBytesOutcome::Internal {
            code: "id-byte-mismatch",
            message: format!(
                "blake2b256(bytes)={} != requested box_id={}",
                hex::encode(hash),
                hex::encode(box_id),
            ),
        };
    }

    BoxBytesOutcome::Found(bytes)
}

#[utoipa::path(get, path = "/api/v1/boxes/{box_id}/bytes", params(
    ("box_id" = String, Path, description = "Box ID (hex, 32 bytes)"),
), responses(
    (status = 200, body = BoxBytesResponse),
    (status = 404, body = BoxBytesError),
    (status = 500, body = BoxBytesError),
))]
pub async fn get_box_bytes(
    State(ctx): State<ApiContext>,
    Path(box_id_hex): Path<String>,
) -> Response {
    let bytes = match hex::decode(&box_id_hex) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(BoxBytesError {
                    error: "invalid-box-id-hex".into(),
                    box_id: box_id_hex,
                    message: None,
                }),
            )
                .into_response();
        }
    };
    let box_id: [u8; 32] = match bytes.try_into() {
        Ok(a) => a,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(BoxBytesError {
                    error: "invalid-box-id-length".into(),
                    box_id: box_id_hex,
                    message: Some("expected 32 bytes".into()),
                }),
            )
                .into_response();
        }
    };

    match compose_box_bytes(&*ctx.db, &ctx.node_client, &ctx.block_tx_cache, &box_id).await {
        BoxBytesOutcome::Found(b) => Json(BoxBytesResponse {
            bytes: hex::encode(b),
        })
        .into_response(),
        BoxBytesOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(BoxBytesError {
                error: "box-not-found".into(),
                box_id: box_id_hex,
                message: None,
            }),
        )
            .into_response(),
        BoxBytesOutcome::Internal { code, message } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(BoxBytesError {
                error: code.to_string(),
                box_id: box_id_hex,
                message: Some(message),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::Path as AxPath;
    use axum::routing::get as axum_get;
    use axum::Router;
    use serde_json::{json, Value};
    use tokio::sync::oneshot;

    use crate::db::sqlite::SqliteDb;
    use crate::db::IndexerDb;
    use crate::types::{IndexedBlock, IndexedBox, IndexedTx, InputRef};

    // Fixture: known-good box from sigma-rust's own JSON round-trip tests.
    // box_id == blake2b256(canonical_bytes), so the invariant check passes
    // when the row + JSON are consistent.
    const FIXTURE_BOX_ID: &str = "dd4e69ae683d7c2d1de2b3174182e6c443fd68abbcc24002ddc99adb599e0193";
    const FIXTURE_TX_ID: &str = "8204d2bbaabf946f89a27b366d1356eb10241dc1619a70b4e4a4a38b520926ce";
    const FIXTURE_HEADER_ID: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";
    const FIXTURE_ERGO_TREE: &str =
        "0008cd03f1102eb87a4166bf9fbd6247d087e92e1412b0e819dbb5fbc4e716091ec4e4ec";

    fn fixture_output_json(box_id: &str, tx_id: &str, idx: u16) -> Value {
        json!({
            "boxId": box_id,
            "value": 1000000,
            "ergoTree": FIXTURE_ERGO_TREE,
            "assets": [],
            "creationHeight": 268539,
            "additionalRegisters": {},
            "transactionId": tx_id,
            "index": idx,
        })
    }

    fn temp_db_path() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        std::env::temp_dir()
            .join(format!("ergo-indexer-test-{pid}-{nanos}.db"))
            .to_string_lossy()
            .into_owned()
    }

    /// Spawn an axum mock node that serves a single fixed tx-list JSON for any
    /// `/blocks/{id}/transactions` request. Returns (base_url, shutdown_tx).
    async fn spawn_mock_node(response: Value) -> (String, oneshot::Sender<()>) {
        let body = Arc::new(response);
        let app = Router::new().route(
            "/blocks/{id}/transactions",
            axum_get({
                let body = body.clone();
                move |AxPath(id): AxPath<String>| {
                    let body = body.clone();
                    async move {
                        let mut cloned = (*body).clone();
                        if let Some(obj) = cloned.as_object_mut() {
                            obj.insert("headerId".into(), Value::String(id));
                        }
                        axum::Json(cloned)
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
                .unwrap();
        });
        (format!("http://{addr}"), tx)
    }

    /// Like `spawn_mock_node`, but also returns an atomic counter incremented
    /// once per `/blocks/{id}/transactions` request — lets a test assert how
    /// many times the node was actually hit.
    async fn spawn_counting_mock_node(
        response: Value,
    ) -> (String, oneshot::Sender<()>, Arc<AtomicUsize>) {
        let body = Arc::new(response);
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/blocks/{id}/transactions",
            axum_get({
                let body = body.clone();
                let hits = hits.clone();
                move |AxPath(id): AxPath<String>| {
                    let body = body.clone();
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let mut cloned = (*body).clone();
                        if let Some(obj) = cloned.as_object_mut() {
                            obj.insert("headerId".into(), Value::String(id));
                        }
                        axum::Json(cloned)
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
                .unwrap();
        });
        (format!("http://{addr}"), tx, hits)
    }

    /// Insert a single block carrying one tx with `n_outputs` outputs at
    /// header_id/tx_id pulled from FIXTURE constants. Output 0 carries the
    /// fixture box; later outputs are zero-id placeholders.
    async fn seed_fixture(db: &SqliteDb, spent: bool, n_outputs: usize) {
        let header_id_bytes: [u8; 32] = hex::decode(FIXTURE_HEADER_ID).unwrap().try_into().unwrap();
        let tx_id_bytes: [u8; 32] = hex::decode(FIXTURE_TX_ID).unwrap().try_into().unwrap();
        let box_id_bytes: [u8; 32] = hex::decode(FIXTURE_BOX_ID).unwrap().try_into().unwrap();
        let ergo_tree_bytes = hex::decode(FIXTURE_ERGO_TREE).unwrap();

        let mut outputs = Vec::with_capacity(n_outputs);
        outputs.push(IndexedBox {
            box_id: box_id_bytes,
            output_index: 0,
            ergo_tree: ergo_tree_bytes,
            ergo_tree_hash: [0u8; 32],
            address: "test".into(),
            value: 1_000_000,
            tokens: vec![],
            registers: vec![],
            minted_token: None,
        });
        for i in 1..n_outputs {
            outputs.push(IndexedBox {
                box_id: [i as u8; 32],
                output_index: i as u32,
                ergo_tree: vec![],
                ergo_tree_hash: [0u8; 32],
                address: "test".into(),
                value: 1,
                tokens: vec![],
                registers: vec![],
                minted_token: None,
            });
        }

        // Seed the input row that the second block will consume to mark the
        // fixture box as spent (needed only when `spent == true`).
        let pre_input_box = IndexedBox {
            box_id: [0xAA; 32],
            output_index: 0,
            ergo_tree: vec![],
            ergo_tree_hash: [0u8; 32],
            address: "seed".into(),
            value: 1,
            tokens: vec![],
            registers: vec![],
            minted_token: None,
        };
        let block0 = IndexedBlock {
            height: 1,
            header_id: [0xFF; 32],
            timestamp: 0,
            difficulty: 1,
            miner_pk: vec![0],
            block_size: 0,
            transactions: vec![IndexedTx {
                tx_id: [0xCC; 32],
                tx_index: 0,
                size: 0,
                inputs: vec![InputRef { box_id: [0xBB; 32] }],
                outputs: vec![pre_input_box],
            }],
        };
        db.insert_block(&block0).await.unwrap();

        // The block containing the fixture box.
        let block1 = IndexedBlock {
            height: 2,
            header_id: header_id_bytes,
            timestamp: 0,
            difficulty: 1,
            miner_pk: vec![0],
            block_size: 0,
            transactions: vec![IndexedTx {
                tx_id: tx_id_bytes,
                tx_index: 0,
                size: 0,
                inputs: vec![InputRef { box_id: [0xAA; 32] }],
                outputs,
            }],
        };
        db.insert_block(&block1).await.unwrap();

        if spent {
            // A later block whose tx spends the fixture box.
            let block2 = IndexedBlock {
                height: 3,
                header_id: [0xEE; 32],
                timestamp: 0,
                difficulty: 1,
                miner_pk: vec![0],
                block_size: 0,
                transactions: vec![IndexedTx {
                    tx_id: [0xDD; 32],
                    tx_index: 0,
                    size: 0,
                    inputs: vec![InputRef {
                        box_id: box_id_bytes,
                    }],
                    outputs: vec![IndexedBox {
                        box_id: [0x11; 32],
                        output_index: 0,
                        ergo_tree: vec![],
                        ergo_tree_hash: [0u8; 32],
                        address: "test".into(),
                        value: 1_000_000,
                        tokens: vec![],
                        registers: vec![],
                        minted_token: None,
                    }],
                }],
            };
            db.insert_block(&block2).await.unwrap();
        }
    }

    fn box_id_arr() -> [u8; 32] {
        hex::decode(FIXTURE_BOX_ID).unwrap().try_into().unwrap()
    }

    #[tokio::test]
    async fn boxes_bytes_unspent() {
        let db_path = temp_db_path();
        let db = SqliteDb::open(&db_path).unwrap();
        seed_fixture(&db, false, 1).await;

        let tx_json = json!({
            "headerId": FIXTURE_HEADER_ID,
            "transactions": [
                {
                    "id": FIXTURE_TX_ID,
                    "inputs": [],
                    "outputs": [fixture_output_json(FIXTURE_BOX_ID, FIXTURE_TX_ID, 0)],
                }
            ]
        });
        let (base_url, _shutdown) = spawn_mock_node(tx_json).await;
        let client = NodeClient::new(&base_url).unwrap();
        let cache = BlockTxCache::new();

        let outcome = compose_box_bytes(&db, &client, &cache, &box_id_arr()).await;
        match outcome {
            BoxBytesOutcome::Found(bytes) => {
                assert_eq!(hex::encode(blake2b256(&bytes)), FIXTURE_BOX_ID);
            }
            BoxBytesOutcome::NotFound => panic!("expected Found, got NotFound"),
            BoxBytesOutcome::Internal { code, message } => {
                panic!("expected Found, got Internal({code}, {message})")
            }
        }

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn boxes_bytes_spent() {
        let db_path = temp_db_path();
        let db = SqliteDb::open(&db_path).unwrap();
        seed_fixture(&db, true, 1).await;

        // Confirm row reflects spent state.
        let row = db.get_box(&box_id_arr()).await.unwrap().unwrap();
        assert!(
            row.spent_tx_id.is_some(),
            "fixture box must be marked spent"
        );

        let tx_json = json!({
            "headerId": FIXTURE_HEADER_ID,
            "transactions": [
                {
                    "id": FIXTURE_TX_ID,
                    "inputs": [],
                    "outputs": [fixture_output_json(FIXTURE_BOX_ID, FIXTURE_TX_ID, 0)],
                }
            ]
        });
        let (base_url, _shutdown) = spawn_mock_node(tx_json).await;
        let client = NodeClient::new(&base_url).unwrap();
        let cache = BlockTxCache::new();

        let outcome = compose_box_bytes(&db, &client, &cache, &box_id_arr()).await;
        match outcome {
            BoxBytesOutcome::Found(bytes) => {
                assert_eq!(hex::encode(blake2b256(&bytes)), FIXTURE_BOX_ID);
            }
            BoxBytesOutcome::NotFound => panic!("expected Found, got NotFound"),
            BoxBytesOutcome::Internal { code, message } => {
                panic!("expected Found, got Internal({code}, {message})")
            }
        }

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn boxes_bytes_unknown() {
        let db_path = temp_db_path();
        let db = SqliteDb::open(&db_path).unwrap();
        // Note: no seed — the boxes table is empty.

        let tx_json = json!({ "headerId": "", "transactions": [] });
        let (base_url, _shutdown) = spawn_mock_node(tx_json).await;
        let client = NodeClient::new(&base_url).unwrap();
        let cache = BlockTxCache::new();

        let outcome = compose_box_bytes(&db, &client, &cache, &box_id_arr()).await;
        assert!(
            matches!(outcome, BoxBytesOutcome::NotFound),
            "expected NotFound"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn boxes_bytes_hash_invariant() {
        let db_path = temp_db_path();
        let db = SqliteDb::open(&db_path).unwrap();
        seed_fixture(&db, false, 1).await;

        // Craft a JSON whose output bytes deliberately differ from those that
        // hash to FIXTURE_BOX_ID — bump `index` from 0 to 7, which mutates the
        // canonical serialization. We omit the `boxId` field so the sigma-rust
        // deserializer's own id-check passes through, letting our hash gate
        // (the one under test) be the line that catches the mismatch.
        let tx_json = json!({
            "headerId": FIXTURE_HEADER_ID,
            "transactions": [
                {
                    "id": FIXTURE_TX_ID,
                    "inputs": [],
                    // Output 0 has the SAME `boxId` field but DIFFERENT body
                    // bytes (index 7 vs original 0). Sigma-rust's JSON
                    // deserializer with `boxId` field present would normally
                    // reject this, so we drop boxId from the output to bypass
                    // the deserialize-side check and exercise OUR hash gate.
                    "outputs": [{
                        "value": 1000000,
                        "ergoTree": FIXTURE_ERGO_TREE,
                        "assets": [],
                        "creationHeight": 268539,
                        "additionalRegisters": {},
                        "transactionId": FIXTURE_TX_ID,
                        "index": 7
                    }]
                }
            ]
        });
        let (base_url, _shutdown) = spawn_mock_node(tx_json).await;
        let client = NodeClient::new(&base_url).unwrap();
        let cache = BlockTxCache::new();

        let outcome = compose_box_bytes(&db, &client, &cache, &box_id_arr()).await;
        match outcome {
            BoxBytesOutcome::Internal { code, .. } => {
                assert_eq!(code, "id-byte-mismatch", "expected id-byte-mismatch");
            }
            other => panic!(
                "expected Internal(id-byte-mismatch), got {:?}",
                match other {
                    BoxBytesOutcome::Found(_) => "Found",
                    BoxBytesOutcome::NotFound => "NotFound",
                    BoxBytesOutcome::Internal { code, .. } => code,
                }
            ),
        }

        let _ = std::fs::remove_file(&db_path);
    }

    /// The production regression: many concurrent box requests for the SAME
    /// block must collapse to ONE node fetch (was N + retries, melting the node
    /// on fat blocks), while every caller still gets correct box bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn boxes_bytes_concurrent_same_block_one_node_fetch() {
        let db_path = temp_db_path();
        let db = Arc::new(SqliteDb::open(&db_path).unwrap());
        seed_fixture(&db, false, 1).await;

        let tx_json = json!({
            "headerId": FIXTURE_HEADER_ID,
            "transactions": [
                {
                    "id": FIXTURE_TX_ID,
                    "inputs": [],
                    "outputs": [fixture_output_json(FIXTURE_BOX_ID, FIXTURE_TX_ID, 0)],
                }
            ]
        });
        let (base_url, _shutdown, hits) = spawn_counting_mock_node(tx_json).await;
        let client = Arc::new(NodeClient::new(&base_url).unwrap());
        let cache = Arc::new(BlockTxCache::new());

        const N: usize = 64;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let db = db.clone();
            let client = client.clone();
            let cache = cache.clone();
            handles.push(tokio::spawn(async move {
                compose_box_bytes(&*db, &client, &cache, &box_id_arr()).await
            }));
        }

        for h in handles {
            match h.await.unwrap() {
                BoxBytesOutcome::Found(bytes) => {
                    assert_eq!(hex::encode(blake2b256(&bytes)), FIXTURE_BOX_ID);
                }
                BoxBytesOutcome::NotFound => panic!("expected Found, got NotFound"),
                BoxBytesOutcome::Internal { code, message } => {
                    panic!("expected Found, got Internal({code}, {message})")
                }
            }
        }

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "N concurrent box requests for one block must hit the node exactly once"
        );

        let _ = std::fs::remove_file(&db_path);
    }
}
