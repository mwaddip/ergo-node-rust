# Library Crate Refactor — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the P2P crate from a standalone binary to a library that the main `ergo-node-rust` crate calls.

**Architecture:** Move all runtime code (accept loops, outbound manager, peer lifecycle, event loop) from `main.rs` into a new `node.rs` module. Expose `P2pNode::start(config, validator)` as the public entry point that spawns everything and returns a handle for observation. Delete `main.rs`, strip RRD code.

**Tech Stack:** Rust, tokio (existing dependency), no new dependencies.

---

### Task 1: Create `node.rs` with `P2pNode` struct and `start()` skeleton

**Files:**
- Create: `src/node.rs`
- Modify: `src/lib.rs`

This task creates the public API surface. The `start()` function won't do anything yet — just loads config, creates shared state, and returns the handle. No spawning.

- [ ] **Step 1: Create `src/node.rs` with the public types and a skeleton `start()`**

```rust
//! P2P node entry point.
//!
//! The caller provides a config and an optional modifier validator, then calls
//! `P2pNode::start()`. The P2P layer spawns listeners, outbound connections,
//! and the event loop as background tokio tasks. The returned `P2pNode` is a
//! handle for observing state — the caller owns the tokio runtime.

use crate::config::Config;
use crate::protocol::messages::ProtocolMessage;
use crate::protocol::peer::ProtocolEvent;
use crate::routing::router::{Action, Router};
use crate::routing::validator::ModifierValidator;
use crate::routing::latency::LatencyStats;
use crate::transport::connection::Connection;
use crate::transport::frame::Frame;
use crate::transport::handshake::{self, HandshakeConfig};
use crate::types::{Direction, PeerId, ProxyMode, Version};

use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};

type PeerSender = mpsc::Sender<Frame>;

/// Handle to a running P2P node.
///
/// Created by `P2pNode::start()`. Provides read-only observation of the
/// node's state. The P2P layer runs as background tokio tasks — dropping
/// this handle does not stop them. The tasks live until the tokio runtime
/// shuts down.
pub struct P2pNode {
    router: Arc<Mutex<Router>>,
}

impl P2pNode {
    /// Start the P2P layer.
    ///
    /// Loads config, sets up listeners, outbound connections, keepalive, and
    /// the event loop as background tokio tasks. Returns immediately.
    ///
    /// # Contract
    /// - **Precondition**: Called within a tokio runtime.
    /// - **Precondition**: `config` has at least one listener and one seed peer
    ///   (enforced by `Config::load()`).
    /// - **Postcondition**: Background tasks are spawned and running.
    pub async fn start(
        config: Config,
        validator: Option<Box<dyn ModifierValidator>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (ver_major, ver_minor, ver_patch) = config.version_bytes()?;
        let version = Version::new(ver_major, ver_minor, ver_patch);
        let network = config.proxy.network;

        tracing::info!(network = ?network, version = %version, "P2P layer starting");

        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(256);
        let peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let router = Arc::new(Mutex::new(Router::new()));
        let peer_counter = Arc::new(std::sync::atomic::AtomicU64::new(1));

        if let Some(v) = validator {
            router.lock().await.set_validator(v);
        }

        // Listeners, outbound manager, keepalive, event loop — added in next tasks.

        Ok(P2pNode { router })
    }

    /// Number of connected peers (inbound + outbound).
    pub async fn peer_count(&self) -> usize {
        self.router.lock().await.peer_count()
    }

    /// Currently connected outbound peer IDs.
    pub async fn outbound_peers(&self) -> Vec<PeerId> {
        self.router.lock().await.outbound_peers()
    }

    /// Currently connected inbound peer IDs.
    pub async fn inbound_peers(&self) -> Vec<PeerId> {
        self.router.lock().await.inbound_peers()
    }

    /// Aggregate latency statistics across all tracked peers.
    pub async fn latency_stats(&self) -> Option<LatencyStats> {
        self.router.lock().await.latency_stats()
    }
}
```

- [ ] **Step 2: Add `pub mod node;` to `src/lib.rs`**

```rust
pub mod config;
pub mod types;
pub mod transport;
pub mod protocol;
pub mod routing;
pub mod node;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: success. `start()` creates unused variables (`event_tx`, `event_rx`, `peer_senders`, `peer_counter`, `version`, `network`) — that's fine, they'll be wired in subsequent tasks.

- [ ] **Step 4: Commit**

```bash
git add src/node.rs src/lib.rs
git commit -m "feat(node): add P2pNode struct with start() skeleton"
```

---

### Task 2: Move event loop into `node.rs`

**Files:**
- Modify: `src/node.rs`

Move the event loop from `main.rs` into `start()`. This is the central dispatch that processes `ProtocolEvent`s through the router and sends resulting actions to peers.

- [ ] **Step 1: Add the event loop as a spawned task in `start()`**

Replace the placeholder comment `// Listeners, outbound manager, keepalive, event loop — added in next tasks.` with:

