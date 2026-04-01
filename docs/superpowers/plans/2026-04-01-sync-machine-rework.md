# Sync Machine Rework — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rewrite the header sync state machine to match the JVM's exchange pattern — two-batch cycles, peer SyncInfo response, peer rotation — eliminating sync stalls.

**Architecture:** Event-driven sync loop reacting to three sources (P2P events, pipeline progress channel, 20s timer). The pipeline sends chain height after each batch; the sync machine sends SyncInfo immediately to trigger the next batch. Peer rotation on stall.

**Tech Stack:** Rust, tokio, no new dependencies.

---

### Task 1: Add progress channel to ValidationPipeline

**Files:**
- Modify: `src/pipeline.rs`
- Modify: `src/lib.rs` (no change needed — `ValidationPipeline` already exported)

- [ ] **Step 1: Update ValidationPipeline to accept and use progress sender**

In `src/pipeline.rs`, add `progress_tx` field and update constructor and `process_batch`:

Add field to struct:
```rust
pub struct ValidationPipeline {
    rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
    chain: Arc<Mutex<HeaderChain>>,
    tracker: HeaderTracker,
    pending: HashMap<BlockId, Header>,
    progress_tx: mpsc::Sender<u32>,
}
```

Update `new`:
```rust
pub fn new(
    rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
    chain: Arc<Mutex<HeaderChain>>,
    progress_tx: mpsc::Sender<u32>,
) -> Self {
    Self {
        rx,
        chain,
        tracker: HeaderTracker::new(),
        pending: HashMap::new(),
        progress_tx,
    }
}
```

At the end of `process_batch`, after `drop(chain)`, add the progress notification:

```rust
if height_after > height_before {
    tracing::info!(
        chain_height = height_after,
        batch_size = valid_headers.len(),
        pending = self.pending.len(),
        "pipeline: chained headers from height {height_before}"
    );
    let _ = self.progress_tx.try_send(height_after);
}
```

- [ ] **Step 2: Update tests to pass progress sender**

Update `test_pipeline` helper:

```rust
fn test_pipeline() -> (
    ValidationPipeline,
    mpsc::Sender<(u8, [u8; 32], Vec<u8>)>,
    mpsc::Receiver<u32>,
) {
    let (tx, rx) = mpsc::channel(256);
    let (progress_tx, progress_rx) = mpsc::channel(4);
    let chain = Arc::new(Mutex::new(HeaderChain::new(ChainConfig::testnet())));
    let pipeline = ValidationPipeline::new(rx, chain, progress_tx);
    (pipeline, tx, progress_rx)
}
```

Update every test that calls `test_pipeline()` to destructure the third element:

```rust
let (mut pipeline, _tx, _progress_rx) = test_pipeline();
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ergo-node-rust`
Expected: all 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/pipeline.rs
git commit -m "Add progress channel to ValidationPipeline

Pipeline sends chain height via try_send after each batch that
increases the chain. The sync machine will use this to trigger
the two-batch cycle."
```

---

### Task 2: Rewrite sync state machine

**Files:**
- Rewrite: `sync/src/state.rs`

- [ ] **Step 1: Write the complete new state.rs**

Replace the full contents of `sync/src/state.rs`:

```rust
use std::collections::HashSet;

use enr_p2p::protocol::messages::ProtocolMessage;
use enr_p2p::protocol::peer::ProtocolEvent;
use enr_p2p::types::PeerId;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::traits::{SyncChain, SyncTransport};

/// Header modifier type ID (NetworkObjectTypeId).
const HEADER_TYPE_ID: u8 = 101;

/// Minimum time between scheduled SyncInfo sends to the same peer.
/// The JVM enforces a hard 20-second minimum per peer in
/// `ErgoSyncTracker.MinSyncInterval`.
const MIN_SYNC_INTERVAL: Duration = Duration::from_secs(20);

/// How long without pipeline progress before rotating to a new peer.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to send SyncInfo when synced, to check for new blocks.
const SYNCED_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Header chain sync state machine.
///
/// Event-driven loop matching the JVM's sync exchange pattern:
/// - Sends SyncInfo, receives Inv, sends ModifierRequest
/// - On pipeline progress: sends SyncInfo again (two-batch cycle)
/// - On peer SyncInfo: responds with our SyncInfo (bidirectional exchange)
/// - On stall: rotates to a different peer
pub struct HeaderSync<T: SyncTransport, C: SyncChain> {
    transport: T,
    chain: C,
    progress: mpsc::Receiver<u32>,
    /// Peer we're currently syncing from.
    sync_peer: Option<PeerId>,
    /// Peers that failed to produce progress — skipped during peer selection.
    stalled_peers: HashSet<PeerId>,
    /// When the last chain height increase was observed.
    last_progress: Instant,
    /// When we last sent a scheduled SyncInfo (20s floor applies to these).
    last_scheduled_sync: Instant,
    /// Total SyncInfo messages sent this session (diagnostics).
    sync_sent_count: u32,
}

