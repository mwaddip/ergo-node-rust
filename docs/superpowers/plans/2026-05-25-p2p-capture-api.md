# P2P Wire-Traffic Capture — `api/` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the `CaptureAccess` trait (shipped from the `p2p/` side per `2026-05-25-p2p-capture-p2p.md`) into the api/ crate and expose the three HTTP endpoints from `facts/p2p-capture.md`:
- `GET /debug/p2p-capture/info`
- `GET /debug/p2p-capture/dump` (with optional `?peer=&since_secs=&direction=` filters)
- `POST /debug/p2p-capture/reset`

**Architecture:** `ApiState` gains an `Arc<dyn ergo_p2p::capture::CaptureAccess>` field. Three new axum handlers in `api/src/handlers.rs` (or a new `api/src/handlers/debug.rs` if the existing file is large enough to split) consume the trait. Routes registered in `api/src/lib.rs` under the existing `/debug/` prefix. JSON for info/reset, `application/vnd.tcpdump.pcap` for dump (single in-memory buffer in v1; streaming is a v2 nice-to-have).

**Tech Stack:** `axum` (existing), `serde`/`serde_json` (existing), `ergo_p2p::capture` (the trait shipped from the p2p plan).

**Boundary:** `api/` only. Reads outside (`../facts/p2p-capture.md`, `../p2p/src/capture/handle.rs` for the trait shape) are fine; writes are not. Do NOT touch `p2p/`, `src/main.rs`, or any other crate.

**Prerequisite:** the p2p-side plan must have landed and the workspace must build. Specifically, `ergo_p2p::capture::{CaptureAccess, CaptureInfo, DumpFilter}` must be reachable. Run `cargo check -p ergo-api` after pulling the latest main — if it fails because those types don't exist, stop and report; the prerequisite is not met.

---

## File Structure

**Modified**:
- `api/src/lib.rs` — register three new routes under `/debug/p2p-capture/`; add `capture: Arc<dyn CaptureAccess>` to the `ApiState` struct
- `api/src/handlers.rs` — three new handler functions
- `api/Cargo.toml` — confirm `ergo-p2p` is already a dep (it is)

**Tests**:
- Handler unit tests inline in `api/src/handlers.rs` against a mock `CaptureAccess`

---

## Task 1: Add `CaptureAccess` to `ApiState`

**Files:**
- Modify: `api/src/lib.rs` (the `ApiState` struct definition)
- Modify: existing constructor calls in tests that build an `ApiState`

- [ ] **Step 1: Read the existing `ApiState`**

Locate the struct in `api/src/lib.rs`. It currently looks roughly like:

```rust
pub struct ApiState {
    pub chain: Arc<RwLock<HeaderChain>>,
    pub store: Arc<BlockStore>,
    pub mempool: Arc<Mutex<Mempool>>,
    pub utxo_state: Arc<SnapshotReader>,
    pub peers: Arc<RwLock<PeerManager>>,
    pub parameters: Arc<RwLock<Parameters>>,
    pub node_config: NodeConfig,
    pub api_key_hash: Option<[u8; 32]>,
    pub state_context: Arc<RwLock<ErgoStateContext>>,
}
```

(Match what's actually there; don't blind-write the above.)

- [ ] **Step 2: Add the capture field**

```rust
pub struct ApiState {
    // ... existing fields ...

    /// Optional capture access. None when p2p_capture is disabled.
    pub capture: Option<Arc<dyn ergo_p2p::capture::CaptureAccess>>,
}
```

- [ ] **Step 3: Update construction sites**

Any test or production code that constructs an `ApiState` needs `capture: None` (or `Some(...)` if test wants to exercise it).

- [ ] **Step 4: Build clean**

```
cargo build -p ergo-api
```

Expected: clean. Existing tests pass (they all use `None` for capture).

- [ ] **Step 5: Commit**

```
git add api/src/lib.rs
git commit -m "feat(api): add CaptureAccess to ApiState"
```

---

## Task 2: `GET /debug/p2p-capture/info` handler

**Files:**
- Modify: `api/src/handlers.rs` (or split into `handlers/debug.rs` if cleaner)

- [ ] **Step 1: Write the failing test with a mock CaptureAccess**

