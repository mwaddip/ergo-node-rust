//! Operator-only stats endpoint.
//!
//! Spec: `facts/stats.md`. Cumulative counters since process start exposed on
//! a separate, loopback-by-default HTTP listener. No auth — the bind address
//! is the security boundary.
//!
//! The `api/` crate owns the cross-crate counter interface (`P2pCountersSource`).
//! The `p2p/` crate implements it in a follow-up dispatch; until then, the
//! `StubP2pCounters` impl in this module returns an all-zero snapshot.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::STATS_VERSION;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Stats endpoint config. Operator opts in by including a `[stats]` section in
/// `ergo.toml`; the main crate parses it into this struct.
#[derive(Clone, Debug)]
pub struct StatsConfig {
    /// Bind address for the operator stats endpoint. Loopback-only by default
    /// per `facts/stats.md`; a non-loopback bind emits a startup WARN.
    pub bind_address: SocketAddr,
}

/// Cross-crate interface for snapshotting P2P traffic counters. The `p2p/`
/// crate provides the real implementation; this crate ships `StubP2pCounters`
/// for use until then and for tests.
pub trait P2pCountersSource: Send + Sync {
    fn snapshot(&self) -> P2pCountersSnapshot;
}

/// Modifier-type keys for the `inv`, `modifier_request`, and `modifier_response`
/// sections of the response. The string emitted in JSON is the snake_case
/// returned by [`Self::key`] — stable across versions per `facts/stats.md`.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub enum ModifierTypeKey {
    /// modifier byte 1 — renders as `"header"`
    Header,
    /// modifier byte 2 — renders as `"transaction"`
    Transaction,
    /// modifier byte 3 — renders as `"block_transactions"`
    BlockTransactions,
    /// modifier byte 4 — renders as `"ad_proofs"`
    AdProofs,
    /// modifier byte 5 — renders as `"extension"`
    Extension,
}

impl ModifierTypeKey {
    /// Every key the contract requires in every modifier-by-type block. The
    /// `/stats/p2p` handler iterates this list so missing keys still render
    /// as zero counters (the contract permits zero values).
    pub const ALL: &'static [ModifierTypeKey] = &[
        Self::Header,
        Self::Transaction,
        Self::BlockTransactions,
        Self::AdProofs,
        Self::Extension,
    ];

    /// JSON key string. Stable across versions.
    pub fn key(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Transaction => "transaction",
            Self::BlockTransactions => "block_transactions",
            Self::AdProofs => "ad_proofs",
            Self::Extension => "extension",
        }
    }
}

/// Snapshot-sync codes 76–81 keyed by descriptive name per `facts/stats.md`.
/// Mapping is stable across versions.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub enum SnapshotCodeKey {
    /// code 76 — renders as `"request_manifest"`
    RequestManifest,
    /// code 77 — renders as `"manifest"`
    Manifest,
    /// code 78 — renders as `"request_subtree"`
    RequestSubtree,
    /// code 79 — renders as `"subtree"`
    Subtree,
    /// code 80 — renders as `"request_utxo_chunk"`
    RequestUtxoChunk,
    /// code 81 — renders as `"utxo_chunk"`
    UtxoChunk,
}

impl SnapshotCodeKey {
    pub const ALL: &'static [SnapshotCodeKey] = &[
        Self::RequestManifest,
        Self::Manifest,
        Self::RequestSubtree,
        Self::Subtree,
        Self::RequestUtxoChunk,
        Self::UtxoChunk,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::RequestManifest => "request_manifest",
            Self::Manifest => "manifest",
            Self::RequestSubtree => "request_subtree",
            Self::Subtree => "subtree",
            Self::RequestUtxoChunk => "request_utxo_chunk",
            Self::UtxoChunk => "utxo_chunk",
        }
    }
}

/// In/out cumulative counters for a single message family.
#[derive(Clone, Copy, Default, Debug)]
pub struct DirectionalCounter {
    pub in_count: u64,
    pub in_bytes: u64,
    pub out_count: u64,
    pub out_bytes: u64,
}

/// Snapshot of all counters at a single moment. Counters are cumulative since
/// `since_unix_seconds`; consumers compute deltas across samples.
#[derive(Clone, Debug)]
pub struct P2pCountersSnapshot {
    /// Unix seconds at which counting started — typically process start. RRD
    /// consumers detect counter resets by comparing this against the previous
    /// sample's value.
    pub since_unix_seconds: u64,
    pub handshake: DirectionalCounter,
    pub get_peers: DirectionalCounter,
    pub peers: DirectionalCounter,
    pub sync_info: DirectionalCounter,
    pub inv_by_modifier: BTreeMap<ModifierTypeKey, DirectionalCounter>,
    pub modifier_request_by_modifier: BTreeMap<ModifierTypeKey, DirectionalCounter>,
    pub modifier_response_by_modifier: BTreeMap<ModifierTypeKey, DirectionalCounter>,
    pub snapshot_by_code: BTreeMap<SnapshotCodeKey, DirectionalCounter>,
    pub unknown: DirectionalCounter,
}

