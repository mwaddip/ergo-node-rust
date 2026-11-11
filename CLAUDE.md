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

## Interface Contracts (RULE)

**Contract specs live in the `facts/` directory, and a contract always changes before the code.** Read the relevant contract before modifying any code that touches that boundary; do not rely on memory or assumptions about how a component interfaces. When a contract needs changing, update `facts/` and commit it first, then implement against it.

## One Session, the Whole Repo (RULE)

**The node is feature complete and gets bugfixes only, so one session works across the whole repo:** every crate's source and tests, `facts/`, `addons/`, and the top level. Nothing inside this repo is dispatched to per-crate sessions. The crate scopes under **Crates** below still decide where a change belongs: a fix lands in the crate that owns the behavior.

**Other projects keep their own sessions.** The sigma-rust fork and SANTA are separate repos with their own sessions; never edit them from here. Requests for them go in `prompts/` (gitignored) as markdown: the user routes them, or they are sent to that project's kitty window.

**A sigma-rust pin bump touches every manifest.** The rev is pinned in seven workspace `Cargo.toml`s and both addons. Grep the old rev to zero, re-lock the workspace and each addon (`cargo metadata --manifest-path …`), and check that the lock diff swaps only sigma-rust sources. Cargo can't pin an unpushed rev.

## S.P.E.C.I.A.L.

**`SPECIAL.md` at the repo root sets per-component attention weights.** Apply the profile of the crate you are editing.

## Skill Ownership (PERSISTENT RULE)

**You own the `ergo-node-development` skill.** Maintain it when new patterns emerge. When the user gives instructions that conflict with the skill, call it out — don't silently override. The skill is the accumulated wisdom of the project; it should be updated, not bypassed.

## Goal

Replace the JVM reference node with a Rust implementation that is memory-safe, efficient, and IPv6-native. The P2P layer lives in this repo as the `p2p/` crate — handshake, framing, routing, IPv4/IPv6 — and everything above it is built here too: header validation, block validation, UTXO state management, mempool, chain sync, and storage.

## Architecture

One Cargo workspace. Each crate owns one component; **Crates** below says what each owns, what it doesn't, and where its contract is. The main crate (`src/`) wires them together.

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
| REST API | `api/` | **Done** | 43 endpoints — 38 JVM-compatible + `/debug/memory`, 3 × `/debug/p2p-capture/*`, `/stats/p2p` |
| Mining | `mining/` | **Done** | Autolykos v2 candidate assembly: mempool selection bounded by cost and serialized section size, fee collection capped by measured box size, candidate regenerated on TTL expiry. Verified at runtime against mainnet tip |
| Soft-fork voting | (in `chain/`, `validation/`) | **Done** | Epoch-boundary parameter tracking, v6.0.3-compatible |
| At-tip memory tuning | (in `sync/`) | **Done** | Runtime AVL DB cache resize on synced() (v0.4.0+) |
| Contracts | `facts/` | — | Per-component contract markdown |

## Crates

### `p2p/` (enr-p2p)
The network boundary: TCP (IPv4 and IPv6), handshake, framing, message parsing, routing, peer management (connect, disconnect, bounded write queues, penalties, keepalive, the PeerDb), broadcast. **Every byte from a peer is untrusted.** Contracts: `facts/p2p-transport.md`, `p2p-protocol.md`, `p2p-routing.md`, `p2p-node.md`, `p2p-peerdb.md`, `p2p-capture.md`.
- **The node is a peer, not a proxy.** The router answers the peer that sent a message, from what the node has, or does nothing; it never passes a message from one peer to another, and every `Action::Send` it emits targets that sender (a `debug_assert!` enforces it).
- **Three layers.** Transport encodes and decodes frames and runs the handshake; it never interprets content. Protocol parses messages into typed variants and runs the peer state machine. Router decides what to do: it answers `GetPeers` from the PeerDb and `ModifierRequest`s through the local-serve hook, records gossiped `Peers`, emits each received modifier as `Action::Validate`, and does nothing with `Inv`, `SyncInfo` or unknown codes. A light listener's peers get nothing for block-related requests (101, 102, 104, 108).
- **The main crate owns the edges.** It injects the local-serve hook and owns the modifier sink's receiver; every other event reaches its consumer (sync, the mempool task, the snapshot and NiPoPoW handlers) through the event subscriber.
- **Wire facts.** VLQ everywhere: Scorex method names like `putUShort`/`putUInt` mislead. The exception is the frame header's body length, 4-byte big-endian. Magic: mainnet `[01,00,02,04]`, testnet `[02,03,02,03]`. Modifier types: 101 Header, 2 Transaction, 102 BlockTransactions, 104 ADProofs, 108 Extension. Full spec: `docs/protocol/ergo-p2p-wire-format.md`.
- Doesn't own: validation, storage, chain or UTXO state, what to sync or when, what the node has to serve.