impl<T: SyncTransport, C: SyncChain> HeaderSync<T, C> {
    pub fn new(transport: T, chain: C, progress: mpsc::Receiver<u32>) -> Self {
        Self {
            transport,
            chain,
            progress,
            sync_peer: None,
            stalled_peers: HashSet::new(),
            last_progress: Instant::now(),
            last_scheduled_sync: Instant::now(),
            sync_sent_count: 0,
        }
    }

    /// Run the sync loop. Returns only if all event sources close.
    pub async fn run(&mut self) {
        tracing::info!("header sync started");
        loop {
            // Phase 1: wait for outbound peers
            if !self.pick_sync_peer().await {
                return; // event stream ended
            }

            // Phase 2: sync from the selected peer
            match self.sync_from_peer().await {
                SyncOutcome::Synced => self.synced().await,
                SyncOutcome::Stalled => {
                    if let Some(peer) = self.sync_peer.take() {
                        tracing::warn!(
                            peer = %peer,
                            height = self.chain.chain_height().await,
                            syncs_sent = self.sync_sent_count,
                            "sync stalled, rotating peer"
                        );
                        self.stalled_peers.insert(peer);
                    }
                }
                SyncOutcome::PeerDisconnected | SyncOutcome::StreamEnded => {
                    self.sync_peer = None;
                }
            }
        }
    }

    /// Wait until we have an outbound peer to sync from.
    /// Returns false if the event stream ends.
    async fn pick_sync_peer(&mut self) -> bool {
        loop {
            let peers = self.transport.outbound_peers().await;
            if let Some(peer) = self.select_peer(&peers) {
                self.sync_peer = Some(peer);
                self.last_progress = Instant::now();
                self.sync_sent_count = 0;
                tracing::info!(
                    peer = %peer,
                    height = self.chain.chain_height().await,
                    "starting header sync"
                );
                return true;
            }

            // No eligible peers — wait for a connection event
            match self.transport.next_event().await {
                Some(_) => {}
                None => return false,
            }
        }
    }

    /// Pick an outbound peer, preferring those not in the stalled set.
    fn select_peer(&mut self, peers: &[PeerId]) -> Option<PeerId> {
        let choice = peers
            .iter()
            .find(|p| !self.stalled_peers.contains(p))
            .or_else(|| {
                // All peers stalled — clear and retry
                self.stalled_peers.clear();
                peers.first()
            });
        choice.copied()
    }

    /// Run the event-driven sync cycle with the current peer.
    async fn sync_from_peer(&mut self) -> SyncOutcome {
        let peer = match self.sync_peer {
            Some(p) => p,
            None => return SyncOutcome::StreamEnded,
        };

        // Send initial SyncInfo to kick off the exchange
        if self.send_sync_info(peer).await.is_err() {
            return SyncOutcome::PeerDisconnected;
        }
        self.last_scheduled_sync = Instant::now();

        loop {
            // Compute time until next scheduled SyncInfo
            let until_next = MIN_SYNC_INTERVAL
                .saturating_sub(self.last_scheduled_sync.elapsed());

            tokio::select! {
                // P2P events: Inv, peer SyncInfo, disconnect
                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            match self.handle_event(peer, event).await {
                                EventResult::Continue => {}
                                EventResult::Synced => return SyncOutcome::Synced,
                                EventResult::PeerGone => return SyncOutcome::PeerDisconnected,
                            }
                        }
                        None => return SyncOutcome::StreamEnded,
                    }
                }

                // Pipeline progress: chain height increased → two-batch trigger
                Some(height) = self.progress.recv() => {
                    tracing::debug!(height, "pipeline progress");
                    self.last_progress = Instant::now();
                    self.stalled_peers.clear();

                    // Send SyncInfo with updated chain state (second batch)
                    let _ = self.send_sync_info(peer).await;
                }

