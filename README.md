# ergo-node-rust

A ground-up Ergo blockchain full node in Rust. Not a port of the JVM reference node — a new implementation that reuses existing Rust crates for cryptography and script evaluation, with a new P2P networking layer already built and tested against testnet.

## Status

**Highly experimental.** Syncing headers, downloading block sections, and validating state transitions on testnet. Headers are validated (PoW, difficulty, parent linkage) and persisted via redb. Block sections are validated in digest mode — AD proofs verify that each block's transactions correctly transform the UTXO state root without maintaining the full UTXO set. Survives restarts without re-syncing or re-validating.

**Running continuously on testnet** since April 2026 — stays synced with the network, validates every block via AD proofs, handles chain reorganizations, and maintains persistent connections without getting banned by peers.

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
| 5 | UTXO state management — AVL+ tree backed, apply/rollback blocks | Next |
| 6 | Full node — chain sync state machine, mempool, REST API | Planned |

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
- **Transaction validation**: above a configurable checkpoint height, every transaction's spending proofs (sigma protocols) are verified via ergo-lib's `TransactionContext::validate()`. Input boxes extracted from AD proof output, parameters tracked from Extension sections at voting epoch boundaries. Validated 300+ consecutive testnet blocks with full ErgoScript evaluation — zero failures.
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
| `validation/` | — | Block validation: section parsing, state changes, AD proof verification |
| `state/` | [enr-state](https://github.com/mwaddip/enr-state) | UTXO state management via AVL+ authenticated tree |
| `store/` | [enr-store](https://github.com/mwaddip/enr-store) | Persistent storage for headers, blocks, and modifiers |
| `facts/` | [ergo-node-facts](https://github.com/mwaddip/ergo-node-facts) | Interface contracts between components |

The main crate wires components together via traits — the P2P layer doesn't know what validation means, and the validation layer doesn't know about networking. Integration happens at the top.

Transaction validation and ErgoScript evaluation are handled by the existing [sigma-rust](https://github.com/ergoplatform/sigma-rust) ecosystem (`ergo-lib`, `ergotree-interpreter`).

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