### `chain/` (enr-chain)
The single authority on whether a chain of headers is valid: header parsing, best-height tracking, Autolykos v2 PoW, epoch difficulty adjustment (ported from `ergo-core`), parent linkage and timestamps, NiPoPoW verification. It also states the tree-version rules once (`block_transactions_read_version`, `block_transactions_write_version`, `activated_tree_version`). Contract: `facts/chain.md`. Doesn't own block bodies, persistence, request timing or network I/O. Depends on `ergo-chain-types`, `ergo-nipopow`, `sigma-ser`.

### `validation/` (ergo-validation)
The final arbiter on block validity, and consensus-critical glue: header checks from `chain/`, transaction validation through `ergo-lib`, AD proofs, UTXO lookups from `state/`. Digest and UTXO modes share section parsing, state-change computation and transaction validation; only state-root verification differs (`BatchAVLVerifier` against `PersistentBatchAVLProver`). It also extracts parameters at voting-epoch boundaries, checks the block rules, and owns `reset_to`. Contract: `facts/validation.md`. Doesn't own header chain logic, AVL persistence, the mempool, or script-evaluation internals (sigma-rust).

### `state/` (enr-state)
The UTXO state: `RedbAVLStorage`, the `VersionedAVLStorage` impl for the forked `ergo_avltree_rust`. It persists tree nodes by digest label, keeps a version chain for rollback within `keep_versions`, records the block height, bootstraps genesis, and rolls back a half-applied update on the next open. Contract: `facts/state.md`.
- **Durability.** `update()` writes with `Durability::None`; the caller flushes periodically and on shutdown. Every write transaction sets `set_quick_repair(true)`: without it, opening after an unclean shutdown scans the whole file (~2 min on mainnet).
- JVM parity is required on rollback semantics and version metadata (`avldb`).

### `store/` (enr-store)
Persistence for headers and block sections (101, 102, 104, 108), fork-aware header tables (PRIMARY, HEADER_FORKS, HEADER_SCORES, BEST_CHAIN), cumulative scores, chain metadata, and one-shot schema migrations on open. Contract: `facts/store.md`.
- **No dependency on any Ergo domain crate**, and that isolation is load-bearing: the store stays a byte-keyed KV layer that never tracks sigma-rust upgrades. Work that needs domain types (e.g. computing scores from `n_bits`) runs in the main crate over primitives the store exposes.
- Writes use `Durability::None` plus a caller `flush()`; `put_batch` is atomic across tables.
- No JVM analog: the schema follows Ergo's access patterns, not the JVM's LevelDB layout.

### `sync/` (ergo-sync)
The sync state machine across full, digest and UTXO-snapshot modes: request and download coordination (the sliding window, delivery tracking), reorgs, restart and resume (watermarks, gap recovery, sweep and backoff), at-tip behavior (the AVL cache resize on `synced()`). It drives the router; it doesn't implement it. It must survive partitions, reorgs and restarts at any point. Contract: `facts/sync.md`.

### `mempool/` (ergo-mempool)
The transaction pool, in memory by design: a crash means an empty pool. Validate on entry, ordering, eviction, conflict detection, replace-by-fee, family weighting, fee statistics, rate limiting. The validation itself is `ergo-lib`, through `validation/`. Contract: `facts/mempool.md`.

