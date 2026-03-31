# Async Validation Pipeline

> Decouple modifier validation from the P2P event loop so validation never blocks routing.

## Problem

The `ModifierValidator` trait runs synchronously inside the P2P router's `handle_event`, which runs inside the event loop under `Arc<Mutex<Router>>`. When a 400-header ModifierResponse arrives, the validator PoW-checks each header, chain-validates, buffers out-of-order headers, and drains the pending buffer — all while the event loop is blocked. No events are tapped to the subscriber, no messages are received, no SyncInfo is processed. One client syncing headers chokes the entire P2P layer.

Observed on testnet: sync progresses in 400-header bursts, then stalls for 30+ seconds with zero events flowing through the system. The stall height varies per run (7183, 11572) because it depends on when the event loop happens to be blocked while the JVM sends its next Inv response.

This gets worse as the node adds more validation (transactions, blocks, AD proofs) — each new modifier type adds more synchronous work to the event loop.

## Design

### Approach: Single pipeline task with batch processing

One tokio task drains a modifier channel in batches. Headers within a batch are sorted by height before chain-validation, eliminating most out-of-order buffering. A pending buffer catches only cross-batch stragglers.

```
P2P event loop ──Action::Validate──> mpsc(4096) ──> pipeline task ──> HeaderChain
                 (microseconds)                     (batch drain,
                                                     sort by height,
                                                     PoW + chain validate)
```

The P2P event loop emits `Action::Validate` (a channel send — microseconds) and moves on immediately. Routing, forwarding, and event tapping are never blocked by validation.

### P2P layer changes (submodule prompt)

**Remove:**
- `ModifierValidator` trait (`p2p/src/routing/validator.rs`)
- `validator: Option<Box<dyn ModifierValidator>>` field from `Router`
- `set_validator()` method
- Validator call in the `ModifierResponse` handler

**Add:**
- New action variant: `Action::Validate { modifier_type: u8, id: [u8; 32], data: Vec<u8> }`
- The `ModifierResponse` handler emits one `Action::Validate` per modifier alongside the existing `Action::Send` for forwarding
- Event loop dispatches `Action::Validate` by sending to a channel

**Change `P2pNode::start` signature:**
- Replace `validator: Option<Box<dyn ModifierValidator>>` with `modifier_sink: Option<tokio::sync::mpsc::Sender<(u8, [u8; 32], Vec<u8>)>>`
- If no sink is provided, `Action::Validate` is dropped (pure proxy mode)
- The event loop sends `(modifier_type, id, data)` tuples to the sink for each `Action::Validate`

**ModifierResponse handler becomes:**
```rust
ProtocolMessage::ModifierResponse { modifier_type, modifiers } => {
    let mut actions = Vec::new();
    for (id, data) in &modifiers {
        self.latency_tracker.record_response(id);
        // Emit for async validation
        actions.push(Action::Validate {
            modifier_type,
            id: *id,
            data: data.clone(),
        });
        // Forward to requester (unchanged)
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

### Validation pipeline (main crate: `src/pipeline.rs`)

**Struct:**
```rust
pub struct ValidationPipeline {
    rx: mpsc::Receiver<(u8, [u8; 32], Vec<u8>)>,
    chain: Arc<Mutex<HeaderChain>>,
    tracker: HeaderTracker,
    pending: HashMap<BlockId, Header>,
}
```

**Run loop — batch drain:**
```rust
loop {
    // Block until at least one modifier arrives
    let first = rx.recv().await;   // None = channel closed, exit

    // Drain all available without blocking
    let mut batch = vec![first];
    while let Ok(item) = rx.try_recv() {
        batch.push(item);
    }

    // Process batch
    self.process_batch(batch).await;
}
```

**Batch processing:**
1. Filter to headers only (modifier_type == 101). Other types are logged and dropped for now — future modifier types (transactions, blocks) will get their own processing paths.
2. Parse each header from raw bytes. Drop unparseable.
3. PoW-verify each header. Drop invalid PoW.
4. Sort valid headers by height (ascending).
5. Lock `HeaderChain` once for the entire batch.
6. For each header in height order: try `chain.try_append()`. If parent not found, add to pending buffer.
7. After all headers in the batch are processed, drain the pending buffer: iteratively check if any buffered header's parent is now in the chain. Repeat until no more can be chained.
8. Release the chain lock.
9. Log progress: batch size, chain height after, pending buffer size.

**Pending buffer:** `HashMap<BlockId, Header>` keyed by parent_id. Same as the current validator's buffer, same drain logic. Capped at 10,000 entries. Headers within a single sorted batch rarely need buffering — the buffer primarily catches cross-batch gaps.

### Sync machine changes

None to the sync machine itself. The benefits are indirect:
- The P2P event loop no longer blocks on validation, so events flow through the subscriber channel without congestion
- The 33-second stalls disappear because `Action::Validate` is a channel send (microseconds), not 400 PoW checks
- The `try_lock` dance in the old validator is gone — both pipeline and sync machine use normal `.lock().await` on the chain mutex

### Integration (`main.rs`)

```rust
// Modifier channel — P2P produces, pipeline consumes
let (modifier_tx, modifier_rx) = mpsc::channel(4096);

