use axum::extract::State;
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

use super::ApiContext;

/// GET /api/v1/debug/memory response — where the indexer process's memory
/// goes. Mirrors the node's `/debug/memory` shape so operators can correlate.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DebugMemory {
    pub process: ProcessMemory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jemalloc: Option<JemallocMemory>,
    pub components: ComponentMemory,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessMemory {
    pub rss_anon_bytes: u64,
    pub rss_file_bytes: u64,
    pub rss_total_bytes: u64,
    pub rss_peak_bytes: u64,
    pub vm_size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pss_bytes: Option<u64>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JemallocMemory {
    pub allocated_bytes: u64,
    pub active_bytes: u64,
    pub resident_bytes: u64,
    pub retained_bytes: u64,
    pub metadata_bytes: u64,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComponentMemory {
    /// Highest fully-indexed block height. Approximate proxy for how
    /// much state the indexer has committed to disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_height: Option<u64>,
    /// DB backend name: "sqlite" or "postgres".
    pub db_backend: String,
    /// Total bytes the SQLite .db file occupies on disk. None for
    /// postgres (lives server-side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_on_disk_bytes: Option<u64>,
    /// Per-connection page cache ceiling. Multiply by `dbConnections`
    /// for a worst-case in-process cache footprint. None for postgres.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_cache_bytes_per_conn: Option<u64>,
    /// Open DB connections held by the indexer process.
    pub db_connections: u32,
}

#[utoipa::path(
    get,
    path = "/api/v1/debug/memory",
    responses((status = 200, body = DebugMemory))
)]
pub async fn get_debug_memory(State(ctx): State<ApiContext>) -> Json<DebugMemory> {
    let process = read_proc_memory();
    let jemalloc = read_jemalloc();
    let db_stats = ctx.db.memory_stats().await;
    let indexed_height = ctx.db.get_indexed_height().await.ok().flatten();

    let components = ComponentMemory {
        indexed_height,
        db_backend: db_stats.backend.to_string(),
        db_on_disk_bytes: db_stats.on_disk_bytes,
        db_cache_bytes_per_conn: db_stats.cache_bytes_per_conn,
        db_connections: db_stats.connection_count,
    };

    Json(DebugMemory {
        process,
        jemalloc,
        components,
    })
}

/// Read anon/file/peak RSS and VmSize from `/proc/self/status`, PSS from
/// `/proc/self/smaps_rollup`. All fields fall back to 0 on parse failure.
/// Mirrors the node's `read_proc_memory` so operators can compare the two
/// directly.
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

#[cfg(feature = "jemalloc")]
fn read_jemalloc() -> Option<JemallocMemory> {
    use tikv_jemalloc_ctl::{epoch, stats};
    // jemalloc caches stats per epoch; advance() refreshes them. Without
    // this, repeated reads return the same snapshot until the next epoch
    // tick (which is internal, infrequent).
    let _ = epoch::advance();
    Some(JemallocMemory {
        allocated_bytes: stats::allocated::read().unwrap_or(0) as u64,
        active_bytes: stats::active::read().unwrap_or(0) as u64,
        resident_bytes: stats::resident::read().unwrap_or(0) as u64,
        retained_bytes: stats::retained::read().unwrap_or(0) as u64,
        metadata_bytes: stats::metadata::read().unwrap_or(0) as u64,
    })
}

#[cfg(not(feature = "jemalloc"))]
fn read_jemalloc() -> Option<JemallocMemory> {
    None
}
