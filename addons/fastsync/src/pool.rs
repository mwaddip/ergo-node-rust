//! Multi-peer pool for parallel header and block section fetching.
//!
//! Wraps multiple [`PeerFetcher`] instances and distributes work across them.
//! Phase 1 (headers): shared chunk queue, results buffered and pushed in
//! ascending height order. Phase 2 (block sections): round-robin — order
//! doesn't matter for sections.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ergo_chain_types::Header;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::fetcher::{block_batch_size, PeerFetcher};
use crate::ingest::IngestClient;
use crate::types::JvmFullBlock;
use crate::wire::{block_sections_to_modifiers, header_to_modifier, parse_header_json};

// ---------------------------------------------------------------------------
// Per-peer tracking
// ---------------------------------------------------------------------------

struct Peer {
    fetcher: Arc<PeerFetcher>,
    url: String,
    healthy: bool,
    headers_fetched: u32,
    blocks_fetched: u32,
    total_latency: Duration,
    request_count: u32,
    errors: u32,
}

impl Peer {
    fn new(url: &str) -> Result<Self> {
        let fetcher = Arc::new(PeerFetcher::new(url)?);
        Ok(Self {
            fetcher,
            url: url.to_string(),
            healthy: true,
            headers_fetched: 0,
            blocks_fetched: 0,
            total_latency: Duration::ZERO,
            request_count: 0,
            errors: 0,
        })
    }

    fn record_success(&mut self, latency: Duration, count: u32, is_headers: bool) {
        self.total_latency += latency;
        self.request_count += 1;
        if is_headers {
            self.headers_fetched += count;
        } else {
            self.blocks_fetched += count;
        }
    }

    fn record_error(&mut self) {
        self.errors += 1;
    }

    fn avg_latency_ms(&self) -> u64 {
        if self.request_count == 0 {
            return 0;
        }
        self.total_latency.as_millis() as u64 / self.request_count as u64
    }
}

// ---------------------------------------------------------------------------
// Peer pool
// ---------------------------------------------------------------------------

/// Multi-peer pool that distributes fetch work across discovered peers.
pub struct PeerPool {
    peers: Vec<Peer>,
    chunk_size: u32,
    /// Local node URL used to refresh the peer list mid-fetch.
    /// None when peers came from `--peer-url` (single-peer mode).
    node_url: Option<String>,
    last_refresh: Instant,
}

/// Minimum time between /peers/api-urls refresh queries.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

impl PeerPool {
    /// Build a pool from peer REST API URLs.
    pub fn new(urls: Vec<String>, chunk_size: u32) -> Result<Self> {
        if urls.is_empty() {
            bail!("no peer URLs provided");
        }
        let peers = urls
            .iter()
            .map(|url| Peer::new(url))
            .collect::<Result<Vec<_>>>()?;
        info!(count = peers.len(), "peer pool");
        for (i, p) in peers.iter().enumerate() {
            info!(idx = i, url = %p.url, "  peer");
        }
        Ok(Self {
            peers,
            chunk_size,
            node_url: None,
            // Old enough that the first `maybe_refresh()` call fires
            // unconditionally. Avoids the single-peer-fails-first-task
            // deadlock where no task ever completes and no refresh runs.
            last_refresh: Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(Instant::now),
        })
    }

    /// Enable mid-fetch peer discovery by querying this node's /peers/api-urls.
    pub fn with_node_refresh(mut self, node_url: String) -> Self {
        self.node_url = Some(node_url);
        self
    }

