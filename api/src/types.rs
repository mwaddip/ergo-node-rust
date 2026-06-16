use serde::Serialize;

/// Standard error response matching the JVM node's format.
#[derive(Serialize, Debug)]
pub struct ApiError {
    pub error: u16,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// GET /info response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub name: String,
    pub app_version: String,
    pub network: String,
    pub full_height: u32,
    pub headers_height: u32,
    pub downloaded_height: u32,
    pub best_full_header_id: String,
    pub best_header_id: String,
    pub state_root: String,
    pub state_type: String,
    pub peers_count: usize,
    pub unconfirmed_count: usize,
    pub is_mining: bool,
    pub current_time: u64,
    /// Always present — see `facts/journal-events.md`.
    pub journal_events_version: String,
    /// Present only when the operator stats endpoint is enabled — see `facts/stats.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_version: Option<String>,
}

/// GET /emission/at/{height} response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmissionInfo {
    pub miner_reward: u64,
    pub total_coins_issued: u64,
    pub total_remain_coins: u64,
}

/// Fee recommendation response.
#[derive(Serialize)]
pub struct FeeResponse {
    pub fee: u64,
}

/// GET /peers/api-urls response entry.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerApiUrl {
    pub peer_id: u64,
    pub url: String,
}

/// Single peer entry for GET /peers/all and GET /peers/connected.
/// `connection_type` is "Outgoing" / "Incoming" for connected peers, null otherwise.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfoEntry {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    pub connection_type: Option<String>,
}

/// GET /peers/status response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatus {
    /// Unix epoch ms of the last incoming P2P message; null if none yet.
    pub last_incoming_message: Option<u64>,
    /// Current local clock (Unix epoch ms).
    pub current_network_time: u64,
}

/// GET /peers/blacklisted response.
#[derive(Serialize)]
pub struct PeersBlacklisted {
    /// Penalty-banned peer addresses as "host:port".
    pub addresses: Vec<String>,
}

/// Single entry inside GET /utxo/getSnapshotsInfo's `availableManifests`.
#[derive(Serialize)]
pub struct SnapshotManifestEntry {
    pub height: u32,
    /// Hex-encoded Blake2b256 manifest digest.
    pub digest: String,
}

/// GET /utxo/getSnapshotsInfo response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotsInfo {
    pub available_manifests: Vec<SnapshotManifestEntry>,
}

/// GET /debug/memory response — breakdown of where the process's memory goes.
///
/// Helps answer "do we actually need everything that's resident?" Combines
/// kernel-reported process-level counters with allocator-internal stats (when
/// built with jemalloc) and a few coarse in-process accounts (chain, mempool).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugMemory {
    /// Kernel view: `/proc/self/status` + `smaps_rollup`.
    pub process: ProcessMemory,
    /// Allocator view: `jemalloc.stats.*`. Null when not built with jemalloc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jemalloc: Option<JemallocMemory>,
    /// Ergo component accounts (approximate).
    pub components: ComponentMemory,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessMemory {
    /// Anonymous resident pages (heap). This is what the dial targets.
    pub rss_anon_bytes: u64,
    /// File-backed resident pages (binary + mapped files).
    pub rss_file_bytes: u64,
    /// Total resident set (anon + file + shmem).
    pub rss_total_bytes: u64,
    /// Peak resident set since process start (monotonic).
    pub rss_peak_bytes: u64,
    /// Virtual address space committed — much larger than RSS for jemalloc.
    pub vm_size_bytes: u64,
    /// PSS (proportional set size, accounts for shared pages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pss_bytes: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JemallocMemory {
    /// Bytes currently allocated by the application (live heap).
    /// What `flush_heap_threshold_mb` is compared against.
    pub allocated_bytes: u64,
    /// Bytes jemalloc considers "active" — allocated + internal fragmentation
    /// inside allocated extents.
    pub active_bytes: u64,
    /// Resident bytes jemalloc has mapped and backed by physical pages.
    /// `resident - allocated` = jemalloc retention (freed-but-not-returned).
    pub resident_bytes: u64,
    /// Bytes jemalloc holds as "retained" — munmap'd but kept for reuse.
    pub retained_bytes: u64,
    /// Bytes used by jemalloc's own bookkeeping.
    pub metadata_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentMemory {
    /// Estimated bytes held by the in-memory header chain
    /// (`chain.by_height.len() * avg_header_bytes`). Coarse — real headers
    /// vary in size from ~220 bytes to ~1 KB depending on interlink vector.
    pub chain_header_estimate_bytes: u64,
    pub chain_header_count: u32,
    /// Mempool transaction count.
    pub mempool_tx_count: u32,
}

/// `GET /blocks/{headerId}/validation-fragments` response.
///
/// Index-aligned with `/blocks/{headerId}`: `transactions[i]` here pairs
/// with `transactions[i]` there. Clients walk both responses in parallel
/// and pair by index — no id-based lookup required.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationFragments {
    /// Canonical block header bytes, hex-encoded. The byte sequence whose
    /// blake2b256 is the header id.
    pub header_bytes: String,
    /// Parameters parsed from the block's extension. `None` (serialized as
    /// `null`) when the extension's parameter-vote encoding fails to parse —
    /// most commonly for very early v1 blocks. Never substituted with
    /// `Parameters::default()`; the client owns the fallback policy.
    pub parameters: Option<ValidationFragmentsParameters>,
    pub transactions: Vec<ValidationFragmentsTx>,
}

/// Parameters subset surfaced by validation-fragments — only the field the
/// harness compares directly. Add fields here only when an external consumer
/// needs them, not speculatively.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationFragmentsParameters {
    pub max_block_cost: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationFragmentsTx {
    /// `Transaction::bytes_to_sign()` hex-encoded — inputs without
    /// proofs/extensions, then data-inputs, then outputs concatenated. NOT
    /// the full canonical tx bytes. This is what every input's signature
    /// commits to.
    pub signing_message: String,
    /// Full canonical transaction bytes from
    /// `Transaction::sigma_serialize_bytes()`, hex-encoded — each input as
    /// boxId + spending proof + ContextExtension, then data-inputs, then
    /// outputs. The on-chain ContextExtension wire order is preserved
    /// byte-for-byte (NOT sorted), unlike the JSON endpoints which normalize
    /// extension keys ascending. `blake2b256(bytes)` is the transaction id.
    pub bytes: String,
}
