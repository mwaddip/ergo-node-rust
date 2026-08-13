# ergo-node-rust

Full Ergo blockchain node in Rust. Not a port of the JVM reference node — a ground-up implementation that reuses [sigma-rust](https://github.com/ergoplatform/sigma-rust) for cryptography and ErgoScript evaluation, with a custom P2P layer reverse-engineered from the JVM wire protocol.

Validated from genesis through mainnet with no checkpoints.

## Features

- **Full validation** — UTXO mode (persistent AVL+ tree) and digest mode (AD-proof verification)
- **Parallel validation** — transactions within a block are evaluated concurrently. Script evaluation runs *inside* block application, before anything is persisted, so a block that reaches `state.redb` has already had its scripts checked
- **P2P** — IPv4/IPv6, peer discovery, deep reorg support
- **Mempool** — validate-on-entry, replace-by-fee, family weighting, fee statistics, P2P relay
- **REST API** — 38 JVM-compatible endpoints (blocks, transactions, UTXO, peers, mining, NiPoPoW) plus `/debug/memory`. `/info` advertises `journalEventsVersion` (always) and `statsVersion` (when the optional `[stats]` section is configured) so downstream tooling can detect contract drift.
- **Operator stats endpoint** — opt-in `[stats]` section binds a loopback-only `/stats/p2p` endpoint with cumulative P2P traffic counters by message type. Supports external diagnostics (e.g. the Ergo Node Doctor) and an RRD harness under `tools/`.
- **Stable journal-event contract** — `facts/journal-events.md` names a versioned set of structured tracing events (startup phases, validation sweeps, reorgs, peer penalties, etc.) so log-parsing tools don't break on refactors.
- **Mining** — Autolykos v2 candidate assembly with EIP-27 re-emission, solution validation
- **Soft-fork voting** — epoch-boundary parameter tracking, v6.0.3-compatible
- **NiPoPoW** — build and verify proofs, light-client bootstrap mode
- **UTXO snapshot sync** — bootstrap from peer snapshots; serve snapshots to peers
- **At-tip memory tuning** — opt-in `synced_*` config swaps to a smaller redb cache once at tip. Measured on mainnet at 1.85M blocks with `cache_mb = 1024` / `synced_cache_mb = 128`: **1.9 GB during cold sync → 950 MB at tip**
- **Fast crash recovery** — header chain state is reconstructable from the store; redb writes use `quick_repair` so `Database::open` after `kill -9` skips the full-file allocator scan. Restart-to-API on a fully-synced mainnet node is sub-second.

## Addons

Optional binaries:

| Addon | What |
|-------|------|
| **fastsync** | Fast bootstrap via JVM peer REST API; parallel multi-peer fetching. Auto-spawns at startup if installed. |
| **indexer** | SQLite transaction/box indexer with 19 REST endpoints and Swagger UI (port 9054). |

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

| Crate | Role |
|-------|------|
| `p2p/` | P2P networking, handshake, message framing, routing |
| `chain/` | Header parsing, PoW, difficulty adjustment, chain validation |
| `sync/` | Sync state machine, section download, validation coordination |
| `validation/` | Block validation: digest mode + UTXO mode, section serializers |
| `state/` | UTXO state via authenticated AVL+ tree over redb |
| `store/` | Persistent storage for headers, blocks, modifiers |
| `mempool/` | Transaction pool, replace-by-fee, family weighting |
| `mining/` | Candidate assembly, emission tx, PoW validation |
| `api/` | REST API (axum), 39 endpoints |
| `facts/` | Per-component contract markdown |

Components communicate through traits — the P2P layer doesn't know what validation means, and the validation layer doesn't know about networking.

`p2p/`, `chain/`, `state/`, `store/`, and `facts/` were originally separate repositories absorbed into the main repo at v0.4.5. Their pre-absorb histories are preserved at the archived origins:

| Crate | Pre-absorb origin (archived) | Last submodule commit |
|-------|------------------------------|------------------------|
| `p2p/` | [mwaddip/enr-p2p](https://github.com/mwaddip/enr-p2p) | `e8185cc` |
| `chain/` | [mwaddip/enr-chain](https://github.com/mwaddip/enr-chain) | `8e28fc0` |
| `state/` | [mwaddip/enr-state](https://github.com/mwaddip/enr-state) | `9077d10` |
| `store/` | [mwaddip/enr-store](https://github.com/mwaddip/enr-store) | `b997da5` |
| `facts/` | [mwaddip/ergo-node-facts](https://github.com/mwaddip/ergo-node-facts) | `8a8ad45` |

## Development

This project is built with a multi-session AI workflow: one main session coordinates interface contracts (in `facts/`) and integration; per-crate dispatched sessions implement against those contracts within their crate's directory boundary. Every cross-crate disagreement is resolved by updating the contract first, not by writing glue code — "fix the interface, never wrap the mismatch."

Codebase-specific guidance — design principles, S.P.E.C.I.A.L. attention weights, common mistakes, protocol references — lives in [`agents/ergo-node-development/SKILL.md`](agents/ergo-node-development/SKILL.md). The dispatch mechanics (kitty window automation, executor identity, anti-recursion guards) live in [`mwaddip/claude-dbc`](https://github.com/mwaddip/claude-dbc) — install that skill pair (`dispatching-prompts` + `receiving-prompts`) to use the same workflow.

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

# Cold-sync (initial sync from genesis or snapshot).
# cache_mb is the TOTAL redb page cache across both databases,
# split by cache_store_pct (modifiers.redb gets that %, state.redb
# the rest). It sized state.redb alone before v0.8.0.
cache_mb = 1024
cache_store_pct = 50
flush_heap_threshold_mb = 2048

# At-tip mirrors. Once sync reaches tip, the AVL state DB reopens
# with the smaller cache (~ms pause). Omit to keep cold-sync values.
synced_cache_mb = 256
synced_flush_heap_threshold_mb = 512
synced_flush_max_blocks = 10
synced_flush_min_blocks = 5
```

See `ergo.toml.example` for the full annotated reference (every
supported option with its default), or `man ergo-node-rust.conf` for
the per-key syntax.

## Documentation

Operator-facing documentation lives under `docs/`:

- [`docs/operator-guide.md`](docs/operator-guide.md) — install,
  configure, first run, verify, common tasks, indexer/mining,
  troubleshooting (task-oriented)
- [`docs/operations-manual.md`](docs/operations-manual.md) —
  networking, memory tuning, storage and retention, logging,
  monitoring, recovery, snapshots, voting, upgrades, security,
  backups (topic-oriented reference)
- [`facts/openapi.yaml`](facts/openapi.yaml) — full REST API
  schema (OpenAPI 3.1, 43 endpoints). View via Swagger Editor,
  Redoc, or any OpenAPI viewer.
- [`facts/api.md`](facts/api.md) — cross-cutting API rationale
  (auth surface, JVM compatibility, error model, naming)
- `man ergo-node-rust(8)`, `man ergo-node-rust.conf(5)`,
  `man sharpen(8)` — installed by the `.deb` package

## Upstream dependencies

The node depends on two upstream Rust crates for consensus-critical primitives, consumed via forks that carry changes contributed back as open PRs.

### sigma-rust

[`mwaddip/sigma-rust`](https://github.com/mwaddip/sigma-rust) — `ergo-chain-types`, `ergo-lib`, `ergo-nipopow`, `sigma-ser`

**45 open, 10 merged** against [ergoplatform/sigma-rust](https://github.com/ergoplatform/sigma-rust) —
[live list](https://github.com/ergoplatform/sigma-rust/pulls?q=is%3Apr+author%3Amwaddip).
They are not enumerated here because the set turns over faster than a README
does; the query above is always current.

Merged so far: NiPoPoW prefix-connection lookback (#852) and `pack_interlinks`
key encoding (#866), non-pair tuple rejection (#868), `Global.powHit` return
type (#877), checked ERG summation (#891), `Option` data non-zero tag (#895),
`checkType` ingress divergences (#897), context-extension key domain (#906),
signed header-version gate (#909), and v0.30.0 style gating (#915).

⚠ Upstream's `develop` branch is frozen; **`v0.30.0` is the live integration
branch**, and every merge above landed there. Open PRs still target `develop`,
so GitHub reporting them all as `MERGEABLE` means only "mergeable into a branch
nobody moves" — it is not a readiness signal.

### ergo_avltree_rust

[`mwaddip/ergo_avltree_rust`](https://github.com/mwaddip/ergo_avltree_rust)

**5 open, 6 merged** against [ergoplatform/ergo_avltree_rust](https://github.com/ergoplatform/ergo_avltree_rust) —
[live list](https://github.com/ergoplatform/ergo_avltree_rust/pulls?q=is%3Apr+author%3Amwaddip).

- [#27](https://github.com/ergoplatform/ergo_avltree_rust/pull/27) — persistent-prover support: proof cycle, rewind path, storage flush. Supersedes #11/#18/#19/#22 and carries the `restore_root` fix this node pins for
- [#13](https://github.com/ergoplatform/ergo_avltree_rust/pull/13) — `contains_recursive` fail-safes on unresolvable `LabelOnly`
- [#14](https://github.com/ergoplatform/ergo_avltree_rust/pull/14), [#24](https://github.com/ergoplatform/ergo_avltree_rust/pull/24) — `Err` instead of abort on malformed proofs and out-of-range params
- [#16](https://github.com/ergoplatform/ergo_avltree_rust/pull/16) — `Resolver` as `Arc<dyn Fn>` with a non-breaking constructor

Merged: JVM-oracle proof comparison tests (#17), proof-deserialization input
guards (#20), `is_new = false` on deserialized nodes (#21), rustfmt (#23), and
checked `UpdateLongBy` delta (#25).

## Credits

- [ergoplatform/sigma-rust](https://github.com/ergoplatform/sigma-rust) — ErgoScript interpreter, transaction validation, chain types
- [ergoplatform/ergo_avltree_rust](https://github.com/ergoplatform/ergo_avltree_rust) — Authenticated AVL+ tree
- [arkadianet/ergo](https://github.com/arkadianet/ergo) — `chainSlice` / parallel peer REST fetching technique used by fastsync

## License

MIT — see [LICENSE](LICENSE).