### `mining/` (ergo-mining)
The logic behind the mining endpoints: candidate assembly (selection bounded by cost and serialized size, the transactions root), emission and fee transactions, `WorkMessage` shaping, PoW solution validation, voting fields. Contract: `facts/mining.md`.
- **A real GPU miner consumes `WorkMessage`**, so field types and omitted fields matter: `b` is a bare number, `proof`/`h` are left out when absent, and the candidate is served by validated height.
- **`WorkMessage.b` comes only from `enr_chain::pow_target(n_bits)`.** Never re-derive it from `decode_compact_bits`, which returns the difficulty; that is how v0.8.0 shipped a serve path no miner could satisfy.

### `api/` (ergo-api)
The REST API: axum handlers, request validation, response shaping, and the `ApiError` envelope (`reason` + `detail`). It reads the other crates through shared state and their public APIs and owns no business logic. Contract: `facts/openapi.yaml` (every endpoint, schema and error reason) and `facts/api.md` (conventions and the compatibility tags `full`, `deviation`, `rust-only`).
- **`api/clippy.toml` bans `axum::Json`.** Handlers use the crate's `ApiJson`, `ApiQuery`, `ApiPath`, `ApiBytes` and `response::Json`, so every error keeps the envelope.

### The main crate (`src/`) and `addons/`
`src/` wires the components together: startup, layered configuration (`conf.d/`), component lifecycle, the validation pipeline, and the local-serve hook. `addons/fastsync` and `addons/indexer` are separate packages, `exclude`d from the workspace with their own lockfiles. They pin the same sigma-rust rev as the node; when one changes, its version and lockfile change with it.

## Design Principles

- **Design by Contract**: every component boundary has explicit preconditions, postconditions, and invariants. Contracts are documented in `facts/` and enforced via `debug_assert!`.
- **The wire is the spec**: the Ergo P2P protocol has no formal specification. Protocol behavior was reverse-engineered from the JVM reference node and verified against pcap captures. See `docs/protocol/` for the wire format spec.
- **Reuse before building**: the Rust Ergo ecosystem has substantial existing components. Use them. See the ecosystem inventory below.
- **Incremental validation**: each phase adds one capability without breaking what came before. Early builds forwarded traffic without understanding it and gained validation layers progressively; the `[proxy]` config section is a leftover of that era, not a statement about what the node does now.

## Existing Rust Ecosystem (Inventory)

### Ready to Use

*Dates are the upstream repo's last push, checked 2026-08-13 via the GitHub API
— not the crates.io release date, which is much older: `ergo-lib-v0.28.0` was
published 2024-08-09. Every sigma-rust crate shares one repo and therefore one
date. We consume them from `mwaddip/sigma-rust` pinned by rev, not from
crates.io, so the version column is the last published release rather than what
the build actually resolves.*


