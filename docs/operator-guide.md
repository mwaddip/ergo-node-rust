# Operator guide

> Initial draft, machine-generated. Expect rough edges; the human-touched
> version will accumulate over time.

This guide walks through using `ergo-node-rust` as an operator: installing
it, configuring it, starting it up, querying it, and troubleshooting the
common ways things break. For the topic-by-topic reference on production
operations (tuning, recovery, networking specifics), see
[`operations-manual.md`](operations-manual.md). For the day-to-day CLI
and config reference, the `.deb` package installs proper manpages:
`man 8 ergo-node-rust` and `man 5 ergo-node-rust.conf`.

## Contents

- [Installation](#installation)
- [Configuration](#configuration)
- [First run](#first-run)
- [Verifying the node](#verifying-the-node)
- [Common tasks](#common-tasks)
- [Running with the indexer](#running-with-the-indexer)
- [Mining](#mining)
- [Troubleshooting](#troubleshooting)
- [See also](#see-also)

## Installation

Three paths, all building from the same source.

### .deb package (recommended for systemd)

Grab the matching `ergo-node-rust_<version>_<arch>.deb` from the
[releases page](https://github.com/mwaddip/ergo-node-rust/releases) and
install:

```sh
sudo apt install ./ergo-node-rust_0.6.5_amd64.deb
sudo systemctl status ergo-node-rust
sudo journalctl -u ergo-node-rust -f
```

The package installs the binary at `/usr/bin/ergo-node-rust`, the
config at `/etc/ergo-node/ergo.toml` (a dpkg conffile — your edits are
preserved across upgrades), the systemd unit, the manpages, and the
documented example config at
`/usr/share/doc/ergo-node-rust/examples/ergo.toml.example`.

The optional `ergo-indexer` and `ergo-fastsync` addons ship as
separate `.deb` packages from the same release.

### Tarball

Pick the matching `ergo-node-rust-<version>-linux-<arch>.tar.gz`,
extract it, and run:

```sh
tar -xzf ergo-node-rust-0.6.5-linux-amd64.tar.gz
cd ergo-node-rust-0.6.5-linux-amd64
./install.sh           # interactive setup, writes ./ergo.toml
./ergo-node-rust       # or simply this — see the next section
```

You can also start the binary with no setup at all; see
[First run](#first-run) for what happens then.

### From source

Requires a stable Rust toolchain and the git submodules:

```sh
git clone --recursive https://github.com/mwaddip/ergo-node-rust
cd ergo-node-rust
cargo build --release --bin ergo-node-rust
./target/release/ergo-node-rust
```

## Configuration

### Config search order

Running `./ergo-node-rust` with no arguments searches for a config in
this order, first match wins:

1. `./ergo.toml` (current working directory)
2. `~/.config/ergo-node/ergo.toml` (user-scoped install)
3. `/etc/ergo-node/ergo.toml` (system-wide, via `.deb`)

An explicit positional path overrides the search:

```sh
./ergo-node-rust /path/to/my-config.toml
```

If nothing is found in any search location, the node writes a default
`./ergo.toml` (testnet, full archival, IPv6 listener), creates
`./ergo-node-data/`, and starts. A `WARN` log line tells you it
happened. See [First run](#first-run).

### Interactive setup (tarball only)

The tarball ships `install.sh`. It asks ~8 questions (network, state
type, storage path, memory limits, API bind) and writes a working
`./ergo.toml`:

```sh
./install.sh
```

Press Enter to accept the bracketed default at each prompt. The script
refuses to clobber an existing `./ergo.toml` without confirmation.

### Manual configuration

For everything `install.sh` doesn't ask about — peer settings, mining
config, soft-fork voting, at-tip memory mirrors, debug captures — the
reference is `ergo.toml.example`. It documents every supported option
with its default commented in. `.deb` installs find it at
`/usr/share/doc/ergo-node-rust/examples/ergo.toml.example`; tarball
installs find it next to the binary.

### Key settings

| Setting | Default | Notes |
|---|---|---|
| `[proxy].network` | `"testnet"` | Switch to `"mainnet"` requires updating `seed_peers` to mainnet peers (see the example config). |
| `[node].data_dir` | `./ergo-node-data` | `.deb` installs override this to `/var/lib/ergo-node/data`. |
| `[node].state_type` | `"utxo"` | `"utxo"` = full state, can mine. `"digest"` = state root only, smaller footprint, can't mine. `"light"` = NiPoPoW-bootstrapped, headers + sliding window. |
| `[node].blocks_to_keep` | `-1` | `-1` = full archive. `0` = at-tip only (pruned). `N` = retain last N blocks. |
| `[node].cache_mb` | `256` | redb cache during cold sync. Bump on machines with plenty of RAM for faster sync. |
| `[node].flush_heap_threshold_mb` | `4096` | Live-heap threshold that triggers a redb flush. Tune down on memory-constrained hosts. |
| `[node].fastsync` | `true` | Auto-spawn `ergo-fastsync` on startup if the binary is in PATH. Skip with `false` for pure-P2P sync. |
| `[node].api_address` | `0.0.0.0:9053` mainnet / `:9052` testnet | REST API bind. Keep on loopback (`127.0.0.1:9053`) if you don't want external access. |

## First run

### From a fresh tarball with no setup

```sh
./ergo-node-rust
```

You'll see something like:

```
INFO ergo_node_rust: Ergo node starting version="0.6.5" network="testnet"
WARN ergo_node_rust: no config found — wrote a default (testnet, full archival, state in ./ergo-node-data). Edit ./ergo.toml or run ./install.sh for interactive setup. written="./ergo.toml" search_paths="./ergo.toml, ~/.config/ergo-node/ergo.toml, /etc/ergo-node/ergo.toml"
INFO ergo_node_rust: node config state_type=Utxo verify_transactions=true blocks_to_keep=-1 ...
INFO ergo_node_rust: opening modifier store path="./ergo-node-data/modifiers.redb"
```

The node creates `./ergo-node-data/`, writes a `./ergo.toml` with
sensible defaults, and starts syncing from genesis. Stop it with
`Ctrl-C`; restart it the same way.

### From a `.deb` install

systemd manages the lifecycle:

```sh
sudo systemctl start ergo-node-rust
sudo journalctl -u ergo-node-rust -f
```

Initial sync from genesis on mainnet takes wall-clock-hours to days
depending on hardware and whether `fastsync` is enabled.
[Snapshot bootstrap](operations-manual.md) is the faster alternative
for fresh installs that don't need the full archival history.

## Verifying the node

### Reading the logs

The node logs to stdout (tarball: directly visible; `.deb`: journald
via systemd). Key things to watch:

- `Ergo node starting version=...` — the node is up.
- `peer_added` lines — outbound connections succeeding.
- `applied_block` / `applied_header` lines — sync progress.
- `WARN` and `ERROR` are worth reading; everything below is routine.
- `validation_stuck` — repeated apply-state failures at the same
  height (see operations manual; usually means state corruption,
  reach for `sharpen`).

### Quick health endpoints

Replace `9053` with `9052` for testnet, or whatever you set as
`api_address`.

```sh
# Is the node up, what version, what network, what height?
curl -s localhost:9053/info | jq '.appVersion, .network, .fullHeight'

# How many peers do we have connected?
curl -s localhost:9053/peers/connected | jq '. | length'

# Are we at the chain tip or still catching up?
curl -s localhost:9053/info | jq '.headersHeight, .fullHeight'

# Detailed peer state (penalty scores, last seen)
curl -s localhost:9053/peers/all | jq
```

If `headersHeight == fullHeight` and that number matches what other
nodes are reporting, you're at tip.

## Common tasks

### Look up a UTXO box

```sh
# By box ID (hex string)
curl -s localhost:9053/utxo/byId/<box_id> | jq
```

### Look up a transaction

```sh
# Unconfirmed (in mempool)
curl -s localhost:9053/transactions/unconfirmed | jq '.[] | .id' | head

# By ID via the indexer (the node itself does not currently expose
# direct tx-by-id lookup outside of /blocks/{id})
curl -s localhost:9054/api/v1/transactions/<tx_id> | jq
```

### Submit a transaction

```sh
curl -s -X POST -H 'Content-Type: application/json' \
    -d @signed-tx.json localhost:9053/transactions
```

A successful submission returns the transaction ID. Failures return
a structured error indicating which validation step rejected the
transaction.

### Get a block by ID

```sh
curl -s localhost:9053/blocks/<header_id> | jq
```

### Check the fee curve

```sh
# What fee should I pay for a tx of <bytes> bytes to confirm soon?
curl -s "localhost:9053/transactions/getFee?bytes=<bytes>&waitTime=<seconds>"
```

### Pull a mining candidate

Only works if mining is configured (`[node.mining].miner_pk` set).

```sh
curl -s localhost:9053/mining/candidate | jq
```

### NiPoPoW proof for a light client

```sh
# Proof of length-k with security parameter m
curl -s "localhost:9053/nipopow/proof/12/6" | jq
```

## Running with the indexer

`ergo-indexer` is the optional sidecar that exposes a rich query API
(by-address lookup, transaction history, token metadata) over SQLite
or PostgreSQL. It pulls blocks from the node's HTTP API and runs on
its own port (default `:9054`).

### Quick setup (tarball)

```sh
# In the ergo-indexer-*-linux-*.tar.gz directory:
./indexer-install.sh                       # interactive
./ergo-indexer --config indexer.toml       # run
```

The indexer needs a running node. Default config points at
`http://127.0.0.1:9053`; change in `indexer.toml` if your node is on a
different host or port.

### .deb setup

```sh
sudo apt install ./ergo-indexer_0.2.0_amd64.deb
sudo systemctl start ergo-indexer
```

The .deb config lives at `/etc/ergo-node/indexer.toml`. Default
backend is SQLite at `/var/lib/ergo-indexer/indexer.db`. To switch to
PostgreSQL, edit `storage.db` to a `postgres://` URL and restart.

### Schema migration

The `.deb` and tarball both ship `ergo-indexer-migratedb`, a
bidirectional SQLite ↔ PostgreSQL migrator:

```sh
ergo-indexer-migratedb \
    --source sqlite:///var/lib/ergo-indexer/indexer.db \
    --target postgres://localhost/ergo_indexer
```

See `ergo-indexer-migratedb --help` for the full flag set including
`--resume`, `--update-config`, and the precondition checks.

## Mining

### Solo mining

Set `[node.mining].miner_pk` in `ergo.toml` to your miner's hex
public key (33 bytes, compressed EC point). Optionally set `votes` to
encode soft-fork preferences (see operations manual). Restart the
node, then your miner can pull from `/mining/candidate` and submit
back to `/mining/solution`.

### Pool mining

Pools typically pull `/mining/candidate` from a node they trust. The
node itself doesn't need any special configuration to feed a pool —
just expose the API to the pool (carefully, see operations manual on
auth surface). Set `miner_pk` to a pool-controlled address.

### Voting

`[node.mining].votes` is a 3-byte hex string. Each byte is a slot
holding a signed parameter ID (`i8`); a node casts up to three
votes per block. Empty `"000000"` = no votes. The full ID table
(StorageFeeFactor through SubblocksPerBlock, plus 120 = soft-fork
yes-vote) is in the [operations manual](operations-manual.md#soft-fork-voting).

## Troubleshooting

### Few or no peers

```sh
curl -s localhost:9053/peers/connected | jq '. | length'
```

If under 3 after a few minutes:

- Check that your firewall isn't blocking outbound on the seed peer
  ports (mostly `:9030` mainnet, `:9020–:9023` testnet).
- Confirm the listener bound: log line `listening_on=...`.
- IPv4-only hosts may need `[listen.ipv4]` configured instead of
  `[listen.ipv6]` — by default the example config ships IPv6.
- `[upnp].enabled = true` helps for residential connections behind
  NAT; otherwise consider port-forwarding.

### Sync is slow

- Confirm `fastsync = true` and that `ergo-fastsync` is in PATH
  (`.deb` installs put it at `/usr/bin/`).
- Bump `cache_mb` to `1024` or higher if you have memory to spare.
- Snapshot bootstrap (`utxo_bootstrap = true` + a snapshot-serving
  peer) is the fastest cold-start. See operations manual.

### High RSS / memory pressure

- Check `/debug/memory` for the jemalloc + process + per-component
  breakdown.
- Tune `flush_heap_threshold_mb` downward — flushes more often, keeps
  the redb dirty-page cache smaller.
- At tip, set the `synced_*` mirrors (`synced_cache_mb=256`,
  `synced_flush_heap_threshold_mb=512`, etc.) to drop the steady-state
  working set.

### Sync stuck at a height, repeated apply-state errors

Search the journal for `validation_stuck`. If you see it, the local
state has likely drifted. Options:

- `sharpen utxo_bootstrap --config /etc/ergo-node/ergo.toml` to
  rollback and re-bootstrap from a peer snapshot.
- `sharpen --help` for the full toolset.

### `Permission denied` at startup (`code: 13`)

This was the v0.6.4 tarball-install pitfall. The compiled-in default
`data_dir` used to point at `/var/lib/ergo-node/data`. v0.6.5 changed
the default to `./ergo-node-data`. If you hit this on v0.6.5+, your
`ergo.toml` likely has an explicit `data_dir` pointing somewhere
unwritable — check the `[node].data_dir` line.

### Logs are too noisy / too quiet

Set the `RUST_LOG` environment variable:

```sh
RUST_LOG=info ./ergo-node-rust              # default level
RUST_LOG=ergo_node_rust=debug ./ergo-node-rust
RUST_LOG=warn ./ergo-node-rust              # quieter
```

For systemd: edit the unit's `Environment=` line or create a
drop-in at `/etc/systemd/system/ergo-node-rust.service.d/`.

## See also

- [Operations manual](operations-manual.md) — production tuning,
  recovery, networking specifics
- `man 8 ergo-node-rust` — CLI invocation (`.deb` installs)
- `man 5 ergo-node-rust.conf` — config syntax (`.deb` installs)
- `ergo.toml.example` — every supported config option, with defaults
- [`facts/openapi.yaml`](../facts/openapi.yaml) — full REST API
  schema (OpenAPI 3.1). View via Swagger Editor, Redoc, or any
  OpenAPI viewer.
- [`facts/api.md`](../facts/api.md) — cross-cutting API rationale
  (auth surface, JVM compatibility, error model, naming
  conventions)
- [JVM reference node](https://github.com/ergoplatform/ergo) — the
  protocol-level reference, used for behavior questions where the
  Rust implementation is ambiguous
