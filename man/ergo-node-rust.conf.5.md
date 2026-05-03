% ERGO-NODE-RUST.CONF(5) ergo-node-rust | File Formats Manual

# NAME

**ergo.toml** - configuration file for ergo-node-rust(8)

# SYNOPSIS

**/etc/ergo-node/ergo.toml**

# DESCRIPTION

The ergo-node-rust daemon reads its configuration from a TOML file passed
as its sole positional argument. The Debian package installs a default at
**/etc/ergo-node/ergo.toml** and the systemd unit invokes the daemon with
that path.

The file is divided into sections. The **[proxy]**, **[listen.\*]**,
**[outbound]**, and **[identity]** sections are required for any node
that opens P2P connections. The **[node]** section configures the
validating node's behavior; if absent, every key takes its compiled-in
default. The **[network]**, **[upnp]**, and **[node.mining]** sections
are optional.

Settings map to the JVM reference node's *application.conf* where
applicable.

# [proxy]

**network** = *"mainnet" | "testnet"*
:   Network to join. Determines magic bytes, genesis block, peer
    discovery, and default API port. Required.

# [listen.ipv6] / [listen.ipv4]

At least one listener must be configured. IPv6 is preferred — Ergo
mainnet peers are increasingly IPv6-native, and IPv6 sidesteps NAT.

**address** = *"\[::\]:9030" | "0.0.0.0:9020"*
:   Socket address to bind. The default mainnet ports are 9030 (this
    node) and 9020 (JVM convention).

**mode** = *"full" | "light"*
:   **full** advertises this node as a full archival peer.
    **light** advertises as NiPoPoW-bootstrapped (gossip only).

**max_inbound** = *integer*
:   Maximum simultaneous inbound peer connections.

# [outbound]

**min_peers** = *integer*
:   Minimum desired outbound peer count. The connection manager keeps
    dialing until at least this many are connected.

**max_peers** = *integer*
:   Hard cap on outbound peers. Must be ≥ **min_peers**.

**seed_peers** = *\[ "host:port", ... \]*
:   Initial peer list for bootstrap. At least one entry is required.
    Once the peer database is populated these become one source among
    many.

# [identity]

**agent_name** = *string*
:   Software identifier reported in the handshake.
    Default: **"ergo-node-rust"**.

**peer_name** = *string*
:   Human-readable node name reported in the handshake.

**protocol_version** = *"X.Y.Z"*
:   Ergo protocol version this node implements. Format is exactly three
    dot-separated bytes.

# [network] (optional)

JVM-equivalent timing and batch parameters. Defaults match the JVM
reference node and are appropriate for mainnet.

**get_peers_interval_secs** = *integer*
:   Interval between **GetPeers** keepalive messages. Default: **120**.

**delivery_timeout_secs** = *integer*
:   How long to wait for a requested modifier before re-requesting from
    another peer. Default: **10**.

**max_delivery_checks** = *integer*
:   Maximum re-request attempts for a single modifier before giving up.
    Default: **100**.

**desired_inv_objects** = *integer*
:   Target number of modifier IDs in each **Inv** / **Request** batch.
    Default: **400**.

**max_peer_spec_objects** = *integer*
:   Maximum **PeerSpec** entries in a single **Peers** message.
    Default: **64**.

**handshake_timeout_secs** = *integer*
:   Handshake completion timeout. Default: **30**.

**inactive_connection_deadline_secs** = *integer*
:   Drop connections idle longer than this. Default: **600**.

**temporal_ban_duration_mins** = *integer*
:   Default temporary ban duration for misbehaving peers.
    Default: **60**.

# [upnp] (optional, IPv4 only)

**enabled** = *bool*
:   Attempt UPnP gateway discovery and port mapping for the IPv4
    listener at startup. IPv6 addresses are globally routable and never
    need UPnP. Default: **false**.

**discover_timeout_secs** = *integer*
:   Gateway discovery timeout. Default: **5**.

# [node]

## Storage and validation

**data_dir** = *path*
:   Directory containing **state.redb** and **modifiers.redb**.
    Default: **"/var/lib/ergo-node/data"**.

**state_type** = *"utxo" | "digest"*
:   **utxo** maintains the full UTXO set as an authenticated AVL+ tree
    (required for mining and ad-hoc UTXO queries). **digest** keeps only
    the state root and verifies AD proofs against it (lightweight).
    Default: **"utxo"**.

**verify_transactions** = *bool*
:   Run the ErgoScript interpreter on every transaction. Required
    (effectively) when **state_type = "utxo"**. Default: **true**.

**blocks_to_keep** = *integer*
:   Number of recent blocks for which to keep full block sections.
    **-1** keeps everything from genesis. Default: **-1**.

**revalidate** = *bool*
:   On startup, replay validation against every stored block from
    genesis. Headers and sections are kept — nothing is re-downloaded.
    Useful when changing validation logic. Default: **false**.

**checkpoint_height** = *integer*
:   ErgoScript validation checkpoint. Blocks at or below this height
    skip script evaluation; AD-proof verification alone is considered
    sufficient. **0** validates everything. Unset uses the default
    (tip − 100).

## Snapshot bootstrap

**utxo_bootstrap** = *bool*
:   Bootstrap UTXO state by downloading a snapshot from peers instead of
    replaying from genesis. Default: **false**.

**min_snapshot_peers** = *integer*
:   Number of peers that must announce the same snapshot before it is
    trusted enough to download. Default: **2**.

