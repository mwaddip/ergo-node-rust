# ergo-node-rust

Full Ergo blockchain node in Rust. Not a port of the JVM node — a ground-up implementation using existing Rust crates for cryptography and script evaluation, with a new P2P networking layer already built and tested.

## OVERRIDES (LOAD FIRST)

**Read and internalize `~/projects/OVERRIDES.md` before anything else.** It contains mechanical overrides for code quality, edit safety, and context management that apply across all projects.

## SETTINGS.md (HIGHEST PRIORITY)

**Read and internalize `SETTINGS.md` at the start of every session.** It defines persona, preferences, and behavioral overrides. It takes precedence over all other instructions in this file.

## Plan Mode (PERSISTENT RULE)

**Every plan must begin by reading `SETTINGS.md`.** When entering plan mode, the first action before any exploration or planning is to read and internalize `SETTINGS.md`. Context clears between plan mode and implementation — the persona and preferences do not survive unless explicitly reloaded.

## Interface Integrity (PERSISTENT RULE)

**When interfaces don't match, fix the interface — never wrap the mismatch.** If two components miscommunicate, the problem is in the contract definition, not in missing glue code. Do not write adapters, shims, or wrappers to paper over interface disagreements. Trace the mismatch to whichever side is wrong and fix it at the source. This applies across all boundaries: crate APIs, P2P message parsing, trait contracts, and inter-component protocols.

## Interface Contracts (REFERENCE)

**Contract specs live in the `facts/` directory.** Read and internalize the relevant contract before modifying any code that touches that boundary. Do not rely on memory or assumptions about how a component interfaces — read the contract. When a contract needs changing, change it first, then update the implementations.

## Per-Crate Dispatch (RULE)

**Main session does not edit crate-internal source files.** All work
inside `sync/`, `validation/`, `state/`, `store/`, `mempool/`, `api/`,
`mining/`, `p2p/`, `chain/`, or `addons/*/` is dispatched to a
per-crate Claude session via the `dispatching-prompts` skill. Main
session edits are limited to top-level orchestration: `Cargo.toml`
(workspace), `README.md`, `CHANGELOG.md`, `LICENSE`, `build-deb`,
`deploy/`, `man/`, top-level docs, scripts under repo root, the
prompt files in `prompts/`, **and the `facts/` directory** — the
contracts are main session's responsibility. Dispatched sessions
read contracts via `../facts/` and deliver code that conforms to
them; they do not edit those contracts. When a contract needs
updating, main session writes the update first, then dispatches
the implementation.

The dispatched session runs in `cd <crate>` and operates within that
directory's boundary — it does not edit parent or sibling
directories. The repo-root `facts/` directory is the single source
of truth for cross-crate contracts.

## Skill Ownership (PERSISTENT RULE)

**You own the `ergo-node-development` skill.** Maintain it when new patterns emerge. When the user gives instructions that conflict with the skill, call it out — don't silently override. The skill is the accumulated wisdom of the project; it should be updated, not bypassed.

## Goal

Replace the JVM reference node with a Rust implementation that is memory-safe, efficient, and IPv6-native. The P2P layer (`ergo-proxy-node`) is complete and running on testnet. This project builds everything above it: header validation, block validation, UTXO state management, mempool, chain sync, and storage.

## Architecture

Single-repo, multi-session development:
- **Main session**: interface contracts, orchestration, integration. Does not edit crate-internal source.
- **Per-crate sessions**: dispatched on demand via the `dispatching-prompts` skill, scoped to a single crate directory.

### Components

