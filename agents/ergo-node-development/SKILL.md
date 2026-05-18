---
name: ergo-node-development
description: Use when working on the ergo-node-rust project or any of its submodules — covers architecture philosophy, interface contracts, multi-session workflow, Ergo protocol specifics, S.P.E.C.I.A.L. system, and development principles
---

# Ergo Node Development

## Overview

ergo-node-rust is a ground-up Rust implementation of the Ergo blockchain full node. Not a port of the JVM reference — a new implementation using the existing sigma-rust ecosystem for cryptography and script evaluation, with a custom P2P networking layer reverse-engineered from the JVM node's wire protocol.

The codebase follows a multi-session development model: one main session coordinates interface contracts and integration, while submodule sessions implement individual components within their contract boundaries.

## Core Philosophy

### Design by Contract

Every component boundary has explicit preconditions, postconditions, and invariants. Contracts are documented in `facts/` and enforced via `debug_assert!` in Rust. The contract change pipeline: update the contract first, push it, then implement against it. Never change code first and contract after.

### The wire is the spec

The Ergo P2P protocol has no formal specification. Protocol behavior was reverse-engineered from the JVM reference node and verified against pcap captures. When the JVM source disagrees with observed traffic, the traffic wins. Capture first, implement second.

### Interface integrity

When interfaces don't match, fix the interface — never wrap the mismatch. No adapters, no shims, no glue code. Trace the mismatch to whichever side is wrong and fix it at the source. Duct tape hides the bug and breeds more duct tape.

### Reuse before building

The Rust Ergo ecosystem has substantial existing components. Use them. `ergo-lib` has transaction validation, `ergo-chain-types` has header structs, `ergo_avltree_rust` has the authenticated tree. Don't reimplement what exists and works. Ship self-contained binaries, not framework towers.

### Incremental validation

Each phase adds one capability without breaking what came before. The node was built incrementally — each layer (P2P, header chain validation, PoW, block validation, state, mempool, REST API, mining) was useful on its own before the next was added. The same approach should guide future large features.

### Dependency minimalism

If a crate does 300 things and you need 3, consider whether to use it or write the 3. But for consensus-critical math (PoW, script evaluation, AVL trees), reuse the battle-tested implementations. The line is: crypto and consensus = reuse, plumbing = build.

## Architecture

### Repository structure

```
ergo-node-rust/                     # Main repo — integration, orchestration
  facts/                            # Interface contracts + SPECIAL.md
  docs/protocol/                    # P2P wire format spec (reverse-engineered)
  docs/                             # Roadmap, design docs
  prompts/                          # Prompts for submodule sessions
  src/                              # Main crate — wires components together
```

### Component model

Components are crates with explicit trait-based boundaries. Each component:
- Defines its public API as traits in its contract
- Accepts dependencies as trait objects (not concrete types)
- Owns its internal state
- Is testable in isolation via mock implementations of its dependencies

### Phased build order

1. **Header awareness** — parse headers from P2P, track chain tip
2. **PoW verification** — verify Autolykos v2 before accepting headers
3. **Header chain validation** — parent hashes, timestamps, difficulty adjustment
4. **Transaction validation** — validate against input boxes (ergo-lib does the heavy lifting)
5. **UTXO state management** — AVL+ tree backed, apply/rollback blocks
6. **Full node** — storage, chain sync, mempool, REST API

## S.P.E.C.I.A.L. System

Analytical bias weights per component. Not instructions — attention allocation. Scale 1-10, where 5 = standard professional competence (always maintained). Stats above 5 indicate where to invest extra scrutiny.

```
S  Strength      Robustness, error handling, input validation
P  Perception    Security awareness, trust boundaries, untrusted input
E  Endurance     Reliability, crash recovery, state persistence
C  Charisma      Clarity, API design, naming, readability
I  Intelligence  Architecture, separation of concerns, correct scope
A  Agility       Performance, lean code, efficient paths
L  Luck          Edge cases, race conditions, consensus edge cases
```

