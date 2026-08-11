mod handlers;
pub mod stats;
pub mod types;

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use axum::Router;
use ergo_mempool::Mempool;
use ergo_mining::CandidateGenerator;
use ergo_validation::ErgoStateContext;
use tokio::sync::{Mutex, RwLock};

pub use stats::{
    DirectionalCounter, ModifierTypeKey, P2pCountersSnapshot, P2pCountersSource, SnapshotCodeKey,
    StatsConfig, StubP2pCounters,
};

/// Version of the journal-events contract this build promises.
/// Bumped atomically with `facts/journal-events.md`.
pub const JOURNAL_EVENTS_VERSION: &str = "1.3";

/// Version of the operator stats endpoint schema this build promises.
/// Bumped atomically with `facts/stats.md`.
pub const STATS_VERSION: &str = "1.0";

/// Modifier delivered into the validation pipeline.
/// Tuple: `(type_id, modifier_id, raw_bytes, optional_height_hint)`.
pub type ModifierBatchItem = (u8, [u8; 32], Vec<u8>, Option<u64>);

/// Shared state available to all API handlers.
///
/// Constructed by the main crate from its existing shared handles.
/// The API crate never creates these resources — it only reads them.
#[derive(Clone)]
pub struct ApiState {
    /// Header chain — trait methods handle their own locking internally.
    pub chain: Arc<dyn ChainAccess>,
    /// Block storage — full block sections by header ID.
    pub store: Arc<dyn StoreAccess>,
    /// Mempool — transaction submission and queries.
    pub mempool: Arc<Mutex<Mempool>>,
    /// UTXO state — box lookups by ID (read-only snapshot reader).
    pub utxo_reader: Arc<dyn UtxoAccess>,
    /// Current state context for transaction validation.
    /// None during initial sync before first block is validated.
    pub state_context: Arc<RwLock<Option<ErgoStateContext>>>,
    /// Peer count callback — returns (connected, known) counts.
    pub peer_count: Arc<dyn Fn() -> PeerCounts + Send + Sync>,
    /// Node metadata (immutable after construction — Arc avoids cloning 4 Strings per request).
    pub node_info: Arc<NodeMeta>,
    /// Mining state — None if mining is not configured or node is in digest mode.
    pub mining: Option<Arc<CandidateGenerator>>,
    /// Block submitter — handles mined block application + broadcast.
    pub block_submitter: Option<Arc<dyn BlockSubmitter>>,
    /// Last fully validated block height (updated by the validator).
    pub validated_height: Arc<AtomicU32>,
    /// Highest height with all block sections downloaded (updated by sync layer).
    pub downloaded_height: Arc<AtomicU32>,
    /// Highest header height announced by any peer in a `SyncInfo` since
    /// process start (updated by the sync layer). Monotonic high-water mark —
    /// never decreases, including on peer disconnect or reorg. `0` means no
    /// `SyncInfo` has been parsed yet; `/info` reports that as an absent
    /// field rather than height 0. Peer-supplied and unverified — advisory
    /// display data only, never an input to a consensus, validation, or
    /// storage decision.
    pub max_peer_height: Arc<AtomicU32>,
    /// Peer REST URL callback — returns connected peers with their socket addr and REST URL.
    pub peer_api_urls: Arc<dyn Fn() -> Vec<PeerRestInfo> + Send + Sync>,
    /// All known peers (connected + disconnected). For `GET /peers/all`.
    pub peer_all: Arc<dyn Fn() -> Vec<PeerInfo> + Send + Sync>,
    /// Network status summary (last incoming msg time + current network time).
    pub peer_status: Arc<dyn Fn() -> PeerStatusSummary + Send + Sync>,
    /// Currently penalty-banned peers. For `GET /peers/blacklisted`.
    pub peer_blacklisted: Arc<dyn Fn() -> Vec<std::net::SocketAddr> + Send + Sync>,
    /// Manual outbound-connect trigger. For `POST /peers/connect`.
    /// Returns `Err(reason)` if the address is rejected (malformed, banned, already connecting).
    pub peer_connect: Arc<dyn Fn(std::net::SocketAddr) -> Result<(), String> + Send + Sync>,
    /// Available UTXO snapshot manifests as (height, manifest_digest) pairs.
    /// For `GET /utxo/getSnapshotsInfo`. Returns empty vec when none stored.
    pub snapshots_info: Arc<dyn Fn() -> Vec<SnapshotInfoEntry> + Send + Sync>,
    /// Optional API key hash (Blake2b256). None = no auth required.
    /// Authenticated endpoints check this against the `api_key` request header.
    pub api_key_hash: Option<[u8; 32]>,
    /// Modifier pipeline sender — for the /ingest/modifiers endpoint.
    pub modifier_tx: Option<tokio::sync::mpsc::Sender<ModifierBatchItem>>,
    /// Watch channel for validated block height — used by /info/wait long-poll.
    pub height_watch: tokio::sync::watch::Receiver<u32>,
    /// Jemalloc stats probe. Populated by the main crate when built with
    /// jemalloc; None with mimalloc or system allocator. The `/debug/memory`
    /// handler calls this to read live allocator counters.
    pub jemalloc_probe: Option<Arc<dyn Fn() -> JemallocSnapshot + Send + Sync>>,
    /// Whether the operator stats endpoint is enabled. Overwritten by
    /// `serve()` based on its `stats_config` + `p2p_counters` arguments;
    /// callers should leave this `false`. When `true`, `/info` emits
    /// `statsVersion`.
    pub stats_enabled: bool,
    /// Optional p2p capture access. `None` when `[debug.p2p_capture]` is
    /// disabled in the operator config; in that case the
    /// `/debug/p2p-capture/*` handlers return a disabled-shaped response
    /// (200 `{"enabled": false}` for `/info`, 404 for `/dump` and `/reset`).
    pub capture: Option<Arc<dyn enr_p2p::capture::CaptureAccess>>,
}

