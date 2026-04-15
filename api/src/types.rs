use serde::Serialize;

/// Standard error response matching the JVM node's format.
#[derive(Serialize)]
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

