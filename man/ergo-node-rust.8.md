% ERGO-NODE-RUST(8) ergo-node-rust | System Administration Manual

# NAME

**ergo-node-rust** - Ergo blockchain full node

# SYNOPSIS

**ergo-node-rust** \[*CONFIG*\]

**ergo-node-rust** **-V** | **--version**

# DESCRIPTION

**ergo-node-rust** is a full Ergo blockchain node written in Rust. It
validates the header chain, validates blocks against the consensus rules,
maintains the UTXO set as an authenticated AVL+ tree, runs a mempool,
exposes a REST API, and supports mining via the standard candidate
endpoints.

The single positional argument is the path to a TOML configuration file.
If omitted, the daemon looks for **ergo.toml** in the current working
directory. Configuration syntax is documented in
**ergo-node-rust.conf**(5). The systemd unit installed by the Debian
package invokes the daemon with **/etc/ergo-node/ergo.toml**.

# OPTIONS

**-V**, **--version**
:   Print version and exit.

# ENVIRONMENT

**RUST_LOG**
:   Log filter directive (compiled with **tracing-subscriber**'s
    **EnvFilter** syntax). Examples:

    > **info** — default if unset.
    >
    > **debug,enr_p2p=trace** — global debug, P2P trace.
    >
    > **info,ergo_sync::download=trace** — info plus deep sync detail.

**\_RJEM\_MALLOC\_CONF**
:   jemalloc tuning string. The daemon's default allocator is
    tikv-jemallocator, which mangles symbol names — the standard
    **MALLOC_CONF** is silently ignored. Use **\_RJEM\_MALLOC\_CONF**.

    The systemd unit ships **thp:never** to opt jemalloc out of
    Transparent Huge Pages. With kernel **THP=always**, jemalloc cannot
    return memory to the OS via **MADV_DONTNEED** because partial THP
    regions cannot be reclaimed without splitting; RSS grows over time
    even when allocated stays flat. **thp:never** asks jemalloc to
    **MADV_NOHUGEPAGE** its arenas and restores granular reclaim.

    For heap profiling, build with the **jemalloc-prof** feature and set
    **prof:true,prof_prefix:/var/log/ergo-node/jeprof,...**. See the
    jemalloc(3) MALLCTL NAMESPACE for the full grammar.

# SIGNALS

**SIGINT**, **SIGTERM**
:   Initiate graceful shutdown. The P2P layer is dropped first, which
    ends event streams and triggers task shutdown. The sync task runs
    its end-of-sweep flush before exiting; any in-flight write
    transaction commits or rolls back cleanly. After a brief grace
    period the daemon exits.

    Killing the daemon with **SIGKILL** (or via OOM) skips the flush.
    redb is crash-safe — the database opens cleanly on next start —
    but blocks validated since the last flush will be re-validated.

# REST API

The node exposes a JVM-compatible REST API on **0.0.0.0:9052** (mainnet)
or **0.0.0.0:9053** (testnet) by default. Override via **api_address**
in the config.

The endpoint set largely mirrors the JVM reference node's documented
API. Two endpoints worth highlighting:

**GET /info**
:   Standard node-status JSON: chain height, peer count, network,
    software version, mempool size.

**GET /debug/memory**
:   Process and per-component memory breakdown. Includes
    **jemalloc.allocated**, **resident**, and **retained** values along
    with redb cache occupancy and the AVL prover's heap footprint.
    Useful when tuning the **flush_*** keys in **ergo-node-rust.conf**(5).

For the full endpoint catalog, the JVM reference documentation at
<https://api.ergoplatform.com> is the canonical source. Endpoints we
intentionally do not implement (integrated wallet, **/utils/\***) are
documented in the project README.

# LOG FORMAT

The daemon emits structured **tracing** events to standard output. Each
line is timestamped, level-tagged, and includes the originating module.

Peer misbehavior is logged in a single greppable format consumed by the
shipped fail2ban filter:

```
PENALTY peer_ip=<ip> type=<class> reason="<text>"
```

where **\<class\>** is one of:

**permanent**
:   Wire-level attack or unrecoverable protocol violation. The peer
    cannot be trusted to send valid frames; one hit is enough to ban.

**misbehavior**
:   Recoverable misbehavior worth penalizing — e.g. a header that
    fails to parse, a duplicate handshake. Accumulates 10 points per
    hit, banned at 500.

**spam**
:   Unrequested or duplicate modifiers. 25 points per hit, banned at
    500.

**nondelivery**
:   Failure to deliver a requested modifier within the timeout. 2
    points per hit, banned at 500.

The point thresholds match the JVM reference node's scoring.

# FAIL2BAN INTEGRATION

The Debian package installs a fail2ban filter and jail under
**/etc/fail2ban/filter.d/ergo-node.conf** and
**/etc/fail2ban/jail.d/ergo-node-jail.conf**. Both files are listed
as conffiles, so dpkg preserves operator edits across upgrades.

The jail reads from the systemd journal
(**backend=systemd**, **journalmatch=_SYSTEMD_UNIT=ergo-node-rust.service**),
so no log file plumbing is needed. Two sub-jails are defined:

**ergo-node-permanent**
:   **maxretry=1**, **bantime=86400**. Any **type=permanent** PENALTY
    bans the peer for 24 hours.

**ergo-node-misbehavior**
:   **maxretry=50**, **findtime=600**, **bantime=3600**. Approximates
    the JVM 500-point threshold (50 × 10 points/misbehavior). Catches
    **misbehavior**, **spam**, and **nondelivery**.

fail2ban is recommended (Suggests:) but not required. If fail2ban is
not installed at package configuration time, the postinst prints a
notice with the file paths and instructions to enable later. Verify
the jails are loaded:

```
sudo fail2ban-client status
sudo fail2ban-client status ergo-node-permanent
sudo fail2ban-client status ergo-node-misbehavior
```

To tune thresholds, edit the jail file directly and reload:

```
sudo fail2ban-client reload
```

Testnet operators may want to raise **maxretry** substantially —
testnet peer behavior is noisier than mainnet by design.

# OPERATIONAL TASKS

**sharpen**(8)
:   Roll the chain back to a target height. Used to recover from
    corrupt state.redb without re-syncing from genesis.

# FILES

**/etc/ergo-node/ergo.toml**
:   Configuration file. See **ergo-node-rust.conf**(5).

**/var/lib/ergo-node/data/**
:   Default data directory.

**/var/lib/ergo-node/data/state.redb**
:   UTXO state and AVL+ tree.

**/var/lib/ergo-node/data/modifiers.redb**
:   Headers, block sections, chain index.

**/var/log/ergo-node/ergo-node.log**
:   stdout/stderr captured by the systemd unit. Operators are
    encouraged to read from journald instead
    (**journalctl -u ergo-node-rust**).

**/etc/fail2ban/filter.d/ergo-node.conf**, **/etc/fail2ban/jail.d/ergo-node-jail.conf**
:   fail2ban filter and jail definitions.

**/usr/lib/systemd/system/ergo-node-rust.service**
:   systemd unit.

# SEE ALSO

**ergo-node-rust.conf**(5), **sharpen**(8), **fail2ban**(8),
**fail2ban-client**(1), **journalctl**(1)

The project repository is at
<https://github.com/mwaddip/ergo-node-rust>. The JVM reference node is
at <https://github.com/ergoplatform/ergo>.