```rust
        // Event loop: process protocol events through the router
        {
            let router = router.clone();
            let peer_senders = peer_senders.clone();
            tokio::spawn(async move {
                event_loop(event_rx, router, peer_senders).await;
            });
        }
```

Then add the `event_loop` function at the bottom of `node.rs` (after the `impl P2pNode` block):

```rust
async fn event_loop(
    mut event_rx: mpsc::Receiver<ProtocolEvent>,
    router: Arc<Mutex<Router>>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
) {
    loop {
        match event_rx.recv().await {
            Some(event) => {
                let actions = router.lock().await.handle_event(event);
                let senders = peer_senders.lock().await;
                for action in actions {
                    match action {
                        Action::Send { target, message } => {
                            if let Some(tx) = senders.get(&target) {
                                let frame = message.to_frame();
                                if tx.send(frame).await.is_err() {
                                    tracing::warn!(peer = %target, "Failed to send to peer");
                                }
                            }
                        }
                    }
                }
            }
            None => {
                tracing::info!("All event senders dropped, event loop exiting");
                break;
            }
        }
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: success. Warnings about unused variables (`event_tx`, `peer_counter`, `version`, `network`) — they'll be used in subsequent tasks.

- [ ] **Step 3: Commit**

```bash
git add src/node.rs
git commit -m "feat(node): add event loop dispatch"
```

---

### Task 3: Move `run_peer` into `node.rs`

**Files:**
- Modify: `src/node.rs`

The per-peer lifecycle: register in router, split connection, concurrent read/write loops, cleanup on disconnect.

- [ ] **Step 1: Add `run_peer` function to `node.rs`**

Add at the bottom of the file:

```rust
async fn run_peer(
    peer_id: PeerId,
    conn: Connection,
    direction: Direction,
    mode: ProxyMode,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
) {
    let spec = conn.peer_spec().clone();
    tracing::info!(
        peer = %peer_id,
        name = %spec.name,
        agent = %spec.agent,
        version = %spec.version,
        direction = ?direction,
        "Peer active"
    );

    // Register peer in router
    router.lock().await.register_peer(peer_id, direction, mode);

    // Send PeerConnected event
    let _ = event_tx.send(ProtocolEvent::PeerConnected {
        peer_id,
        spec: spec.clone(),
        direction,
    }).await;

    // Split connection for concurrent read/write
    let (mut reader, mut writer, magic, _) = conn.split();

    // Create write channel
    let (write_tx, mut write_rx) = mpsc::channel::<Frame>(64);
    peer_senders.lock().await.insert(peer_id, write_tx);

    // Writer task
    let write_handle = tokio::spawn(async move {
        while let Some(frame) = write_rx.recv().await {
            if let Err(e) = crate::transport::frame::write_frame(&mut writer, &magic, &frame).await {
                tracing::warn!(peer = %peer_id, error = %e, "Write failed");
                break;
            }
        }
    });

    // Reader loop
    loop {
        match crate::transport::frame::read_frame(&mut reader, &magic).await {
            Ok(frame) => {
                match ProtocolMessage::from_frame(&frame) {
                    Ok(msg) => {
                        let event = ProtocolEvent::Message { peer_id, message: msg };
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, error = %e, "Message parse failed");
                    }
                }
            }
            Err(e) => {
                tracing::info!(peer = %peer_id, error = %e, "Connection lost");
                break;
            }
        }
    }

    // Cleanup
    peer_senders.lock().await.remove(&peer_id);
    write_handle.abort();

    let _ = event_tx.send(ProtocolEvent::PeerDisconnected {
        peer_id,
        reason: "connection closed".into(),
    }).await;

    tracing::info!(peer = %peer_id, "Peer removed");
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: success. Warning that `run_peer` is unused — it'll be called from accept_loop and outbound_manager.

- [ ] **Step 3: Commit**

```bash
git add src/node.rs
git commit -m "feat(node): add run_peer lifecycle function"
```

---

### Task 4: Move `accept_loop` and `make_handshake_config` into `node.rs`

**Files:**
- Modify: `src/node.rs`

The inbound listener: accepts TCP connections, performs handshake, spawns per-peer tasks.

- [ ] **Step 1: Add `make_handshake_config` helper to `node.rs`**

Add at the bottom of the file:

