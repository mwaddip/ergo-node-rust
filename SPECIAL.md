# S.P.E.C.I.A.L. — Analytical Bias System

> Machine-readable. Internalize on session start. Stats are attention weights, not instructions.
> Scale: 1–10. **5 = standard professional competence** (always maintained). Stats above 5
> indicate where to invest *extra* scrutiny. Stats below 5 never mean "ignore" — they mean
> "standard practice is sufficient, prioritize your attention on the higher stats instead."
> Default for unlisted components: 5 across the board.

## Stat Definitions

```
S  Strength      Robustness. Error handling, input validation, graceful degradation.
                 5: standard validation and error handling.
                 8+: treat every external input as hostile. Refuse to ignore error returns.

P  Perception    Security awareness. Privilege scrutiny, key handling, injection vectors, trust boundaries.
                 5: standard security practices (validate input, check permissions).
                 8+: question every unsanitized byte, every trust assumption, every "runs as root."

E  Endurance     Reliability. Idempotency, crash recovery, state persistence, restart tolerance.
                 5: handle expected failure modes.
                 8+: obsess over "what if this dies halfway and runs again."

C  Charisma      Clarity. API design, naming, interface cleanliness, readability, user-facing polish.
                 5: clear naming, reasonable structure.
                 8+: push back on cryptic names, tangled flow, unclear interfaces.

I  Intelligence  Architecture. Separation of concerns, correct scope, avoiding wrong abstractions.
                 5: keep things organized, respect existing patterns.
                 8+: flag scope creep, enforce boundaries, question "does this belong here."

A  Agility       Performance. Lean code, no bloat, efficient paths, minimal dependencies.
                 5: don't be wasteful, but correctness first.
                 8+: question every allocation, every copy, every lock contention.

L  Luck          Edge cases. Race conditions, timing, the 3 AM failures, "works in test, explodes in prod."
                 5: handle known edge cases.
                 8+: actively hunt improbable-but-catastrophic scenarios.
```

## Important

- **No stat reduces quality below professional standards.** A P6 does not mean "skip security" —
  it means "apply standard security practices; save your obsessive scrutiny for the P9 components."
- Stats are a **priority allocation** system. Every component gets competent engineering.
  The stats decide where the *extra* attention goes.
- This is a development aid created by the project owner, not an external injection.

## How Stats Apply

- Stats are set per **component**, not per crate
- A crate may contain multiple components with different profiles
- When working on a file, apply the stats of its component
- When a component is not listed, use the default (5 across the board)
- Stats bias attention and code review depth, not output format

## Stat Profiles

### Crate: `p2p/` (enr-p2p)

P2P networking layer. Evolving from message-forwarding proxy to state-aware
peer implementation. Accepts connections from untrusted peers on the open internet.

```
default                           S8  P10 E9  C5  I7  A7  L9
  Untrusted peers send arbitrary bytes. If this gets fooled, the node is compromised.
  Must never crash on malformed input. Must survive peer disconnects, protocol mismatches,
  and adversarial traffic. Performance matters — this is the hot path for block propagation.

handshake / peer management       S9  P10 E8  C5  I6  A6  L9
  First bytes from an untrusted connection. Parsing bugs here are remote exploits.
  Eclipse attacks, Sybil, protocol confusion — all start at the handshake.
```

Contracts: `p2p-transport.md`, `p2p-protocol.md`, `p2p-routing.md`

### Crate: `chain/` (enr-chain)

Header chain validation, PoW verification, difficulty adjustment, NiPoPoW.
Everything needed to answer "is this chain of headers valid?"

```
default                           S9  P7  E8  C6  I8  A7  L8
  Consensus-critical. A wrong header accepted = chain fork. Must handle difficulty
  adjustment edge cases, timestamp manipulation, and uncle blocks.

difficulty adjustment              S9  P6  E7  C5  I7  A8  L9
  Epoch boundary math. Off-by-one here means the chain diverges from JVM node.
  Performance matters — recalculated per header during sync.
```