/// Snapshot of jemalloc stats at a moment in time. The probe calls
/// `epoch::advance()` then reads each stat. Byte-valued fields are u64.
#[derive(Clone, Copy, Default)]
pub struct JemallocSnapshot {
    pub allocated: u64,
    pub active: u64,
    pub resident: u64,
    pub retained: u64,
    pub metadata: u64,
}

/// Static node metadata.
#[derive(Clone)]
pub struct NodeMeta {
    pub name: String,
    pub version: String,
    pub network: String,
    pub state_type: String,
    /// Unix epoch ms at which this process began serving. Constant for the
    /// process lifetime — consumers derive uptime as `currentTime - launchTime`.
    pub launch_time: u64,
}

/// Peer count summary.
pub struct PeerCounts {
    pub connected: usize,
}

/// Per-peer REST info returned by the peer_api_urls callback.
pub struct PeerRestInfo {
    pub peer_id: u64,
    pub addr: std::net::SocketAddr,
    pub rest_url: Option<String>,
}

/// Per-peer info returned by the `peer_all` / `peer_connected` callbacks.
/// `name` is the peer's advertised agent string (None if never handshaked).
/// `last_seen` is Unix epoch ms (None if never connected).
/// `connection_type` is "Outgoing" / "Incoming" for connected peers, None for known-but-disconnected.
pub struct PeerInfo {
    pub address: std::net::SocketAddr,
    pub name: Option<String>,
    pub last_seen: Option<u64>,
    pub connection_type: Option<String>,
}

/// Network status summary for `GET /peers/status`.
/// Both timestamps are Unix epoch milliseconds.
pub struct PeerStatusSummary {
    pub last_incoming_message: Option<u64>,
    pub current_network_time: u64,
}

/// Snapshot inventory entry for `GET /utxo/getSnapshotsInfo`.
pub struct SnapshotInfoEntry {
    pub height: u32,
    pub digest: [u8; 32],
}