```rust
#[cfg(test)]
mod capture_handler_tests {
    use super::*;
    use ergo_p2p::capture::{CaptureAccess, CaptureInfo, DumpFilter};
    use std::sync::Arc;

    struct MockCapture {
        info_response: CaptureInfo,
    }

    impl CaptureAccess for MockCapture {
        fn info(&self) -> CaptureInfo {
            CaptureInfo {
                enabled: self.info_response.enabled,
                path: self.info_response.path.clone(),
                size_mb: self.info_response.size_mb,
                write_head: self.info_response.write_head,
                generation: self.info_response.generation,
                oldest_ts: self.info_response.oldest_ts.clone(),
                newest_ts: self.info_response.newest_ts.clone(),
                filter_mode: self.info_response.filter_mode,
                filter_count: self.info_response.filter_count,
            }
        }
        fn dump(&self, _filter: &DumpFilter) -> Vec<u8> { vec![] }
        fn reset(&self) {}
    }

    #[tokio::test]
    async fn info_returns_json_with_expected_fields() {
        let mock = Arc::new(MockCapture {
            info_response: CaptureInfo {
                enabled: true,
                path: "/tmp/cap.ring".to_string(),
                size_mb: 1024,
                write_head: 12345,
                generation: 2,
                oldest_ts: Some("2026-05-25T10:00:00Z".to_string()),
                newest_ts: Some("2026-05-25T11:00:00Z".to_string()),
                filter_mode: "none",
                filter_count: 0,
            },
        });

        let state = make_test_state_with_capture(mock);
        let response = capture_info_handler(axum::extract::State(state)).await;
        let json = response.into_response();
        // Assert it serialized to JSON with the expected shape.
        // (Exact harness depends on existing test helpers in api/.)
    }

    #[tokio::test]
    async fn info_returns_disabled_when_no_capture() {
        let state = make_test_state_with_capture_none();
        let response = capture_info_handler(axum::extract::State(state)).await;
        // Assert response is {"enabled": false} or 404, depending on policy.
    }
}
```

Decide policy: if capture is `None` (disabled), do we return 404 (not configured) or 200 with `{"enabled": false}`? Per the contract's spirit (operator wants to know status), `200 + {"enabled": false}` is friendlier. Implement that.

- [ ] **Step 2: Implement the handler**

```rust
pub async fn capture_info_handler(
    State(state): State<ApiState>,
) -> impl IntoResponse {
    match &state.capture {
        Some(c) => (StatusCode::OK, Json(c.info())).into_response(),
        None => (
            StatusCode::OK,
            Json(serde_json::json!({
                "enabled": false,
            })),
        ).into_response(),
    }
}
```

- [ ] **Step 3: Wire the route in `api/src/lib.rs`**

```rust
.route("/debug/p2p-capture/info", get(handlers::capture_info_handler))
```

- [ ] **Step 4: Run + pass**

- [ ] **Step 5: Commit**

```
git commit -m "feat(api): /debug/p2p-capture/info handler"
```

---

## Task 3: `GET /debug/p2p-capture/dump` handler (with filters)

**Files:**
- Modify: `api/src/handlers.rs`
- Modify: `api/src/lib.rs` (route registration)

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn dump_returns_pcap_bytes() {
    let mock = Arc::new(MockCapture {
        // Configure dump to return some fake pcap bytes
        dump_response: vec![0xA1, 0xB2, 0xC3, 0xD4, /* ... */],
    });
    let state = make_test_state_with_capture(mock);
    let response = capture_dump_handler(
        axum::extract::State(state),
        axum::extract::Query(DumpQuery::default()),
    ).await.into_response();
    let body = ... /* collect body */;
    assert_eq!(&body[0..4], &[0xA1, 0xB2, 0xC3, 0xD4]);
    let headers = response.headers();
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/vnd.tcpdump.pcap"
    );
    assert!(headers.get("content-disposition")
        .unwrap()
        .to_str().unwrap()
        .contains("attachment"));
}

#[tokio::test]
async fn dump_passes_filters_through() {
    // Setup mock that records what filter it was called with;
    // assert query params translated correctly.
}

