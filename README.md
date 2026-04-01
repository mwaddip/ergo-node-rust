# ergo-node-rust

A ground-up Ergo blockchain full node in Rust. Not a port of the JVM reference node — a new implementation that reuses existing Rust crates for cryptography and script evaluation, with a new P2P networking layer already built and tested against testnet.

## Status

**Highly experimental.** Currently syncing the header chain: connects to Ergo testnet peers, syncs headers via SyncInfo/Inv exchange, validates PoW and chain rules (parent linkage, timestamps, difficulty adjustment), and builds a validated header chain. Running on testnet — syncs 17,000+ headers from genesis.

## Roadmap

| Phase | What | Status |
|---|---|---|
| 1 | **Header awareness** — parse headers, track network height | Done |
| 2 | **PoW verification** — verify Autolykos v2 before forwarding | Done |
| 3 | **Header chain validation** — parent hashes, timestamps, difficulty adjustment | Done |
| 3b | **Header chain sync** — SyncInfo exchange, async validation pipeline, peer rotation | Done |
| 4 | Transaction validation — validate against input boxes via `ergo-lib` | Next |
| 5 | UTXO state management — AVL+ tree backed, apply/rollback blocks | Planned |
| 6 | Full node — block storage, chain sync, mempool, REST API | Planned |

## What works today

- Connects to Ergo testnet peers and maintains persistent connections
- Accepts inbound connections from other nodes
- Routes all P2P messages between peers (Inv, ModifierRequest/Response, SyncInfo, GetPeers)
- **Header chain sync**: sends SyncInfo, requests headers via Inv/ModifierRequest exchange
- **Async validation pipeline**: batch-drain processing, sort-by-height, PoW + chain validation
- **Full chain validation**: parent linkage, timestamp bounds, difficulty adjustment, PoW
- **Event-driven sync**: two-batch cycles matching JVM behavior, peer rotation on stall
- Syncs 17,000+ validated headers from genesis on testnet (~400 headers/second burst)
- Tested: a JVM reference node syncs its full header chain exclusively through this relay

## Architecture

The node is composed of independent submodules, each owning a well-defined boundary:

| Directory | Repo | What it does |
|---|---|---|
| `p2p/` | [enr-p2p](https://github.com/mwaddip/enr-p2p) | P2P networking: handshake, message framing, routing, peer management |
| `chain/` | [enr-chain](https://github.com/mwaddip/enr-chain) | Header parsing, PoW verification, difficulty adjustment, chain validation |
| `sync/` | — | Header chain sync state machine |
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
