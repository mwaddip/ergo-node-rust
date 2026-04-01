# JVM Modifier Buffer & Out-of-Order Handling

> Extracted from ergo v6.0.3 source, 2026-04-01.

## Architecture

The JVM uses two LRU caches to hold modifiers that arrive before their parents:

| Cache | Max Size | Contents |
|-------|----------|----------|
| `headersCache` | 8,192 | Headers whose parents aren't in the chain yet |
| `modifiersCache` | 384 | Block sections (transactions, AD proofs, extensions) waiting for their header |

Both are LRU (least-recently-used) — eviction is size-based, not time-based.

## Flow: Header Arrives With Missing Parent

### Step 1: Parse and Validate (ErgoNodeViewSynchronizer)

`blockSectionsFromRemote()` receives the `ModifierResponse`, parses each modifier, and calls `validateAndSetStatus()`. For headers, this does syntax validation — a missing parent is a **recoverable error**, not a permanent rejection. Valid modifiers are forwarded to the view holder.

### Step 2: Sort and Apply (ErgoNodeViewHolder.processRemoteModifiers)

```
if batch is headers:
    sort by height
    if first header height == current tip + 1:
        apply sequentially (fast path)
        on first gap → put remaining in headersCache
    else:
        put entire batch in headersCache
    
    applyFromCacheLoop(headersCache)  // drain what we can
    headersCache.cleanOverfull()       // LRU evict if > 8192

if batch is block sections:
    put all in modifiersCache
    applyFromCacheLoop(modifiersCache)
    modifiersCache.cleanOverfull()
```

### Step 3: Drain Loop (applyFromCacheLoop)

Recursive function that repeatedly calls `popCandidate()`:

```
def applyFromCacheLoop(cache):
    candidate = cache.popCandidate(history)
    if candidate exists:
        apply(candidate)
        applyFromCacheLoop(cache)  // tail-recursive
```

### Step 4: popCandidate Logic (ErgoModifiersCache.findCandidateKey)

Two-phase search:

1. **Priority path**: Look for block sections matching the next expected full block height
2. **Exhaustive scan**: Search the entire cache for any modifier where `history.applicableTry()` succeeds — meaning its parent now exists in the chain

Skip headers more than 1 height ahead of the best header (optimization to avoid wasted work).

Permanently invalid modifiers (`MalformedModifierError`) are removed from cache immediately.

### Step 5: LRU Eviction

When a cache exceeds its max size, `cleanOverfull()` removes the least-recently-used entries. Evicted modifier IDs are marked `Unknown` in the delivery tracker:

```scala
cleared.foreach(mId => deliveryTracker.setUnknown(mId, modTypeId))
```

This signals the network layer to re-request them during the next `CheckModifiersToDownload` cycle.

## Key Design Decisions

**Buffer, don't drop.** Out-of-order headers are expected during sync — the JVM never penalizes a peer for sending them. The buffer absorbs the disorder, the drain loop resolves it.

**Sort before applying.** Within a batch, headers are sorted by height. This turns a shuffled batch into sequential appends — no buffering needed for in-batch disorder.

**Fast path for sequential delivery.** If the batch starts at `tip + 1`, apply directly without touching the cache. Only fall back to the cache when gaps are detected.

**Re-request on eviction.** Evicted modifiers aren't lost — they're marked `Unknown` in the delivery tracker, which automatically re-requests them. The LRU cache is a temporary holding area, not final storage.

**No time-based expiry.** Modifiers stay in the cache until evicted by size pressure or successfully applied. There's no TTL.

## Source References

| File | Lines | What |
|------|-------|------|
| `ErgoNodeViewHolder.scala` | 60-62 | Cache definitions (sizes) |
| `ErgoNodeViewHolder.scala` | 313-389 | `processRemoteModifiers` — sort, fast path, cache, drain |
| `ErgoModifiersCache.scala` | 14-45 | `findCandidateKey` / `popCandidate` — cache drain logic |
| `ErgoNodeViewSynchronizer.scala` | 708-737 | `blockSectionsFromRemote` — parse, validate, forward |
| `ErgoNodeViewSynchronizer.scala` | 1453-1471 | Cache size monitoring, download-more trigger |
| `HeadersProcessor.scala` | 383-400 | Header validation — missing parent = recoverable error |
