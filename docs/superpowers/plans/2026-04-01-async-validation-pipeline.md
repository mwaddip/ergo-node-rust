# Async Validation Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move modifier validation out of the P2P event loop into an async pipeline so routing never blocks on validation.

**Architecture:** The router emits `Action::Validate` instead of calling a synchronous validator. The event loop dispatches these to a `tokio::sync::mpsc` channel. A `ValidationPipeline` task drains the channel in batches, sorts headers by height, and chain-validates without blocking the P2P layer.

**Tech Stack:** Rust, tokio, no new dependencies.

---

### Task 1: Write P2P submodule prompt — remove validator, add Action::Validate

This task produces a prompt for the P2P submodule session. The prompt goes in `prompts/` and is NOT executed here — the user copies it to the P2P session.

**Files:**
- Create: `prompts/p2p-async-validation.md`

- [ ] **Step 1: Write the submodule prompt**

```markdown
# Remove ModifierValidator, add Action::Validate

## Goal

Decouple modifier validation from the router. The router should emit
`Action::Validate` for each modifier in a `ModifierResponse` instead of
calling a synchronous validator hook. The event loop dispatches these to
a channel provided at startup.

## Changes

### 1. Remove ModifierValidator

Delete `src/routing/validator.rs` entirely.

In `src/routing/mod.rs`, remove the `pub mod validator;` line:

```rust
pub mod inv_table;
pub mod tracker;
pub mod router;
pub mod latency;
```

### 2. Add Action::Validate variant

In `src/routing/router.rs`, add a new variant to `Action` (line 22-27):

```rust
/// A routing directive.
#[derive(Debug)]
pub enum Action {
    Send {
        target: PeerId,
        message: ProtocolMessage,
    },
    /// Forward modifier data to the async validation pipeline.
    Validate {
        modifier_type: u8,
        id: [u8; 32],
        data: Vec<u8>,
    },
}
```

### 3. Update ModifierResponse handler

In `src/routing/router.rs`, replace the `ModifierResponse` arm. Remove the
validator call. Emit `Action::Validate` per modifier. Keep latency tracking
and request fulfillment unchanged.

Remove the validator import, field, and `set_validator()` method from `Router`.

The `ModifierResponse` arm becomes:

```rust
ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
    let mut actions = Vec::new();
    for (id, data) in &modifiers {
        actions.push(Action::Validate {
            modifier_type,
            id: *id,
            data: data.clone(),
        });
        self.latency_tracker.record_response(id);
        if let Some(requester) = self.request_tracker.fulfill(id) {
            actions.push(Action::Send {
                target: requester,
                message: ProtocolMessage::ModifierResponse {
                    modifier_type,
                    modifiers: vec![(*id, data.clone())],
                },
            });
        }
    }
    actions
}
```

### 4. Change P2pNode::start signature

In `src/node.rs`, replace the `validator` parameter with a modifier sink channel:

```rust
pub async fn start(
    config: Config,
    modifier_sink: Option<tokio::sync::mpsc::Sender<(u8, [u8; 32], Vec<u8>)>>,
) -> Result<Self, Box<dyn std::error::Error>>
```

Remove the `set_validator` call on the router (it no longer exists).

Store the `modifier_sink` in the node or pass it to the event loop.

### 5. Dispatch Action::Validate in the event loop

In the event loop (`src/node.rs`), after `router.lock().await.handle_event(event)`
returns actions, dispatch `Action::Validate` to the modifier sink:

```rust
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
        Action::Validate { modifier_type, id, data } => {
            if let Some(ref sink) = modifier_sink {
                let _ = sink.try_send((modifier_type, id, data));
            }
        }
    }
}
```

Use `try_send` (non-blocking) so the event loop never blocks on the pipeline.

### 6. Update tests

Remove any test that references `ModifierValidator`, `ModifierVerdict`,
`set_validator`, or the validator field. Update the `ModifierResponse`
routing test to verify `Action::Validate` is emitted alongside `Action::Send`.

Add a test:

```rust
#[test]
fn modifier_response_emits_validate_action() {
    let mut router = Router::new();
    router.register_peer(PeerId(1), Direction::Outbound, ProxyMode::Full);
    router.register_peer(PeerId(2), Direction::Inbound, ProxyMode::Full);

    // Peer 2 requests via inv route
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::Inv { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });
    router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(2),
        message: ProtocolMessage::ModifierRequest { modifier_type: 1, ids: vec![[0xaa; 32]] },
    });

    // Response arrives
    let actions = router.handle_event(ProtocolEvent::Message {
        peer_id: PeerId(1),
        message: ProtocolMessage::ModifierResponse {
            modifier_type: 1,
            modifiers: vec![([0xaa; 32], vec![1, 2, 3])],
        },
    });

    // Should have both Validate and Send
    let has_validate = actions.iter().any(|a| matches!(a, Action::Validate { .. }));
    let has_send = actions.iter().any(|a| matches!(a, Action::Send { .. }));
    assert!(has_validate, "should emit Action::Validate");
    assert!(has_send, "should emit Action::Send to requester");
}
```

### 7. Run full test suite

Run: `cargo test`
Expected: all tests pass.

### 8. Commit

```bash
git add -A
git commit -m "feat(routing): replace ModifierValidator with Action::Validate