/// Memory attribution for the header chain's in-process structures, as
/// reported by the crate that owns them.
///
/// Every field is a formula over a live count or capacity — never a
/// measurement — and it is computed by `chain/`, not here. The API layer
/// transports these numbers into `GET /debug/memory`; it must never derive
/// them from a local constant describing another crate's internals. The
/// removed `chainHeaderEstimateBytes` did exactly that (`header_count * 800`
/// for a `Vec<Header>` that `chain/` retired in Phase 3) and reported 1.48 GB
/// for a structure that no longer existed.
///
/// Deliberately api-local: this crate does not depend on `enr-chain`, and it
/// does not acquire that dependency just to share a struct. The main crate's
/// adapter maps `chain/`'s `ChainMemoryEstimate` onto this type — that mapping
/// is the integration seam, not licence to reintroduce sizing arithmetic here.
/// See `facts/api.md` § "Component memory attribution".
pub struct ChainMemory {
    /// `HeaderChain::by_id` (BlockId → height). Unbounded — grows with chain
    /// length for the life of the node and is never evicted.
    pub index_bytes: u64,
    /// `LazyHeaderStore` header LRU, current occupancy. Bounded by the
    /// configured cache capacity.
    pub header_cache_bytes: u64,
    /// `LazyHeaderStore` cumulative-score LRU, current occupancy. Bounded by
    /// the configured cache capacity.
    pub score_cache_bytes: u64,
}

/// Trait for chain access — avoids API depending on enr-chain internals.
/// Implementations handle their own locking.
pub trait ChainAccess: Send + Sync {
    fn height(&self) -> u32;
    fn header_at(&self, height: u32) -> Option<ergo_chain_types::Header>;
    fn header_by_id(&self, id: &[u8; 32]) -> Option<ergo_chain_types::Header>;
    fn tip(&self) -> Option<ergo_chain_types::Header>;

    /// Build a NiPoPoW proof anchored at the tip (when `header_id` is `None`)
    /// or at the given header.
    ///
    /// The implementation is expected to call
    /// `enr_chain::nipopow_proof::build_nipopow_proof`. Returns serialized
    /// proof bytes (NipopowProof scorex wire format) or a human-readable
    /// reason on failure. The reason text is propagated to the client as the
    /// HTTP error body.
    ///
    /// Caller must validate that `header_id` exists in the chain *before*
    /// invoking this — chain-level bounds errors (`m`/`k` out of range,
    /// chain too short, MissingPopowHeader) are returned in `Err` and the
    /// handler maps them to 400. Unknown header_id is mapped to 404 via the
    /// caller's pre-check.
    fn build_nipopow_proof(
        &self,
        m: u32,
        k: u32,
        header_id: Option<[u8; 32]>,
    ) -> Result<Vec<u8>, String>;

    /// Header IDs from the chain tip, newest first.
    ///
    /// `offset` skips that many positions from the tip; `limit` caps the result
    /// length. If `offset >= height`, returns an empty vec. If fewer headers
    /// remain than `limit`, returns what's available (not an error).
    fn header_ids(&self, offset: u32, limit: u32) -> Vec<[u8; 32]>;

    /// Serialized `PoPowHeader` for the given header ID (Scorex wire format).
    ///
    /// The handler deserializes via `ergo_nipopow::PoPowHeader::scorex_parse_bytes`
    /// and re-serializes as JVM-compatible JSON. Mirrors the
    /// `build_nipopow_proof` pattern of returning raw bytes.
    ///
    /// - `Ok(Some(bytes))` — header found, PoPowHeader constructed.
    /// - `Ok(None)` — header not in chain (handler returns 404).
    /// - `Err(reason)` — chain-internal failure (e.g., extension missing for an interlink ancestor); handler returns 500.
    fn popow_header_by_id(&self, id: &[u8; 32]) -> Result<Option<Vec<u8>>, String>;

    /// Memory attribution for the chain's in-process structures.
    ///
    /// The implementor computes these figures; `GET /debug/memory` reports
    /// them verbatim and performs no arithmetic on them. Deliberately has no
    /// default body — a default would let an implementor that forgot to
    /// override it silently report zeros, which is precisely how the wrong
    /// `chainHeaderEstimateBytes` survived unnoticed.
    fn memory_estimate(&self) -> ChainMemory;
}

