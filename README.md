# ergo-node-rust

A full Ergo blockchain node in Rust. Not a port of the JVM reference node — a ground-up implementation that reuses the [sigma-rust](https://github.com/ergoplatform/sigma-rust) ecosystem for cryptography and script evaluation, with a custom P2P networking layer reverse-engineered from the JVM node's wire protocol.

Validated from genesis through the full mainnet chain with zero checkpoints — every block, every transaction, every ErgoScript evaluation.

## Features

- **Full validation** — UTXO mode (persistent AVL+ tree) and digest mode (AD proof verification)
- **Parallel pipeline** — intra-block `par_iter` over transactions + cross-block apply_state/evaluate_scripts overlap
- **P2P networking** — IPv4/IPv6, peer discovery, handshake, SyncInfo exchange, section download, deep chain reorg
- **Mempool** — validate-on-entry, replace-by-fee, family weighting, fee statistics, P2P relay
- **REST API** — 19 JVM-compatible endpoints: blocks, transactions, UTXO lookups, peers, emission, mining
- **Mining API** — Autolykos v2 candidate assembly with EIP-27 re-emission, solution validation
- **Soft-fork voting** — epoch-boundary parameter tracking, JVM v6 `matchParameters60` semantics
- **NiPoPoW** — build and verify proofs (P2P codes 90/91), light-client bootstrap mode
- **UTXO snapshot sync** — bootstrap from peer snapshots, serve snapshots to peers
- **Crash recovery** — all stateful components survive `kill -9` and resume correctly

## Addons

Optional binaries that extend the node without adding dependencies to the core:

| Addon | What |
|-------|------|
| **fastsync** | Fast bootstrap via JVM peer REST API. Parallel multi-peer header and block section fetching. Auto-spawns on startup if installed. |
| **indexer** | SQLite transaction/box indexer with 17 REST endpoints and Swagger UI (port 9054). |

## Architecture

```
                    +------------------+
                    |     main crate   |  wires everything together via traits
                    +--------+---------+
                             |
         +-------------------+-------------------+
         |         |         |         |         |
      +--+--+  +--+--+  +---+---+ +---+---+ +---+---+
      | p2p |  |chain|  | sync  | |  api  | |mempool|
      +-----+  +-----+  +---+---+ +-------+ +-------+
                             |
                   +---------+---------+
                   |                   |
              +----+-----+     +------+------+
              |validation|     |    state    |
              +----------+     +-------------+
```

| Crate | Repo | Role |
|-------|------|------|
| `p2p/` | [enr-p2p](https://github.com/mwaddip/enr-p2p) | P2P networking, handshake, message framing, routing |
| `chain/` | [enr-chain](https://github.com/mwaddip/enr-chain) | Header parsing, PoW, difficulty adjustment, chain validation |
| `sync/` | in-repo | Sync state machine, section download, validation coordination |
| `validation/` | in-repo | Block validation: digest mode + UTXO mode, section serializers |
| `state/` | [enr-state](https://github.com/mwaddip/enr-state) | UTXO state via authenticated AVL+ tree over redb |
| `store/` | [enr-store](https://github.com/mwaddip/enr-store) | Persistent storage for headers, blocks, modifiers |
| `mempool/` | in-repo | Transaction pool, replace-by-fee, family weighting |
| `mining/` | in-repo | Candidate assembly, emission tx, PoW validation |
| `api/` | in-repo | REST API (axum), 19 endpoints |
| `facts/` | [ergo-node-facts](https://github.com/mwaddip/ergo-node-facts) | Interface contracts between components |

Components communicate through traits — the P2P layer doesn't know what validation means, and the validation layer doesn't know about networking.

## Building

### From source

```bash
cargo build --release
```

The binary is at `target/release/ergo-node-rust`.

### Debian package

```bash
./build-deb
```

Pre-built `.deb` packages are available on the [releases page](https://github.com/mwaddip/ergo-node-rust/releases).

### Addons

Addons are separate binaries in `addons/`:

```bash
# fastsync
cd addons/fastsync && cargo build --release

# indexer
cd addons/indexer && cargo build --release
```

## Configuration

```toml
[node]
network = "mainnet"
state_type = "utxo"        # "utxo", "digest", or "light"
data_dir = "/var/lib/ergo-node/data"

[p2p]
bind_addr = "[::]:9030"
```

See `local-standalone.toml` for a full example.

## Upstream dependencies

The node depends on two upstream Rust crates for consensus-critical primitives, consumed via forks that carry changes contributed back as open PRs.

### sigma-rust

[`mwaddip/sigma-rust`](https://github.com/mwaddip/sigma-rust) — `ergo-chain-types`, `ergo-lib`, `ergo-nipopow`, `sigma-ser`

Open PRs against [ergoplatform/sigma-rust](https://github.com/ergoplatform/sigma-rust):

- [#847](https://github.com/ergoplatform/sigma-rust/pull/847) — Fix panic in `gen_indexes` when index modulo N equals zero
- [#848](https://github.com/ergoplatform/sigma-rust/pull/848) — `ErgoTreePredef` port + genesis construction
- [#850](https://github.com/ergoplatform/sigma-rust/pull/850) — Soft-fork parameter variants
- [#851](https://github.com/ergoplatform/sigma-rust/pull/851) — `NipopowAlgos::prove_with_reader` for efficient proof serving
- [#852](https://github.com/ergoplatform/sigma-rust/pull/852) — NiPoPoW `has_valid_connections` tolerates skipped prefix entries
- [#854](https://github.com/ergoplatform/sigma-rust/pull/854) — Port JIT costing from sigmastate-interpreter
- [#855](https://github.com/ergoplatform/sigma-rust/pull/855) — Allocation bomb guard for VLQ-decoded message sizes
- [#857](https://github.com/ergoplatform/sigma-rust/pull/857) — BigInt modulo semantics (`mod` vs `rem`)
- [#858](https://github.com/ergoplatform/sigma-rust/pull/858) — Lazy constant resolution in ErgoTree evaluation
- [#859](https://github.com/ergoplatform/sigma-rust/pull/859) — Pre-JIT ErgoScript leniency for v0/v1 scripts

### ergo_avltree_rust

[`mwaddip/ergo_avltree_rust`](https://github.com/mwaddip/ergo_avltree_rust)

- [#10](https://github.com/ergoplatform/ergo_avltree_rust/pull/10) — `Resolver` type change to support disk-backed storage
- [#11](https://github.com/ergoplatform/ergo_avltree_rust/pull/11) — `VersionedAVLStorage::flush()` for durable commits on demand

## Credits

- [ergoplatform/sigma-rust](https://github.com/ergoplatform/sigma-rust) — ErgoScript interpreter, transaction validation, chain types
- [ergoplatform/ergo_avltree_rust](https://github.com/ergoplatform/ergo_avltree_rust) — Authenticated AVL+ tree
- [arkadianet/ergo](https://github.com/arkadianet/ergo) — `chainSlice` / parallel peer REST fetching technique used by fastsync

## License

Public domain. No rights reserved.