Remove synchronous validator hook from the router. ModifierResponse
handling now emits Action::Validate for each modifier, dispatched to
an external channel by the event loop. Routing never blocks on
validation."
```
```

- [ ] **Step 2: Save to prompts/**

Save the above content to `prompts/p2p-async-validation.md`.

- [ ] **Step 3: Commit the prompt**

```bash
git add prompts/p2p-async-validation.md
git commit -m "Add prompt: P2P submodule — replace validator with Action::Validate"
```

---

### Task 2: TDD — ValidationPipeline processes a batch of headers

After the P2P submodule change is applied and pulled, implement the pipeline in the main crate.

**Files:**
- Create: `src/pipeline.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing test — pipeline processes sorted batch**

In `src/pipeline.rs`, write the test first. This test verifies that a batch of out-of-order headers gets sorted, PoW-verified, and chain-validated correctly.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use enr_chain::ChainConfig;

    /// Build a pipeline with a testnet chain and a channel.
    fn test_pipeline() -> (
        ValidationPipeline,
        tokio::sync::mpsc::Sender<(u8, [u8; 32], Vec<u8>)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let chain = Arc::new(Mutex::new(HeaderChain::new(ChainConfig::testnet())));
        let pipeline = ValidationPipeline::new(rx, chain);
        (pipeline, tx)
    }

    #[test]
    fn rejects_unparseable_header() {
        let (mut pipeline, _tx) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], vec![0xff, 0x00, 0x01])];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "bad header should not be chained");
    }

    #[test]
    fn ignores_non_header_modifier_types() {
        let (mut pipeline, _tx) = test_pipeline();
        let batch = vec![(102, [0xaa; 32], vec![0xff; 100])];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "non-header types should be skipped");
    }

    #[test]
    fn accepts_valid_pow_header_into_pending() {
        use enr_chain::Header;
        use sigma_ser::ScorexSerializable;

        let json = r#"{
            "extensionId": "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty": "16384",
            "votes": "000000",
            "timestamp": 4928911477310178288,
            "size": 223,
            "stateRoot": "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height": 614400,
            "nBits": 37748736,
            "version": 2,
            "id": "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot": "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot": "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash": "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions": {
                "pk": "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
                "n": "0000000000003105"
            },
            "parentId": "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;
        let header: Header = serde_json::from_str(json).unwrap();
        let bytes = header.scorex_serialize_bytes().unwrap();

        let (mut pipeline, _tx) = test_pipeline();
        let batch = vec![(HEADER_TYPE_ID, [0xaa; 32], bytes)];
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(pipeline.process_batch(batch));

        // Header has valid PoW but parent is missing, so it goes to pending
        assert_eq!(pipeline.pending.len(), 1, "valid PoW header should be buffered");
        let chain = rt.block_on(pipeline.chain.lock());
        assert_eq!(chain.height(), 0, "unchainable header should not increase height");
    }
}
```

- [ ] **Step 2: Write the pipeline struct and `process_batch` method**

```rust
use std::collections::HashMap;
use std::sync::Arc;

use enr_chain::{BlockId, ChainError, Header, HeaderChain, HeaderTracker};
use tokio::sync::{mpsc, Mutex};

/// Modifier type ID for headers (NetworkObjectTypeId in JVM source).
const HEADER_TYPE_ID: u8 = 101;

/// Max pending headers before we start dropping old entries.
const MAX_PENDING: usize = 10_000;

/// Async validation pipeline for modifiers.
///
/// Receives raw modifier data from the P2P layer via a channel, validates
/// in batches (sort by height, PoW check, chain-validate), and updates
/// the shared HeaderChain. Runs as a single tokio task.
pub struct ValidationPipeline {
    rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
    chain: Arc<Mutex<HeaderChain>>,
    tracker: HeaderTracker,
    pending: HashMap<BlockId, Header>,
}

impl ValidationPipeline {
    pub fn new(
        rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
        chain: Arc<Mutex<HeaderChain>>,
    ) -> Self {
        Self {
            rx,
            chain,
            tracker: HeaderTracker::new(),
            pending: HashMap::new(),
        }
    }

    /// Run the pipeline loop. Returns when the channel closes.
    pub async fn run(&mut self) {
        tracing::info!("validation pipeline started");
        loop {
            let first = match self.rx.recv().await {
                Some(item) => item,
                None => {
                    tracing::info!("validation pipeline channel closed");
                    return;
                }
            };

            let mut batch = vec![first];
            while let Ok(item) = self.rx.try_recv() {
                batch.push(item);
            }

            self.process_batch(batch).await;
        }
    }

    /// Process a batch of raw modifiers.
    pub(crate) async fn process_batch(&mut self, batch: Vec<(u8, [u8; 32], Vec<u8>)>) {
        // Filter to headers only
        let raw_headers: Vec<(u8, [u8; 32], Vec<u8>)> = batch
            .into_iter()
            .filter(|(t, _, _)| *t == HEADER_TYPE_ID)
            .collect();

        if raw_headers.is_empty() {
            return;
        }

        // Parse and PoW-verify
        let mut valid_headers: Vec<Header> = Vec::with_capacity(raw_headers.len());
        for (_modifier_type, _id, data) in &raw_headers {
            let header = match enr_chain::parse_header(data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::debug!("pipeline: rejecting header: parse failed: {e}");
                    continue;
                }
            };
            if let Err(e) = enr_chain::verify_pow(&header) {
                tracing::debug!(
                    "pipeline: rejecting header at height {}: {e}",
                    header.height
                );
                continue;
            }
            valid_headers.push(header);
        }

        if valid_headers.is_empty() {
            return;
        }

        // Sort by height — within a batch this eliminates most buffering
        valid_headers.sort_by_key(|h| h.height);

        // Lock chain once for the whole batch
        let mut chain = self.chain.lock().await;
        let height_before = chain.height();

        for header in &valid_headers {
            self.tracker.observe(header);
            match chain.try_append(header.clone()) {
                Ok(()) => {
                    // Drain pending buffer from this header
                    let mut next_parent = header.id;
                    while let Some(buffered) = self.pending.remove(&next_parent) {
                        let bid = buffered.id;
                        match chain.try_append(buffered.clone()) {
                            Ok(()) => {
                                self.tracker.observe(&buffered);
                                next_parent = bid;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(ChainError::ParentNotFound { .. })
                | Err(ChainError::InvalidGenesisParent { .. })
                | Err(ChainError::InvalidGenesisHeight { .. }) => {
                    if self.pending.len() < MAX_PENDING {
                        self.pending.insert(header.parent_id, header.clone());
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "pipeline: rejecting header at height {}: {e}",
                        header.height
                    );
                }
            }
        }

        let height_after = chain.height();
        drop(chain);

        if height_after > height_before {
            tracing::info!(
                chain_height = height_after,
                batch_size = valid_headers.len(),
                pending = self.pending.len(),
                "pipeline: chained headers from height {height_before}"
            );
        }
    }
}
```

- [ ] **Step 3: Add pipeline module to lib.rs**

Replace the contents of `src/lib.rs`:

```rust
mod bridge;
mod pipeline;

pub use bridge::{P2pTransport, SharedChain};
pub use pipeline::ValidationPipeline;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ergo-node-rust`
Expected: all 3 new pipeline tests pass, 0 validator tests (deleted).

- [ ] **Step 5: Commit**

```bash
git add src/pipeline.rs src/lib.rs
git commit -m "Add ValidationPipeline: async batch-drain header validation

Replaces the synchronous HeaderValidator. Receives modifiers via
channel, drains in batches, sorts headers by height, PoW-verifies,
and chain-validates. Pending buffer catches cross-batch stragglers."
```

---

### Task 3: Delete old validator

**Files:**
- Delete: `src/validator.rs`

- [ ] **Step 1: Delete the file**

```bash
rm src/validator.rs
```

- [ ] **Step 2: Verify build**

Run: `cargo check`
Expected: clean (lib.rs no longer references the validator module).

Note: `main.rs` will NOT compile at this point because it still references `HeaderValidator`. That's fixed in Task 4.

- [ ] **Step 3: Commit**

```bash
git add src/validator.rs
git commit -m "Remove HeaderValidator: replaced by ValidationPipeline"
```

---

### Task 4: Rewire main.rs

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Update main.rs to use the pipeline**

Replace the full contents of `src/main.rs`:

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

    // Start P2P with modifier sink (no validator)
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(modifier_tx)).await?);

    // Validation pipeline
    let pipeline_chain = chain.clone();
    tokio::spawn(async move {
        let mut pipeline = ValidationPipeline::new(modifier_rx, pipeline_chain);
        pipeline.run().await;
    });

    // Subscribe to events for the sync machine
    let events = p2p.subscribe().await;

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), events);
    let sync_chain = SharedChain::new(chain.clone());

    // Start sync in a background task
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(transport, sync_chain);
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

- [ ] **Step 2: Remove HeaderValidator from bridge.rs if referenced**

Check `src/bridge.rs` — it currently does not reference `HeaderValidator`, so no changes needed. Verify:

Run: `cargo check`
Expected: clean compile.

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "Rewire main.rs: pipeline channel replaces validator

P2pNode::start takes a modifier_sink channel instead of a validator.
ValidationPipeline runs as a separate tokio task, decoupling header
validation from the P2P event loop."
```

---

### Task 5: Build, deploy, verify on testnet

**Files:** none (operational verification)

- [ ] **Step 1: Build release deb**

```bash
./build-deb
```

Expected: `target/ergo-node-rust_0.1.0_amd64.deb` built successfully.

- [ ] **Step 2: Deploy to testnet**

```bash
scp target/ergo-node-rust_0.1.0_amd64.deb admin@95.179.246.102:/tmp/
ssh admin@95.179.246.102 "sudo dpkg -i /tmp/ergo-node-rust_0.1.0_amd64.deb"
```

- [ ] **Step 3: Restart with debug logging**

```bash
ssh admin@95.179.246.102 "sudo sed -i 's/RUST_LOG=info/RUST_LOG=debug/' /usr/lib/systemd/system/ergo-node-rust.service && sudo systemctl daemon-reload && sudo systemctl restart ergo-node-rust"
```

- [ ] **Step 4: Verify pipeline processes batches without stalling**

Watch logs for:
- `validation pipeline started` — pipeline is running
- `pipeline: chained headers from height` — batches are being processed
- No 30-second gaps between sync progress events
- Sync progresses continuously past previous stall points (7183, 11572)

```bash
ssh admin@95.179.246.102 "sudo tail -f /var/log/ergo-node/ergo-node.log"
```

- [ ] **Step 5: Restore info logging**

```bash
ssh admin@95.179.246.102 "sudo sed -i 's/RUST_LOG=debug/RUST_LOG=info/' /usr/lib/systemd/system/ergo-node-rust.service && sudo systemctl daemon-reload"
```

- [ ] **Step 6: Commit any fixes if needed, push**

```bash
git push
```
