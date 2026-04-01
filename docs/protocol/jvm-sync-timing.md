# JVM Sync Protocol Timing Reference

> Extracted from ergo v6.0.3 source + pcap capture against testnet peers (2026-04-01).

## Schedulers

Three recurring timers drive the sync process:

**SendLocalSyncInfo** — selects peers that need SyncInfo, sends to those that qualify.
- Initial delay: 2 seconds after startup
- Recurring interval: `syncInterval` (default 5s)
- Implementation: `scheduleWithFixedDelay` (Akka scheduler)
- Handler: `sendSync(historyReader)` → calls `syncTracker.peersToSyncWith()`

**CheckModifiersToDownload** — requests modifiers marked as Unknown/needs-download.
- Interval: `syncInterval` (default 5s)
- Throttled: minimum 50ms between invocations
- Implementation: `scheduleAtFixedRate` (Akka scheduler)
- Distributes requests across peers using `ElementPartitioner`
- Batch sizing: 50-400 headers per peer, 8-12 block sections per peer

**IsChainHealthy** — verifies the chain is still progressing.
- Initial delay: `acceptableChainUpdateDelay` (default 30m)
- Interval: `acceptableChainUpdateDelay / 3` (default 10m)

## Per-Peer Rate Limits

Hardcoded in `ErgoSyncTracker` (not configurable):

| Constant | Value | Location | Effect |
|----------|-------|----------|--------|
| `MinSyncInterval` | 20s | ErgoSyncTracker:20 | Won't send SyncInfo to same peer within 20s |
| `SyncThreshold` | 1m | ErgoSyncTracker:21 | Peer marked "outdated" → gets priority sync |
| `ClearThreshold` | 3m | ErgoSyncTracker:26 | Peer status reset to Unknown (fresh start) |
| `PerPeerSyncLockTime` | 100ms | ErgoNodeViewSynchronizer:93 | Incoming SyncInfo within 100ms silently dropped |

### peersToSyncWith() Logic

Called by `SendLocalSyncInfo` every 5 seconds. Returns peers that should receive SyncInfo:

1. Clear statuses older than 3 minutes → reset to Unknown
2. If any peers are "outdated" (no sync sent for >1 minute) → return those immediately
3. Otherwise, return peers matching:
   - All peers with `Unknown` status
   - All peers with `Fork` status
   - ONE random peer with `Older` status
   - **Only if** ≥20 seconds since last SyncInfo to each non-outdated peer

## Modifier Delivery

| Setting | Default | Location | Effect |
|---------|---------|----------|--------|
| `deliveryTimeout` | 10s | application.conf:531 | Re-request from different peer after timeout |
| `maxDeliveryChecks` | 100 | application.conf:536 | Give up after 100 re-request attempts |
| `maxHeadersPerBucket` | 400 | ErgoNodeViewSynchronizer:88 | Max headers per request batch |
| `minHeadersPerBucket` | 50 | ErgoNodeViewSynchronizer:87 | Min headers per request batch |

### Delivery State Machine

```
Unknown → Requested → Received → Held    (success)
Unknown → Requested → Unknown             (timeout, retry)
Unknown → Requested → Invalid             (max retries exceeded, headers only)
```

### CheckDelivery (on timeout)

When a modifier request times out after `deliveryTimeout` (10s):

| Condition | Action | Penalty |
|-----------|--------|---------|
| Status not Requested | Skip | None |
| Transaction | Drop and forget | None |
| Block section, checks < 100 | Re-request from different peer | NonDelivery (2pts) if not a connectivity issue |
| Block section, checks ≥ 100, header | Mark Invalid | Accumulated from prior timeouts |
| Block section, checks ≥ 100, other | Mark Unknown, stop requesting | Accumulated from prior timeouts |

## Penalty System

| Penalty | Score | Trigger |
|---------|-------|---------|
| NonDeliveryPenalty | 2 | Block section not delivered after timeout |
| MisbehaviorPenalty | 10 | Invalid modifiers, malformed data, semantic failures |
| SpamPenalty | 25 | High-cost transactions, repeated already-applied TX spam |
| PermanentPenalty | 1,000,000,000 | Critical protocol violations |

| Setting | Default | Effect |
|---------|---------|--------|
| `temporalBanDuration` | 60m | Ban duration when threshold reached |
| `penaltySafeInterval` | 2m | Penalty score won't increase within this window |
| `penaltyScoreThreshold` | 500 | Ban triggered at this score |

## Connection Management

| Setting | Default | Effect |
|---------|---------|--------|
| `handshakeTimeout` | 30s | Handshake must complete within this |
| `inactiveConnectionDeadline` | 10m | Connection dropped if no activity for 10 minutes |
| `peerEvictionInterval` | 1h | Random peer eviction for eclipse attack prevention |
| `getPeersInterval` | 2m | Request new peers from random connected peer |

## Observed Sync Cycle (from pcap)

One full cycle as captured from a JVM testnet node syncing from genesis:

```
t+0.000s  CLIENT → SyncInfo     (224-891B, chain state headers)
t+0.025s  PEER   → Inv          (12771B, ~400 header IDs, type=101)
t+0.028s  PEER   → SyncInfo     (891B, peer's own chain state)
t+0.038s  CLIENT → RequestMod   (12771B, requesting same IDs)
t+0.100s  PEER   → Modifier     (~101KB, all ~400 headers)

          [client processes headers, chain height increases]

t+0.170s  CLIENT → SyncInfo     (updated chain state — SECOND send)
t+0.195s  PEER   → Inv          (12771B, next ~400 header IDs)
t+0.203s  CLIENT → RequestMod   (12771B)
t+0.270s  PEER   → Modifier     (~101KB, next ~400 headers)

          [~20 second pause — MinSyncInterval enforced]

t+20.00s  CLIENT → SyncInfo     (next cycle starts)
```

Key observations:
- **Two batches per cycle**: the JVM sends SyncInfo again immediately after processing the first batch, getting ~800 headers per 20-second cycle
- **RequestModifier mirrors Inv exactly**: same byte count, same IDs
- **Peer sends SyncInfo back**: bidirectional state exchange alongside Inv
- **Burst handling**: new block Inv messages (34B, mixed types 101/102/104/108) are handled inline without disrupting the main sync cycle
- **Single peer**: JVM syncs from one peer at a time, uses separate connections for peer discovery

## Source File Reference

| File | Key Lines | Contents |
|------|-----------|----------|
| `ergo-core/.../network/ErgoSyncTracker.scala` | 20-26, 180-205 | Timing constants, peersToSyncWith() |
| `src/.../network/ErgoNodeViewSynchronizer.scala` | 82-93, 275-282, 1230-1293, 1371-1382 | Scheduler setup, delivery checks, download handler |
| `src/.../network/peer/PenaltyType.scala` | 13-32 | All penalty types |
| `src/main/resources/application.conf` | 506-587 | All configurable timing settings |
| `src/.../scorex/core/network/DeliveryTracker.scala` | 30-34, 317 | Modifier state machine, RequestedInfo struct |