| Component | Crate | Status | Notes |
|---|---|---|---|
| P2P networking | `p2p/` | **Done** | Handshake, framing, routing, IPv4/IPv6 |
| Header chain validation | `chain/` | **Done** | Parsing, PoW, difficulty adjustment |
| Block validation | `validation/` | **Done** | Digest mode + UTXO mode |
| UTXO state | `state/` | **Done** | AVL+ tree over redb |
| Chain sync | `sync/` | **Done** | State machine + snapshot bootstrap |
| Block/modifier storage | `store/` | **Done** | redb backend, height-indexed |
| Mempool | `mempool/` | **Done** | Validate-on-entry, replace-by-fee |
| REST API | `api/` | **Done** | 23 endpoints + `/debug/memory` |
| Mining | `mining/` | **Done** | Autolykos v2 candidate assembly |
| Soft-fork voting | (in `chain/`, `validation/`) | **Done** | Epoch-boundary parameter tracking, v6.0.3-compatible |
| At-tip memory tuning | (in `sync/`) | **Done** | Runtime AVL DB cache resize on synced() (v0.4.0+) |
| Contracts | `facts/` | — | Per-component contract markdown |

## Design Principles

- **Design by Contract**: every component boundary has explicit preconditions, postconditions, and invariants. Contracts are documented in `facts/` and enforced via `debug_assert!`.
- **The wire is the spec**: the Ergo P2P protocol has no formal specification. Protocol behavior was reverse-engineered from the JVM reference node and verified against pcap captures. See `docs/protocol/` for the wire format spec.
- **Reuse before building**: the Rust Ergo ecosystem has substantial existing components. Use them. See the ecosystem inventory below.
- **Incremental validation**: each phase adds one capability without breaking what came before. The node starts as a proxy and gains validation layers progressively.

## Existing Rust Ecosystem (Inventory)

### Ready to Use