    /// Query the local node for new REST peers and add any that aren't
    /// already in the pool. Returns the number of peers added.
    ///
    /// Rate-limited to once per `REFRESH_INTERVAL` regardless of how often
    /// this is called.
    pub async fn maybe_refresh(&mut self) -> usize {
        let node_url = match &self.node_url {
            Some(u) => u.clone(),
            None => return 0,
        };
        if self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return 0;
        }
        self.last_refresh = Instant::now();

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let resp = match client
            .get(format!("{node_url}/peers/api-urls"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return 0,
        };
        let entries: Vec<crate::types::PeerApiUrl> = match resp.json().await {
            Ok(v) => v,
            Err(_) => return 0,
        };

        let existing: std::collections::HashSet<&str> =
            self.peers.iter().map(|p| p.url.as_str()).collect();
        let mut added = 0;
        let new_urls: Vec<String> = entries
            .into_iter()
            .map(|e| e.url)
            .filter(|u| !existing.contains(u.as_str()))
            .collect();
        for url in new_urls {
            match Peer::new(&url) {
                Ok(peer) => {
                    info!(url = %url, idx = self.peers.len(), "adding peer to pool");
                    self.peers.push(peer);
                    added += 1;
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "skipping malformed peer url");
                }
            }
        }
        added
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn healthy_count(&self) -> usize {
        self.peers.iter().filter(|p| p.healthy).count()
    }

