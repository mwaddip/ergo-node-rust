# JVM Sync Exchange Protocol — Complete Specification

> Extracted from ergo v6.0.3 source, 2026-04-01. Every condition, every timer, every state transition.

## Overview

The JVM sync exchange is a bidirectional protocol between peers for synchronizing header chains. It uses V2 SyncInfo messages containing full serialized headers (not just IDs). The protocol is event-driven with three initiators: a periodic scheduler, incoming SyncInfo from peers, and modifier application callbacks.

## Constants

| Name | Value | Source |
|------|-------|--------|
| `syncInterval` | 5 seconds | `application.conf`, scheduler period |
| `MinSyncInterval` | 20 seconds | `ErgoSyncTracker:20`, per-peer send floor |
| `PerPeerSyncLockTime` | 100 ms | `ErgoNodeViewSynchronizer:93`, incoming rate limit |
| `SyncThreshold` | 1 minute | `ErgoSyncTracker:21`, "outdated" threshold |
| `ClearThreshold` | 3 minutes | `ErgoSyncTracker:26`, status reset threshold |
| `continuationIds size` | 400 | `processSyncV2`, max headers per Inv |
| `FullV2SyncOffsets` | [0, 16, 128, 512] | `ErgoHistoryReader:601` |
| `ReducedV2SyncOffsets` | [0] | `ErgoHistoryReader:604` |
| `CheckModifiersToDownload` | 50 ms min | `ErgoNodeViewSynchronizer:1373` |

## Per-Peer State (`ErgoPeerStatus`)

Each connected peer has:

```
status:           PeerChainStatus  // Unknown | Younger | Older | Equal | Fork | Nonsense
height:           u32              // peer's declared chain height
lastSyncSentTime: Option<Time>     // when WE last sent SyncInfo TO this peer
lastSyncGetTime:  Option<Time>     // when WE last received SyncInfo FROM this peer
```

### Status Enum (`PeerChainStatus`)

| Status | Meaning | Action |
|--------|---------|--------|
| `Unknown` | Haven't compared chains yet | No action |
| `Younger` | Peer is behind us | Send continuation headers (Inv) |
| `Older` | Peer is ahead of us | Apply headers from SyncInfo / request more |
| `Equal` | Same tip header | No action |
| `Fork` | Different block at same height, common ancestor exists | Send continuation headers (Inv) |
| `Nonsense` | Peer's chain makes no sense | No action |

## V2 SyncInfo Format

```
[VLQ: 0]            (1 byte for value 0)
[byte: 0xFF]         V2 marker
[u8: header_count]   number of headers (0-50)
[for each header:]
  [VLQ: header_size] serialized header byte count
  [bytes: header]    scorex-serialized header
```

### Header Selection Offsets

- **Full** (`full=true`): headers at offsets [0, 16, 128, 512] from tip
- **Reduced** (`full=false`): header at offset [0] only (tip header)

Full mode is used for periodic sync sends. Reduced mode is used after applying headers from a peer (response SyncInfo).

### SyncInfo Caching

The JVM caches V2 SyncInfo keyed by `headersHeight`. If height unchanged, returns cached value regardless of `full` parameter. Cache is effectively disabled during active sync (height changes every batch).

## Event Sources (3 initiators)

### 1. Periodic Scheduler (`SendLocalSyncInfo`)

**Interval:** Every `syncInterval` (5 seconds).

```
scheduler fires every 5s
  → sendSync(history)
    → peers = syncTracker.peersToSyncWith()
    → partition peers into V1 and V2 groups
    → for V2 peers: send getV2SyncInfo(history, full=true)
```

### `peersToSyncWith()` — Peer Selection

Called every 5 seconds. Determines WHO to send SyncInfo to.

```
clearOldStatuses()  // reset peers not synced for > ClearThreshold (3 min)

if any peer has lastSyncSentTime > SyncThreshold (1 min):
  return those outdated peers  // priority: stale peers first
else:
  candidates = Unknown peers + Fork peers + one random Older peer
  filter: only peers where (now - lastSyncSentTime) >= MinSyncInterval (20s)
  return filtered candidates

side effect: updateLastSyncSentTime for all returned peers
```

**Key insight:** A peer with status `Younger` or `Equal` is NEVER selected for proactive sync, unless it becomes outdated (>1 minute since last sync sent). The JVM only proactively syncs with Unknown, Fork, and Older peers.

### 2. Incoming SyncInfo from Peer

```
receive SyncInfo from peer
  → processSync(history, syncInfo, peer)
    → diff = syncTracker.updateLastSyncGetTime(peer)    // always updates timestamp
    → if diff <= PerPeerSyncLockTime (100ms):
        log "spammy sync detected"                       // DROP — do nothing
        return
    → dispatch to processSyncV1 or processSyncV2
```