```rust
fn make_handshake_config(
    identity: &crate::config::IdentityConfig,
    version: Version,
    network: crate::types::Network,
    mode: ProxyMode,
) -> HandshakeConfig {
    HandshakeConfig {
        agent_name: identity.agent_name.clone(),
        peer_name: identity.peer_name.clone(),
        version,
        network,
        mode,
        declared_address: None,
    }
}
```

- [ ] **Step 2: Add `accept_loop` function to `node.rs`**

Add at the bottom of the file:

```rust
async fn accept_loop(
    listener: TcpListener,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    max_inbound: usize,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let remote_ip = addr.ip();
                let inbound_count = router.lock().await.inbound_peers().len();
                if inbound_count >= max_inbound {
                    tracing::warn!(ip = %remote_ip, "F2B_REJECT connection limit exceeded");
                    continue;
                }

                let peer_id = PeerId(peer_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                tracing::info!(peer = %peer_id, ip = %remote_ip, "Inbound connection");

                let hs = HandshakeConfig {
                    agent_name: hs_config.agent_name.clone(),
                    peer_name: hs_config.peer_name.clone(),
                    version: hs_config.version,
                    network: hs_config.network,
                    mode: hs_config.mode,
                    declared_address: hs_config.declared_address,
                };
                let event_tx = event_tx.clone();
                let peer_senders = peer_senders.clone();
                let router = router.clone();

                tokio::spawn(async move {
                    match Connection::inbound(stream, &hs).await {
                        Ok(conn) => {
                            run_peer(peer_id, conn, Direction::Inbound, mode, event_tx, peer_senders, router).await;
                        }
                        Err(e) => {
                            tracing::warn!(ip = %remote_ip, error = %e, "F2B_HANDSHAKE_FAIL bad handshake");
                        }
                    }
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "Accept failed");
            }
        }
    }
}
```

- [ ] **Step 3: Wire listeners into `start()`**

In `start()`, add listener spawning before the event loop spawn. Replace:

```rust
        // Event loop: process protocol events through the router
```

with:

```rust
        // Start listeners
        if let Some(ref listener_cfg) = config.listen.ipv6 {
            let listener = TcpListener::bind(listener_cfg.address).await?;
            tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, "IPv6 listener started");
            let hs_config = make_handshake_config(&config.identity, version, network, listener_cfg.mode);
            tokio::spawn(accept_loop(
                listener, hs_config, listener_cfg.mode, listener_cfg.max_inbound,
                event_tx.clone(), peer_senders.clone(), router.clone(), peer_counter.clone(),
            ));
        }

        if let Some(ref listener_cfg) = config.listen.ipv4 {
            let listener = TcpListener::bind(listener_cfg.address).await?;
            tracing::info!(addr = %listener_cfg.address, mode = ?listener_cfg.mode, "IPv4 listener started");
            let hs_config = make_handshake_config(&config.identity, version, network, listener_cfg.mode);
            tokio::spawn(accept_loop(
                listener, hs_config, listener_cfg.mode, listener_cfg.max_inbound,
                event_tx.clone(), peer_senders.clone(), router.clone(), peer_counter.clone(),
            ));
        }

        // Event loop: process protocol events through the router
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check`
Expected: success. Remaining unused variable warning for outbound-related variables only.

- [ ] **Step 5: Commit**

```bash
git add src/node.rs
git commit -m "feat(node): add accept_loop and wire listeners into start()"
```

---

### Task 5: Move `outbound_manager` into `node.rs`

**Files:**
- Modify: `src/node.rs`

Maintains minimum outbound connections with exponential backoff.

- [ ] **Step 1: Add `outbound_manager` function to `node.rs`**

Add at the bottom of the file:

```rust
async fn outbound_manager(
    seeds: Vec<std::net::SocketAddr>,
    min_peers: usize,
    hs_config: HandshakeConfig,
    mode: ProxyMode,
    event_tx: mpsc::Sender<ProtocolEvent>,
    peer_senders: Arc<Mutex<HashMap<PeerId, PeerSender>>>,
    router: Arc<Mutex<Router>>,
    peer_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut backoff = Duration::from_secs(5);

    loop {
        let current_outbound = router.lock().await.outbound_peers().len();
        if current_outbound < min_peers {
            for addr in &seeds {
                let current = router.lock().await.outbound_peers().len();
                if current >= min_peers {
                    break;
                }

                let addr = *addr;
                tracing::info!(addr = %addr, "Connecting to outbound peer");
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(addr),
                ).await {
                    Ok(Ok(stream)) => {
                        let peer_id = PeerId(peer_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                        let hs = HandshakeConfig {
                            agent_name: hs_config.agent_name.clone(),
                            peer_name: hs_config.peer_name.clone(),
                            version: hs_config.version,
                            network: hs_config.network,
                            mode: hs_config.mode,
                            declared_address: hs_config.declared_address,
                        };
                        let event_tx = event_tx.clone();
                        let peer_senders = peer_senders.clone();
                        let router = router.clone();

                        tokio::spawn(async move {
                            match Connection::outbound(stream, &hs).await {
                                Ok(conn) => {
                                    if handshake::is_proxy(conn.peer_spec()) {
                                        tracing::info!(peer = %peer_id, addr = %addr, "Outbound peer is a proxy, skipping");
                                        return;
                                    }
                                    tracing::info!(peer = %peer_id, "Outbound handshake OK");
                                    run_peer(peer_id, conn, Direction::Outbound, mode, event_tx, peer_senders, router).await;
                                }
                                Err(e) => {
                                    tracing::warn!(peer = %peer_id, addr = %addr, error = %e, "Outbound handshake failed");
                                }
                            }
                        });

                        backoff = Duration::from_secs(5);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(addr = %addr, error = %e, "Connect failed");
                    }
                    Err(_) => {
                        tracing::warn!(addr = %addr, "Connect timeout");
                    }
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}
```