// Shared chain state
let chain = Arc::new(Mutex::new(HeaderChain::new(chain_config)));

// P2P — no validator, just the modifier sink
let p2p = Arc::new(P2pNode::start(config, Some(modifier_tx)).await?);

// Validation pipeline
let pipeline_chain = chain.clone();
tokio::spawn(async move {
    let mut pipeline = ValidationPipeline::new(modifier_rx, pipeline_chain);
    pipeline.run().await;
});

// Sync machine (unchanged)
let events = p2p.subscribe().await;
let transport = P2pTransport::new(p2p.clone(), events);
let sync_chain = SharedChain::new(chain.clone());
tokio::spawn(async move {
    let mut sync = HeaderSync::new(transport, sync_chain);
    sync.run().await;
});
```

### Files changed

| File | Action | Crate |
|------|--------|-------|
| `p2p/src/routing/validator.rs` | Delete | p2p (submodule prompt) |
| `p2p/src/routing/router.rs` | Remove validator, add Action::Validate | p2p (submodule prompt) |
| `p2p/src/routing/mod.rs` | Remove validator re-export | p2p (submodule prompt) |
| `p2p/src/node.rs` | Change start() signature, dispatch Validate actions | p2p (submodule prompt) |
| `src/validator.rs` | Delete | main |
| `src/pipeline.rs` | New — ValidationPipeline | main |
| `src/lib.rs` | Replace validator module with pipeline module, update re-exports | main |
| `src/main.rs` | New wiring: channel + pipeline task | main |
| `src/bridge.rs` | Remove HeaderValidator reference if any | main |

### What this does NOT change

- `chain/` submodule: untouched. HeaderChain, PoW verification, difficulty adjustment stay the same.
- `sync/` crate: untouched. SyncTransport, SyncChain traits, HeaderSync state machine stay the same.
- P2P routing logic: Inv handling, ModifierRequest routing, SyncInfo pairing all stay the same.
- The subscriber tap-before-route pattern: unchanged, but now the routing step is fast.

### Future extensions

- **Transaction validation:** add a second processing path in `process_batch` for modifier_type == 2. Route to a transaction validator (ergo-lib) instead of HeaderChain.
- **Block validation:** compose header + transaction + AD proof validation in the pipeline.
- **PoW gate at the router:** if adversarial peers become a concern, add a stateless `fn(&[u8]) -> bool` PoW check back into the ModifierResponse handler. This is a future optimization, not required for correctness.
- **Multiple pipelines:** if one pipeline can't keep up, shard by modifier type — headers to one task, transactions to another. The channel-based architecture supports this naturally.