### `processSyncV2(history, syncInfo, peer)`

```
(status, syncSendNeeded) = syncTracker.updateStatus(peer, syncInfo, history)

match status:
  Unknown  → log, do nothing
  Nonsense → log warning, do nothing

  Younger  → ext = history.continuationIds(syncInfo, size=400)
             if ext.isEmpty: log warning
             sendExtension(peer, ext)    // → Inv message with up to 400 header IDs

  Fork     → ext = history.continuationIds(syncInfo, size=400)
             if ext.isEmpty: log warning
             sendExtension(peer, ext)

  Older    → applyValidContinuationHeaderV2(syncInfo, history, peer)
             // extract continuation header from SyncInfo, apply directly

  Equal    → do nothing

if syncSendNeeded:
  ownSyncInfo = getV2SyncInfo(history, full=true)
  sendSyncToPeer(peer, ownSyncInfo)
```

### `syncSendNeeded` Calculation

```
syncSendNeeded = (oldStatus != status)        // status changed
              || notSyncedOrOutdated(peer)     // never sent sync, or sent > 1 min ago
              || status == Older               // peer is ahead — always respond
              || status == Fork                // fork detected — always respond
```

For a Younger peer after the first exchange:
- `oldStatus != status` → false (still Younger)
- `notSyncedOrOutdated` → false if sync was sent within 1 minute
- `status == Older` → false
- `status == Fork` → false
- **Result: `syncSendNeeded = false`** — the JVM does NOT send its SyncInfo back to a Younger peer after the first status change, unless 1 minute elapses.

### 3. Modifier Application Callback (`blockSectionsFromRemote`)

When the JVM receives and validates headers from a peer:

```
receive ModifierResponse with headers from peer
  → parse modifiers
  → validate each modifier
  → if any valid headers:
      send valid headers to view holder for application
      if first valid modifier is a Header:
        syncInfo = getV2SyncInfo(history, full=false)   // REDUCED — tip only
        sendSyncToPeer(peer, syncInfo)                  // respond to sender
```

**This is the feedback loop.** After receiving headers, the JVM immediately sends a reduced SyncInfo back to the source peer. This triggers another round of comparison → Inv → request → response → SyncInfo.

## `sendSyncToPeer(peer, syncInfo)`

```
if syncInfo.nonEmpty:
  syncTracker.updateLastSyncSentTime(peer)   // updates per-peer timestamp
  send syncInfo to peer via network
```

**Critical side effect:** Updates `lastSyncSentTime`, which feeds into `peersToSyncWith()` MinSyncInterval filtering and `notSyncedOrOutdated()`.

## `sendExtension(peer, ext)`

```
group extension by modifier type
for each (type, ids):
  send Inv(type, ids) to peer
```

Sends one or more Inv messages. For header sync, this is a single Inv with up to 400 header IDs.

## Chain Comparison (`compareV2`)

```
if we have no headers:
  if peer has no headers: Equal
  else: Older  // peer has headers, we don't

if peer sent empty SyncInfo:
  Younger  // peer has nothing

otherHeight = peer's tip header height
myHeight = our tip header height

if otherHeight == myHeight:
  if otherLastHeader.id == myLastHeader.id: Equal
  else if commonPoint(peer's other headers) exists: Fork
  else: Unknown

if otherHeight > myHeight: Older
if otherHeight < myHeight: Younger
```

## Continuation Headers (`continuationIdsV2`)

```
if peer sent empty SyncInfo:
  return headers from genesis to min(headersHeight, 400)

commonPoint(peer's headers):
  find first header in peer's list whose ID exists in our database
  
if commonPoint found:
  return header IDs from (commonPoint.height + 1) to min(headersHeight, commonPoint.height + 399)

if no commonPoint:
  return empty  // no Inv sent — this is the "Extension is empty" warning
```

## Status Lifecycle for a Syncing Peer

Timeline for a peer that connects and syncs from us:

```
T=0:    Peer connects. Status = not tracked yet (no entry in statuses map).
T=5:    Our scheduler fires. peersToSyncWith() won't find this peer (no entry).
        Peer sends us SyncInfo → processSync:
          - updateLastSyncGetTime: creates/updates entry
          - updateStatus: creates entry with status (e.g., Younger)
          - syncSendNeeded: oldStatus=Unknown, newStatus=Younger → true (status changed)
          - We send Inv (continuationIds) AND our SyncInfo back

T=5+:   Peer receives Inv → sends ModifierRequest.
        Peer receives our SyncInfo → processes it → may send their SyncInfo back.
        We receive ModifierRequest → send ModifierResponse.

T=5+:   Peer receives ModifierResponse → applies headers → sends SyncInfo (reduced).
        We receive peer's SyncInfo:
          - updateLastSyncGetTime: diff > 100ms → process
          - compareV2: Younger (peer still behind)
          - syncSendNeeded: oldStatus=Younger, newStatus=Younger → false
            (unless notSyncedOrOutdated triggers, which it won't within 1 minute)
          - We send Inv (continuationIds) but NOT our SyncInfo
          
T=25:   Our scheduler fires. peersToSyncWith():
          - Peer status is Younger → NOT selected (only Unknown/Older/Fork selected)
          - Unless lastSyncSentTime > SyncThreshold (1 min) → then outdated → selected
          
T=65:   1 minute since last sync sent → notSyncedOrOutdated = true.
        Next incoming SyncInfo from peer: syncSendNeeded = true → we respond with SyncInfo.
        OR: peersToSyncWith() selects peer as outdated → we proactively send SyncInfo.
```