- [ ] **Step 2: Wire outbound manager into `start()`**

In `start()`, add outbound manager spawning between the listener spawns and the event loop spawn. Insert before the `// Event loop` comment:

```rust
        // Start outbound connections
        {
            let hs_config = make_handshake_config(&config.identity, version, network, ProxyMode::Full);
            tokio::spawn(outbound_manager(
                config.outbound.seed_peers.clone(), config.outbound.min_peers,
                hs_config, ProxyMode::Full,
                event_tx.clone(), peer_senders.clone(), router.clone(), peer_counter.clone(),
            ));
        }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: success. No unused variable warnings from `start()` — all shared state is now used.

- [ ] **Step 4: Commit**

```bash
git add src/node.rs
git commit -m "feat(node): add outbound_manager and wire into start()"
```

---

### Task 6: Add keepalive task to `start()`

**Files:**
- Modify: `src/node.rs`

Periodic GetPeers to outbound peers keeps connections alive.

- [ ] **Step 1: Add keepalive spawn in `start()`**

Insert between the outbound manager spawn and the event loop spawn:

```rust
        // Keepalive: send GetPeers every 2 minutes
        {
            let router = router.clone();
            let peer_senders = peer_senders.clone();
            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(120));
                loop {
                    ticker.tick().await;
                    let outbound = router.lock().await.outbound_peers();
                    let senders = peer_senders.lock().await;
                    let frame = ProtocolMessage::GetPeers.to_frame();
                    for pid in outbound {
                        if let Some(tx) = senders.get(&pid) {
                            let _ = tx.send(frame.clone()).await;
                        }
                    }
                }
            });
        }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: success. No new warnings.

- [ ] **Step 3: Commit**

```bash
git add src/node.rs
git commit -m "feat(node): add keepalive task"
```

---

### Task 7: Delete `main.rs`, verify lib-only build

**Files:**
- Delete: `src/main.rs`

This is the point of no return — the crate becomes library-only.

- [ ] **Step 1: Delete `src/main.rs`**

```bash
rm src/main.rs
```

- [ ] **Step 2: Verify the library compiles**

Run: `cargo check`
Expected: success. No binary target compiled (Cargo infers lib-only from absence of `main.rs`).

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: all tests pass. The tests import from the library crate, not the binary, so removing `main.rs` doesn't affect them.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: remove binary target, crate is now lib-only

All runtime code is in the node module. The main ergo-node-rust crate
owns the binary and tokio runtime."
```

---

### Task 8: Rename crate to `enr-p2p`

**Files:**
- Modify: `Cargo.toml:1-2`

The crate is no longer a standalone proxy node. Align the package name with the submodule name.

- [ ] **Step 1: Update `Cargo.toml` package name**

Change:
```toml
[package]
name = "ergo-proxy-node"
```

To:
```toml
[package]
name = "enr-p2p"
```

- [ ] **Step 2: Update all test imports**

In each test file, change `use ergo_proxy_node::` to `use enr_p2p::`:

Files to update:
- `tests/routing_test.rs`
- `tests/transport_test.rs`
- `tests/protocol_test.rs`
- `tests/pcap_verify_test.rs`
- `tests/integration_test.rs`

Search: `ergo_proxy_node`
Replace: `enr_p2p`

- [ ] **Step 3: Update any internal references**

Check `tests/common/` for imports:

```bash
grep -r "ergo_proxy_node" tests/
```

Update all occurrences.

- [ ] **Step 4: Run full test suite**

Run: `cargo test`
Expected: all tests pass with the new crate name.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: rename crate from ergo-proxy-node to enr-p2p"
```