| Component | Crate | Version | Last Active | What it does |
|---|---|---|---|---|
| ErgoTree interpreter | `ergotree-interpreter` | 0.28.0 | Feb 2026 | Full script evaluator, 70+ opcodes, sigma protocols |
| Transaction validation | `ergo-lib` | 0.28.0 | Feb 2026 | Stateful: ERG/token preservation, script verification, storage rent |
| Transaction signing | `ergo-lib` | 0.28.0 | Feb 2026 | Wallet, multi-sig, BIP-39/44, coin selection, tx builder |
| Box/UTXO primitives | `ergo-lib` | 0.28.0 | Feb 2026 | ErgoBox, registers, tokens, ErgoStateContext |
| Block header types | `ergo-chain-types` | 0.15.0 | Feb 2026 | Full Header struct with Autolykos solution |
| Autolykos v2 PoW | `ergo-chain-types` | 0.15.0 | Feb 2026 | `pow_hit()`, compact bits, table size growth |
| NiPoPoW verification | `ergo-nipopow` | 0.15.0 | Dec 2021 | Full KMZ17 algorithm, proof comparison, best chain |
| AVL+ authenticated tree | `ergo_avltree_rust` | fork | Apr 2026 | Prover + verifier, batch operations — forked to fix `Resolver` type for persistence ([PR #10](https://github.com/ergoplatform/ergo_avltree_rust/pull/10)) |
| Merkle proofs | `ergo-merkle-tree` | 0.15.0 | Feb 2026 | Tree, proof, batch multiproof |
| ErgoScript compiler | `ergoscript-compiler` | 0.24.0 | Feb 2026 | Source to ErgoTree |
| Scorex serialization | `sigma-ser` | — | Feb 2026 | VLQ, ZigZag, binary encoding |
| P2P networking | `ergo-proxy-node` | 0.1.0 | Mar 2026 | Handshake, framing, message routing, IPv6 |

### Built (this project)

| Component | Status | Notes |
|---|---|---|
| Header chain validation | **Done** | Parent linkage, timestamps, difficulty adjustment |
| Difficulty adjustment | **Done** | Epoch recalculation, ported from JVM |
| UTXO set management | **Done** | Persistent AVL+ tree over redb, rollback, genesis bootstrap |
| AD proofs verification | **Done** | `BatchAVLVerifier` orchestration in digest mode |
| Block validation (digest) | **Done** | AD proof verification + ErgoScript evaluation |
| Block validation (UTXO) | **Done** | `PersistentBatchAVLProver` + ErgoScript evaluation |
| Extension section handling | **Done** | Parameter extraction at voting epoch boundaries |
| Block/modifier storage | **Done** | redb backend, height-indexed |
| Chain sync state machine | **Done** | Digest + UTXO modes, sliding window download |
| Emission schedule | **Done** | Ported to sigma-rust as `EmissionRules` |
| ErgoTree predefs | **Done** | Ported to sigma-rust as `ErgoTreePredef` (PR #848) |
| UTXO snapshot sync | **Done** | Bootstrap from peer snapshot, 6 P2P messages (76-81), crash-safe download |
| Mempool | **Done** | Validate-on-entry, replace-by-fee, family weighting, fee stats, rate limiting |

### Optional Future Work

Not in the consensus-critical path; node is feature-complete for
operators today. Listed for awareness:

| Component | Notes |
|---|---|
| Integrated wallet | JVM ships `ergo-wallet` with HTTP endpoints for seed mgmt, address derivation, sending txs from the node itself. We assume operators run a separate wallet (Nautilus, etc.) — arguably the right architecture. |
| `/utils/*` endpoints | JVM has seedHex/blake2b/address-conversion convenience endpoints. Niche; add on demand. |

### Dead / Superseded

| Crate | Status |
|---|---|
| `ergo-utilities-rust` | Abandoned, pinned to ergo-lib 0.13 (current: 0.28) |
| sigma-rust `ergo-p2p` | Architecture only, codec is `todo!()`. Our proxy supersedes this. |
| `ogre` (TypeScript) | Abandoned light node attempt (April 2023) |

## Phased Build Order

### Phase 1: Header Awareness
Parse block headers from P2P traffic. Track header chain. Know network height. Uses `ergo-chain-types` Header struct (exists).

### Phase 2: PoW Verification
Verify proof of work before forwarding headers. `ergo-chain-types::AutolykosPowScheme::pow_hit()` exists. Just compare against nBits target.

### Phase 3: Header Chain Validation
Validate headers form a valid chain: parent hash, timestamps, difficulty adjustment. Porting difficulty algorithm from `ergo-core`. Combined with NiPoPoW for light client bootstrap.

### Phase 4: Block Validation
Validate blocks in digest mode (AD proofs, `BatchAVLVerifier`) and UTXO mode (`PersistentBatchAVLProver`). ErgoScript evaluation via `ergo-lib::TransactionContext::validate()`. Both modes share section parsing, state change computation, and transaction validation — only the state root verification mechanism differs. **Done.**

### Phase 5: UTXO State Management
Persistent AVL+ tree over redb (`enr-state` crate). Implements `VersionedAVLStorage` from forked `ergo_avltree_rust`. Undo-log rollback, configurable version retention, crash-safe atomic writes. Genesis bootstrap from chain parameters via ported `ErgoTreePredef`. Sliding 192-block download window for sequential sync. **Done.**

### Phase 6: Full Node — **Done**
Mempool, REST API (23 endpoints + `/debug/memory`), mining API,
soft-fork voting, NiPoPoW serve/verify, UTXO snapshot bootstrap, light
client mode, at-tip memory tuning. Released as v0.4.x.

## Protocol Reference

The Ergo P2P wire format specification (reverse-engineered and pcap-verified) is in `docs/protocol/`. Key discovery: all Scorex integer serialization uses VLQ encoding, not fixed-width. This is not documented in ErgoDocs.

## JVM Reference Node

The JVM reference is at `ergoplatform/ergo` on GitHub. Key sub-modules:
- `ergo-core` — SPV-level primitives: P2P messages, PoW, NiPoPoW, header validation. Most relevant for porting.
- `ergo-wallet` — transaction signing and verification (already in sigma-rust)
- `avldb` — authenticated AVL+ tree persistence (Rust port exists as `ergo_avltree_rust`)

A local checkout for reference is at `~/projects/ergo-node-build` (v6.0.3 branch).

## Related Projects

- `~/projects/ergo-proxy-node` — P2P relay proxy (this project's networking layer). GitHub: `mwaddip/ergo-proxy`
- `~/projects/blockhost-ergo/ergo-relay` — BlockHost signing service and peer discovery