/// Trait for block store access.
pub trait StoreAccess: Send + Sync {
    /// Get raw modifier bytes by type ID and modifier ID.
    fn get(&self, type_id: u8, id: &[u8; 32]) -> Option<Vec<u8>>;
    /// Get raw modifier bytes by type ID and block height.
    /// Looks up the modifier ID at that height first, then fetches the data.
    fn get_at_height(&self, type_id: u8, height: u32) -> Option<Vec<u8>>;

    /// `modifiers.redb` page cache occupancy, `None` when unavailable.
    ///
    /// NEVER return 0 for "unknown" — a zero asserts the cache is empty, the
    /// same class of falsehood as the removed `chainHeaderEstimateBytes`.
    /// `GET /debug/memory` omits the field entirely when this is `None`.
    ///
    /// Deliberately has no default body: a default would let an implementor
    /// that forgot to override it silently report a wrong value, which is
    /// structurally how `AVG_HEADER_BYTES` survived four months describing a
    /// struct that had been deleted. A compile error at every implementation
    /// site is the point.
    fn cache_bytes_used(&self) -> Option<u64>;

    /// Cumulative `modifiers.redb` cache evictions, `None` when unavailable.
    ///
    /// A rising count is the signal that the cache is undersized — otherwise
    /// an undersized cache is indistinguishable from a comfortably large one.
    /// No default body, for the reason given on [`StoreAccess::cache_bytes_used`].
    fn cache_evictions(&self) -> Option<u64>;
}

/// Trait for UTXO lookups.
pub trait UtxoAccess: Send + Sync {
    /// Look up a box by its ID in the confirmed UTXO set.
    fn box_by_id(&self, box_id: &[u8; 32]) -> Option<ergo_validation::ErgoBox>;

    /// `state.redb` page cache occupancy, `None` when unavailable.
    ///
    /// NEVER return 0 for "unknown". No default body, for the reason given on
    /// [`StoreAccess::cache_bytes_used`].
    fn cache_bytes_used(&self) -> Option<u64>;
}

/// Trait for submitting locally-mined blocks to the node's processing pipeline.
///
/// Implementations are responsible for storing the block sections, advancing
/// the chain, and broadcasting to peers. Called by the mining solution handler
/// after a valid PoW solution is received.
pub trait BlockSubmitter: Send + Sync {
    /// Submit a freshly mined block. The header is the assembled Header (with
    /// PoW solution). The byte slices are the raw wire-format sections.
    fn submit(
        &self,
        header: ergo_chain_types::Header,
        block_txs_bytes: Vec<u8>,
        ad_proofs_bytes: Vec<u8>,
        extension_bytes: Vec<u8>,
    ) -> Result<(), String>;
}