#[tokio::test]
async fn dump_404_when_capture_disabled() {
    let state = make_test_state_with_capture_none();
    let response = capture_dump_handler(
        axum::extract::State(state),
        axum::extract::Query(DumpQuery::default()),
    ).await.into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Implement the handler**

```rust
#[derive(Debug, Default, Deserialize)]
pub struct DumpQuery {
    pub peer: Option<String>,           // multiple ?peer=X&peer=Y? axum needs Vec; use #[serde(default)]
    pub since_secs: Option<u64>,
    pub direction: Option<String>,      // "inbound" | "outbound"
}

pub async fn capture_dump_handler(
    State(state): State<ApiState>,
    Query(query): Query<DumpQuery>,
) -> impl IntoResponse {
    let capture = match &state.capture {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "capture-disabled"})),
            ).into_response();
        }
    };

    let mut filter = ergo_p2p::capture::DumpFilter::default();
    if let Some(peer_str) = &query.peer {
        match peer_str.parse() {
            Ok(ip) => filter.peer = Some(ip),
            Err(_) => return bad_request("peer must be a valid IP address"),
        }
    }
    filter.since_secs = query.since_secs;
    filter.direction = match query.direction.as_deref() {
        None => None,
        Some("inbound") => Some(ergo_p2p::capture::Direction::Inbound),
        Some("outbound") => Some(ergo_p2p::capture::Direction::Outbound),
        Some(other) => return bad_request(&format!("direction must be inbound|outbound (got {})", other)),
    };

    let pcap_bytes = capture.dump(&filter);
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let filename = format!("p2p-capture-{now}.pcap");

    (
        StatusCode::OK,
        [
            ("content-type", "application/vnd.tcpdump.pcap"),
            ("content-disposition", &format!("attachment; filename=\"{filename}\"")),
        ],
        pcap_bytes,
    ).into_response()
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "bad-request", "message": msg})),
    ).into_response()
}
```

(Adapt to the existing handlers.rs style; the above is a sketch.)

- [ ] **Step 3: Register the route**

```rust
.route("/debug/p2p-capture/dump", get(handlers::capture_dump_handler))
```

- [ ] **Step 4: Run + pass**

- [ ] **Step 5: Commit**

```
git commit -m "feat(api): /debug/p2p-capture/dump handler with filters"
```

---

## Task 4: `POST /debug/p2p-capture/reset` handler

**Files:**
- Modify: `api/src/handlers.rs`
- Modify: `api/src/lib.rs`

- [ ] **Step 1: Test**

```rust
#[tokio::test]
async fn reset_calls_capture_reset() {
    let mock = Arc::new(MockCaptureWithResetCounter::default());
    let state = make_test_state_with_capture(mock.clone());
    let response = capture_reset_handler(axum::extract::State(state)).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.reset_count(), 1);
}
```

- [ ] **Step 2: Handler**

```rust
pub async fn capture_reset_handler(
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let capture = match &state.capture {
        Some(c) => c,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "capture-disabled"
        }))).into_response(),
    };
    capture.reset();
    let info = capture.info();
    (StatusCode::OK, Json(serde_json::json!({
        "reset": true,
        "current_generation": info.generation,
    }))).into_response()
}
```

- [ ] **Step 3: Route**

```rust
.route("/debug/p2p-capture/reset", post(handlers::capture_reset_handler))
```

- [ ] **Step 4: Run + commit**

```
git commit -m "feat(api): /debug/p2p-capture/reset handler"
```

---

## Task 5: Final clippy + fmt

- [ ] **Step 1**

```
cargo fmt -p ergo-api
cargo test -p ergo-api
cargo clippy -p ergo-api --all-targets -- -D warnings
```

Fix any lints. Pre-existing issues in OTHER crates remain out of scope.

- [ ] **Step 2: Commit any final fixes**

```
git commit -m "chore(api): final fmt + clippy after p2p-capture handlers"
```

---

## Coordination — what happens after this plan completes

The main session is responsible for the final wiring:

1. Read `[debug.p2p_capture]` from `ergo.toml` in `src/main.rs` (or `src/config.rs`).
2. If enabled, call `p2p::capture::init(resolved_config)?` to obtain a `CaptureHandle`.
3. Pass `Some(handle.tap())` to the p2p transport constructor.
4. Pass `Some(Arc::new(handle) as Arc<dyn CaptureAccess>)` to `ApiState` construction.
5. Smoke test: start the node with debug capture enabled, generate P2P traffic, `curl http://127.0.0.1:9053/debug/p2p-capture/info`, verify the response.

This plan does NOT do the integration. Surface the trait wiring you need (which it expects to find at `ergo_p2p::capture::CaptureAccess`); the main session will do the connect.

## Coordination back-channel (kitty)

When done (or blocked), report back to the main session window via
`kitty @ send-text --match=id:9 '...summary...' && kitty @ send-text --match=id:9 $'\r'`.

**Capture the main session's window ID at dispatch time** — see
`feedback_kitty_id_dynamic` in project memory. The dispatcher
substitutes the live id when sending the prompt instruction.

Single send-text + single `$'\r'` for the report. Multi-chunk
reports have been observed to drop the second submit.

## Self-Review

**Spec coverage** (against `facts/p2p-capture.md` § API endpoints):
- `GET /debug/p2p-capture/info` → Task 2
- `GET /debug/p2p-capture/dump` (with peer/since_secs/direction filters) → Task 3
- `POST /debug/p2p-capture/reset` → Task 4
- Loopback-only bind → inherited from existing `/debug/` policy in api/

**Out of scope** (intentionally):
- Streaming the dump body — v1 returns a single `Vec<u8>`. For 1 GB rings this is a noticeable memory blip; v2 can add streaming via `Body::from_stream`.
- Auto-dump triggers (on disconnect / on shutdown) — those are p2p-side concerns, not api-side, and explicitly v2 in the contract.

**Placeholder scan**: No "TODO" / "implement later" in step content.
The mock `CaptureAccess` is sketched; the dispatched session fills
in the exact shape based on the existing test patterns in `api/src/handlers.rs`.

**Type consistency**: `CaptureAccess`, `CaptureInfo`, `DumpFilter`,
`Direction` referenced uniformly. All imported from `ergo_p2p::capture`.

## Open implementation questions

1. **Behavior when capture is None**: 200 with `{"enabled": false}` for `/info`, but 404 for `/dump` and `/reset`. Defensible: info is a status probe, dump/reset are operations that require an enabled subsystem. Decided in this plan but worth confirming during review.
2. **Multi-`peer` query parameter**: the contract example shows the query taking `peer=X` once. Should we support multiple `?peer=X&peer=Y`? axum can handle `Vec<String>` if so. v1: single peer only (simpler); v2 add multi-peer if operators ask.
3. **`Content-Length` for dumps**: large pcap dumps benefit from a Content-Length header so clients know progress. The v1 in-memory dump knows its full size at send time, so trivially settable.
