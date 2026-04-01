# Delivery Tracker — Design Spec (Follow-up)

> Track pending modifier requests with timeouts and retry. Not implemented in the sync rework — specced here for the next phase.

## Problem

The sync machine sends ModifierRequest and hopes headers arrive. If a response is lost (network glitch, peer unresponsive), the request is never retried. The only recovery is the stall timeout (60s), which wastes time and relies on the next SyncInfo cycle to re-discover the same headers.

The JVM tracks every modifier request individually and re-requests from a different peer after 10 seconds.

## Design

### DeliveryTracker struct

Lives in the sync crate (`sync/src/delivery.rs`). Tracks pending modifier requests:

```rust
struct PendingRequest {
    peer: PeerId,
    requested_at: Instant,
    checks: u32,
}

struct DeliveryTracker {
    pending: HashMap<[u8; 32], PendingRequest>,  // id → request info
    timeout: Duration,       // 10s default, matching JVM
    max_checks: u32,         // 100, matching JVM
}
```

### State machine per modifier

```
Unknown → Requested → Received
              ↓
         (timeout) → Unknown (retry from different peer)
              ↓
         (max checks) → Abandoned
```

### Integration points

**On ModifierRequest sent:** `tracker.mark_requested(id, peer)`

**On pipeline progress (batch processed):** for each header ID in the batch, `tracker.mark_received(id)`

**On check timer (every 5s, matching JVM's CheckModifiersToDownload interval):**
- For each pending request where `elapsed > timeout`:
  - If `checks < max_checks`: re-request from a different outbound peer, increment checks
  - If `checks >= max_checks`: mark abandoned, log warning

**On peer disconnect:** re-request all pending from that peer via a different peer

### What this replaces

The current fire-and-forget in `handle_event`:
```rust
let _ = self.transport.send_to(peer_id, ModifierRequest { ... }).await;
```

Becomes:
```rust
self.delivery.request(peer_id, modifier_type, ids, &self.transport).await;
```

### JVM reference

| Setting | JVM Value | Our Value |
|---------|-----------|-----------|
| `deliveryTimeout` | 10s | 10s |
| `maxDeliveryChecks` | 100 | 100 |
| Check interval | 5s (syncInterval) | 5s |
| Re-request penalty | NonDeliveryPenalty (2pts) | Not applicable (no penalty system yet) |

### Scope

This is a sync crate change only. No P2P or pipeline changes needed. The delivery tracker wraps the existing `transport.send_to()` calls with tracking and retry logic.

### Dependencies

Requires the sync machine rework (event-driven loop) to be in place first — the delivery check timer needs to be a fourth event source in the `tokio::select!`.