Full profiles are in `facts/SPECIAL.md`. Key highlights:
- P2P networking is P10 (untrusted peers send arbitrary bytes)
- Block validation is S10 (wrong validation = corrupted state)
- UTXO state management is E10 (crash recovery IS the product)
- Chain sync is E10 + L9 (must survive anything and resume)
- Integration/orchestration is I10 (architecture rot starts here)

## Multi-Session Workflow

The dispatch mechanics — kitty automation, executor identity, anti-recursion guards — live in the `dispatching-prompts` and `receiving-prompts` skill pair at `github.com/mwaddip/claude-dbc`. Install both before working on this codebase if you intend to dispatch.

### Role Detection (read this first)

This skill is loaded by both the main session and per-crate dispatched
sessions. The rules below apply differently depending on which you are.
Detect via your working directory:

- **Main session**: `pwd` is the repo root (`…/ergo-node-rust`). You
  coordinate, edit `facts/` + top-level docs + `prompts/`, and dispatch
  per-crate work via the `dispatching-prompts` skill (kitty
  `--type=window` + `ac` + send-text "use the receiving-prompts
  skill..."). You do not edit crate-internal source.
- **Per-crate dispatched session**: `pwd` is a crate subdirectory
  (`…/ergo-node-rust/<crate>`). You edit files in your crate. You do
  NOT dispatch further. The canonical mechanism for executor identity
  + anti-recursion rules is the `receiving-prompts` skill (loaded by
  the dispatch message). If you are reading THIS section as an
  executor, you should have loaded `receiving-prompts` first; do so
  now and follow its rules.

### Session roles

**Main session** (cwd = repo root):
- Owns `facts/` — the interface contracts
- Coordinates component integration
- Writes prompts for per-crate sessions and dispatches them via the
  `dispatching-prompts` skill (kitty `--type=window` + `ac`)
- Pulls and integrates work from dispatched sessions
- **Never edits crate-internal source code directly**

**Per-crate dispatched sessions** (cwd = a crate subdirectory):
- Receive a prompt file path as their first instruction
- Read the prompt, the boilerplate it references, and the relevant
  `facts/<area>.md` contract
- Implement within their crate's directory boundary
- Report completion back to the main session via the coordination
  block at the bottom of the prompt (kitty `send-text --match=id:<main>`)
- Never edit files outside their crate. If a cross-cutting change
  is needed (e.g. a contract update), stop and surface it to main

### Making interface changes

1. Main session updates the contract in `facts/` first
2. Main session dispatches per-crate sessions to implement against the
   updated contract
3. Main session verifies integration

Never change code first and contract after. The contract leads.

## Protocol Reference

### P2P Wire Format

Full spec in `docs/protocol/ergo-p2p-wire-format.md` and `docs/protocol/ergo-p2p-spec.yaml`.

Key discoveries:
- **All Scorex integers use VLQ encoding** — method names like `putUShort` are misleading
- **Handshake feature lengths are u16 BE** (not VLQ)
- **Mode feature (ID 16)** contains state type, verifying flag, NiPoPoW flag, block population count
- **Message framing**: `[magic_4B | code_1B | body_length_4B | checksum_4B | body]`
- **Checksum**: Blake2b256(body)[0..4]
- **Magic bytes**: Mainnet `[01,00,02,04]`, Testnet `[02,03,02,03]` (post Feb 2026 reset)

### JVM Reference

The canonical source for protocol behavior is `github.com/ergoplatform/ergo`. Key sub-modules:
- `ergo-core` — SPV primitives: P2P messages, PoW, NiPoPoW, header validation
- `ergo-wallet` — transaction signing and verification (already in sigma-rust)
- `avldb` — authenticated AVL+ tree persistence (Rust port exists)

Keep a local checkout of the v6.0.3 tag (or whatever the current consensus tag is) for grep-able reference when porting protocol behavior.

## Design Patterns

### The wire is the spec

The Ergo P2P protocol was reverse-engineered from pcap captures of JVM node traffic. When Scala source and observed bytes disagree, trust the bytes. This applies to any protocol interaction — the running system is the authority, not the documentation.

### Interfaces write themselves

Good documentation in `facts/` produces correct implementations even when prompts don't explicitly specify details. If the contract is precise, the submodule session builds the right thing. Invest in the contract; save on the prompt.

### Derivation over configuration

If both sides can compute a value, don't configure it. Port numbers, magic bytes, protocol versions — these are functions of the network type, not config entries.

### Block height over timestamps

All timing logic must use block height, not timestamps. Block height is deterministic and monotonically increasing. Timestamps are miner-adjustable and drift across implementations. Height is the universal clock.

### Crash recovery as a feature

The node will be killed. Power will fail. The kernel will OOM. Design every stateful component to survive `kill -9` at any point and resume correctly. This isn't a nice-to-have — it's the primary design constraint for storage and state management.

### Untrusted input everywhere

Every byte from the network is adversarial until validated. Message length fields are DoS vectors. VLQ-encoded values can claim arbitrary sizes. Peers send protocol versions from the future. Cap allocations, validate before parsing, and never trust declared sizes.

## Verification Methodology

Every change must be verified against three sources of truth:

1. **The spec** (`docs/protocol/`) — what the protocol should do
2. **The JVM node** (a local checkout of `ergoplatform/ergo` at the current consensus tag) — what the reference implementation does
3. **The pcap** — what actually goes on the wire

Any two agreeing against the third is a finding. The pcap is the tiebreaker — observed behavior trumps both documentation and source code.

**For consensus-critical code** (PoW verification, header validation, transaction validation): test against real mainnet/testnet data, not just synthetic test vectors. Unit tests verify intent; chain data verifies correctness. The sigma-rust `gen_indexes` panic was only found by syncing real headers — all unit tests passed.

**For P2P code**: capture traffic from a live JVM node, replay it through the Rust implementation, compare byte-for-byte. Any divergence is a bug until proven otherwise.

**Debugging protocol failures**: When communication stalls or a peer disconnects, never focus on the failing message in isolation. Treat the failure as a culmination of past events — the bug is usually in an earlier message that introduced a silent offset or state corruption. Work backwards from the stall to the first message that parsed differently than expected. A 4-byte checksum read on a zero-length body shifted every subsequent message and took a full day to find because the symptoms appeared 50 messages later.

## Common Mistakes

| Mistake | Fix |
|---------|-----|
| Trusting VLQ-encoded lengths without bounds checking | Cap all allocations at sane maximums (256KB for P2P messages) |
| Implementing fixed-width integer parsing for Scorex fields | Everything is VLQ. Check the pcap. |
| Porting JVM code line-by-line | Understand the intent, implement idiomatically in Rust. The type system catches different bugs. |
| Adding `unwrap()` in network-facing code | Use `?` or explicit error handling. A panic in the P2P layer is a remote crash vulnerability. |
| Conflating "compiles" with "works" | `cargo test` after every change. The Rust type system is good but not omniscient. |
| Writing integration tests that need a live network | Mock the P2P layer for unit tests. Integration tests use recorded pcap fixtures. |
| Crossing component boundaries without a trait | If component A imports component B's internal types, the boundary is violated. Use traits. |
| Ignoring the JVM reference for edge cases | The JVM node IS the consensus. Match its behavior exactly, including bugs, until there's a spec that says otherwise. |

## Ergo-Specific Knowledge

### Autolykos v2 PoW
Memory-hard, ASIC-resistant. The miner doesn't run in the node — external GPU miners poll for work via REST API and submit solutions. The node only verifies solutions.

### Sigma Protocols
Ergo's script language is based on sigma protocols (zero-knowledge proofs). The interpreter is in `ergotree-interpreter`. Don't reimplement — this is consensus-critical crypto.

### Storage Rent
Boxes (UTXOs) that aren't spent pay rent after 4 years. This is part of transaction validation — `ergo-lib` handles it, but the node must provide the correct context (current height, box age).

### NiPoPoWs
Non-Interactive Proofs of Proof-of-Work. Enable light client bootstrap without downloading the full chain. `ergo-nipopow` crate exists. Relevant for chain sync modes.

### Extension Sections
Block extensions carry: protocol parameters (voting results), interlink vectors (NiPoPoW), key-value pairs. Must be parsed and validated as part of full block validation.
