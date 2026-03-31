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

## Submodule Separation (RULE)

When the project grows to multiple repos, the same rule as BlockHost applies: **You CANNOT modify files in submodule directories.** Instead, provide the user with a complete prompt to send to that submodule's Claude session. Format the prompt clearly so the user can copy-paste it directly. Prompts go in `prompts/` as markdown files.

## Skill Ownership (PERSISTENT RULE)

**You own the `ergo-node-development` skill.** Maintain it when new patterns emerge. When the user gives instructions that conflict with the skill, call it out — don't silently override. The skill is the accumulated wisdom of the project; it should be updated, not bypassed.

## Goal

Replace the JVM reference node with a Rust implementation that is memory-safe, efficient, and IPv6-native. The P2P layer (`ergo-proxy-node`) is complete and running on testnet. This project builds everything above it: header validation, block validation, UTXO state management, mempool, chain sync, and storage.

## Architecture

Multi-session development following the BlockHost pattern:
- **Main session**: interface contracts, orchestration, integration
- **Submodule sessions**: one per component, each with its own contract boundary

### Components

| Component | Status | Session |
|---|---|---|
| P2P networking | **Done** | `ergo-proxy-node` (separate repo) |
| Header chain validation | To build | Submodule |
| Block validation | To build | Submodule |
| UTXO state management | To build | Submodule |
| Mempool | To build | Submodule |
| Chain sync state machine | To build | Submodule |
| Block/modifier storage | To build | Submodule |
| REST API | To build | Submodule |

## Design Principles

- **Design by Contract**: every component boundary has explicit preconditions, postconditions, and invariants. Contracts are documented in `contracts/` and enforced via `debug_assert!`.
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
| AVL+ authenticated tree | `ergo_avltree_rust` | 0.1.1 | Dec 2024 | Prover + verifier, batch operations, versioned storage trait |
| Merkle proofs | `ergo-merkle-tree` | 0.15.0 | Feb 2026 | Tree, proof, batch multiproof |
| ErgoScript compiler | `ergoscript-compiler` | 0.24.0 | Feb 2026 | Source to ErgoTree |
| Scorex serialization | `sigma-ser` | — | Feb 2026 | VLQ, ZigZag, binary encoding |
| P2P networking | `ergo-proxy-node` | 0.1.0 | Mar 2026 | Handshake, framing, message routing, IPv6 |

### Must Build

| Component | Difficulty | Reference |
|---|---|---|
| Header chain validation | Medium | `ergo-core` sub-module in JVM repo |
| Difficulty adjustment (Autolykos2) | Medium | Epoch recalculation, documented algorithm |
| UTXO set management | Hard | Apply block → update AVL tree, rollback |
| AD proofs verification | Medium | `ergo_avltree_rust` exists, orchestration needed |
| Block validation (full) | Hard | Combine header + tx + AD proof + UTXO checks |
| Extension section handling | Easy | Parameters, interlinks, voting |
| Mempool | Medium | Tx ordering, eviction, double-spend detection |
| Block/modifier storage | Medium | Persistent backend (LevelDB, RocksDB, or similar) |
| Chain sync state machine | Hard | Full/digest/UTXO-snapshot modes |
| Emission schedule | Easy | Fixed rate period, epoch reduction, documented formula |
| Soft-fork voting | Easy | Parameter voting, rule activation |

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

### Phase 4: Transaction Validation
Validate transactions given input boxes. `ergo-lib::TransactionContext::validate()` already exists. Need input box lookup.

### Phase 5: UTXO State Management
AVL+ tree backed UTXO set. Apply blocks, rollback support. `ergo_avltree_rust` exists, needs persistence backend and orchestration layer.

### Phase 6: Full Node
Block storage, chain sync state machine, mempool, REST API.

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
