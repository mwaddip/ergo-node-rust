# ergo-node-rust

Full Ergo blockchain node in Rust. Not a port of the JVM reference node — a ground-up implementation that reuses [sigma-rust](https://github.com/ergoplatform/sigma-rust) for cryptography and ErgoScript evaluation, with a custom P2P layer reverse-engineered from the JVM wire protocol.

Validated from genesis through mainnet with no checkpoints.

## Features

- **Full validation** — UTXO mode (persistent AVL+ tree) and digest mode (AD-proof verification)
- **Parallel validation** — concurrent transaction evaluation within blocks, pipelined across blocks
- **P2P** — IPv4/IPv6, peer discovery, deep reorg support
- **Mempool** — validate-on-entry, replace-by-fee, family weighting, fee statistics, P2P relay
- **REST API** — 23 JVM-compatible endpoints (blocks, transactions, UTXO, peers, mining) plus `/debug/memory`
- **Mining** — Autolykos v2 candidate assembly with EIP-27 re-emission, solution validation
- **Soft-fork voting** — epoch-boundary parameter tracking, v6.0.3-compatible
- **NiPoPoW** — build and verify proofs, light-client bootstrap mode
- **UTXO snapshot sync** — bootstrap from peer snapshots; serve snapshots to peers
- **At-tip memory tuning** — opt-in `synced_*` config swaps to a smaller redb cache once at tip (~80% RSS reduction on mainnet, 7.3 GB → 1.35 GB)
- **Crash recovery** — clean resume after `kill -9`

## Addons

Optional binaries:

| Addon | What |
|-------|------|
| **fastsync** | Fast bootstrap via JVM peer REST API; parallel multi-peer fetching. Auto-spawns at startup if installed. |
| **indexer** | SQLite transaction/box indexer with 18 REST endpoints and Swagger UI (port 9054). |

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
| `api/` | in-repo | REST API (axum), 23 endpoints |
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

Separate binaries in `addons/`:

```bash
cd addons/fastsync && cargo build --release
cd addons/indexer  && cargo build --release
```

## Running

The `.deb` installs a systemd unit and a working default config at
`/etc/ergo-node/ergo.toml`:

```bash
sudo systemctl start ergo-node-rust
journalctl -u ergo-node-rust -f
```

Operator docs ship as manpages: `man ergo-node-rust`,
`man ergo-node-rust.conf`, `man sharpen`.

The `sharpen(8)` tool rolls the chain back to a target height —
useful for recovering from corrupt state without resyncing from
genesis.

## Configuration

Memory dials are what most operators tune. Defaults are conservative.

```toml
[proxy]
network = "mainnet"

[node]
data_dir = "/var/lib/ergo-node/data"
state_type = "utxo"             # "utxo" | "digest" | "light"

# Cold-sync (initial sync from genesis or snapshot)
cache_mb = 1024
flush_heap_threshold_mb = 2048

# At-tip mirrors. Once sync reaches tip, the AVL state DB reopens
# with the smaller cache (~ms pause). Omit to keep cold-sync values.
synced_cache_mb = 256
synced_flush_heap_threshold_mb = 512
synced_flush_max_blocks = 5
synced_flush_min_blocks = 1
```

See `mainnet.toml` for a full annotated example, or
`man ergo-node-rust.conf` for every key with its default.

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
- [#860](https://github.com/ergoplatform/sigma-rust/pull/860) — Parse-time `SOption(T)` leniency in `check_post_eval_tpe` (matches JVM `OneArgumentOperationSerializer`)

### ergo_avltree_rust

[`mwaddip/ergo_avltree_rust`](https://github.com/mwaddip/ergo_avltree_rust)

- [#10](https://github.com/ergoplatform/ergo_avltree_rust/pull/10) — `Resolver` type change to support disk-backed storage
- [#11](https://github.com/ergoplatform/ergo_avltree_rust/pull/11) — `VersionedAVLStorage::flush()` for durable commits on demand
- [#13](https://github.com/ergoplatform/ergo_avltree_rust/pull/13) — `contains_recursive` fail-safes on unresolvable `LabelOnly` (prevents `removed_nodes()` over-deletion with persistent backends)

## Credits

- [ergoplatform/sigma-rust](https://github.com/ergoplatform/sigma-rust) — ErgoScript interpreter, transaction validation, chain types
- [ergoplatform/ergo_avltree_rust](https://github.com/ergoplatform/ergo_avltree_rust) — Authenticated AVL+ tree
- [arkadianet/ergo](https://github.com/arkadianet/ergo) — `chainSlice` / parallel peer REST fetching technique used by fastsync

## License

Public domain. No rights reserved.
