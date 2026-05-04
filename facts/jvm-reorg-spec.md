# JVM Chain Reorganization — Complete Specification

> Extracted from `ergoplatform/ergo` v6.0.3. Every claim cross-referenced against
> the source files listed. This is the canonical reference for porting reorg to Rust.

## Architecture Overview

The JVM has reorg at **two independent levels**, each with its own best-chain pointer:

| Level | Pointer | Trigger | What reorgs |
|-------|---------|---------|-------------|
| **Header chain** | `BestHeaderKey` | New header has higher cumulative score | Which header is "best" at each height |
| **Full block chain** | `BestFullBlockKey` | New full block's chain has higher cumulative score | UTXO state, mempool, full block status |

Header-level reorg is implicit — it just moves pointers. Full-block-level reorg
produces a `ProgressInfo` with `toRemove` and `toApply` lists that the state
machine uses to roll back and replay.

---

## 1. Cumulative Score (Difficulty) Tracking

**Source:** `HeadersProcessor.toInsert()` (HeadersProcessor.scala:134–198)

Every header stores a cumulative score:

```
score(header) = score(parent) + header.requiredDifficulty
score(genesis) = genesis.requiredDifficulty
```

where `requiredDifficulty = DifficultySerializer.decodeCompactBits(nBits)` — the
actual difficulty value decoded from the compact `nBits` encoding. This is a BigInt,
not the compact u32.

Stored as: `headerScoreKey(id) → BigInt.toByteArray`

where `headerScoreKey(id) = Blake2b256("score" ++ idToBytes(id))`

The current best header's chain score:

```scala
bestHeadersChainScore = bestHeaderIdOpt.flatMap(scoreOf).getOrElse(0)
```

A new header becomes the best header when `score > bestHeadersChainScore`.

---

## 2. Height → Header IDs Index

**Source:** `HeadersProcessor` (HeadersProcessor.scala:203–276)

At each height, a byte array stores concatenated 32-byte header IDs:

```
heightIdsKey(height) → [best_id_32B | alt1_id_32B | alt2_id_32B | ...]
```

- **First 32 bytes** = best chain header at this height
- **Remaining** = alternative chain headers (orphans/forks)

### When a header becomes the new best (`bestBlockHeaderIdsRow`):

1. Put this header's ID first at its height
2. Walk backward through parents via `headerChainBack`
3. For each parent NOT already in best chain: move its ID to first position at that height
4. This "repaints" the best-chain path through the height index

```scala
// HeadersProcessor.scala:212-226
private def bestBlockHeaderIdsRow(h: Header, score: Difficulty) = {
  val self = heightIdsKey(h.height) → (Seq(h.id) ++ headerIdsAtHeight(h.height))
  val forkHeaders = headerChainBack(h.height, parent, h => isInBestChain(h))
    .headers.filter(h => !isInBestChain(h))
  val forkIds = forkHeaders.map { header =>
    val otherIds = headerIdsAtHeight(header.height).filter(id => id != header.id)
    heightIdsKey(header.height) → (Seq(header.id) ++ otherIds)
  }
  forkIds :+ self
}
```

### When a header is NOT the best (`orphanedBlockHeaderIdsRow`):

Append the header's ID to the end of the list at its height.

```scala
// HeadersProcessor.scala:203-206
heightIdsKey(h.height) → (headerIdsAtHeight(h.height) :+ h.id)
```

### Query functions:

```scala
bestHeaderIdAtHeight(height) = first 32 bytes of heightIdsKey(height)
headerIdsAtHeight(height)    = all 32-byte chunks from heightIdsKey(height)
isInBestChain(id)            = bestHeaderIdAtHeight(heightOf(id)) == id
```

---

## 3. Header Processing Flow

**Source:** `HeadersProcessor.process()` (HeadersProcessor.scala:112–126)

When a header arrives:

