//! ergo-fastsync — fast bootstrap for ergo-node-rust via peer REST API.
//!
//! Fetches headers and blocks from JVM peers over HTTP, validates header PoW,
//! and pushes everything into the running node via POST /ingest/modifiers.
//! The node handles full validation and storage.
//!
//! Based on the "fast header sync" technique from Arkadia Network's ergo-rust
//! implementation (<https://github.com/arkadianet/ergo>). Their approach —
//! fetching headers via the JVM node's `/blocks/chainSlice` REST endpoint and
//! full blocks via `POST /blocks/headerIds` — demonstrated genesis-to-tip
//! header sync in ~5 minutes using parallel peers. This crate adapts that
//! pattern for ergo-node-rust's plugin architecture.

mod fetcher;
mod ingest;
mod pool;
mod types;
mod wire;

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::info;

use crate::ingest::IngestClient;
use crate::pool::PeerPool;
use crate::types::{LocalNodeInfo, PeerApiUrl};

/// Fast bootstrap for ergo-node-rust.
#[derive(Parser)]
#[command(name = "ergo-fastsync", version)]
struct Cli {
    /// Local node REST API URL.
    #[arg(long, default_value = "http://127.0.0.1:9052")]
    node_url: String,

    /// Override peer URL instead of discovering via /peers/api-urls.
    #[arg(long)]
    peer_url: Option<String>,

    /// Stop fetching this many blocks before the peer's tip (hand off to P2P).
    /// Default matches the main node's `fastsync_threshold_blocks` (25,000 ≈
    /// 35 days of chain depth). When spawned by the main node, the threshold
    /// is passed explicitly so both sides agree even if the node's config is
    /// tuned.
    #[arg(long, default_value_t = 25_000)]
    handoff_distance: u32,

    /// Skip block section fetching — only push headers.
    #[arg(long)]
    headers_only: bool,

    /// Headers per chainSlice request (max 16384, default 2000).
    #[arg(long, default_value_t = 2000)]
    chunk_size: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ergo_fastsync=info".into()),
        )
        .init();

    let cli = Cli::parse();
    run(cli).await
}

async fn run(cli: Cli) -> Result<()> {
    let node_url = cli.node_url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // --- Local node state ---
    let node_info: LocalNodeInfo = http
        .get(format!("{node_url}/info"))
        .send()
        .await
        .context("connect to local node")?
        .json()
        .await
        .context("parse local /info")?;

    info!(
        headers = node_info.headers_height,
        validated = node_info.full_height,
        downloaded = node_info.downloaded_height,
        state = %node_info.state_type,
        "local node"
    );

    // --- Build peer pool ---
    // When --peer-url is set, pin to that single peer (no refresh).
    // Otherwise, discover REST peers from the local node and enable
    // mid-fetch refresh so newly-connected peers join the pool.
    let (urls, allow_refresh) = match cli.peer_url {
        Some(url) => (vec![url], false),
        None => {
            let peers: Vec<PeerApiUrl> = http
                .get(format!("{node_url}/peers/api-urls"))
                .send()
                .await
                .context("GET /peers/api-urls")?
                .json()
                .await
                .context("parse /peers/api-urls")?;

            if peers.is_empty() {
                bail!(
                    "no peers with REST API URLs found — wait for the node to \
                     connect to JVM peers, or use --peer-url to specify one"
                );
            }
            info!(count = peers.len(), "discovered peer REST URLs");
            (peers.into_iter().map(|p| p.url).collect(), true)
        }
    };

    let mut pool = PeerPool::new(urls, cli.chunk_size)?;
    if allow_refresh {
        pool = pool.with_node_refresh(node_url.to_string());
    }
    let ingest = IngestClient::new(node_url)?;

    // --- Peer tip ---
    let (peer_tip, _) = pool.query_tip().await?;
    let target = peer_tip.saturating_sub(cli.handoff_distance);

    info!(peer_tip, target, handoff = cli.handoff_distance, "target");

    // --- Phase 1: Headers ---
    let mut headers_pushed = 0u32;
    let mut phase1_elapsed = Duration::ZERO;
    let mut header_ids: Vec<(u32, String)> = Vec::new();

    if target <= node_info.headers_height {
        info!("headers already synced — skipping phase 1");
    } else {
        let start_height = node_info.headers_height + 1;
        info!(from = start_height, to = target, "phase 1: fetching headers");

        let phase1_start = Instant::now();
        let (pushed, ids) = pool
            .fetch_headers(start_height, target, &ingest, !cli.headers_only)
            .await?;
        headers_pushed = pushed;
        header_ids = ids;
        phase1_elapsed = phase1_start.elapsed();

        info!(
            headers = headers_pushed,
            elapsed = format!("{:.1}s", phase1_elapsed.as_secs_f64()),
            "phase 1 complete"
        );
    }

    // --- Phase 2: Block sections ---
    // If headers were already synced, build the ID list for the gap
    // between full_height and target.
    if header_ids.is_empty() && !cli.headers_only {
        // Use downloaded_height (what's actually in the store) rather than
        // full_height (what's validated). After a state wipe, full_height=0
        // but downloaded_height may be much higher if sections persist.
        let sections_from = node_info.downloaded_height.max(node_info.full_height) + 1;
        if sections_from <= target {
            info!(from = sections_from, to = target, "building block ID list for sections gap");
            header_ids = pool.header_ids_for_range(sections_from, target).await?;
        }
    }

    if cli.headers_only || header_ids.is_empty() {
        info!("nothing to fetch — headers and blocks up to date");
        pool.log_stats();
        return Ok(());
    }

    info!(blocks = header_ids.len(), "phase 2: fetching block sections");

    let phase2_start = Instant::now();
    let sections_pushed = pool.fetch_blocks(&header_ids, &ingest).await?;
    let phase2_elapsed = phase2_start.elapsed();

    info!(
        sections = sections_pushed,
        elapsed = format!("{:.1}s", phase2_elapsed.as_secs_f64()),
        "phase 2 complete"
    );

    // --- Summary ---
    let total = phase1_elapsed + phase2_elapsed;
    info!(
        total = format!("{:.1}s", total.as_secs_f64()),
        headers = headers_pushed,
        sections = sections_pushed,
        "fastsync finished — P2P takes over for the last {} blocks",
        cli.handoff_distance
    );

    pool.log_stats();
    Ok(())
}