/// Build the axum Router with all API routes.
pub fn router(state: ApiState) -> Router {
    use axum::routing::{get, head, post};

    Router::new()
        // Node info
        .route("/info", get(handlers::get_info))
        .route("/info/wait", get(handlers::info_wait))
        // Blocks
        .route("/blocks", get(handlers::get_blocks))
        .route(
            "/blocks/at/{height}",
            get(handlers::get_block_ids_at_height),
        )
        .route(
            "/blocks/modifier/{modifier_id}",
            get(handlers::get_block_modifier),
        )
        .route(
            "/blocks/lastHeaders/{count}",
            get(handlers::get_last_headers),
        )
        .route("/blocks/{header_id}", get(handlers::get_full_block))
        .route(
            "/blocks/{header_id}/header",
            get(handlers::get_block_header),
        )
        .route(
            "/blocks/{header_id}/transactions",
            get(handlers::get_block_transactions),
        )
        .route(
            "/blocks/{header_id}/validation-fragments",
            get(handlers::get_block_validation_fragments),
        )
        // Transactions
        .route("/transactions", post(handlers::post_transaction))
        .route("/transactions/check", post(handlers::check_transaction))
        .route("/transactions/unconfirmed", get(handlers::get_unconfirmed))
        .route(
            "/transactions/unconfirmed/{tx_id}",
            head(handlers::head_unconfirmed),
        )
        .route(
            "/transactions/unconfirmed/transactionIds",
            get(handlers::get_unconfirmed_ids),
        )
        .route(
            "/transactions/unconfirmed/byTransactionId/{tx_id}",
            get(handlers::get_unconfirmed_by_id),
        )
        .route("/transactions/getFee", get(handlers::get_recommended_fee))
        .route("/transactions/waitTime", get(handlers::get_wait_time))
        .route(
            "/transactions/poolHistogram",
            get(handlers::get_pool_histogram),
        )
        // UTXO
        .route("/utxo/byId/{box_id}", get(handlers::get_utxo_by_id))
        .route(
            "/utxo/withPool/byId/{box_id}",
            get(handlers::get_utxo_with_pool),
        )
        .route(
            "/utxo/withPool/byIds",
            post(handlers::post_utxo_with_pool_by_ids),
        )
        .route("/utxo/getSnapshotsInfo", get(handlers::get_snapshots_info))
        // Peers
        .route("/peers/all", get(handlers::get_all_peers))
        .route("/peers/connected", get(handlers::get_connected_peers))
        .route("/peers/status", get(handlers::get_peers_status))
        .route("/peers/blacklisted", get(handlers::get_blacklisted_peers))
        .route("/peers/connect", post(handlers::post_peers_connect))
        .route("/peers/api-urls", get(handlers::get_peer_api_urls))
        // Ingest (localhost only)
        .route("/ingest/modifiers", post(handlers::post_ingest_modifiers))
        // Emission
        .route("/emission/at/{height}", get(handlers::get_emission_at))
        // NiPoPoW
        .route("/nipopow/proof/{m}/{k}", get(handlers::get_nipopow_proof))
        .route(
            "/nipopow/proof/{m}/{k}/{header_id}",
            get(handlers::get_nipopow_proof_by_header),
        )
        .route(
            "/nipopow/popowHeader/last",
            get(handlers::get_popow_header_last),
        )
        .route(
            "/nipopow/popowHeader/{header_id}",
            get(handlers::get_popow_header_by_id),
        )
        // Mining
        .route("/mining/candidate", get(handlers::get_mining_candidate))
        .route("/mining/solution", post(handlers::post_mining_solution))
        .route(
            "/mining/rewardAddress",
            get(handlers::get_mining_reward_address),
        )
        // Debug
        .route("/debug/memory", get(handlers::get_debug_memory))
        .route("/debug/p2p-capture/info", get(handlers::get_capture_info))
        .route("/debug/p2p-capture/dump", get(handlers::get_capture_dump))
        .route(
            "/debug/p2p-capture/reset",
            post(handlers::post_capture_reset),
        )
        .with_state(state)
}

/// Start the API server on the given address.
///
/// `stats_config` and `p2p_counters` enable the operator stats endpoint (see
/// `facts/stats.md`). Both must be `Some` for the listener to start and for
/// `/info` to advertise `statsVersion`. If exactly one is `Some` the function
/// logs an error and refuses to start the listener — `/info` omits
/// `statsVersion` in that case.
///
/// Returns a future that runs until the public REST server stops. The stats
/// listener is spawned as a child task and dies with it.
pub async fn serve(
    mut state: ApiState,
    bind_addr: std::net::SocketAddr,
    stats_config: Option<stats::StatsConfig>,
    p2p_counters: Option<Arc<dyn stats::P2pCountersSource>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match (stats_config, p2p_counters) {
        (Some(cfg), Some(counters)) => {
            state.stats_enabled = true;
            tokio::spawn(async move {
                if let Err(e) = stats::serve_stats(cfg, counters).await {
                    tracing::error!(error = %e, "stats endpoint failed");
                }
            });
        }
        (Some(_), None) | (None, Some(_)) => {
            tracing::error!(
                "stats endpoint misconfigured: stats_config and p2p_counters must \
                 both be provided or both omitted; stats listener disabled"
            );
            state.stats_enabled = false;
        }
        (None, None) => {
            state.stats_enabled = false;
        }
    }

    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(bind = %bind_addr, "REST API listening");
    axum::serve(listener, app).await?;
    Ok(())
}