**storing_snapshots** = *integer*
:   Number of recent UTXO snapshots to retain for serving to other peers.
    **0** disables snapshot serving. Default: **0**.

**snapshot_interval** = *integer*
:   Blocks between snapshot creation points. Default: **52224**
    (matches JVM mainnet).

## Mempool

**mempool_capacity** = *integer*
:   Maximum transactions retained in the mempool. Default: **1000**.

**min_fee** = *integer*
:   Minimum fee in nanoERG required to enter the mempool.
    Default: **1_000_000** (0.001 ERG).

## REST API

**api_address** = *"host:port"*
:   Bind address for the REST API. Unset defaults to **0.0.0.0:9052**
    on mainnet and **0.0.0.0:9053** on testnet.

## Fastsync

ergo-fastsync is an optional sidecar that fetches headers and blocks
from JVM peers over HTTP and pushes them into the node via the
**/ingest** endpoint. Substantially faster than P2P for cold starts.
The binary must be on **PATH**.

**fastsync** = *bool*
:   Auto-spawn ergo-fastsync at startup if the binary is available.
    Default: **true**.

**fastsync_peer** = *URL*
:   Override the auto-discovered peer URL.
    Example: **"http://213.239.193.208:9053"**. Unset uses
    **/peers/api-urls** discovery.

**fastsync_threshold_blocks** = *integer*
:   Minimum gap (peer_chain_tip − downloaded_height) that triggers
    fastsync. Below this, the node goes straight to P2P. Also passed to
    fastsync as **--handoff-distance**. Default: **25_000**.

**fastsync_peer_wait_timeout_sec** = *integer*
:   How long boot waits for the first peer **SyncInfo** before deciding
    whether to start fastsync. If no peer reports a tip in this window,
    fastsync is skipped. Default: **30**.

## Storage tuning (cold sync)

These govern the redb cache and write-transaction flush cadence during
initial sync. On a memory-constrained box, lowering **cache_mb** and
**flush_heap_threshold_mb** reduces RSS at the cost of more disk I/O.

**cache_mb** = *integer*
:   redb cache size in megabytes. Default: **256**.

**flush_heap_threshold_mb** = *integer*
:   Live-heap threshold (jemalloc.allocated, MB) above which the
    validation sweep commits the redb write transaction mid-sweep.
    **0** disables the memory trigger; flushes then degenerate to every
    **flush_max_blocks**. Default: **4096**.

**flush_max_blocks** = *integer*
:   Upper bound on blocks between flushes. Caps crash-recovery work.
    Default: **100**.

**flush_min_blocks** = *integer*
:   Lower bound on blocks between flushes. Prevents storm-flushing when
    heap growth is driven by something other than the redb write tx.
    Default: **5**.

## Storage tuning (at tip)

Each of the four **synced_\*** keys mirrors the corresponding cold-sync
key but takes effect once chain sync reaches tip. The transition swaps
the flush settings live and reopens **state.redb** with
**synced_cache_mb**. Each is unset by default — leaving the cold-sync
value in effect.

**synced_cache_mb** = *integer*
:   redb cache size at tip. Smaller = lower steady-state RSS; tradeoff
    is more disk reads when cold-restarting at tip.

**synced_flush_heap_threshold_mb** = *integer*
:   Live-heap flush threshold at tip. Per-block updates are tiny at tip
    cadence, so a much lower threshold is essentially free.

**synced_flush_max_blocks** = *integer*
:   Upper bound on blocks between flushes at tip.

**synced_flush_min_blocks** = *integer*
:   Lower bound on blocks between flushes at tip.

# [node.mining] (optional)

Configure block-candidate generation. Setting **miner_pk** to a non-empty
hex string enables the **/mining/\*** endpoints; leaving it empty
disables mining entirely.

**miner_pk** = *hex string*
:   Miner public key as a 33-byte compressed EC point in hex. Empty =
    mining disabled. Default: empty.

**votes** = *6-character hex string*
:   Soft-fork voting preferences. Three bytes encoded as hex.
    **"000000"** votes for nothing. Default: **"000000"**.

**reward_delay** = *integer*
:   Maturity delay in blocks before mined ERG can be spent.
    Default: **720** (matches mainnet).

**candidate_ttl_secs** = *integer*
:   Maximum candidate lifetime before forced regeneration.
    Default: **15**.

# EXAMPLE

A minimal mainnet configuration sufficient to validate the chain:

```toml
[proxy]
network = "mainnet"

[listen.ipv6]
address = "[::]:9030"
mode = "full"
max_inbound = 20

[outbound]
min_peers = 3
max_peers = 10
seed_peers = ["213.239.193.208:9020", "176.9.15.237:9020"]

[identity]
agent_name = "ergo-node-rust"
peer_name = "my-ergo-node"
protocol_version = "6.0.3"

[node]
data_dir = "/var/lib/ergo-node/data"
```

# FILES

**/etc/ergo-node/ergo.toml**
:   Default config file path used by the systemd unit.

**/var/lib/ergo-node/data/state.redb**
:   UTXO state and AVL+ tree (when **state_type = "utxo"**).

**/var/lib/ergo-node/data/modifiers.redb**
:   Headers, block sections, and chain index.

# SEE ALSO

**ergo-node-rust**(8), **sharpen**(8)

The JVM reference node's *application.conf* is the canonical model for
the **[network]** parameters and several **[node]** keys. See the v6.0.3
sources at <https://github.com/ergoplatform/ergo>.