    fn healthy_indices(&self) -> Vec<usize> {
        self.peers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.healthy)
            .map(|(i, _)| i)
            .collect()
    }

    fn next_healthy_excluding(&self, exclude: usize) -> Option<usize> {
        self.peers
            .iter()
            .enumerate()
            .find(|(i, p)| p.healthy && *i != exclude)
            .map(|(i, _)| i)
    }

    /// Query first reachable peer for chain tip.
    pub async fn query_tip(&mut self) -> Result<(u32, u32)> {
        for i in 0..self.peers.len() {
            if !self.peers[i].healthy {
                continue;
            }
            match self.peers[i].fetcher.peer_info().await {
                Ok(info) => {
                    info!(
                        url = %self.peers[i].url,
                        tip = info.headers_height,
                        full = info.full_height,
                        "peer tip"
                    );
                    return Ok((info.headers_height, info.full_height));
                }
                Err(e) => {
                    warn!(url = %self.peers[i].url, error = %e, "unreachable");
                    self.peers[i].healthy = false;
                }
            }
        }
        bail!("no peers reachable")
    }

    // -----------------------------------------------------------------------
    // Phase 1: parallel header fetch with in-order push
    // -----------------------------------------------------------------------

    /// Fetch headers from `start..=target` using all healthy peers.
    ///
    /// Headers are fetched in parallel chunks but pushed to the local node
    /// in strict ascending height order. Returns `(headers_pushed, header_ids)`
    /// where `header_ids` is populated only when `collect_ids` is true.
    pub async fn fetch_headers(
        &mut self,
        start: u32,
        target: u32,
        ingest: &IngestClient,
        collect_ids: bool,
    ) -> Result<(u32, Vec<(u32, String)>)> {
        let chunk_size = self.chunk_size;

        // Build the work queue — each entry is (from_height, to_height)
        let mut chunks: VecDeque<(u32, u32)> = VecDeque::new();
        {
            let mut from = start;
            while from <= target {
                let to = (from + chunk_size - 1).min(target);
                chunks.push_back((from, to));
                from = to + 1;
            }
        }

        // Buffer for out-of-order results, keyed by chunk start height
        let mut buffer: BTreeMap<u32, (u32, Vec<Header>)> = BTreeMap::new();
        let mut next_flush = start;
        let mut headers_pushed = 0u32;
        let mut header_ids = if collect_ids {
            Vec::with_capacity((target - start + 1) as usize)
        } else {
            Vec::new()
        };

        let phase_start = Instant::now();
        let mut last_log = phase_start;
        let max_empties = (self.peer_count() * 3) as u32;
        let mut total_empties = 0u32;

        type Task = (usize, u32, u32, Duration, Result<Vec<Header>>);
        let mut tasks: JoinSet<Task> = JoinSet::new();

        // Track which peer indices are currently working, so new peers
        // added mid-fetch can be seeded without double-spawning.
        let mut in_flight: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // Refresh peers BEFORE seeding. Otherwise, if the initial pool
        // had only one peer and its first task fails, no task completes
        // to trigger a refresh — deadlocks the sweep.
        self.maybe_refresh().await;

        // Seed: one chunk per healthy peer
        for idx in self.healthy_indices() {
            if let Some((from, to)) = chunks.pop_front() {
                let fetcher = self.peers[idx].fetcher.clone();
                in_flight.insert(idx);
                tasks.spawn(async move {
                    let t = Instant::now();
                    let r = fetcher.chain_slice(from, to).await;
                    (idx, from, to, t.elapsed(), r)
                });
            }
        }

        while let Some(join_result) = tasks.join_next().await {
            let (peer_idx, from, to, latency, result) =
                join_result.context("header task panicked")?;

            match result {
                // Empty response — peer is behind the requested range
                Ok(headers) if headers.is_empty() => {
                    in_flight.remove(&peer_idx);
                    total_empties += 1;
                    warn!(
                        peer = %self.peers[peer_idx].url, from, to,
                        "empty chainSlice ({total_empties}/{max_empties})"
                    );
                    self.peers[peer_idx].healthy = false;
                    chunks.push_back((from, to));

                    if total_empties > max_empties || self.healthy_count() == 0 {
                        warn!(
                            height = next_flush.saturating_sub(1),
                            "all peers behind target — truncating"
                        );
                        break;
                    }

                    // Reassign to a different peer
                    if let Some(alt) = self.next_healthy_excluding(peer_idx) {
                        if let Some((nf, nt)) = chunks.pop_front() {
                            let fetcher = self.peers[alt].fetcher.clone();
                            in_flight.insert(alt);
                            tasks.spawn(async move {
                                let t = Instant::now();
                                let r = fetcher.chain_slice(nf, nt).await;
                                (alt, nf, nt, t.elapsed(), r)
                            });
                        }
                    }
                }

                // Successful fetch
                Ok(headers) => {
                    let count = headers.len() as u32;
                    self.peers[peer_idx].record_success(latency, count, true);
                    buffer.insert(from, (to, headers));

                    // Flush contiguous chunks in ascending order
                    while let Some((chunk_to, hdrs)) = buffer.remove(&next_flush) {
                        let mut batch = Vec::with_capacity(hdrs.len());
                        for header in &hdrs {
                            // Autolykos v1 headers (version < 2) — skip PoW
                            if header.version >= 2 {
                                if !header.check_pow().with_context(|| {
                                    format!("PoW at height {}", header.height)
                                })? {
                                    bail!("PoW failed at height {}", header.height);
                                }
                            }
                            batch.push(header_to_modifier(header)?);
                        }

                        if collect_ids {
                            for header in &hdrs {
                                header_ids
                                    .push((header.height, hex::encode(header.id.0 .0)));
                            }
                        }

                        // Push to local node with one retry
                        match ingest.push(&batch).await {
                            Ok(n) => headers_pushed += n,
                            Err(e) => {
                                warn!(error = %e, "ingest failed, backing off");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                headers_pushed += ingest
                                    .push(&batch)
                                    .await
                                    .context("ingest retry failed")?;
                            }
                        }

                        next_flush = chunk_to + 1;
                    }

                    // Throttled progress log (every 2s)
                    if last_log.elapsed() >= Duration::from_secs(2) {
                        let elapsed = phase_start.elapsed().as_secs_f64();
                        let rate = headers_pushed as f64 / elapsed;
                        let remaining = target.saturating_sub(next_flush - 1);
                        let eta = if rate > 0.0 {
                            remaining as f64 / rate
                        } else {
                            0.0
                        };
                        info!(
                            height = next_flush - 1,
                            pushed = headers_pushed,
                            buffered = buffer.len(),
                            peers = self.healthy_count(),
                            rate = format!("{rate:.0}/s"),
                            eta = format!("{eta:.0}s"),
                            "headers"
                        );
                        last_log = Instant::now();
                    }

                    // The peer that just finished is now idle
                    in_flight.remove(&peer_idx);

                    // Feed this peer its next chunk (if still healthy)
                    if self.peers[peer_idx].healthy {
                        if let Some((nf, nt)) = chunks.pop_front() {
                            let fetcher = self.peers[peer_idx].fetcher.clone();
                            in_flight.insert(peer_idx);
                            tasks.spawn(async move {
                                let t = Instant::now();
                                let r = fetcher.chain_slice(nf, nt).await;
                                (peer_idx, nf, nt, t.elapsed(), r)
                            });
                        }
                    }

                    // Mid-fetch peer discovery: refresh the pool and seed
                    // any newly-added peers that have no in-flight work.
                    if self.maybe_refresh().await > 0 {
                        for idx in self.healthy_indices() {
                            if in_flight.contains(&idx) {
                                continue;
                            }
                            if let Some((nf, nt)) = chunks.pop_front() {
                                let fetcher = self.peers[idx].fetcher.clone();
                                in_flight.insert(idx);
                                tasks.spawn(async move {
                                    let t = Instant::now();
                                    let r = fetcher.chain_slice(nf, nt).await;
                                    (idx, nf, nt, t.elapsed(), r)
                                });
                            } else {
                                break;
                            }
                        }
                    }
                }

                // Fetch error — connection/timeout/HTTP failure
                Err(e) => {
                    in_flight.remove(&peer_idx);
                    warn!(
                        peer = %self.peers[peer_idx].url,
                        from, to, error = %e,
                        "fetch failed — marking unhealthy"
                    );
                    self.peers[peer_idx].record_error();
                    self.peers[peer_idx].healthy = false;
                    chunks.push_back((from, to));

                    if self.healthy_count() == 0 {
                        bail!(
                            "all peers unhealthy — stuck at height {}",
                            next_flush.saturating_sub(1)
                        );
                    }

                    // Reassign to a healthy peer
                    if let Some(alt) = self.next_healthy_excluding(peer_idx) {
                        if let Some((nf, nt)) = chunks.pop_front() {
                            let fetcher = self.peers[alt].fetcher.clone();
                            in_flight.insert(alt);
                            tasks.spawn(async move {
                                let t = Instant::now();
                                let r = fetcher.chain_slice(nf, nt).await;
                                (alt, nf, nt, t.elapsed(), r)
                            });
                        }
                    }
                }
            }
        }

        // Final progress line
        if headers_pushed > 0 {
            let elapsed = phase_start.elapsed().as_secs_f64();
            let rate = headers_pushed as f64 / elapsed;
            info!(
                height = next_flush.saturating_sub(1),
                pushed = headers_pushed,
                rate = format!("{rate:.0}/s"),
                elapsed = format!("{elapsed:.1}s"),
                "headers done"
            );
        }

        if next_flush <= target && !chunks.is_empty() {
            warn!(
                reached = next_flush.saturating_sub(1),
                target,
                remaining = chunks.len(),
                "header fetch truncated"
            );
        }

        Ok((headers_pushed, header_ids))
    }

    // -----------------------------------------------------------------------
    // Phase 2: parallel block section fetch (round-robin)
    // -----------------------------------------------------------------------

    /// Fetch block sections for the given header IDs, push to local node.
    ///
    /// Batches are distributed round-robin across healthy peers. Order
    /// doesn't matter for block sections — the node accepts them in any order.
    pub async fn fetch_blocks(
        &mut self,
        header_ids: &[(u32, String)],
        ingest: &IngestClient,
    ) -> Result<u32> {
        let batch_size = block_batch_size();
        let mut sections_pushed = 0u32;
        let phase_start = Instant::now();
        let mut last_log = phase_start;

        // Pre-build owned batches: (min_height, max_height, header_ids)
        let mut queue: VecDeque<(u32, u32, Vec<String>)> = VecDeque::new();
        for chunk in header_ids.chunks(batch_size) {
            let min_h = chunk.first().map(|(h, _)| *h).unwrap_or(0);
            let max_h = chunk.last().map(|(h, _)| *h).unwrap_or(0);
            let ids: Vec<String> = chunk.iter().map(|(_, id)| id.clone()).collect();
            queue.push_back((min_h, max_h, ids));
        }
        let total_batches = queue.len() as u32;
        let mut completed = 0u32;

        // Return ids from task so we can re-queue on failure
        type Task = (usize, u32, u32, Vec<String>, Duration, Result<Vec<JvmFullBlock>>);
        let mut tasks: JoinSet<Task> = JoinSet::new();

        // Track in-flight peer indices so new peers discovered mid-fetch
        // can be seeded without double-spawning.
        let mut in_flight: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // Refresh peers BEFORE seeding.
        self.maybe_refresh().await;

        // Seed: one batch per healthy peer
        for idx in self.healthy_indices() {
            if let Some((min_h, max_h, ids)) = queue.pop_front() {
                let fetcher = self.peers[idx].fetcher.clone();
                in_flight.insert(idx);
                tasks.spawn(async move {
                    let t = Instant::now();
                    let r = fetcher.blocks_by_ids(&ids).await;
                    (idx, min_h, max_h, ids, t.elapsed(), r)
                });
            }
        }

        while let Some(join_result) = tasks.join_next().await {
            let (peer_idx, min_h, max_h, ids, latency, result) =
                join_result.context("block task panicked")?;

            match result {
                Ok(blocks) => {
                    self.peers[peer_idx]
                        .record_success(latency, blocks.len() as u32, false);

                    for block in &blocks {
                        let header: Header = parse_header_json(&block.header)
                            .with_context(|| format!("parse header {min_h}..{max_h}"))?;
                        let mods = block_sections_to_modifiers(block, &header)?;
                        match ingest.push(&mods).await {
                            Ok(n) => sections_pushed += n,
                            Err(e) => {
                                warn!(
                                    height = header.height, error = %e,
                                    "section push failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                if let Err(e2) = ingest.push(&mods).await {
                                    error!(
                                        height = header.height, error = %e2,
                                        "section retry failed"
                                    );
                                }
                            }
                        }
                    }

                    completed += 1;

                    // Throttled progress (every 2s)
                    if last_log.elapsed() >= Duration::from_secs(2) {
                        let elapsed = phase_start.elapsed().as_secs_f64();
                        let blocks_est = sections_pushed / 3;
                        let rate = blocks_est as f64 / elapsed;
                        info!(
                            batch = format!("{completed}/{total_batches}"),
                            sections = sections_pushed,
                            peers = self.healthy_count(),
                            rate = format!("{rate:.0} blk/s"),
                            "blocks"
                        );
                        last_log = Instant::now();
                    }

                    in_flight.remove(&peer_idx);

                    // Next batch for this peer (if still healthy)
                    if self.peers[peer_idx].healthy {
                        if let Some((nm, nx, nids)) = queue.pop_front() {
                            let fetcher = self.peers[peer_idx].fetcher.clone();
                            in_flight.insert(peer_idx);
                            tasks.spawn(async move {
                                let t = Instant::now();
                                let r = fetcher.blocks_by_ids(&nids).await;
                                (peer_idx, nm, nx, nids, t.elapsed(), r)
                            });
                        }
                    }

                    // Mid-fetch peer discovery
                    if self.maybe_refresh().await > 0 {
                        for idx in self.healthy_indices() {
                            if in_flight.contains(&idx) {
                                continue;
                            }
                            if let Some((nm, nx, nids)) = queue.pop_front() {
                                let fetcher = self.peers[idx].fetcher.clone();
                                in_flight.insert(idx);
                                tasks.spawn(async move {
                                    let t = Instant::now();
                                    let r = fetcher.blocks_by_ids(&nids).await;
                                    (idx, nm, nx, nids, t.elapsed(), r)
                                });
                            } else {
                                break;
                            }
                        }
                    }
                }

                Err(e) => {
                    in_flight.remove(&peer_idx);
                    warn!(
                        peer = %self.peers[peer_idx].url,
                        min_h, max_h, error = %e,
                        "block fetch failed — marking unhealthy"
                    );
                    self.peers[peer_idx].record_error();
                    self.peers[peer_idx].healthy = false;
                    queue.push_back((min_h, max_h, ids));

                    if self.healthy_count() == 0 {
                        bail!(
                            "all peers unhealthy — block fetch at {completed}/{total_batches}"
                        );
                    }

                    if let Some(alt) = self.next_healthy_excluding(peer_idx) {
                        if let Some((nm, nx, nids)) = queue.pop_front() {
                            let fetcher = self.peers[alt].fetcher.clone();
                            in_flight.insert(alt);
                            tasks.spawn(async move {
                                let t = Instant::now();
                                let r = fetcher.blocks_by_ids(&nids).await;
                                (alt, nm, nx, nids, t.elapsed(), r)
                            });
                        }
                    }
                }
            }
        }

        // Final progress
        if completed > 0 {
            let elapsed = phase_start.elapsed().as_secs_f64();
            let blocks_est = sections_pushed / 3;
            let rate = blocks_est as f64 / elapsed;
            info!(
                batches = format!("{completed}/{total_batches}"),
                sections = sections_pushed,
                rate = format!("{rate:.0} blk/s"),
                elapsed = format!("{elapsed:.1}s"),
                "blocks done"
            );
        }

        Ok(sections_pushed)
    }

    // -----------------------------------------------------------------------
    // Header ID collection (for blocks-only gap fill)
    // -----------------------------------------------------------------------

    /// Fetch header IDs for a height range without pushing headers.
    /// Used when headers are already synced but block sections are missing.
    pub async fn header_ids_for_range(
        &mut self,
        from: u32,
        to: u32,
    ) -> Result<Vec<(u32, String)>> {
        let mut ids = Vec::with_capacity((to - from + 1) as usize);
        let chunk_size = self.chunk_size;
        let mut height = from;

        // Refresh peers before starting — the pool may have been built
        // with stale discovery data.
        self.maybe_refresh().await;

        while height <= to {
            let chunk_to = (height + chunk_size - 1).min(to);
            let peer_idx = *self.healthy_indices().first().ok_or_else(|| {
                anyhow::anyhow!("no healthy peers for header ID fetch")
            })?;
            let fetcher = self.peers[peer_idx].fetcher.clone();
            let start = Instant::now();

            match fetcher.chain_slice(height, chunk_to).await {
                Ok(headers) => {
                    let elapsed = start.elapsed();
                    self.peers[peer_idx].request_count += 1;
                    self.peers[peer_idx].total_latency += elapsed;
                    for h in &headers {
                        ids.push((h.height, hex::encode(h.id.0 .0)));
                    }
                    info!(
                        from = height,
                        to = chunk_to,
                        count = headers.len(),
                        "collected header IDs"
                    );
                    height = chunk_to + 1;
                }
                Err(e) => {
                    warn!(peer = peer_idx, error = %e, "chainSlice failed during ID collection");
                    self.peers[peer_idx].healthy = false;
                    self.peers[peer_idx].errors += 1;
                }
            }
            // Opportunistic peer discovery — new peers can help parallelize
            // subsequent phases even though this loop is sequential.
            self.maybe_refresh().await;
        }

        Ok(ids)
    }

    // Stats
    // -----------------------------------------------------------------------

    /// Log per-peer summary.
    pub fn log_stats(&self) {
        info!("--- peer stats ---");
        for (i, p) in self.peers.iter().enumerate() {
            info!(
                idx = i,
                url = %p.url,
                healthy = p.healthy,
                headers = p.headers_fetched,
                blocks = p.blocks_fetched,
                requests = p.request_count,
                errors = p.errors,
                avg_ms = p.avg_latency_ms(),
                "  peer"
            );
        }
    }
}
