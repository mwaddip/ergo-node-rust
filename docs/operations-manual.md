# Operations manual

> Initial draft, machine-generated. Expect rough edges; the human-touched
> version will accumulate over time.

Reference material for operators running `ergo-node-rust` in production.
Topic-oriented, not narrative — pick the section that matches what
you're trying to do. For first-install and day-to-day tasks, see
[`operator-guide.md`](operator-guide.md).

## Contents

- [Networking](#networking)
- [Memory tuning](#memory-tuning)
- [Storage and retention](#storage-and-retention)
- [Logging](#logging)
- [Monitoring](#monitoring)
- [Recovery and repair](#recovery-and-repair)
- [Snapshot bootstrap](#snapshot-bootstrap)
- [Soft-fork voting](#soft-fork-voting)
- [Upgrades and downgrades](#upgrades-and-downgrades)
- [Security hardening](#security-hardening)
- [Backup strategies](#backup-strategies)

## Networking

### Ports

| Port | Purpose | Notes |
|---|---|---|
| 9030 | Mainnet P2P (Rust default) | Both IPv4 and IPv6 connect to mainnet peers on `:9030`. |
| 9023 | Testnet P2P | Magic bytes `[2,3,2,3]` since the Feb 2026 testnet reset. |
| 9053 | Mainnet REST API | Default for `api_address` on mainnet. |
| 9052 | Testnet REST API | Default for `api_address` on testnet. |
| 9054 | Indexer REST API | Default; configurable in `indexer.toml`. |
| 9055 | Operator stats (`/stats/p2p`) | Loopback-only by default, opt-in via `[stats]` section. |

### Listener configuration

The node accepts inbound P2P connections on one or both of
`[listen.ipv4]` and `[listen.ipv6]`. At least one must be set.

```toml
[listen.ipv6]
address = "[::]:9030"
mode = "full"               # "light" advertises as NiPoPoW-bootstrapped
max_inbound = 20

[listen.ipv4]
address = "0.0.0.0:9030"
mode = "full"
max_inbound = 20
```

`mode = "full"` advertises the node as a full archival peer and forwards
everything. `mode = "light"` advertises as NiPoPoW-bootstrapped and
gossips a reduced subset; pair with `state_type = "light"`.

### Seed peers

Configured under `[outbound].seed_peers`. The defaults in
`ergo.toml.example` are split between testnet and mainnet — uncomment
the appropriate set when you switch networks. Peers are persisted in
`data_dir/peers.redb` after first connection; the seed list becomes
mostly fallback once the node has discovered enough peers via Peers
gossip.

### Peer discovery

Real peer discovery landed in v0.5.1 (PeerDb + Peers/GetPeers gossip
+ outbound fill phase 3). Before that the node relied on
seed-peers-only, which left typical deployments at 3 peers indefinitely.
On v0.5.1+ a fresh node climbs from seed-set to 30+ peers within ~20
minutes.

### UPnP

```toml
[upnp]
enabled = false              # set true for residential NAT
discover_timeout_secs = 5
```

UPnP is IPv4-only — IPv6 addresses are globally routable so the node
doesn't try IPv6 mappings. Disabled by default; turn on for home
deployments behind consumer NAT, leave off for data-center deployments
with public addresses.

### Address sanity filter

v0.5.3+ rejects PeerSpec entries whose addresses are link-local,
loopback, multicast, or RFC 1918 private when running on mainnet.
Peers that gossip such addresses get a permanent ban. Testnet is
permissive to allow lab setups with private addressing. This is also
implemented as the equivalent Scala patch in JVM PR
[#2307](https://github.com/ergoplatform/ergo/pull/2307).

### fail2ban (`.deb` only)

The `.deb` installs a fail2ban filter at
`/etc/fail2ban/filter.d/ergo-node.conf` and a jail definition at
`/etc/fail2ban/jail.d/ergo-node-jail.conf`. Both monitor systemd
journald entries for repeated peer misbehavior penalties and ban the
source IP at the firewall layer.

Notes:

- The jail file must be named `*.conf` or `*.local` — fail2ban
  silently ignores `*.jail` files.
- Backend is `systemd` with a journalmatch on
  `_SYSTEMD_UNIT=ergo-node-rust.service`.
- Operators with custom logging setups need to adapt the filter
  regex.

## Memory tuning

### Cold-sync vs at-tip

The node has two memory regimes:

- **Cold sync** — applies from start until the chain reaches tip.
  Larger redb cache + larger flush threshold = faster throughput.
- **At tip** — applies once `synced()` flips. Per-block updates are
  tiny at tip cadence (~1 block / 2 minutes on mainnet), so a much
  smaller working set is essentially free.

Configure cold-sync values directly; at-tip mirrors are optional and
inherit the cold-sync parent when unset.

```toml
[node]
# Cold sync
cache_mb = 1024                       # redb cache during sync
flush_heap_threshold_mb = 2048        # live-heap trigger for redb flush
flush_max_blocks = 100                # upper bound between flushes
flush_min_blocks = 5                  # lower bound, prevents storm

# At tip (optional; if unset, mirrors inherit the cold-sync value)
synced_cache_mb = 256
synced_flush_heap_threshold_mb = 512
synced_flush_max_blocks = 10
synced_flush_min_blocks = 5
```

### `flush_heap_threshold_mb` semantics

Set to `0` to disable the memory trigger entirely (flushes are then
governed solely by `flush_max_blocks`). Otherwise the validation sweep
polls jemalloc's `stats.allocated` between blocks and commits the
redb write transaction mid-sweep when the threshold is exceeded.

Single-block memory spikes can overshoot the threshold by GBs — the
flush dial polls between blocks, not within them. If you're seeing
RSS spikes, the dial is reactive rather than preventative.

### `synced_flush_min_blocks` minimum

Keep `synced_flush_min_blocks` at 5 or higher. With value 1 (per-block
flush at tip), a remove-then-reinsert AVL pattern across two adjacent
blocks can leak past the prover's dirty-node tracking and orphan a
node on disk. A wider window lets the pattern cancel within prover
state before any disk write.

### jemalloc

The default build links `tikv-jemallocator`. `[debug.memory]` and
process-level monitoring show jemalloc-specific stats:

- `allocated` — live heap (the dial's threshold target)
- `resident` — physical RSS as jemalloc sees it
- `retained` — **virtual** address space jemalloc holds for future use,
  NOT physical RAM. Don't panic-tune off `retained`; trust `resident`
  and the OS's RSS reading instead.

### `_RJEM_MALLOC_CONF` for jemalloc profiling

The `jemalloc-prof` feature flag enables jemalloc's built-in heap
profiler. Activate at runtime via `_RJEM_MALLOC_CONF`, not the
standard `MALLOC_CONF` — `tikv-jemallocator` prefixes the env var to
avoid colliding with system jemalloc.

```sh
_RJEM_MALLOC_CONF=prof:true,prof_prefix:/var/log/ergo-node/jeprof,lg_prof_sample:19 \
    ./ergo-node-rust
```

Build with `--no-default-features --features jemalloc-prof` to enable.

## Storage and retention

### `blocks_to_keep`

| Value | Behavior |
|---|---|
| `-1` | Keep all blocks from genesis (full archival). Default. |
| `0` | Keep only the at-tip working set. Header chain remains intact; non-header sections (BlockTransactions, ADProofs, Extension) are pruned. |
| `N >= 1` | Retain the last N blocks. Older sections pruned at flush time. |

Headers are never pruned. The flush dial's `flush_max_blocks` is capped
at `blocks_to_keep` when set, so the `validated_height → tip` gap can
never exceed what archived bodies cover. Crash recovery is safe at
any retention setting.

### redb tuning

```toml
[node]
cache_mb = 1024
```

The redb cache is a page cache — set it generously on machines with
spare RAM, set it lower on memory-constrained hosts. Lowering
`cache_mb` makes startup chain-walk slower (one-time cost) but
reduces steady-state RSS.

All write transactions are committed with `set_quick_repair(true)`
internally, which adds ~5 µs per commit but ensures `Database::open`
after `kill -9` walks only the dirty pages, not the entire file. On a
multi-GB store this can be a minutes-saved-per-restart difference.

### `data_dir` placement

For long-running production nodes:

- Put `data_dir` on an SSD; the random-IO pattern doesn't work well
  on spinning rust.
- For systemd-managed installs on `/DATA`-style separate filesystems,
  add `RequiresMountsFor=/DATA` to the service file — the default
  systemd target doesn't guarantee LUKS/`noauto` mounts arrive before
  service start.
- Filesystem should support fsync — avoid network filesystems.

## Logging

### Levels

`RUST_LOG=info` is the default. Common overrides:

```sh
RUST_LOG=ergo_node_rust=debug                    # verbose at the binary
RUST_LOG=ergo_sync=debug,ergo_validation=debug   # per-crate
RUST_LOG=warn                                    # quieter
```

### journald (systemd installs)

```sh
sudo journalctl -u ergo-node-rust -f             # tail
sudo journalctl -u ergo-node-rust --since=-1h    # recent
sudo journalctl -u ergo-node-rust -g WARN        # filter warns+
```

### Notable structured events

The node emits events the Ergo Node Doctor adapter and RRD harness
consume. Schema documented in `facts/journal-events.md`. Notable:

- `peer_added` / `peer_removed` — connection lifecycle
- `applied_header` — header chain progress
- `applied_block` — full block validated and applied
- `validation_stuck` — repeated apply-state failures (5+) at the same
  (height, error_kind). Signals state-store divergence.
- `flush_*` — redb flush triggers and outcomes
- `at_tip` — sync reached chain tip
- `synced` — at-tip transition complete, at-tip memory mirrors active

## Monitoring

### Built-in diagnostic endpoints

| Endpoint | Returns |
|---|---|
| `/info` | Version, network, height, peer count, sync status, state digest |
| `/debug/memory` | jemalloc + process + per-component memory breakdown |
| `/stats/p2p` | Cumulative inbound/outbound traffic counters (opt-in, loopback-default) |
| `/peers/connected` | Connected peers with state |
| `/peers/all` | Connected + known + banned peers |
| `/peers/blacklisted` | Currently banned peers and reasons |

### RRD harness

The `.deb` installs example RRD scripts at
`/usr/share/doc/ergo-node-rust/examples/rrd-{create,update,graph,demo-fill}.sh`.
Adapt as a starting point for an RRD-based monitoring setup. The
update script polls `/stats/p2p` and graph script renders bandwidth
+ peer count over time.

Note: RRD DS names are capped at 19 characters; longer names fail
at create time with an unhelpful "invalid DS format" error.

### P2P wire capture (opt-in)

For debugging peer-protocol issues, the node can capture every P2P
frame to a pcap-compatible mmap ring:

```toml
[debug.p2p_capture]
enabled = true
path = "./capture.ring"
size_mb = 1024
sync_interval_secs = 60
include_ips = []                 # mutually exclusive with exclude_ips
exclude_ips = []
```

Three HTTP endpoints expose the captured data:

```sh
curl localhost:9053/debug/p2p-capture/info
curl localhost:9053/debug/p2p-capture/dump > capture.pcap
curl -X POST localhost:9053/debug/p2p-capture/reset
```

See [`facts/p2p-capture.md`](../facts/p2p-capture.md) for the full
spec. Off by default — meaningful disk pressure when enabled.

## Recovery and repair

### `sharpen` — the operator's swiss-army knife

`sharpen` is bundled with the .deb (also in the source tree under
`src/bin/`). It handles state recovery, snapshot operations, and
maintenance tasks the running daemon shouldn't do itself.

```sh
sharpen --help
sharpen utxo_bootstrap --config /etc/ergo-node/ergo.toml
sharpen reset_validated_height --config /etc/ergo-node/ergo.toml --height N
```

Common scenarios:

- **State divergence after a hard kill** — `sharpen utxo_bootstrap`
  rolls back state and re-bootstraps from a peer snapshot.
- **`validation_stuck` errors** — see whether `sharpen reset_validated_height`
  to a known-good height clears the loop. If not, snapshot bootstrap.

### `--reset-scores-migration`

A maintenance flag that clears the v0.5.0 scores-backfill sentinel.
Forces the next normal start to re-run the migration that populates
the per-header score column. Operator tool, intentionally hidden from
`--help`.

```sh
ergo-node-rust --reset-scores-migration                       # uses config search
ergo-node-rust --reset-scores-migration /path/to/ergo.toml    # explicit
```

### `revalidate = true`

Re-validates all stored blocks from genesis on startup. Headers and
sections are NOT re-downloaded — only the validation passes are
re-run. Used to test validation logic changes against the full chain
history without a full resync. Slow on archival nodes; expect
hours-to-days depending on hardware.

### What sharpen can't fix

If the redb files themselves are corrupted (filesystem damage,
incomplete `kill -9` during a write before quick-repair), the only
path is to remove the data dir and resync (or snapshot-bootstrap).
Back up `data_dir/peers.redb` separately if you want to preserve peer
state across a resync.

## Snapshot bootstrap

### What it does

`utxo_bootstrap = true` skips replaying blocks from genesis and
instead downloads a UTXO snapshot from a peer that's serving one.
A typical mainnet bootstrap takes minutes to hours, versus the
hours-to-days a full genesis replay needs.

```toml
[node]
utxo_bootstrap = true
min_snapshot_peers = 2          # min peers announcing same snapshot before downloading
storing_snapshots = 0           # how many to keep for serving (0 = don't serve)
snapshot_interval = 52224       # ~72 days between snapshot creation points
```

### Serving snapshots to other nodes

Set `storing_snapshots = 1` (or higher) to retain snapshots and serve
them to other peers. Each retained snapshot is a few GB. Operators
running infrastructure for the community typically set
`storing_snapshots = 2` so peers always have a recent one.

The JVM reference node currently does NOT serve AD-proofs in
`utxoBootstrap` mode (a documented gap), so Rust↔JVM snapshot interop
falls back to Rust-only sources for the AD-proof piece.

### Initial deployment

For a fresh deployment from a tarball:

```toml
[node]
utxo_bootstrap = true
min_snapshot_peers = 1          # lower to bootstrap from a single trusted source
```

Trust assumption: snapshots are content-addressed, so a snapshot from
an untrusted source still validates against the network's claimed
state root — you can't be tricked into accepting forged state.

## Soft-fork voting

### Encoding votes

`[node.mining].votes` is a 3-byte hex string. Each byte is a vote
slot holding a signed parameter ID (interpreted as `i8`). A node
casts up to three votes per block, one per slot. The mempool /
miner adds these to the block header's `votes` field; epoch-boundary
tallying takes care of the rest.

| Byte value (hex) | Meaning |
|---|---|
| `00` | Empty slot — no vote in this position |
| `01` to `09` | Vote to **increase** an ordinary parameter (see table below) |
| `FF` to `F7` | Vote to **decrease** the same parameter (i8 of `-1` through `-9`) |
| `78` | Vote **yes** on the currently active soft-fork proposal (id 120) |

`votes = "000000"` means no votes (default — also what the JVM node
ships).

### Parameter IDs

Mapping in `chain/src/voting.rs::ordinary_param`, mirrors JVM
`Parameters.scala`. Values are `i8` so the same ID with the sign bit
flipped means "decrease."

| ID | Parameter | What changes |
|---|---|---|
| 1 | `StorageFeeFactor` | Storage rent factor |
| 2 | `MinValuePerByte` | Minimum value (nanoERG) per byte of box size |
| 3 | `MaxBlockSize` | Maximum block size in bytes |
| 4 | `MaxBlockCost` | Maximum total validation cost per block |
| 5 | `TokenAccessCost` | Per-token-access cost |
| 6 | `InputCost` | Per-input cost |
| 7 | `DataInputCost` | Per-data-input cost |
| 8 | `OutputCost` | Per-output cost |
| 9 | `SubblocksPerBlock` | Subblocks per block (6.0 soft-fork; auto-active at BlockVersion 4) |
| 120 | `SoftForkVote` | Special: yes-vote on the active soft-fork proposal, not a parameter change |

### Step sizes

A parameter change at activation is a percentage step, not a freely
chosen target. JVM `stepsTable` hardcodes step sizes for the first
three IDs; all others use `max(1, v / 100)` (1% of the current
value, with a floor of 1). So a vote is directional ("increase /
decrease") and granular by step, not an absolute target.

### Example

A miner who wants to increase `MaxBlockSize` and vote yes on the
active soft-fork:

```toml
[node.mining]
votes = "037800"          # slot 1: id 3 (MaxBlockSize+), slot 2: id 120 (soft-fork yes), slot 3: empty
```

### Activation cadence

Votes are tallied per voting epoch (1024 blocks on mainnet, 128 on
testnet). A soft-fork requires `soft_fork_epochs` (32) consecutive
qualifying epochs to be approved, then `activation_epochs` (32) more
before the `BlockVersion` increments. Ordinary parameter changes
activate at the next epoch boundary after the supermajority is met.

`[node].reconciliation_trust_threshold` interacts with voting state:
see `facts/sync.md` "Cross-DB Durability Handshake" for the
interaction. The default (100) is safe for normal operations.

### Activation cadence

Votes are tallied per voting epoch (1024 blocks on mainnet). A
proposed parameter change requires a supermajority over the
3-epoch sliding window to activate. The activation height equals
the next epoch boundary after the supermajority is reached.

`[node].reconciliation_trust_threshold` interacts with voting
state: see `facts/sync.md` "Cross-DB Durability Handshake" for
the interaction details. The default (100) is safe for normal
operations.

## Upgrades and downgrades

### `.deb` upgrade flow

```sh
sudo apt install ./ergo-node-rust_0.6.6_amd64.deb
```

The postinst hook runs `systemctl try-restart` (not `restart`), which
respects an operator-stopped service. If you've intentionally stopped
the node, the upgrade leaves it stopped — start manually after.

### Conffile prompts

`/etc/ergo-node/ergo.toml` is a dpkg conffile. On upgrade, dpkg will
prompt if both the packaged default AND your local copy have changed:

```
Configuration file '/etc/ergo-node/ergo.toml'
 ==> Modified (by you or by a script) since installation.
 ==> Package distributor has shipped an updated version.
   What would you like to do about it ?
```

Recommended: keep your version (`N`), then diff the new version
(`/etc/ergo-node/ergo.toml.dpkg-dist`) against yours and merge any
new options manually. Do NOT blindly accept the new file — you'll
lose your customizations.

#### Non-interactive upgrades

`apt install` and `dpkg -i` are interactive by default — the
conffile prompt waits on stdin. In non-interactive contexts (CI,
remote provisioning, scripted deploys, or just `apt install`
called without a controlling terminal), this hangs the upgrade
and dpkg eventually aborts with `end of file on stdin at conffile
prompt`, leaving the package half-configured.

To upgrade non-interactively while keeping your existing config:

```sh
sudo DEBIAN_FRONTEND=noninteractive apt install -y \
    -o Dpkg::Options::="--force-confold" \
    ./ergo-node-rust_<version>_<arch>.deb
```

Or, if you've already hit the half-configured state:

```sh
sudo DEBIAN_FRONTEND=noninteractive dpkg --configure -a \
    --force-confold
```

`--force-confold` accepts the user's existing config (equivalent
to answering `N` at the prompt). `--force-confnew` accepts the
packaged default (`Y`) — almost never what you want for this
config file.

### Downgrade

`apt` won't downgrade automatically. Force with:

```sh
sudo apt install ./ergo-node-rust_0.6.5_amd64.deb --allow-downgrades
```

Verify schema compatibility before downgrading — major version
downgrades may require a state resync if storage formats changed.
Patch-level downgrades are generally safe.

## Security hardening

### REST API exposure

The default `api_address` binds to `0.0.0.0:<port>`. For production
deployments NOT meant to expose a public API:

```toml
[node]
api_address = "127.0.0.1:9053"
```

Or front the API with a reverse proxy (nginx, Caddy) and an
authentication layer. The node itself doesn't implement API auth.

### `/stats/p2p` endpoint

Loopback-only by default (`127.0.0.1:9055`). Don't bind to `0.0.0.0`
without auth — it leaks cumulative traffic counters that can
fingerprint operator activity.

### `/debug/p2p-capture/*` endpoints

When enabled, the capture endpoints expose raw P2P bytes including
peer IPs. Loopback-only is appropriate. Captures themselves should be
treated as sensitive — they contain transaction propagation patterns
that, in aggregate, leak network topology.

### systemd sandboxing

The `.deb` ships a systemd unit with reasonable sandbox defaults
(NoNewPrivileges, ProtectSystem=strict, ProtectHome, etc.). Review
`/usr/lib/systemd/system/ergo-node-rust.service` and adjust for your
deployment.

### Running as a dedicated user

The `.deb` creates `ergo-node` system user and group; the systemd
unit runs the daemon as that user. For manual installs, create a
similar user:

```sh
sudo adduser --system --group --no-create-home --home /var/lib/ergo-node ergo-node
sudo chown -R ergo-node:ergo-node /var/lib/ergo-node /var/log/ergo-node
```

## Backup strategies

### What's worth backing up

| Path | Backup cadence | Notes |
|---|---|---|
| `data_dir/peers.redb` | Weekly | Cheap to back up, saves peer-rediscovery time after resync. |
| `data_dir/state.redb` | Don't | Re-derivable from blocks; backup is a waste of space. |
| `data_dir/modifiers.redb` | Don't, unless you serve snapshots | Re-downloadable from network. Multi-GB. |
| `/etc/ergo-node/ergo.toml` | Daily | Your custom config; cheap to back up. |
| `~/.ssh/`, etc. | Per your usual policy | Out of scope here. |

### Restore from a backup

The node is stateless from the chain's perspective — you can wipe
`data_dir` and resync, or snapshot-bootstrap. The data that
ACTUALLY needs backup is your config and any local customizations.
The chain itself is the network's responsibility to preserve.

### Snapshot operators

If you serve snapshots to other operators (`storing_snapshots > 0`),
back up the snapshot retention directory or accept that a resync
costs your community a bootstrap source until you regenerate.