                // Scheduled SyncInfo: 20-second floor
                _ = tokio::time::sleep(until_next) => {
                    let _ = self.send_sync_info(peer).await;
                    self.last_scheduled_sync = Instant::now();

                    // Check for stall
                    if self.last_progress.elapsed() > STALL_TIMEOUT {
                        return SyncOutcome::Stalled;
                    }
                }
            }
        }
    }

    /// Handle a single P2P event during sync.
    async fn handle_event(&mut self, peer: PeerId, event: ProtocolEvent) -> EventResult {
        match event {
            ProtocolEvent::Message { peer_id, message } => match message {
                // Inv with header IDs: request them
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == HEADER_TYPE_ID && !ids.is_empty() =>
                {
                    tracing::debug!(count = ids.len(), "requesting announced headers");
                    let _ = self
                        .transport
                        .send_to(
                            peer_id,
                            ProtocolMessage::ModifierRequest { modifier_type, ids },
                        )
                        .await;
                    EventResult::Continue
                }

                // Empty Inv: peer has no more headers
                ProtocolMessage::Inv { modifier_type, ids }
                    if modifier_type == HEADER_TYPE_ID && ids.is_empty() =>
                {
                    let height = self.chain.chain_height().await;
                    tracing::info!(height, "peer reports no more headers");
                    EventResult::Synced
                }

                // Peer's SyncInfo: respond with ours (bidirectional exchange)
                ProtocolMessage::SyncInfo { body } => {
                    if let Ok(info) = self.chain.parse_sync_info(&body) {
                        let peer_heights = C::sync_info_heights(&info);
                        let our_height = self.chain.chain_height().await;

                        if let Some(&peer_tip) = peer_heights.first() {
                            if peer_tip <= our_height {
                                tracing::info!(our_height, peer_tip, "caught up with peer");
                                return EventResult::Synced;
                            }
                        }
                    }

                    // Respond with our SyncInfo (JVM does this when syncSendNeeded)
                    let _ = self.send_sync_info(peer).await;
                    EventResult::Continue
                }

                _ => EventResult::Continue,
            },

            ProtocolEvent::PeerDisconnected { peer_id, .. } if peer_id == peer => {
                tracing::info!(peer = %peer_id, "sync peer disconnected");
                EventResult::PeerGone
            }

            _ => EventResult::Continue,
        }
    }

    /// Synced: periodically check for new blocks.
    async fn synced(&mut self) {
        let mut ticker = tokio::time::interval(SYNCED_POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let peers = self.transport.outbound_peers().await;
                    if let Some(&peer) = peers.first() {
                        let _ = self.send_sync_info(peer).await;
                    } else {
                        tracing::info!("no outbound peers, returning to idle");
                        self.sync_peer = None;
                        return;
                    }
                }

                event = self.transport.next_event() => {
                    match event {
                        Some(event) => {
                            if let ProtocolEvent::Message { peer_id, message: ProtocolMessage::Inv {
                                modifier_type, ids
                            }} = event {
                                if modifier_type == HEADER_TYPE_ID && !ids.is_empty() {
                                    tracing::debug!(new_headers = ids.len(), "new headers while synced");
                                    let _ = self.transport.send_to(
                                        peer_id,
                                        ProtocolMessage::ModifierRequest { modifier_type, ids },
                                    ).await;
                                }
                            }
                        }
                        None => {
                            self.sync_peer = None;
                            return;
                        }
                    }
                }

                Some(height) = self.progress.recv() => {
                    tracing::debug!(height, "pipeline progress while synced");
                    // New headers validated — if we're still behind, go back to syncing
                    // (will be detected on next SyncInfo exchange)
                }
            }
        }
    }

    /// Send our current SyncInfo to a peer.
    async fn send_sync_info(
        &mut self,
        peer: PeerId,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        let body = self.chain.build_sync_info().await;
        self.sync_sent_count += 1;
        self.transport
            .send_to(peer, ProtocolMessage::SyncInfo { body })
            .await
    }
}

/// Result of handling a single event.
enum EventResult {
    /// Keep syncing.
    Continue,
    /// Caught up with the peer.
    Synced,
    /// Sync peer disconnected.
    PeerGone,
}