1. **Validate** the header (parent exists, timestamp, difficulty, PoW)
2. **Compute score** = `scoreOf(parent) + requiredDifficulty`
3. **Compare** score against `bestHeadersChainScore`
4. **If better**: update `BestHeaderKey`, call `bestBlockHeaderIdsRow` (repaints fork path)
5. **If not better**: call `orphanedBlockHeaderIdsRow` (append to alternatives)
6. **Store**: score row, height row, header object, optional NiPoPoW proof
7. **Return** `ProgressInfo`:
   - If verifying transactions AND this is the new best header: `toDownload` = required sections
   - Otherwise: empty (header-only nodes don't need to do anything with state)

**Key insight**: The header level NEVER rolls back. It just updates which header is
"first" at each height. All headers are stored permanently. The concept of "best
chain" is a view over the height index, not a structural property.

---

## 4. Fork Point Detection: `commonBlockThenSuffixes`

**Source:** `ErgoHistoryReader` (ErgoHistoryReader.scala:503–533)

### Outer function (entry point):

```scala
// ErgoHistoryReader.scala:503-522
def commonBlockThenSuffixes(header1: Header, header2: Header): (HeaderChain, HeaderChain) = {
  val heightDiff = max(header1.height - header2.height, 0)

  def loop(numberBack: Int, otherChain: HeaderChain): (HeaderChain, HeaderChain) = {
    val r = commonBlockThenSuffixes(otherChain, header1, numberBack + heightDiff)
    if (r._1.head == r._2.head) {
      r  // heads match → found the fork point
    } else {
      if (!otherChain.head.isGenesis) {
        // Expand otherChain backward and retry with bigger limit
        val biggerOther = headerChainBack(numberBack, otherChain.head, _ => false) ++ otherChain.tail
        loop(biggerOther.size, biggerOther)
      } else {
        // Reached genesis — prepend PreGenesisHeader as sentinel
        (HeaderChain(PreGenesisHeader +: r._1.headers),
         HeaderChain(PreGenesisHeader +: r._2.headers))
      }
    }
  }

  loop(numberBack = 2, otherChain = HeaderChain(Seq(header2)))
}
```

### Inner function (one iteration):

```scala
// ErgoHistoryReader.scala:524-533
def commonBlockThenSuffixes(otherChain: HeaderChain, startHeader: Header, limit: Int) = {
  def until(h: Header) = otherChain.exists(_.id == h.id)
  val ourChain = headerChainBack(limit, startHeader, until)
  val commonBlock = ourChain.head  // headerChainBack INCLUDES the until-header
  val suffix = otherChain.takeAfter(commonBlock)
  (ourChain, suffix)
}
```

### Algorithm walkthrough:

1. Start with `otherChain = [header2]` (the competing tip), `numberBack = 2`
2. Walk `header1` backward up to `numberBack + heightDiff` steps, stopping when hitting a header that exists in `otherChain`
3. If `ourChain.head == otherChain.head` → fork point found. Return both suffix chains.
4. If heads don't match → `otherChain` wasn't long enough. Expand it backward by `numberBack` headers and retry with `numberBack = biggerOther.size`.
5. Repeat until fork point found or genesis reached.

`heightDiff = max(header1.height - header2.height, 0)` accounts for the case where
the old best (`header1`) is at a greater height than the new best (`header2`). This
happens when a shorter fork has higher cumulative difficulty. The extra walk-back
ensures we reach `header2`'s height level on `header1`'s chain.

### Result format:

Both returned chains start at the **common block** (fork point, inclusive):

```
prevChain = [commonBlock, ..., header1]   // our chain from fork point
newChain  = [commonBlock, ..., header2]   // competing chain from fork point
```

So `prevChain.tail` = headers to remove, `newChain.tail` = headers to apply.

---

## 5. `headerChainBack` — Walking Backward

**Source:** `HeadersProcessor` (HeadersProcessor.scala:285–307)

```scala
def headerChainBack(limit: Int, startHeader: Header, until: Header => Boolean): HeaderChain = {
  @tailrec
  def loop(header: Header, acc: Buffer[Header]): Seq[Header] = {
    if (acc.length == limit || until(header)) {
      acc
    } else {
      typedModifierById(header.parentId) match {
        case Some(parent) => loop(parent, acc += parent)
        case None if acc.contains(header) => acc
        case _ => acc += header
      }
    }
  }

  HeaderChain(loop(startHeader, Buffer(startHeader)).reverse)
}
```

**Critical detail**: The `until` condition is checked BEFORE adding the parent.
When `until(header)` is true, `header` IS included in the result (it's already in `acc`).
The result is reversed → chronological order (earliest first).

So if `until` matches at the fork point, the fork point is `result.head`.

---

## 6. Full Block Processing Chain

**Source:** `FullBlockProcessor` (FullBlockProcessor.scala:49–60)

When a full block (header + transactions + proofs) arrives:

```scala
def processFullBlock(fullBlock, newMod) = {
  val bestFullChainAfter = calculateBestChain(fullBlock.header)
  val newBestBlockHeader = typedModifierById(bestFullChainAfter.last)
  processing(ToProcess(fullBlock, newMod, newBestBlockHeader, bestFullChainAfter))
}
```

`processing` is a chain of partial functions tried in order:

1. **`processValidFirstBlock`** — First full block ever (at minimalFullBlockHeight)
2. **`processBetterChain`** — New chain is better → reorg
3. **`nonBestBlock`** — Not better → cache for potential future use

---

## 7. `processBetterChain` — The Reorg

**Source:** `FullBlockProcessor` (FullBlockProcessor.scala:83–116)

### Preconditions (all must be true):

```scala
bestFullBlockOpt.nonEmpty         // we have a current best full block
&& isBetterChain(newBestBlockHeader.id)  // new chain has higher score
&& isLinkable(fullBlock.header)   // can trace back to main chain or genesis
```

### `isBetterChain`:

```scala
// FullBlockProcessor.scala:123-128
def isBetterChain(id: ModifierId): Boolean = {
  (scoreOf(bestFullBlockId), scoreOf(id)) match {
    case (Some(prevBestScore), Some(score)) if score > prevBestScore => true
    case _ => false
  }
}
```

Strictly greater. Equal score = no reorg. This is important — it means the
first-seen chain wins on ties.

### `isLinkable`:

```scala
// FullBlockProcessor.scala:147-171
def isLinkable(header: Header): Boolean = {
  if (bestFullBlockId == header.parentId) {
    true  // direct child of current best
  } else {
    // Walk back through parent chain (via cache then storage)
    // until reaching main chain or dead end
    val headOpt = loop(header.parentId, header.height - 1, Seq.empty).headOption
    headOpt.contains(GenesisParentId) ||
      headOpt.flatMap(id => getFullBlock(id)).isDefined
  }
}
```

Uses `nonBestChainsCache` for fast parent lookups of blocks not in the best chain.

### The reorg itself:

```scala
// FullBlockProcessor.scala:83-116
val prevBest = bestFullBlockOpt.get

// Find the fork point and both suffix chains
val (prevChain, newChain) = commonBlockThenSuffixes(prevBest.header, newBestBlockHeader)

// Blocks to roll back (our chain after fork point)
val toRemove: Seq[ErgoFullBlock] = prevChain.tail.headers.flatMap(getFullBlock)

// Blocks to apply (new chain after fork point)
val toApply: Seq[ErgoFullBlock] = newChain.tail.headers
  .flatMap(h => if (h == fullBlock.header) Some(fullBlock) else getFullBlock(h))
  .ensuring(_.length == newChain.length - 1)  // ALL must be available

// Fork point ID (None if toRemove is empty — shouldn't happen in reorg)
val branchPoint = toRemove.headOption.map(_ => prevChain.head.id)

// Update storage: mark new chain as best, old chain as non-best
val additionalIndexes =
  toApply.map(b => chainStatusKey(b.id) → BestChainMarker) ++
  toRemove.map(b => chainStatusKey(b.id) → NonBestChainMarker)

// Prune non-best chains cache: drop entries below (tip - keepVersions)
val minForkRootHeight = toApply.last.height - nodeSettings.keepVersions
nonBestChainsCache = nonBestChainsCache.dropUntil(minForkRootHeight)

// Return instructions for the state machine
ProgressInfo(branchPoint, toRemove, toApply, Seq.empty)
```

### `.ensuring` on `toApply`:

The JVM requires ALL full blocks in the new chain to be available. If any are
missing, this assertion fails. This means the reorg only fires when the entire
competing chain is assembled.

---

## 8. `nonBestBlock` — Caching Alternative Blocks

**Source:** `FullBlockProcessor` (FullBlockProcessor.scala:130–141)

When a full block doesn't trigger a reorg:

```scala
if (block.header.height > fullBlockHeight - keepVersions) {
  nonBestChainsCache = nonBestChainsCache.add(block.id, block.parentId, block.height)
}
// Store the modifier data, return empty ProgressInfo
```

The cache (`IncompleteFullChainCache`) is a `TreeMap[(height, id) → parentId]` that
enables fast parent-chain traversal for `isLinkable` and `continuationChains`.

---

## 9. `calculateBestChain` — Finding the Best Fork

**Source:** `FullBlockProcessor` (FullBlockProcessor.scala:210–215)

```scala
def calculateBestChain(header: Header): Seq[ModifierId] = {
  continuationChains(header)
    .map(_.tail)
    .map(header.id +: _)
    .maxBy(c => scoreOf(c.last))
}
```

### `continuationChains(fromHeader)`:

**Source:** `FullBlockProcessor` (FullBlockProcessor.scala:176–205)

Finds all possible forward chains from a header by walking height-by-height:

```
height N:   [fromHeader]
height N+1: find all headers at N+1 whose full blocks exist
            match each to its parent chain
height N+2: same
...
until no more continuations found
```

Uses `nonBestChainsCache.getParentId` first, falls back to storage lookup.
Returns `Seq[Seq[ModifierId]]` — each inner seq is one possible continuation.

---

## 10. `reportModifierIsInvalid` — Reorg on Invalid Block

**Source:** `ErgoHistory` (ErgoHistory.scala:122–190)

When state validation rejects a block that was already applied:

1. Find all headers downstream of the invalid one (`continuationHeaderChains`)
2. Mark all as invalid (validity byte = 0)
3. Three cases:
   - **(false, false)**: Invalid block is NOT in best header chain or best full chain → just mark invalid, no reorg
   - **(true, false)**: Only best header chain affected → update `BestHeaderKey` to next valid header at same height
   - **(true, true)** or **(false, true)**: Best full chain affected:
     a. Walk back from best full block to find non-invalidated prefix
     b. `branchPointHeader` = first valid ancestor (or PreGenesisHeader if genesis is invalidated)
     c. Find best valid continuation from branch point using `continuationHeaderChains` with filter `hasFullBlock && !invalidated`
     d. Update chain status markers
     e. Return `ProgressInfo(branchPoint, toRemove=invalidatedChain.tail, toApply=validChain, Seq.empty)`

---

## 11. Chain Status Markers

**Source:** `FullBlockProcessor` (FullBlockProcessor.scala:281–289)

```scala
val BestChainMarker: Array[Byte] = Array(1)
val NonBestChainMarker: Array[Byte] = Array(0)

def chainStatusKey(id: ModifierId) = Blake2b256("main_chain" ++ idToBytes(id))
```

Every full block has a chain status marker. Used by:
- `isInBestFullChain(id)` — checks if marker == BestChainMarker
- Continuation chain search — filters for blocks with full data
- Reorg — flips markers between best and non-best

---

## 12. `ProgressInfo` — Reorg Instructions

**Source:** `ProgressInfo.scala` (consensus/ProgressInfo.scala:1–31)

```scala
case class ProgressInfo[PM <: BlockSection](
  branchPoint: Option[ModifierId],  // fork point (None = no rollback)
  toRemove: Seq[PM],                // blocks to roll back (old chain)
  toApply: Seq[PM],                 // blocks to apply (new chain)
  toDownload: Seq[(TypeId, ModifierId)]  // sections to fetch
)

// Invariant:
if (toRemove.nonEmpty) require(branchPoint.isDefined)

lazy val chainSwitchingNeeded: Boolean = toRemove.nonEmpty
```

The node view holder uses this to:
1. Roll back state to `branchPoint`
2. Unapply `toRemove` blocks from UTXO state
3. Apply `toApply` blocks to UTXO state
4. Request `toDownload` from peers

---

## 13. `continuationHeaderChains` — All Forks from a Point

**Source:** `ErgoHistoryReader` (ErgoHistoryReader.scala:334–353)

Different from `continuationChains` (which is in `FullBlockProcessor` and checks
for full block availability). This one operates on headers only:

```scala
def continuationHeaderChains(header: Header, withFilter: Header => Boolean): Seq[Seq[Header]] = {
  def loop(currentHeight: Option[Int], acc: Seq[Seq[Header]]): Seq[Seq[Header]] = {
    val nextLevelHeaders = currentHeight.toList
      .flatMap(h => headerIdsAtHeight(h + 1))
      .flatMap(id => typedModifierById[Header](id))
      .filter(withFilter)
    if (nextLevelHeaders.isEmpty) {
      acc.map(_.reverse)
    } else {
      val updatedChains = nextLevelHeaders.flatMap { h =>
        acc.find(chain => chain.nonEmpty && h.parentId == chain.head.id).map(c => h +: c)
      }
      val nonUpdatedChains = acc.filter(chain => !nextLevelHeaders.exists(_.parentId == chain.head.id))
      loop(currentHeight.map(_ + 1), updatedChains ++ nonUpdatedChains)
    }
  }
  loop(heightOf(header.id), Seq(Seq(header)))
}
```

Used by `reportModifierIsInvalid` to find all downstream headers of an invalid block.

---

## Summary: Key Properties of JVM Reorg

1. **Cumulative score decides everything.** No tie-breaking by timestamp, block hash, or arrival time. Strictly greater score wins; equal score = no reorg.

2. **Headers are never deleted.** Fork headers persist at their height. The "best chain" is a view (first entry at each height), not a structural property.

3. **Full block reorg requires all blocks available.** The `.ensuring` on `toApply` means the entire competing chain must be assembled before the reorg fires. Partial forks are cached, not applied.

4. **Fork point is found by walking backward.** `commonBlockThenSuffixes` expands both chains backward until they share a head. The search is O(fork_depth × chain_lookups), not O(chain_length).

5. **Two caches assist reorg:**
   - `heightIdsKey` index — multiple headers per height for fork detection
   - `nonBestChainsCache` — parent chain links for non-best blocks (avoids storage lookups)

6. **Reorg is atomic.** The storage update (status markers + best block pointer) happens in a single `insert` call. If it fails, nothing changes.

7. **Chain status markers** (`BestChainMarker` / `NonBestChainMarker`) track which blocks are in the active chain. This enables efficient `isInBestFullChain` queries without walking the chain.

8. **`isLinkable` prevents orphan reorgs.** A block can only trigger a reorg if its parent chain reaches either genesis or the current best chain. Orphan blocks are cached but never applied.