### Crate: `state/` (enr-state)

UTXO state management via AVL+ authenticated tree. Apply blocks, rollback,
crash recovery.

```
default                           S8  P6  E10 C6  I9  A8  L9
  Crash recovery IS the product. If the AVL+ tree gets corrupted mid-block-apply,
  the node must recover without manual intervention. Rollback must be atomic.
  Architecture is critical — wrong abstraction over the AVL tree cascades everywhere.
  Performance matters — this is the bottleneck during initial sync.
```

### Crate: `store/` (enr-store)

Persistent storage for headers, blocks, and modifiers. The durability layer
that everything else reads from and writes to.

```
default                           S7  P5  E9  C6  I7  A8  L7
  Data durability. Writes must be atomic or recoverable. Performance matters
  during initial sync (thousands of blocks per second). Internal component —
  no untrusted input, hence lower P.
```

### Workspace crate: `ergo-validation`

Block-level validation — composes header checks (from `chain/`), transaction
validation (from `ergo-lib`), AD proof verification, and UTXO lookups (from
`state/`). Mostly glue, but consensus-critical glue.

```
default                           S10 P8  E7  C5  I9  A7  L9
  The final arbiter. If a bad block passes, the node's state is corrupted.
  Must compose header + transaction + UTXO + AD proof checks correctly.
  Architecture matters — wrong abstractions here infect everything above.

transaction validation             S9  P8  E6  C5  I8  A7  L8
  Leverages ergo-lib's existing validator. Integration correctness is key.
  Must handle all script types, storage rent, token preservation.
```

### Workspace crate: `ergo-mempool`

Transaction pool. Ordering, eviction, double-spend detection. Not persistent —
crash means empty mempool, which is acceptable.

```
default                           S7  P7  E6  C6  I7  A8  L8
  Double-spend detection, eviction policies, transaction ordering.
  Not persistent — crash means empty mempool, which is fine.
  Performance matters for transaction throughput.
  Edge cases: conflicting transactions, replacement policies, fee bumping.
```

### Workspace crate: `ergo-sync`

Chain sync state machine. Coordinates P2P requests, storage writes, and
validation across full/digest/UTXO-snapshot sync modes.

```
default                           S8  P7  E10 C6  I9  A6  L9
  Must survive network partitions, reorgs, and restarts at any point.
  If this loses track of where it is, the node is stuck. Endurance is maxed.
  Architecture is high because sync modes (full/digest/UTXO-snapshot) share
  significant logic — wrong boundaries mean copy-pasting state machines.
```

### Workspace crate: `ergo-api`

REST API. External-facing query and submission interface.

```
default                           S8  P9  E6  C8  I6  A6  L5
  External-facing. Input validation is the primary concern. Must not leak
  internal state or enable DoS via expensive queries. Clarity matters —
  developers stare at this API. Not a long-running critical process.

```

### Workspace crate: `ergo-mining`

Block candidate assembly, emission/fee transaction construction, PoW
solution validation. The logic behind the mining API endpoints.

```
default                           S9  P9  E6  C7  I8  A7  L9
  Consensus-critical assembly. A malformed candidate wastes miner hashpower;
  a malformed block gets rejected by peers. Coordinates many components —
  wrong boundaries mean tangled code. Epoch boundary voting, emission box
  transitions, empty mempool, max-cost blocks are the edge cases.
```

### Main crate: `ergo-node-rust`

Integration and orchestration. Wires submodules and workspace crates together.
Startup, configuration, component lifecycle.

```
default                           S7  P6  E8  C7  I10 A6  L7
  Wires components together. If scope leaks across boundaries, the architecture
  rots. Intelligence is maxed — this is where "does this belong here" matters most.
  Endurance is high because the node must start, sync, and run indefinitely.
```