/// Outcome of a sync_from_peer session.
enum SyncOutcome {
    /// Caught up with the peer's chain tip.
    Synced,
    /// No progress for STALL_TIMEOUT — rotate peer.
    Stalled,
    /// Sync peer disconnected.
    PeerDisconnected,
    /// Event stream closed (shutdown).
    StreamEnded,
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check`
Expected: `sync` compiles, `main.rs` fails (constructor signature changed). That's expected — fixed in Task 3.

- [ ] **Step 3: Commit**

```bash
git add sync/src/state.rs
git commit -m "Rewrite sync machine: event-driven JVM-compatible sync

Three event sources: P2P events, pipeline progress, 20s timer.
Two-batch cycle via progress channel. Responds to peer SyncInfo.
Peer rotation on stall via stalled_peers set. Explicit sync cycle
with SyncOutcome/EventResult enums for control flow."
```

---

### Task 3: Rewire main.rs

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add progress channel wiring**

Replace `src/main.rs` contents:

```rust
use std::sync::Arc;

use enr_chain::{ChainConfig, HeaderChain};
use ergo_node_rust::{P2pTransport, SharedChain, ValidationPipeline};
use ergo_sync::HeaderSync;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ergo.toml".to_string());

    let config = enr_p2p::config::Config::load(&config_path)?;

    // Derive chain config from P2P network setting
    let chain_config = match config.proxy.network {
        enr_p2p::types::Network::Testnet => ChainConfig::testnet(),
        enr_p2p::types::Network::Mainnet => ChainConfig::mainnet(),
    };

    // Shared header chain: pipeline writes, sync reads
    let chain = Arc::new(Mutex::new(HeaderChain::new(chain_config)));

    // Modifier channel — P2P produces, pipeline consumes
    let (modifier_tx, modifier_rx) = tokio::sync::mpsc::channel(4096);

    // Progress channel — pipeline produces, sync machine consumes
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(4);

    // Start P2P with modifier sink (no validator)
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(modifier_tx)).await?);

    // Validation pipeline
    let pipeline_chain = chain.clone();
    tokio::spawn(async move {
        let mut pipeline = ValidationPipeline::new(modifier_rx, pipeline_chain, progress_tx);
        pipeline.run().await;
    });

    // Subscribe to events for the sync machine
    let events = p2p.subscribe().await;

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), events);
    let sync_chain = SharedChain::new(chain.clone());

    // Start sync in a background task
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(transport, sync_chain, progress_rx);
        sync.run().await;
    });

    tracing::info!("Ergo node running");

    // Run until interrupted
    tokio::signal::ctrl_c().await?;

    let height = chain.lock().await.height();
    let peers = p2p.peer_count().await;
    tracing::info!(chain_height = height, peers, "Shutting down");

    Ok(())
}
```

- [ ] **Step 2: Verify full compilation and tests**

Run: `cargo test`
Expected: all tests pass (pipeline tests + sync crate compiles).

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "Wire progress channel: pipeline → sync machine

Pipeline sends chain height after each batch. Sync machine
receives it as a trigger for the two-batch cycle."
```

---

### Task 4: Build, deploy, verify on testnet

**Files:** none (operational verification)

- [ ] **Step 1: Build release deb**

```bash
./build-deb
```

Expected: `target/ergo-node-rust_0.1.0_amd64.deb` built successfully.

- [ ] **Step 2: Deploy**

```bash
scp target/ergo-node-rust_0.1.0_amd64.deb admin@95.179.246.102:/tmp/
ssh admin@95.179.246.102 "sudo dpkg -i /tmp/ergo-node-rust_0.1.0_amd64.deb && sudo truncate -s 0 /var/log/ergo-node/ergo-node.log && sudo systemctl restart ergo-node-rust"
```

- [ ] **Step 3: Verify — two-batch cycles visible**

Watch logs for the two-batch pattern:

```bash
ssh admin@95.179.246.102 "sudo tail -f /var/log/ergo-node/ergo-node.log"
```

Expected pattern — two pipeline batches in quick succession, then a ~20s pause:
```
pipeline: chained headers from height 0    chain_height=400
pipeline: chained headers from height 400  chain_height=799
                                            [~20 second gap]
pipeline: chained headers from height 799  chain_height=1198
pipeline: chained headers from height 1198 chain_height=1597
```

- [ ] **Step 4: Verify — no stall past 14000 headers**

Wait ~10 minutes. Check that sync continues past previous stall points (7183, 11572, 12370, 13966):

```bash
ssh admin@95.179.246.102 "sudo grep 'pipeline:' /var/log/ergo-node/ergo-node.log | tail -5"
```

Expected: chain_height continuously increasing, no "sync stalled" messages.

- [ ] **Step 5: Verify — peer rotation on stall (if it occurs)**

If a stall does occur, check that peer rotation happens:

```bash
ssh admin@95.179.246.102 "sudo grep -E 'stall|starting header' /var/log/ergo-node/ergo-node.log | tail -10"
```

Expected: "sync stalled, rotating peer" followed by "starting header sync" with a DIFFERENT peer.

- [ ] **Step 6: Push**

```bash
git push
```