### Critical Observation

For a Younger peer, the JVM:
1. Sends Inv (continuationIds) on EVERY processSync call — unlimited
2. Sends its own SyncInfo back only when `syncSendNeeded` is true:
   - First time (status change from Unknown → Younger)
   - After 1 minute of no sync sent (outdated threshold)
   - When status changes (Younger → Equal, Younger → Older, etc.)

The Inv is the primary mechanism. The SyncInfo response is secondary. **The JVM does NOT need to send SyncInfo back to keep sending Inv.** It sends Inv every time it processes incoming SyncInfo from a Younger peer, regardless of syncSendNeeded.

## What Keeps the Exchange Going

In JVM-to-JVM sync:

```
A sends SyncInfo → B
B: Younger → sends Inv to A
A: receives Inv → sends ModifierRequest to B  
B: sends ModifierResponse to A
A: receives headers → blockSectionsFromRemote:
   sends REDUCED SyncInfo back to B          ← THIS IS THE FEEDBACK LOOP
B: receives A's SyncInfo → processSync:
   - PerPeerSyncLockTime check (100ms) — usually passes
   - compareV2: Younger → sends Inv to A
A: receives Inv → cycle continues
```

The feedback loop is: **after receiving headers, immediately send SyncInfo back to the source peer.** This triggers another comparison → Inv → request → response → SyncInfo cycle. The 20s scheduler is just a fallback.

## What We Were Missing

Our Rust node was not sending SyncInfo in response to:
1. Incoming peer SyncInfo — fixed (now responds)
2. After receiving and chaining headers — partially fixed via progress trigger, but async

The remaining gap: our progress-triggered SyncInfo fires after the pipeline processes headers asynchronously, which may be delayed relative to the JVM's synchronous `blockSectionsFromRemote` callback. The JVM sends SyncInfo DURING modifier processing (same handler). We send it AFTER the pipeline chains headers and the progress channel fires.

## Source File Index

| Component | File | Key Lines |
|-----------|------|-----------|
| processSync gate | ErgoNodeViewSynchronizer.scala | 385-397 |
| processSyncV2 | ErgoNodeViewSynchronizer.scala | 453-494 |
| sendSync (scheduler) | ErgoNodeViewSynchronizer.scala | 342-361 |
| sendSyncToPeer | ErgoNodeViewSynchronizer.scala | 366-371 |
| sendExtension | ErgoNodeViewSynchronizer.scala | 374-380 |
| peersToSyncWith | ErgoSyncTracker.scala | 180-205 |
| updateStatus / syncSendNeeded | ErgoSyncTracker.scala | 66-81 |
| notSyncedOrOutdated | ErgoSyncTracker.scala | 51-59 |
| clearOldStatuses | ErgoSyncTracker.scala | 133-142 |
| compareV2 | ErgoHistoryReader.scala | 162-198 |
| continuationIdsV2 | ErgoHistoryReader.scala | 274-292 |
| commonPoint | ErgoHistoryReader.scala | 131-135 |
| syncInfoV2 / offsets | ErgoHistoryReader.scala | 393-409, 598-605 |
| blockSectionsFromRemote | ErgoNodeViewSynchronizer.scala | 708-737 |
| getV2SyncInfo (caching) | ErgoNodeViewSynchronizer.scala | 316-325 |
| PerPeerSyncLockTime | ErgoNodeViewSynchronizer.scala | 92-93 |
| ErgoPeerStatus | ErgoPeerStatus.scala | 17-25 |
| PeerChainStatus | PeerChainStatus.scala | 6-36 |
| SyncV2Filter (version check) | VersionBasedPeerFilteringRule.scala | 52-59 |
| applyValidContinuationHeaderV2 | ErgoNodeViewSynchronizer.scala | 503-523 |
| CheckModifiersToDownload | ErgoNodeViewSynchronizer.scala | 1371-1382 |
| requestDownload | ErgoNodeViewSynchronizer.scala | 648-683 |