| Component | Crate | Version | Upstream last push | What it does |
|---|---|---|---|---|
| ErgoTree interpreter | `ergotree-interpreter` | 0.28.0 | 2026-08-04 | Full script evaluator, 70+ opcodes, sigma protocols |
| Transaction validation | `ergo-lib` | 0.28.0 | 2026-08-04 | Stateful: ERG/token preservation, script verification, storage rent |
| Transaction signing | `ergo-lib` | 0.28.0 | 2026-08-04 | Wallet, multi-sig, BIP-39/44, coin selection, tx builder |
| Box/UTXO primitives | `ergo-lib` | 0.28.0 | 2026-08-04 | ErgoBox, registers, tokens, ErgoStateContext |
| Block header types | `ergo-chain-types` | 0.15.0 | 2026-08-04 | Full Header struct with Autolykos solution |
| Autolykos v2 PoW | `ergo-chain-types` | 0.15.0 | 2026-08-04 | `pow_hit()`, compact bits, table size growth |
| NiPoPoW verification | `ergo-nipopow` | 0.15.0 | 2026-08-04 | Full KMZ17 algorithm, proof comparison, best chain |
| AVL+ authenticated tree | `ergo_avltree_rust` | fork, pinned by rev | 2026-08-03 | Prover + verifier, batch operations. Fork carries persistent-prover support ([PR #27](https://github.com/ergoplatform/ergo_avltree_rust/pull/27)); `Resolver` is [#16](https://github.com/ergoplatform/ergo_avltree_rust/pull/16), superseding the closed #10 |
| Merkle proofs | `ergo-merkle-tree` | 0.15.0 | 2026-08-04 | Tree, proof, batch multiproof |
| ErgoScript compiler | `ergoscript-compiler` | 0.24.0 | 2026-08-04 | Source to ErgoTree. **Available, not a dependency** — the node never compiles source |
| Scorex serialization | `sigma-ser` | 0.19.0 | 2026-08-04 | VLQ, ZigZag, binary encoding |

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
| sigma-rust `ergo-p2p` | Architecture only, codec is `todo!()`. This repo's `p2p/` crate supersedes it. |
| P2P **relay proxy** (the original `ergo-proxy-node` concept) | Abandoned as impractical. Several nodes sharing one proxy share one peer identity, so a single misbehaving node behind it gets **the proxy banned for everybody** — the well-behaved nodes lose their peers for someone else's conduct, with no way to attribute or isolate the offender. The networking code was worth keeping and is now `p2p/`; the relay in front of it was not. |
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
Mempool, REST API (43 endpoints, 38 of them JVM-compatible), mining API,
soft-fork voting, NiPoPoW serve/verify, UTXO snapshot bootstrap, light
client mode, at-tip memory tuning. First released as v0.4.x; current release
is **v0.8.0**, which removed deferred script evaluation and split
`BlockValidator` into three traits (see `CHANGELOG.md`).

## Protocol Reference

The Ergo P2P wire format specification (reverse-engineered and pcap-verified) is in `docs/protocol/`. Key discovery: all Scorex integer serialization uses VLQ encoding, not fixed-width. This is not documented in ErgoDocs.

## JVM Reference Node

The JVM reference is at `ergoplatform/ergo` on GitHub. Key sub-modules:
- `ergo-core` — SPV-level primitives: P2P messages, PoW, NiPoPoW, header validation. Most relevant for porting.
- `ergo-wallet` — transaction signing and verification (already in sigma-rust)
- `avldb` — authenticated AVL+ tree persistence (Rust port exists as `ergo_avltree_rust`)

A local checkout for reference is at `~/projects/ergo-node-build` (v6.0.3 branch).

**When behavior is ambiguous, the JVM source is correct. When it disagrees with observed traffic or network behavior, the traffic wins.** For the mining serve format, a real miner's parsing wins: it is the consumer.

Where each component's behavior lives in the JVM source (`ergo-core/src/main/scala/org/ergoplatform/` unless noted):
- **P2P:** `network/` and `network/peer/`; connection handling and the penalty system in `src/main/scala/scorex/core/network/`
- **Headers, PoW, difficulty:** `modifiers/history/header/`, `mining/`, `nodeView/history/`
- **Blocks and sections:** `modifiers/`; transaction validation is `ergo-wallet` (already in `ergo-lib`)
- **Sync:** `nodeView/` (`NodeViewSynchronizer`, modifier download and apply)
- **Mempool:** `nodeView/mempool/` (`ErgoMemPool`, `OrderedTxPool`)
- **Mining:** `mining/` (`CandidateGenerator`, `AutolykosPowScheme`), plus `ErgoMiner` and the `/mining/*` routes
- **UTXO state:** the `avldb` module

## Related Projects

- `~/projects/ergo-proxy-node` — where the P2P layer was originally written, now vendored here as `p2p/`. GitHub: `mwaddip/ergo-proxy`. The **relay** idea it was named for is abandoned; see "Dead / Superseded".
- `~/projects/blockhost-ergo/ergo-relay` — BlockHost signing service and peer discovery