/// Zero-counter source anchored at process start. Used until the real
/// `p2p/` implementation lands.
pub struct StubP2pCounters {
    process_start: u64,
}

impl StubP2pCounters {
    pub fn new() -> Self {
        let process_start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { process_start }
    }
}

impl Default for StubP2pCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl P2pCountersSource for StubP2pCounters {
    fn snapshot(&self) -> P2pCountersSnapshot {
        P2pCountersSnapshot {
            since_unix_seconds: self.process_start,
            handshake: DirectionalCounter::default(),
            get_peers: DirectionalCounter::default(),
            peers: DirectionalCounter::default(),
            sync_info: DirectionalCounter::default(),
            inv_by_modifier: BTreeMap::new(),
            modifier_request_by_modifier: BTreeMap::new(),
            modifier_response_by_modifier: BTreeMap::new(),
            snapshot_by_code: BTreeMap::new(),
            unknown: DirectionalCounter::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Router state + JSON rendering
// ---------------------------------------------------------------------------

/// Axum router state for the stats listener.
#[derive(Clone)]
pub struct StatsState {
    pub counters: Arc<dyn P2pCountersSource>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatsResponse {
    stats_version: &'static str,
    since: u64,
    messages: MessagesBlock,
}

#[derive(Serialize)]
struct MessagesBlock {
    handshake: DirectionalJson,
    get_peers: DirectionalJson,
    peers: DirectionalJson,
    sync_info: DirectionalJson,
    inv: serde_json::Map<String, serde_json::Value>,
    modifier_request: serde_json::Map<String, serde_json::Value>,
    modifier_response: serde_json::Map<String, serde_json::Value>,
    snapshot: serde_json::Map<String, serde_json::Value>,
    unknown: DirectionalJson,
}

#[derive(Serialize, Default, Clone, Copy)]
struct DirectionalJson {
    #[serde(rename = "in")]
    inbound: DirectionJson,
    #[serde(rename = "out")]
    outbound: DirectionJson,
}

#[derive(Serialize, Default, Clone, Copy)]
struct DirectionJson {
    count: u64,
    bytes: u64,
}

impl From<DirectionalCounter> for DirectionalJson {
    fn from(c: DirectionalCounter) -> Self {
        Self {
            inbound: DirectionJson {
                count: c.in_count,
                bytes: c.in_bytes,
            },
            outbound: DirectionJson {
                count: c.out_count,
                bytes: c.out_bytes,
            },
        }
    }
}

fn render_by_modifier(
    map: &BTreeMap<ModifierTypeKey, DirectionalCounter>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for k in ModifierTypeKey::ALL {
        let c = map.get(k).copied().unwrap_or_default();
        out.insert(
            k.key().to_string(),
            serde_json::to_value(DirectionalJson::from(c)).expect("DirectionalJson serializes"),
        );
    }
    out
}

fn render_by_snapshot_code(
    map: &BTreeMap<SnapshotCodeKey, DirectionalCounter>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for k in SnapshotCodeKey::ALL {
        let c = map.get(k).copied().unwrap_or_default();
        out.insert(
            k.key().to_string(),
            serde_json::to_value(DirectionalJson::from(c)).expect("DirectionalJson serializes"),
        );
    }
    out
}

fn render_snapshot(snapshot: P2pCountersSnapshot) -> serde_json::Value {
    let body = StatsResponse {
        stats_version: STATS_VERSION,
        since: snapshot.since_unix_seconds,
        messages: MessagesBlock {
            handshake: snapshot.handshake.into(),
            get_peers: snapshot.get_peers.into(),
            peers: snapshot.peers.into(),
            sync_info: snapshot.sync_info.into(),
            inv: render_by_modifier(&snapshot.inv_by_modifier),
            modifier_request: render_by_modifier(&snapshot.modifier_request_by_modifier),
            modifier_response: render_by_modifier(&snapshot.modifier_response_by_modifier),
            snapshot: render_by_snapshot_code(&snapshot.snapshot_by_code),
            unknown: snapshot.unknown.into(),
        },
    };
    serde_json::to_value(body).expect("StatsResponse serializes")
}

// ---------------------------------------------------------------------------
// Handler + router + server
// ---------------------------------------------------------------------------

pub async fn get_stats_p2p(State(state): State<StatsState>) -> Json<serde_json::Value> {
    Json(render_snapshot(state.counters.snapshot()))
}

pub fn stats_router(state: StatsState) -> Router {
    Router::new()
        .route("/stats/p2p", get(get_stats_p2p))
        .with_state(state)
}

/// Emit the contract-mandated WARN if `bind_address` is not loopback. Factored
/// out so the bind decision is testable without binding a real socket.
pub(crate) fn warn_if_non_loopback(bind_address: &SocketAddr) {
    if !bind_address.ip().is_loopback() {
        tracing::warn!(
            bind = %bind_address,
            "stats endpoint exposed on non-loopback"
        );
    }
}

/// Bind and serve the operator stats endpoint until the future is dropped.
pub async fn serve_stats(
    cfg: StatsConfig,
    counters: Arc<dyn P2pCountersSource>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    warn_if_non_loopback(&cfg.bind_address);
    let app = stats_router(StatsState { counters });
    let listener = tokio::net::TcpListener::bind(cfg.bind_address).await?;
    tracing::info!(bind = %cfg.bind_address, "stats endpoint listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeInfo;
    use crate::{JOURNAL_EVENTS_VERSION, STATS_VERSION};
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // /info shape — journalEventsVersion always, statsVersion gated
    // -----------------------------------------------------------------------

    fn sample_info(stats_version: Option<String>) -> NodeInfo {
        NodeInfo {
            name: "ergo-node-rust".into(),
            app_version: "0.5.3".into(),
            network: "mainnet".into(),
            full_height: 0,
            headers_height: 0,
            downloaded_height: 0,
            best_full_header_id: String::new(),
            best_header_id: String::new(),
            state_root: String::new(),
            state_type: "utxo".into(),
            peers_count: 0,
            unconfirmed_count: 0,
            is_mining: false,
            current_time: 0,
            journal_events_version: JOURNAL_EVENTS_VERSION.to_string(),
            stats_version,
        }
    }

    #[test]
    fn info_emits_journal_events_version_unconditionally() {
        let info = sample_info(None);
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["journalEventsVersion"], JOURNAL_EVENTS_VERSION);
    }

    #[test]
    fn info_omits_stats_version_when_disabled() {
        let info = sample_info(None);
        let json = serde_json::to_value(&info).unwrap();
        let obj = json.as_object().expect("NodeInfo serializes as object");
        assert!(
            !obj.contains_key("statsVersion"),
            "statsVersion must be absent when stats endpoint is disabled"
        );
    }

    #[test]
    fn info_includes_stats_version_when_enabled() {
        let info = sample_info(Some(STATS_VERSION.to_string()));
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["statsVersion"], STATS_VERSION);
        assert_eq!(json["journalEventsVersion"], JOURNAL_EVENTS_VERSION);
    }

    // -----------------------------------------------------------------------
    // /stats/p2p with the stub counter source — full shape, all zeros
    // -----------------------------------------------------------------------

    fn assert_directional_zero(v: &serde_json::Value, path: &str) {
        let inbound = &v["in"];
        let outbound = &v["out"];
        assert_eq!(inbound["count"], 0, "{path}.in.count");
        assert_eq!(inbound["bytes"], 0, "{path}.in.bytes");
        assert_eq!(outbound["count"], 0, "{path}.out.count");
        assert_eq!(outbound["bytes"], 0, "{path}.out.bytes");
    }

    #[test]
    fn stats_p2p_stub_renders_full_shape_with_zeros() {
        let counters = Arc::new(StubP2pCounters::new());
        let state = StatsState { counters };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let Json(body) = rt.block_on(get_stats_p2p(State(state)));

        assert_eq!(body["statsVersion"], STATS_VERSION);
        let since = body["since"].as_u64().expect("since is u64");
        assert!(
            since > 0,
            "since must be populated with process-start unix seconds"
        );

        let messages = &body["messages"];
        for top in ["handshake", "get_peers", "peers", "sync_info", "unknown"] {
            assert_directional_zero(&messages[top], top);
        }

        for parent in ["inv", "modifier_request", "modifier_response"] {
            for child in ModifierTypeKey::ALL.iter().map(|k| k.key()) {
                let path = format!("{parent}.{child}");
                assert_directional_zero(&messages[parent][child], &path);
            }
        }

        for child in SnapshotCodeKey::ALL.iter().map(|k| k.key()) {
            let path = format!("snapshot.{child}");
            assert_directional_zero(&messages["snapshot"][child], &path);
        }
    }

    // -----------------------------------------------------------------------
    // Non-loopback bind WARN — capture tracing output
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriterHandle;
        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriterHandle(self.0.clone())
        }
    }

    struct CaptureWriterHandle(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriterHandle {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_warn<F: FnOnce()>(f: F) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let guard = buf.lock().unwrap();
        String::from_utf8_lossy(&guard).into_owned()
    }

    #[test]
    fn non_loopback_bind_emits_warn_line() {
        let addr: SocketAddr = "0.0.0.0:9055".parse().unwrap();
        let captured = capture_warn(|| warn_if_non_loopback(&addr));
        assert!(
            captured.contains("stats endpoint exposed on non-loopback"),
            "expected contract marker; got: {captured}"
        );
        assert!(
            captured.contains("bind=0.0.0.0:9055"),
            "expected bind= field; got: {captured}"
        );
    }

    #[test]
    fn loopback_bind_does_not_warn() {
        let addr: SocketAddr = "127.0.0.1:9055".parse().unwrap();
        let captured = capture_warn(|| warn_if_non_loopback(&addr));
        assert!(
            captured.is_empty(),
            "loopback bind must not emit anything; got: {captured}"
        );
    }
}
