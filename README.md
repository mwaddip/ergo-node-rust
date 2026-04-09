# ergo-node-rust

A ground-up Ergo blockchain full node in Rust. Not a port of the JVM reference node — a new implementation that reuses existing Rust crates for cryptography and script evaluation, with a new P2P networking layer already built and tested against testnet.

## Status

**Highly experimental.** Syncing headers, downloading block sections, and validating state transitions on testnet. Supports two validation modes: **digest mode** (AD proof verification, no UTXO set) and **UTXO mode** (persistent AVL+ tree, full state management). Headers are validated (PoW, difficulty, parent linkage) and persisted via redb. Survives restarts without re-syncing or re-validating.

**Running continuously on testnet** since April 2026. Synced from genesis to tip (271k+ blocks) in UTXO mode — building the UTXO set block by block with state roots verified against every header. Also validated 267k+ blocks in digest mode via AD proofs. **Validated by JVM**: a JVM reference node configured to peer exclusively with this Rust node has independently re-validated every block we serve, all the way to tip. The Rust node serves as a fully canonical chain source.

REST API and mining API are live. The mining endpoints generate Autolykos v2 candidates from real chain state and accept solutions from external GPU miners.

## Roadmap

| Phase | What | Status |
|---|---|---|
| 1 | **Header awareness** — parse headers, track network height | Done |
| 2 | **PoW verification** — verify Autolykos v2 before forwarding | Done |
| 3 | **Header chain validation** — parent hashes, timestamps, difficulty adjustment | Done |
| 3b | **Header chain sync** — SyncInfo exchange, async validation pipeline, peer rotation | Done |
| 3c | **Persistent storage** — headers and block sections survive restarts (redb) | Done |
| 3d | **Block section download** — request BlockTransactions + Extensions (UTXO mode) | Done |
| 3e | **Block assembly** — track `full_block_height` watermark for complete blocks | Done |
| 3f | **Honest Mode advertisement** — handshake advertises actual capabilities (`blocks_to_keep`) | Done |
| 3g | **Deep chain reorg** — fork-aware header storage, cumulative difficulty scoring, multi-block reorg | Done |
| 4a | **Digest-mode validation** — verify state transitions via AD proofs (BatchAVLVerifier) | Done |
| 4b | **Transaction validation** — ErgoScript evaluation via `ergo-lib` | Done |
| 5 | **UTXO state management** — persistent AVL+ tree, apply/rollback blocks, genesis bootstrap | Done |
| 5b | **UTXO snapshot sync** — bootstrap from peer snapshots, serve snapshots to peers | Done |
| 6a | **Mempool** — validate-on-entry, replace-by-fee, family weighting, rate limiting | Done |
| 6b | **REST API** — 19 endpoints (info, blocks, transactions, UTXO, peers, emission, mining) | Done |
| 6c | **Mining API** — Autolykos v2 candidate assembly, EIP-27 re-emission (network-aware), solution validation, wiring test against sigma-rust's JVM-pinned serializer | Done |
| 7a | **Soft-fork voting** — parameter tracking across epoch boundaries, JVM `matchParameters60` semantics | Done |
| 7b | **NiPoPoW serve + verify** — codes 90/91, build proofs from local chain, verify incoming proofs | Done |
| 7c | **NiPoPoW light-client sync** — `StateType::Light`, single-peer bootstrap state machine, install verified suffix as chain origin, transition to tip-following sync | Done |

## What works today

