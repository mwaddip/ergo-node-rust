# Delivery Tracker + Modifier Buffer — Design Spec (Follow-up)

> Track pending modifier requests with timeouts and retry. Buffer out-of-order modifiers with LRU eviction. Re-request evicted modifiers automatically.

## Problem

The Ergo sync protocol is fundamentally lossy. Each step (SyncInfo → Inv → ModifierRequest → ModifierResponse) can fail silently. The JVM node survives this because its delivery tracker re-requests on 10-second timeouts. Without it, the protocol falls apart after the first glitch.

**Observed behavior (2026-04-01):** Against both remote testnet peers and a local fully-synced JVM, our node consistently syncs ~12 batches (~4800 headers) per connection before the JVM stops processing our SyncInfo. JVM debug logs show the message is received but never reaches `processSync`. Reconnection starts a new session and syncs ~12 more batches. The protocol is correct — the delivery tracker is what makes it robust.

Specific issues:

1. **No delivery tracking.** The sync machine sends ModifierRequest and hopes headers arrive. If a response is lost or the peer goes quiet, the request is never retried. The only recovery is the 60-second stall timeout — 6x slower than the JVM's 10-second retry.

2. **Naive pending buffer.** The pipeline's `HashMap<BlockId, Header>` pending buffer has a hard cap (10,000) and drops new entries when full. Headers that can't chain are never re-requested. Duplicate batches from overlapping SyncInfo responses fill the buffer (mitigated by height check + stale purge, but not eliminated).

The JVM solves both with a delivery tracker (timeout + re-request) and an LRU modifier cache (buffer + evict + re-request evicted). The tracker isn't optional — it's the mechanism that makes the lossy protocol usable.

## Design

### Part 1: Delivery Tracker

Lives in the sync crate (`sync/src/delivery.rs`). Tracks pending modifier requests:

```rust
struct PendingRequest {
    peer: PeerId,
    requested_at: Instant,
    checks: u32,
}

pub struct DeliveryTracker {
    pending: HashMap<[u8; 32], PendingRequest>,
    timeout: Duration,       // 10s, matching JVM deliveryTimeout
    max_checks: u32,         // 100, matching JVM maxDeliveryChecks
}
```

#### State machine per modifier

```
Unknown → Requested → Received
              ↓
         (timeout) → Unknown (retry from different peer)
              ↓
         (max checks) → Abandoned
```

#### Integration points

**On ModifierRequest sent:** `tracker.mark_requested(id, peer)`

**On modifier received by pipeline:** `tracker.mark_received(id)`

**On check timer (every 5s):**
- For each pending request where `elapsed > timeout`:
  - If `checks < max_checks`: re-request from a different outbound peer, increment checks
  - If `checks >= max_checks`: mark abandoned, log warning

**On peer disconnect:** re-request all pending from that peer via a different peer

**On eviction from modifier buffer (Part 2):** mark evicted IDs as Unknown → eligible for re-request on next check

### Part 2: Modifier Buffer (replaces pipeline pending HashMap)

Replaces the current `HashMap<BlockId, Header>` in the pipeline with an LRU cache, matching the JVM's `ErgoModifiersCache`.

```rust
pub struct ModifierBuffer {
    cache: LinkedHashMap<BlockId, Header>,  // insertion-ordered for LRU
    max_size: usize,                        // 8192, matching JVM headersCache
}
```

#### Behavior

**On header with missing parent:** `buffer.put(header.parent_id, header)` — if cache is full, evict LRU entry.

**Drain after each successful chain append:** same recursive drain as today, but from the LRU cache instead of a plain HashMap.

**On eviction:** evicted header IDs are sent to the delivery tracker as Unknown → will be re-requested automatically.

**Fast path (matching JVM):** Before buffering, check if the batch starts at `tip + 1`. If so, apply sequentially without touching the buffer. Only buffer on the first gap.

### Part 3: Integration

The sync machine's `tokio::select!` gains a fourth event source — the delivery check timer (5s interval):

```rust
tokio::select! {
    event = self.transport.next_event() => { ... }
    Some(height) = self.progress.recv() => { ... }
    _ = tokio::time::sleep(until_next_sync) => { ... }
    _ = tokio::time::sleep(until_next_delivery_check) => {
        self.delivery.check_timeouts(&self.transport).await;
    }
}
```

### JVM reference

| Component | JVM | Ours |
|-----------|-----|------|
| Delivery timeout | 10s | 10s |
| Max re-request attempts | 100 | 100 |
| Check interval | 5s | 5s |
| Header buffer size | 8,192 (LRU) | 8,192 (LRU) |
| Block section buffer | 384 (LRU) | Not needed yet (headers only) |
| Re-request on eviction | Yes (mark Unknown) | Yes |
| Penalty on non-delivery | 2 pts (NonDeliveryPenalty) | Not applicable yet |

### What this replaces

**In sync machine:** fire-and-forget `transport.send_to(ModifierRequest)` → tracked `delivery.request(ids, peer)`

**In pipeline:** `HashMap<BlockId, Header>` pending buffer with hard cap → `ModifierBuffer` with LRU eviction and re-request signaling

### Dependencies

- Sync machine rework (event-driven loop) — **done**
- Pipeline progress channel — **done**
- Modifier buffer needs a channel back to the delivery tracker for eviction notifications

### Scope

- `sync/src/delivery.rs` — new file, DeliveryTracker
- `src/pipeline.rs` — replace HashMap pending with ModifierBuffer, add eviction channel
- `sync/src/state.rs` — add delivery check timer to select!, wire tracker into handle_event
- `src/main.rs` — wire eviction channel

### Protocol reference

See `docs/protocol/jvm-modifier-buffer.md` for the full JVM implementation analysis.
