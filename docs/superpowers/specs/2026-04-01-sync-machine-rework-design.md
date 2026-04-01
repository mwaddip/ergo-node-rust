# Sync Machine Rework — Event-Driven JVM-Compatible Sync

> Rewrite the header sync state machine to match the JVM node's exchange pattern, eliminating sync stalls caused by behavioral mismatches.

## Problem

The current sync machine stalls after ~30 batches (~2 minutes) regardless of the SyncInfo interval. Pcap comparison reveals behavioral differences between our node and the JVM that cause the peer to stop responding:

1. **Single-batch cycles** — the JVM sends SyncInfo again immediately after processing a batch, getting ~800 headers per 20-second cycle. We send once and wait, getting 400.
2. **No response to peer SyncInfo** — the peer sends SyncInfo back during the exchange. The JVM responds with its own SyncInfo. We silently consume it. The peer's sync tracker for our connection may deprioritize us.
3. **No peer rotation** — we always pick `peers[0]`. If that peer stops responding, we retry the same peer forever.
4. **Timer-based polling** — the sync machine polls chain height every 3 seconds, gated by a 20-second minimum. The JVM reacts to events (Inv received, headers processed) and sends SyncInfo in response.

## Design

### Approach: Event-driven sync loop

Replace the timer-based Syncing state with an event-driven loop that reacts to three sources via `tokio::select!`:

1. **P2P events** — Inv (header IDs from peer), SyncInfo (peer's chain state), peer disconnect
2. **Pipeline progress** — `mpsc::Receiver<u32>` carrying chain height after each validated batch
3. **Sync timer** — fires every 20 seconds as a floor for the next scheduled SyncInfo

### Sync cycle

One full cycle matches the JVM's observed pattern:

```
send SyncInfo ──► receive Inv ──► send ModifierRequest
                                        │
    ┌───────────────────────────────────┘
    ▼
wait for pipeline progress notification
    │
    ▼
send SyncInfo (second batch) ──► receive Inv ──► send ModifierRequest
                                                       │
    ┌──────────────────────────────────────────────────┘
    ▼
wait for pipeline progress notification
    │
    ▼
wait for 20s timer ──► next cycle
```

The 20-second timer is a floor. If both batches complete in 2 seconds, the machine waits until 20 seconds total have elapsed. This matches the JVM's `MinSyncInterval` (20s) enforced by `ErgoSyncTracker.peersToSyncWith()`.

### Pipeline progress channel

The `ValidationPipeline` gets a `mpsc::Sender<u32>` at construction. After each batch that increases the chain height, it sends the new height via `try_send` (non-blocking, pipeline never waits on the sync machine). Channel capacity: 4.

The sync machine reads progress notifications in its `tokio::select!`. When a notification arrives, it immediately sends SyncInfo with the updated chain state — this is the second-batch trigger. The machine no longer polls `chain_height()` on a timer for progress detection.

### Responding to peer SyncInfo

When the sync peer sends us SyncInfo, we respond with our own SyncInfo. The JVM does this when `syncSendNeeded` is true (which includes status=Older, our case during sync). This response doesn't count against the 20-second timer — it's a conversational response, not a scheduled send.

### Peer rotation on stall

A `stalled_peers: HashSet<PeerId>` tracks peers that failed to produce progress within the stall timeout (60s).

On stall:
1. Add current peer to `stalled_peers`
2. Pick first outbound peer NOT in the set
3. Send SyncInfo to new peer, reset stall timer
4. If all peers stalled: clear the set, retry (network may have recovered)

On successful pipeline progress: clear `stalled_peers` — all peers are eligible again.

No state transition to Idle on stall — the loop stays running and tries the next peer. Only transitions to Idle if no outbound peers are available at all.

### Stall detection

Stall timeout: 60 seconds since the last pipeline progress notification. "Progress" means chain height increased — not that we sent a SyncInfo or received an Inv.

### States

The three states (Idle, Syncing, Synced) remain conceptually but are handled as conditions within the main loop rather than separate methods:

- **Idle**: no outbound peers — wait for peer connect events
- **Syncing**: running the event-driven cycle described above
- **Synced**: peer reports no more headers or peer tip ≤ our height — switch to periodic polling (30s SyncInfo, react to Inv for new blocks)

### Files changed

| File | Change |
|------|--------|
| `sync/src/state.rs` | Full rewrite — event-driven sync loop |
| `src/pipeline.rs` | Add `progress_tx: Sender<u32>`, send height after each batch |
| `src/main.rs` | Wire progress channel between pipeline and sync machine |

Unchanged: `sync/src/traits.rs`, `sync/src/lib.rs`, `src/bridge.rs`, `chain/`, `p2p/`.

### Interface change

`HeaderSync::new` gains a progress receiver:

```rust
pub fn new(
    transport: T,
    chain: C,
    progress: mpsc::Receiver<u32>,
) -> Self
```

### What this does NOT include

**Delivery tracker** — tracking individual modifier requests with timeouts and re-request from different peers. This is specced separately (see `2026-04-01-delivery-tracker-design.md`) for follow-up implementation. The current fire-and-forget approach continues; the two-batch cycle and peer rotation should resolve the stalls without delivery tracking.

### Timing constants

| Constant | Value | Source |
|----------|-------|--------|
| `MIN_SYNC_INTERVAL` | 20s | JVM `MinSyncInterval` (ErgoSyncTracker:20) |
| `STALL_TIMEOUT` | 60s | JVM `SyncThreshold` is 1m for outdated peer detection |
| `SYNCED_POLL_INTERVAL` | 30s | Existing, reasonable for tip monitoring |
| Progress channel capacity | 4 | Enough for burst, sync machine drains fast |

### Success criteria

- Sync progresses continuously past 14,000 headers without stalling (matching JVM behavior on same peers)
- Two batches per cycle visible in logs (~800 headers per 20-second cycle)
- Peer rotation on stall: visible in logs when switching peers
- No regressions: Synced state still detects new blocks, idle state still waits for peers