- Connects to Ergo testnet peers and maintains persistent connections
- Accepts inbound connections from other nodes
- Routes all P2P messages between peers (Inv, ModifierRequest/Response, SyncInfo, GetPeers)
- **Header chain sync**: bidirectional SyncInfo exchange, Inv/ModifierRequest header download
- **Delivery tracker**: 10-second timeout retry, re-request from different peer, LRU modifier buffer (8192 headers) with eviction-triggered re-request — matches JVM's `DeliveryTracker`
- **Async validation pipeline**: batch-drain processing, sort-by-height, PoW + chain validation
- **Full chain validation**: parent linkage, timestamp bounds, difficulty adjustment, PoW
- **Event-driven sync**: progress-triggered and timer-based SyncInfo cycles, peer rotation on stall
- **Persistent storage**: headers written to redb after validation, restored on startup — no re-sync after restart
- **Block section download**: mode-aware — UTXO mode downloads BlockTransactions + Extension, digest mode scaffolding downloads ADProofs too
- **Block assembly tracking**: `downloaded_height` watermark advances as sections arrive, identifies blocks ready for validation
- **Digest-mode block validation**: verifies state transitions using AD proofs — each block's transactions are converted to AVL+ tree operations, and `BatchAVLVerifier` confirms the state root transition matches the header. No UTXO set needed.
- **Transaction validation**: above a configurable checkpoint height, every transaction's spending proofs (sigma protocols) are verified via ergo-lib's `TransactionContext::validate()`. Input boxes extracted from AD proof output (digest mode) or AVL+ tree lookups (UTXO mode), parameters tracked from Extension sections at voting epoch boundaries.
- **UTXO state management**: persistent AVL+ tree over redb via `PersistentBatchAVLProver`. Genesis state bootstrapped from chain parameters using ported `ErgoTreePredef` (emission contract, foundation script built from IR — no hardcoded hex). Block state changes applied to the tree, state root verified against headers, with configurable rollback depth (200 blocks). Crash-safe — atomic redb transactions for all state updates.
- **Sliding window sync**: sequential 192-block download window (matching JVM's `FullBlocksToDownloadAhead`), recomputed each cycle from current state. Delivery tracker with type-aware timeout retries.
- **Mempool**: in-memory transaction pool with full JVM parity — validate-on-entry, replace-by-fee, family weighting, fee statistics, rate limiting, periodic revalidation. Confirmed transactions purged after each validated block. P2P transaction relay (receive → validate → broadcast → rebroadcast).
- **REST API**: 19 endpoints in `api/` crate (axum), JVM path-compatible. Covers `/info`, `/blocks/*`, `/transactions/*`, `/utxo/*`, `/peers/*`, `/emission/*`, `/mining/*`.
- **Mining API**: full Autolykos v2 candidate assembly. Pre-computes emission tx + state proofs after each validated block (no validator lock contention with sync). EIP-27 re-emission token handling, network-aware (`ReemissionRules::mainnet()` / `::testnet()` dispatched by config). CPU-mined integration tests prove the loop end-to-end. Solution endpoint validates PoW, computes header ID, and submits the assembled block to the local pipeline. **Cross-verification gate**: `mining/tests/work_message_wiring.rs` catches any field-mapping bug in `build_work_message` by independently constructing the equivalent `Header` and asserting byte-equal serialization against sigma-rust, which is itself JVM-pinned upstream against the canonical `548c3e60...` hex from `AutolykosPowSchemeSpec.scala`.
- **Soft-fork voting**: epoch-boundary parameter tracking with JVM v6 `matchParameters60` semantics. Network-aware default parameters (testnet starts at `BlockVersion=4`, mainnet has multi-epoch voting lifecycle). Validated against testnet past 100+ epoch boundaries with zero parameter mismatches.
- **NiPoPoW serve + verify**: P2P codes 90 (`GetNipopowProof`) and 91 (`NipopowProof`). Builds proofs from local chain on demand and verifies proofs received from peers. Genesis (height 1) interlinks synthesized in-process; for h ≥ 2, the integrator clamps the build anchor to the current validated tip so the proof walk never runs off the validated edge. End-to-end verified via `tests/nipopow_serve_integration.rs` against the live testnet deployment.
- **NiPoPoW light-client mode**: configure `[node] state_type = "light"`. On startup with an empty chain, the bootstrap state machine asks one peer for `GetNipopowProof(m=6, k=10)`, verifies the response via `enr_chain::verify_nipopow_proof_bytes`, and installs the suffix as the chain's starting point via `HeaderChain::install_from_nipopow_proof`. From there, normal tip-following sync takes over — no validator, no block-body downloads, `light_client_mode` flag in `HeaderChain` skips `expected_difficulty` recalculation (standard SPV behavior). End-to-end smoke test against testnet: empty chain → installed at the network tip → tip-following past the install boundary in under 5 seconds wall time. Single-peer trust model for first release; multi-peer best-arg comparison (KMZ17 §4.3) is tracked as hardening. Fixing this loop required two upstream fixes: a chain-side defensive check in `ChainPopowReader::popow_header_at_height` that rejects extension bytes whose embedded `header_id` doesn't match the queried block (prevents silent corruption when the modifier store's backward-walk recovery returns stale data at BEST_CHAIN holes), and a sigma-rust `NipopowProof::has_valid_connections` rewrite that ports JVM's tolerant `useLastEpochs + 2` lookback window so the verifier accepts proofs with skipped intermediate prefix entries from continuous-mode difficulty headers and sparse-superlevel walks. The sigma-rust fix is on the `ergo-node-integration` branch as `1e3fe28` and is upstream-PR-ready on `mwaddip/sigma-rust:fix/nipopow-prefix-connection-lookback` against `ergoplatform/sigma-rust:develop`.
- **Honest Mode feature**: handshake advertises `state_type`, `verifying`, and `blocks_to_keep` from node config — peers don't request blocks we can't serve
- **Deep chain reorg**: fork-aware header storage keeps all validated headers across forks. Cumulative difficulty scoring selects the best chain. Multi-block reorganization is a local operation — zero network traffic, reads fork headers from the store and swaps the in-memory chain atomically. Handles testnet forks automatically.
- Continuous header sync from genesis on testnet — no connection stalls
- Wire format fully compatible: verified byte-identical serialization against JVM test vectors
- Tested: a JVM reference node syncs its full header chain exclusively through this relay

## Architecture

The node is composed of independent submodules, each owning a well-defined boundary:

| Directory | Repo | What it does |
|---|---|---|
| `p2p/` | [enr-p2p](https://github.com/mwaddip/enr-p2p) | P2P networking: handshake, message framing, routing, peer management |
| `chain/` | [enr-chain](https://github.com/mwaddip/enr-chain) | Header parsing, PoW verification, difficulty adjustment, chain validation |
| `sync/` | — | Chain sync state machine, section download, validation coordination |
| `validation/` | — | Block validation: digest mode (AD proofs) and UTXO mode (persistent tree), section serializers |
| `state/` | [enr-state](https://github.com/mwaddip/enr-state) | UTXO state management via AVL+ authenticated tree |
| `store/` | [enr-store](https://github.com/mwaddip/enr-store) | Persistent storage for headers, blocks, and modifiers |
| `mempool/` | — | In-memory transaction pool with replace-by-fee, family weighting, revalidation |
| `mining/` | — | Block candidate assembly: emission tx, fee tx, extension, header serialization, PoW validation |
| `api/` | — | REST API (axum) — 19 endpoints, JVM path-compatible |
| `facts/` | [ergo-node-facts](https://github.com/mwaddip/ergo-node-facts) | Interface contracts between components |

The main crate wires components together via traits — the P2P layer doesn't know what validation means, and the validation layer doesn't know about networking. Integration happens at the top.

Transaction validation and ErgoScript evaluation are handled by the existing [sigma-rust](https://github.com/ergoplatform/sigma-rust) ecosystem (`ergo-lib`, `ergotree-interpreter`). UTXO state is backed by [ergo_avltree_rust](https://github.com/ergoplatform/ergo_avltree_rust)'s authenticated AVL+ tree. Both are consumed via forks that carry upstream PRs not yet merged — see **Upstream dependencies and forks** below.

## Upstream dependencies and forks

The node leans on two upstream Rust crates for consensus-critical primitives. Both are consumed via forks ([`mwaddip/sigma-rust`](https://github.com/mwaddip/sigma-rust), [`mwaddip/ergo_avltree_rust`](https://github.com/mwaddip/ergo_avltree_rust)) that carry changes we've contributed back upstream as open PRs. Once those PRs merge and are released, the workspace switches back to crates.io.

### sigma-rust

The workspace `Cargo.toml` pins `ergo-chain-types`, `ergo-lib`, `ergo-nipopow`, and `sigma-ser` to a local `ergo-node-integration` branch that merges the following independent feature branches. Each PR is a standalone branch off `upstream/develop` — the integration branch exists only so the ergo-node-rust workspace can consume all of them at once while review is in flight.

- **[ergoplatform/sigma-rust#848](https://github.com/ergoplatform/sigma-rust/pull/848) — `ErgoTreePredef` port + genesis construction.** Ports the JVM's `ErgoTreePredef` helper and `EmissionRules` to sigma-rust so the Rust node can build genesis boxes from chain parameters (emission contract, foundation script) without hardcoding hex. Also folds in two smaller fixes along the way: `n_bits` type should be `u32`, not `u64` (JVM writes 4 BE bytes), and `pow_distance` should parse as an unsigned `BigUint::from_bytes_be` to match the JVM's `BigIntegers.fromUnsignedByteArray`.
- **[ergoplatform/sigma-rust#850](https://github.com/ergoplatform/sigma-rust/pull/850) — Soft-fork parameter variants.** Adds three missing `Parameter` enum variants (`SubblocksPerBlock = 9`, `SoftForkVotesCollected = 121`, `SoftForkStartingHeight = 122`) so the Rust node can track the full post-6.0 soft-fork voting state. The original enum carried a `// TODO: soft-fork parameter` comment acknowledging the gap; this PR closes it for the three `i32`-valued slots. `SoftForkDisablingRules = 124` is deliberately out of scope — its variable-length encoded value is incompatible with the existing `HashMap<Parameter, i32>` storage and is tracked separately on the chain crate's `active_disabling_rules: Vec<u8>` field.
- **[ergoplatform/sigma-rust#851](https://github.com/ergoplatform/sigma-rust/pull/851) — `NipopowAlgos::prove_with_reader`.** Ports JVM `NipopowProverWithDbAlgs.prove` to sigma-rust as a production primitive for NiPoPoW proof serving. The existing `NipopowAlgos::prove(&[PoPowHeader])` is a Rust port of the JVM's **in-memory test helper** (`NipopowAlgos.prove(Seq[PoPowHeader])`) and requires the caller to materialize the full chain as `PoPowHeader`s up front — `O(N)` cost per request, prohibitive for P2P serving on chains of any meaningful length (roughly five minutes of single-threaded work on a 100k-block testnet chain, hours on mainnet). `prove_with_reader` takes a new `PopowHeaderReader` trait as a callback and walks only the interlink hierarchy it actually needs — `O(m + k + m · log₂ N)` fetches, roughly three orders of magnitude fewer on a 270k-block chain. Additive, no breaking changes to the existing `prove`. This is the structural fix for the NiPoPoW build perf issue that was blocking first release.

### ergo_avltree_rust

The `enr-state` crate consumes `ergo_avltree_rust` via the fork pinned to rev `28862a1`.

- **[ergoplatform/ergo_avltree_rust#10](https://github.com/ergoplatform/ergo_avltree_rust/pull/10) — `Resolver` type.** Changes `Resolver` from a bare `fn(&Digest32) -> Node` function pointer to `Arc<dyn Fn(&Digest32) -> Node + Send + Sync>`. The original type makes real disk-backed storage impossible — a bare function pointer cannot capture a database handle, which is why the upstream crate's own test suite only ever exercises an in-memory mock. This PR is a prerequisite for the `enr-state` crate's `RedbAVLStorage` implementation, which provides a closure-based resolver that reads AVL+ nodes from redb on demand.

## Other Rust Ergo implementations

Independent Rust Ergo node efforts we're aware of:

- [arkadianet/ergo](https://github.com/arkadianet/ergo)

## Building

```bash
cargo build --release
```

Or build a Debian package:

```bash
./build-deb
```

## Contributing

Contributions are welcome — this is a large effort and help is appreciated. Testing is especially valuable: running the node against testnet, verifying wire format parsing, catching edge cases in protocol handling.

If you're interested in contributing to a specific component, check the interface contracts in `facts/` for the boundaries and expectations.

## License

Public domain. No rights reserved.
