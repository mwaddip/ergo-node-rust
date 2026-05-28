# Changelog

## v0.6.7 — 2026-05-28

Stability fixes for the REST API under concurrent load and a
peer-filtering correctness fix, both surfaced by running the
external validation harness against the live mainnet validator.

### API: `/blocks/{id}/transactions` no longer starves the reactor

Under concurrent load on fat blocks (the ~900k–1M region has
transactions whose deserialization dominates), this endpoint ran
its CPU-bound parse + serialize **synchronously on the async
worker threads**. The validation harness — fetching a block's
transactions once per box, 64-wide, with retries — saturated all
async workers, so requests queued past the client timeout, the
retries piled into a thundering-herd, and the node fell behind on
consensus block application (multi-minute gaps) while burning
~29 cores in gridlock.

Fix: the parse + serialize now run on the blocking thread pool
(`spawn_blocking`), keeping the async reactor free for
consensus-critical P2P work. Request latency stays bounded under
concurrency, so the retry storm can't form. Single-request wall
time is unchanged (~0.5s for a fat block) — the floor is
sigma-rust's `Transaction` deserialization (~76% of the time),
left as a separate future optimization. Response byte-shape is
unchanged.

### P2P: bogus-address filtering no longer penalizes the gossiper

The v0.5.3 address-sanity filter permanent-banned any peer that
gossiped a peer list containing a CGNAT/RFC1918/link-local
address. On a NAT'd network that's normal behavior, not
misbehavior — the filter was blacklisting legitimate mainnet seed
nodes. Now matches JVM 6.0.3: bogus addresses are filtered out of
intake but the gossiper is not penalized. Filtering is gated
behind the new `[network].filter_bogus_addresses` (default
`true`); set `false` to ingest every syntactically-valid address.
The malformed-`Peers` ban (a genuine protocol violation) is
unchanged.

### Packaging / docs

- `deploy/fail2ban/ergo-node.conf`: retired the now-dead
  `address_sanity` penalty kind from the failregex (five
  permanent-ban kinds, not six).
- Operations manual: added a non-interactive-upgrade section
  (`Dpkg::Options::="--force-confold"`) covering the conffile
  prompt that aborts scripted `apt install`.

## v0.6.6 — 2026-05-27

JVM-compatibility fixes across four REST endpoints surfaced by
the v0.6.5 OpenAPI conversion, plus a fix for the indexer's
mid-sync reorg-detection blind spot.

### REST API: JVM-compatibility fixes

Four endpoints documented in `facts/openapi.yaml` as
"Rust-specific deviations" during the v0.6.5 conversion are now
aligned with JVM:

- **`/peers/connected`** returns an array of peer objects rather
  than a `{connectedPeers: N}` count wrapper. The wrapper shape
  broke any consumer iterating `.length` or expecting the
  per-peer detail JVM provides.
- **`/peers/api-urls`** verified as a Rust-only endpoint with no
  JVM counterpart; its host-IP filter (peers whose advertised
  api-url host doesn't match their connection IP are dropped) is
  kept as intentional anti-spoofing and documented in the openapi.
- **`/mining/rewardAddress`** returns a Base58-encoded P2S
  address rather than raw 33-byte EC public key hex. Notable:
  JVM uses `Pay2SAddress(rewardOutputScript(delay, pk))` rather
  than `P2PKAddress`; the script encodes the reward maturity
  delay (default 720 blocks).
- **Error response shape** unified across the API surface.
  `/blocks/{id}/validation-fragments` and `/debug/p2p-capture/*`
  now emit errors as `{error, reason, detail}` matching every
  other endpoint, instead of their custom `{errorCode, ...}`
  shape.

### Indexer: mid-sync reorg detection

The pre-fix `check_canonical_or_rollback` was a no-op during the
inner sync loop — it checked the *target* height, which is always
empty pre-insert. Reorgs that fired between block inserts went
undetected until the indexer crashed with a
`UNIQUE constraint failed: transactions.tx_id` violation
(witnessed on the laptop validator at mainnet height 1794422,
where canonical block re-included transactions from a one-block
orphan the indexer had already indexed).

Fix: parent-linkage verification on every block. The indexer
compares `target.parent_id` (from the fetched header) against its
stored `header_id` at `last_indexed`. Mismatch triggers walk-back
via canonical-id comparison to find the fork point, rolls back
the DB, and resumes from there. Operators see the recovery as a
`WARN parent linkage mismatch — rolled back` log line; no manual
intervention required.

Catches both single-block reorgs (the common case) and multi-block
reorgs (walk-back handles arbitrary depth). One DB lookup per
indexed block — no extra HTTP cost.

### CI: release notes sourced from tag annotation

`actions/checkout@v4` on a tag-push trigger sometimes leaves the
checked-out ref as a lightweight tag, and
`git tag -l --format='%(contents)'` then falls back to the
underlying commit message. v0.6.5's release body initially showed
the commit message instead of the tag annotation; fixed at the
workflow level via an explicit `git fetch --tags --force` before
extraction.

### Documentation

- `facts/openapi.yaml` updated alongside each API fix; the
  "Rust-specific deviation" notes for the four fixed endpoints
  are removed.
- `facts/indexer.md` "Known gap: reorg handling" section
  rewritten to describe the new behavior; stability table updated
  from "Future minor" to "Shipped".
- `CLAUDE.md` clarified — `facts/` belongs to main session's
  edit surface, not the per-crate dispatch list.

## v0.6.5 — 2026-05-26

Tarball install path is now first-class. The binary auto-finds a
config across pwd, `~/.config/ergo-node/`, and `/etc/ergo-node/`,
and cold-bootstraps a default `./ergo.toml` + `./ergo-node-data/`
when none is found. Interactive `install.sh` scripts ship in both
the node and indexer tarballs. The indexer tarball also now
includes the `ergo-indexer-migratedb` binary, which was missing
from v0.6.4.

### Config search + cold bootstrap

`./ergo-node-rust` with no arguments now searches for a config in
this order, first match wins:

1. `./ergo.toml` (current working directory)
2. `~/.config/ergo-node/ergo.toml` (user-scoped)
3. `/etc/ergo-node/ergo.toml` (system-wide, via .deb)

An explicit positional path overrides the search. When nothing is
found, the binary writes a minimal default `./ergo.toml` (testnet,
full archival, IPv6 listener), logs a `tracing::warn!` pointing
operators at `install.sh` for customization, and starts. The
compiled-in `data_dir` default moves from `/var/lib/ergo-node/data`
to `./ergo-node-data` — the .deb's new `deploy/ergo.toml` now sets
the system path explicitly. Existing .deb operators get the usual
dpkg conffile prompt on upgrade; their `data_dir` customization is
preserved.

### Interactive `install.sh` (node + indexer)

Both tarballs ship a bash script that asks a handful of questions
(network, state type, storage path, memory limits, API bind for
the node; backend, DB URL, API bind for the indexer) and writes a
working `./ergo.toml` or `./indexer.toml`. Refuses to overwrite an
existing config without confirmation.

### Annotated example configs

`ergo.toml.example` (272 lines) documents every supported node
option with its default commented in — `[node]` (30 fields),
`[node.mining]` (4), `[network]` (8), `[upnp]`, `[stats]`,
`[debug.p2p_capture]`. The .deb ships it at
`/usr/share/doc/ergo-node-rust/examples/`. The indexer's
`ergo-indexer.toml.example` covers the 4-field schema with SQLite
and PostgreSQL examples.

### Indexer packaging: migrator binary included

The `ergo-indexer-migratedb` binary added in v0.6.4 is now
included in both the tarball and the .deb. (`build-addons-tar`
and `build-addons-deb` were both missing it.) The .deb's stale
"Rich query API over SQLite" description is also updated to
acknowledge PostgreSQL support that landed in indexer v0.2.0.

## v0.6.4 — 2026-05-24

Storage pruning, two new endpoints for external validation
harnesses, and a sigma-rust pin bump.

### `blocks_to_keep` storage pruning

The `blocks_to_keep` config setting now honors its name. Default
`-1` skips pruning (full archive, unchanged behavior). With a
non-negative value, sync prunes non-header block sections (102
BlockTransactions, 104 ADProofs, 108 Extension) older than the
retention horizon at flush time. Headers (101) are never pruned.
The flush dial's min/max guardrails cap at `blocks_to_keep` so the
`validated_height → tip` gap can never exceed what archived bodies
cover, making crash recovery safe at any retention setting.

### New endpoint: `GET /blocks/{id}/validation-fragments`

Per-block canonical-byte view returning `headerBytes`,
`parameters` (from Extension), and per-tx `signingMessage`
(`Transaction::bytes_to_sign()`). Designed for external tools that
need byte-exact equivalence with the node's internal representation
— e.g. cross-validating an independent serializer against the
reference. Stateless w.r.t. UTXO state; serves any block at any
node uptime.

### New indexer endpoint: `GET /api/v1/boxes/{box_id}/bytes`

Returns canonical `ErgoBox::sigma_serialize_bytes` for any indexed
box, spent or unspent. Hash-verified (`blake2b256(bytes) ==
box_id`) before serving. Counterpart to the node endpoint above —
together they let an external harness reproduce per-block
validation client-side, with the indexer owning historical box
data and the node owning current chain state.

### Indexer service: lenient restart policy

Systemd unit changed from `Restart=always` to
`Restart=on-failure` with `StartLimitBurst=3` /
`StartLimitIntervalSec=60`. Deterministic failures (e.g. SQLite
constraint violations from reorg-handling debt) now stop after 3
attempts in 60s instead of looping forever and filling the
journal.

### sigma-rust pin bump

`3aa0832f` → `fbcdc9bd`. Picks up a fix for a latent panic in
`wrap_spanned_with_src` when `reduce_to_crypto` produces a
non-Spanned `EvalError` variant. Production-safe (parse-time
type-checking prevents the triggering variants from real scripts),
but fuzzers and external reduce-helper code could previously hit
it.

## v0.6.3 — 2026-05-18

Graceful shutdown — `systemctl stop` now persists in-memory state
before the process exits, instead of relying on the v0.6.2 cross-DB
durability handshake to re-validate the gap on every restart.

### Shutdown-flush via explicit oneshot signal

Previous shutdown was fire-and-forget: drop the P2P node, sleep 500ms,
exit. Sync's `run()` had no flush on its exit paths and no way to know
the host was shutting down. `Durability::None` commits accumulated
since the last sweep flush were lost on exit; the cross-DB handshake's
`regressed` reconciliation branch caught it on next start and
re-validated forward, but the recovery cost real CPU on every restart.

The fix is an explicit `tokio::sync::oneshot::channel::<()>` created
in `src/main.rs` and passed into `HeaderSync::new`. The SIGTERM
handler sends `()` on the sender; sync's `run()` wraps `run_inner()`
in `tokio::select!` against the receiver, falling through to a new
`shutdown_flush()` that runs the same three-step flush as the
per-sweep flush trigger (`validator.flush()` →
`store.set_validated_height(M)` → `store.flush()`). Main awaits the
sync task's `JoinHandle` with a 30s bounded timeout (previously a
blind 500ms sleep with no JoinHandle at all).

An earlier design pass tried to use event-stream-closure as the
implicit shutdown signal — drop the P2P node, let sync's
`next_event().await` return `None`, exit. That doesn't work:
`P2pTransport` holds an `Arc<P2pNode>` and the host clones that Arc
to mining, mempool, REST API, and the snapshot / nipopow serve paths.
Dropping main's reference releases one of many — the node stays
alive, its event-emitting tasks stay alive, and sync hangs in
`next_event().await` until systemd SIGKILLs at `TimeoutStopSec`. The
oneshot is the only deterministic signal.

Live measurement on the laptop validator: 32 ms from SIGTERM to
`Deactivated successfully`, with the full flush sequence visible in
the journal (`shutdown signal received`, `header sync exiting —
flushing state`, `header sync stopped`, `sync task exited cleanly`).
No state regression across the cycle. The cross-DB handshake's
`regressed` branch becomes unreachable for clean shutdowns; it
remains as defense for `kill -9`, OOM, and hardware faults.

Implementation note: a direct `tokio::select!` with `_ = &mut
self.shutdown_rx` fails the borrow checker (E0499 — two mutable
borrows of `self` across method-call and field-access arms). The
compiling pattern moves the receiver out via `std::mem::replace` and
keeps a sentinel sender alive for the duration of `run()`. Documented
in `facts/sync.md` § "Graceful shutdown".

### Clippy cleanup in `src/`

`src/pipeline.rs` accumulated five copies of two complex tuple types
during the chain-reorg and put-batch work. Factored into two type
aliases at the top of the file:

```rust
type StoreEntry = (u8, [u8; 32], u32, Vec<u8>, Option<Vec<u8>>);
type ForkBranch = (u32, Vec<(Header, Vec<u8>)>);
```

`src/main.rs::ValidatorInner` carries a 340-byte size difference
between its two variants (UTXO mode's persistent prover vs digest
mode's stateless verifier). With exactly one validator per process,
boxing the larger variant would save ~330 bytes once at the cost of a
heap indirection on every `apply_state` / `flush`. Not worth it;
`#[allow(clippy::large_enum_variant)]` with a comment explaining the
trade-off.

The mining hot path's `proofs_for_transactions(&[emission_tx.clone()])`
became `proofs_for_transactions(std::slice::from_ref(&emission_tx))`
— same semantics, one fewer clone per mined-block candidate.

### State test-suite clippy

`state/tests/storage_tests.rs` had three `hasher.update(&[byte])`
sites flagged for `needless_borrows_for_generic_args`. Dropped the
leading `&` on each. No behavioral change.

## v0.6.2 — 2026-05-18

Three independent improvements that share a single release. Two
operator-facing bug fixes plus a defense-in-depth measure for crash
recovery across the state and modifier-store databases.

### Postinst `try-restart` on upgrade

`debian/ergo-node-rust.postinst` previously used `systemctl start` on
upgrades, which is a no-op when the service is already running — so
every `.deb` upgrade left the old (deleted) binary executing in
memory. Operators had to manually `systemctl restart` to pick up the
new binary, and most didn't notice the gap.

Switched to `try-restart`, which restarts the service only if it was
already active (respecting operator-stopped state) and replaces the
running binary with the upgraded one. Fresh installs are unaffected
— the postinst still falls back to `start` when the service has no
prior active state. Commit `f068010`.

### Stuck `apply_state` retry-loop detection

@odiseusme reported in #10 that a node could deterministically retry
the same block forever after state-DB corruption from an unclean
shutdown, with no operator-visible signal beyond the steady per-sweep
`ERROR apply_state failed` lines. The Doctor adapter and a casual
operator both have no obvious "this node is wedged" indicator.

Added a per-`(height, error_kind)` consecutive-failure tracker in
`sync/`. After 5 consecutive failed `apply_state` calls on the same
(height, kind), emits the contract event:

```
WARN validation stuck height=<H> attempts=<N> error_kind=<kind>
```

`error_kind=missing_key` carries the additional `missing_key=<hex>`
field for cases where the prover hit a `LabelOnly` placeholder during
traversal. Journal-events contract bumped to v1.1.0 (additive). The
detection is observation-only — it doesn't auto-recover, just makes
the retry loop visible.  Commit `2e800a7`.

### Cross-DB durability handshake

`state.redb` (UTXO state) and `modifiers.redb` (chain index +
sections) are independent redb databases. Each commits atomically per
transaction, but the two are flushed independently. The sync sweep's
flush pair (`validator.flush()` then `store.flush()`) had a race
window where an unclean shutdown between the two could leave state
durable at a height the modifier store didn't know about, and vice
versa.

This release adds a durable mirror of state's `META_BLOCK_HEIGHT` in
the modifier store's `chain_meta` table (`b"validated_height"`),
written between the two flushes. On startup the node reads both
values and reconciles drift per a four-branch policy:

- **M == V**: consistent — no-op.
- **M > V, gap ≤ `reconciliation_trust_threshold`** (default 100):
  trust state, bring V forward with one Immediate write. Normal
  flush-window race.
- **M > V, gap > threshold**: roll state back to V via
  `validator.reset_to(V, header.state_root)`, sync re-validates
  forward. Catches drift that exceeded a single inter-flush window.
- **M > V, header lookup at V fails**: forced trust (bring V forward
  + loud WARN). First-deployment migration path lands here on a node
  that has state.redb but no recorded V yet.
- **M < V**: post-reorg recovery — sync re-validates forward; no
  startup action needed.

Each non-consistent case emits the contract event
`validated_height_drift` (WARN) with `state_height`, `store_height`,
`mode` (`forward` | `rollback` | `forced_trust` | `regressed`), and
`gap` fields. Journal-events contract bumped to v1.2.0 (additive).
The Doctor adapter and operators get a stable startup signal for
cross-DB drift without parsing free-text log lines.

`keep_versions` bumped 200 → 256 to give margin above the default
threshold for the rollback branch.

#### Scope and limits

This work prevents one class of future drift — cross-DB horizon
mismatch after a flush-pair race. It does **not** fix existing
state-internal corruption such as #10 (an AVL tree missing a box
that META says should be there); the prover's `LabelOnly` resolver
miss path that produces the "Key does not exist" error is a
different failure mode, and operators hitting it still need
`sharpen(8)` or `utxo_bootstrap = true` to recover. The detection
layer for that case shipped earlier in this release as
`validation_stuck`.

#### Operator-visible

First restart after upgrading to v0.6.2 emits exactly one
`validated_height_drift mode=forced_trust gap=<state_height>` WARN at
startup — expected and benign. The `validated_height` chain_meta key
bootstraps to the current state height on first run; subsequent
restarts see `M == V` and emit nothing.

New config knob `[node] reconciliation_trust_threshold` (default
100) controls the M > V trust vs. rollback decision. Bounded above
by `state.keep_versions` (256).

#### Contracts

- `facts/store.md` — `b"validated_height"` registered in `chain_meta`
  table; durability invariant added.
- `facts/sync.md` — new "Cross-DB Durability Handshake" section with
  V1/V2 invariants, three-step flush ordering, startup reconciliation
  algorithm.
- `facts/state.md` — cross-DB durability cross-reference (state stays
  canonical; no API additions).
- `facts/journal-events.md` — `validated_height_drift` event added,
  contract version 1.1.0 → 1.2.0.

## v0.6.1 — 2026-05-16

Hotfix: v0.6.0 panics deterministically on startup against a non-empty
existing store. Operators on v0.6.0 must upgrade.

The `header chain restored` journal-event alignment in v0.6.0 changed
the `tip` field from `chain.height()` (a u32) to `chain.tip().id` (a
hex BlockId), but `chain.tip()` reads through the lazy header store —
which has no loader wired at that point in startup. `HeaderChain::restore`
only populates the score/height table, not the cache. The header
loader is wired later (further down `main`), so the intermediate
`tip` access panics with:

```
thread 'main' panicked at chain/src/chain.rs:330:14:
tip header unavailable — cache evicted with no loader wired
```

Fixed by capturing the tip BlockId directly from the entries we just
read from the store, before consuming the iterator. No detour
through the lazy header store, identical operator-visible field.

The empty-chain case (fresh install before genesis) now emits the
event with `headers=0` and no `tip` field, matching the prose contract
(the contract specifies `tip` for the non-empty case).

Reported by @odiseusme in #9. No data corruption — v0.6.0 panics
before any block work, on-disk state is untouched.

### Workaround for v0.6.0 operators

Downgrade to v0.5.3 *or* upgrade to v0.6.1.

## v0.6.0 — 2026-05-16

Ergo Node Doctor support. Stable contracts for what the node exposes
to external diagnostics tooling, plus the supporting infrastructure:
an operator stats endpoint with cumulative P2P traffic counters, a
versioned journal-event registry parsers can write against, and a
reference RRD harness. Motivated by @odiseusme's `Ergo Node Doctor`
spec (`gist f5015bd91aa1cba3213db66344313334`) — the Rust adapter for
that tool can now write its parsers against contracts, not free-text
log lines.

### Breaking changes

**Fail2ban filter regex.** The PENALTY journal line changed from

    PENALTY peer_ip=<ip> type=<class> reason="<text>"

to the contract-mandated named-field shape

    PENALTY peer=<ip> kind="<kind>" detail=...

The shipped fail2ban filter (`/etc/fail2ban/filter.d/ergo-node.conf`)
and jail (`/etc/fail2ban/jail.d/ergo-node-jail.conf`) were updated to
match. Both files are conffiles — dpkg will prompt operators who
have edited them locally. **Operators upgrading from v0.5.x must
accept the new conffile or merge the new failregex into their edits;
otherwise fail2ban silently stops banning peers.** Verify with
`fail2ban-client status ergo-node-permanent`.

The new jail layout is a single jail (one-hit-bans on the six
permanent-ban kinds) instead of the previous two-jail scoring
emulation. The two logged-only kinds (`message_parse_failed`,
`connection_limit_exceeded`) aren't auto-banned by default; the
node's in-memory blacklist handles repeat offenders. Operators who
want a second jail for those kinds can add it.

**Public crate API.** Three signatures gained parameters as the
traffic-counter wiring landed:

- `enr_p2p::routing::router::Router::new` and `Router::with_peer_db`
  take a `&Arc<TrafficCounters>`.
- `enr_p2p::transport::connection::Connection::outbound` takes a
  `&Arc<TrafficCounters>`.
- `ergo_api::serve` takes two new optional parameters,
  `Option<ergo_api::stats::StatsConfig>` and
  `Option<Arc<dyn ergo_api::stats::P2pCountersSource>>`. Pass
  `(None, None)` to preserve the old behavior with no stats listener.

In-tree consumers are updated. External consumers depending on these
crates as libraries will need a one-line adaptation.

**Log markers.** Some operator-relevant tracing emissions were
aligned with the journal-events contract:

- `header chain restored`: `tip` field is now a hex BlockId, not a
  u32 height. Field name unchanged.
- `deep reorg succeeded`: `old_tip` / `new_tip` are hex BlockIds, not
  u32 heights. Names unchanged.
- `VALIDATION SWEEP STARTED` / `VALIDATION SWEEP COMPLETE`: the `===`
  decoration was stripped from the markers; fields are now u64 (were
  u32). Log-grepping scripts using a regex anchored on the `===`
  decoration need to drop the surrounding equals signs.
- `opening UTXO state storage`: the parenthetical free-text suffix
  was trimmed; the `path` and `cache_mb` fields are unchanged.
- `state_storage_open_complete` (`UTXO state storage opened`) is now
  emitted by `state` from inside `RedbAVLStorage::open`, not by the
  main crate after the open call. The marker text is the same; only
  the source module differs.

**`/info` adds two new fields** (additive — strict-schema validators
may notice): `journalEventsVersion` (always present, currently `"1.0"`)
and `statsVersion` (present only when the `[stats]` section is
configured).

### New features

**Operator stats endpoint.** Optional `[stats]` section in the config
binds a loopback-only HTTP listener (default `127.0.0.1:9055`) on a
separate port from the public REST API. `GET /stats/p2p` returns
cumulative P2P traffic counters keyed by `(message code, modifier
type, direction)` — 7 logical buckets (headers, blocks, transactions,
peer-discovery, sync-info, snapshot, control) × 4 series each (in
count/bytes, out count/bytes). Schema in `facts/stats.md`; designed
for RRD `COUNTER`-style consumption, Prometheus exporters, or ad-hoc
`curl`.

The listener doesn't start unless the section is present in the
config. The shipped `/etc/ergo-node/ergo.toml` ships with a
commented-out template; the operator opts in by uncommenting it.

**Traffic counters.** Lock-free atomic counters at the
transport↔protocol boundary in `enr-p2p`. Counters are exposed via
`Router::traffic_snapshot()` and `P2pNode::traffic_snapshot()`,
returning a plain-data snapshot suitable for adapter wiring. Each
message direction is counted once per frame; bytes include the
13-byte framing header so operators graph link utilization rather
than payload alone.

**Journal-event contract.** `facts/journal-events.md` v1.0 names a
stable set of structured tracing events (startup phases, validation
sweeps, reorgs, peer lifecycle, peer penalties, mining-block-found,
etc.) with marker prefixes, field schemas, stability levels, and a
contract version. The contract version is advertised via
`/info::journalEventsVersion` so downstream log parsers can detect
contract drift. Events outside the contract are internal and may
still move freely.

**RRD harness scripts** in `tools/`:

- `rrd-create.sh` — one-shot creation of `chain.rrd` (GAUGE) and
  `p2p.rrd` (28 COUNTER DSes).
- `rrd-demo-fill.sh` — backfills synthetic data for offline graph
  demos (Python).
- `rrd-update.sh` — production cron updater. Polls `/stats/p2p` +
  `/info`, computes bucket sums via `jq`, calls `rrdupdate`.
- `rrd-graph.sh` — renders four example PNGs (sync-height,
  sync-rate, traffic-count, traffic-bytes).

Packaged as examples under
`/usr/share/doc/ergo-node-rust/examples/`; the `.deb` lists
`rrdtool` in `Suggests:`. Operator-editable, not invoked by the
systemd unit.

### Internal changes

- `peer_penalised` `kind` vocabulary now documented in
  `p2p/src/blacklist.rs`; six permanent-ban kinds, two logged-only
  kinds. New emit sites must reuse existing kinds rather than invent
  new ones; if a new category is genuinely needed, the contract,
  the fail2ban filter, and the man page all need updating in
  lockstep.
- `tracing-test` adopted as a dev-dependency in `state/`, `mining/`
  for journal-event capture tests with the `no-env-filter` feature
  (the macro's default `EnvFilter` filters out emissions from
  external crates — surprisingly easy to miss).

## v0.5.3 — 2026-05-15

Network-aware address sanity filter on `Peers` gossip. Observed on
the laptop: a gossiped `169.254.0.2:9030` (IPv4 link-local) sat in
the PeerDb and the outbound fill phase kept timing out trying to
dial it every ~40 seconds. No legitimate Ergo peer has a link-local
address; some upstream peer is shipping junk in its `Peers` body
and there's no enforcement layer between the wire and our DB.

The new `protocol::address_sanity::is_bogus_address(addr, network)`
classifier splits unroutable addresses into two sets:

**Always-bogus** (any network): loopback (127/8, ::1), link-local
(169.254/16, fe80::/10), multicast (224/4, ff00::/8), broadcast
(255.255.255.255), unspecified (0.0.0.0, ::), benchmark (198.18/15),
reserved Class E (240/4), IPv4-mapped IPv6 (::ffff:0:0/96).

**Mainnet-only-bogus**: RFC 1918 private (10/8, 172.16/12,
192.168/16), CGN (100.64/10), unique-local IPv6 (fc00::/7), and
documentation ranges (192.0.2/24, 198.51.100/24, 203.0.113/24,
2001:db8::/32). On testnet these are legitimate (a developer running
a testnet inside a LAN).

Wiring:

1. **Peers ingest**: bogus entries are dropped silently; if any
   entry in the body is bogus, the source peer earns a permanent
   ban (same shape as the existing malformed-Peers ban — fail2ban
   picks it up). Legitimate entries in the same body are still
   recorded.
2. **GetPeers response selection**: bogus entries are filtered out
   defensively before serialization. Even if a legacy or future
   bug leaves a bogus row in our PeerDb, we never relay it.
3. **Outbound fill candidate selection**: same filter applied
   before dialing. Pre-v0.5.3 PeerDb rows persisted on disk are
   covered without a forced migration.

`Network` (mainnet vs testnet) is threaded through the router and
the background context so each call site picks the right
classification.

### Side change

The router's constructor signature gained a `network: Network`
parameter. API breaking only for direct consumers of `Router::new`
/ `Router::with_peer_db`; the in-tree `P2pNode::start` was updated
in the same dispatch.

## v0.5.2 — 2026-05-14

Self-loop fix. With v0.5.1, the outbound manager's fill phase
would occasionally dial our own declared address — a JVM peer
gossips us back to ourselves in a `Peers` message, the candidate
selection sees a recently-seen entry, and we dial it. The
listener accepts, the handshake completes, and we end up with one
outbound + one inbound peer that are both ourselves. Harmless,
but wastes two peer slots and pollutes `/peers/all`.

`PeerDb::new` now takes a `self_addresses: HashSet<SocketAddr>`
assembled by `P2pNode::start` from every listener's declared
address (post-UPnP, post-IPv6-auto-detect). `record()` drops
entries whose address is in that set; `load_all` filters them out
of the in-memory population at startup. Persisted self entries on
disk are NOT deleted — a self-address today (current IPv6 prefix)
may legitimately be a different host tomorrow, so the disk row
stays viable.

The outbound side is the only mechanism we needed to plug — once
gossiped self-entries never enter the in-memory DB, candidate
selection never sees them, no self-dial happens, no self-loop is
formed. No inbound-side handshake rejection needed.

## v0.5.1 — 2026-05-14

Real peer discovery. v0.4.x and v0.5.0 had `GetPeers` and `Peers`
stubbed in `enr-p2p`'s router — we replied with empty lists and
discarded incoming peer specs. Result: the node was capped at
`seed_peers ∩ operators who accept us` (3 in practice on a laptop
operating under network-wide 3600-day bans from earlier misbehaviour).
This release lands the four working pieces:

1. **Wire-format Peers codec.** VLQ count + sequence of `PeerSpec`
   entries (capped at `max_peer_spec_objects`, default 64). Reuses
   the existing handshake `parse_peer_entry` / serializer.
2. **In-memory PeerDb.** Soft cap 1000 entries, write-through to a
   persistent backing store, blacklist-filtered, evict-oldest-by-
   `last_seen` on cap hit. Lives in `enr-p2p`; persistence is a
   trait so other backends are pluggable.
3. **Real `GetPeers` / `Peers` handlers.** GetPeers returns up to 8
   most-recently-seen non-blacklisted peers (the JVM 5.0.8
   convention — `max_peer_spec_objects / 8`); malformed Peers
   triggers a permanent ban of the sender (matches JVM
   `PeerSynchronizer.penalizeMaliciousPeer`).
4. **Outbound manager fill phase.** Above `min_peers`, dial one
   PeerDb candidate every 30s up to `max_peers`. Slow-trickle to
   avoid burst-dial spam.

Persistence lands in `enr-store` as a new `peer_db` redb table:
`put_peer` / `delete_peer` / `list_peers`, keyed by encoded
`SocketAddr`. Records are opaque to the store; the main crate's
`PeerStorageAdapter` owns the wire-irrelevant length-prefixed
codec. quick_repair covers the new table for free.

### Operator notice

**Bump `max_peers` if you want more than 10 outbound connections.**
The fill phase respects the config — a node with `min_peers=3,
max_peers=10` will now actually climb to 10 (previously stuck at
3). Operators on stable connections may want `max_peers=32` or so.
`min_peers` stays the dial-aggressively floor.

The PeerDb is empty on first start with this binary. Within ~2
minutes (the keepalive `GetPeers` cadence), peers in the seed pool
will gossip back a fresh peer list — observed on the laptop:
3 → 10 connected peers in ~3 minutes, 64 entries in the PeerDb,
zero malformed-Peers bans.

### Side change: Blacklist sync API

`enr_p2p::Blacklist` moved from `tokio::sync::Mutex` to
`std::sync::Mutex` because `PeerDb` runs inside a sync mutex and
can't await. Critical sections are `HashSet` ops — bounded in time,
no awaits. API breaking only for direct consumers of `Blacklist` —
the internal callers (`record_permanent` in `transport::frame`)
were updated in-tree.

## v0.5.0 — 2026-05-14

Crash recovery overhaul. The v0.4.x silent-loading window after an
unclean shutdown ([#6](https://github.com/mwaddip/ergo-node-rust/issues/6))
is closed: on the laptop's full-mainnet store, `kill -9` → REST API
listening went from **7m54s** to **10.6s** (most of which is
systemd's `RestartSec=10` cooldown — actual node startup work is
~440ms). Four changes layer to get there:

1. **Real cumulative scores in `HEADER_SCORES`.** v0.4.x wrote empty
   placeholders for main-chain headers (a deferred chain-crate
   migration), forcing the chain to maintain a ~105 MB in-memory
   `scores: Vec<BigUint>` safety net and to replay the entire header
   chain via `try_append` on every restart. v0.5.0 carries real
   scores in the store and retires the Vec.
2. **`HeaderChain::restore`**. New constructor that builds the chain
   in O(n) HashMap inserts from a `(height, header_id)` iterator —
   no header parsing, no PoW recheck, no difficulty recalc — instead
   of the v0.4.x backward-walk-then-replay (the ~28-min step the
   issue #6 reporter saw silently chewing CPU).
3. **`load_tips` bound.** `RedbModifierStore::new` no longer
   iterates the entire `HEIGHT_INDEX` (~5.3M entries at full
   mainnet) to find per-type tips. One backward range scan per type
   instead. Measured on a 90k-entry index: 254ms → <1ms.
4. **redb quick-repair.** Every write transaction in
   `enr-store` and `enr-state` calls `set_quick_repair(true)`. redb
   saves the allocator state per commit, so `Database::open` after
   `kill -9` skips the full-file scan that previously dominated
   recovery (5m43s for modifiers.redb, 1m57s for state.redb on the
   laptop's mainnet store — both drop to ~20ms).

Measured restart-to-API timings on the laptop (mainnet store at
height 1.78M):

|                              | v0.4.4    | v0.5.0  |
|------------------------------|-----------|---------|
| Modifier store open (cold)   | 6m21s     | 18ms    |
| Chain restore                | 28+ min   | 404ms   |
| `state.redb` open (cold)     | 3m10s     | 13ms    |
| Total kill-to-API            | 9m31s+    | 10.6s   |

### **Operator notice: one-time scores backfill migration on first start**

The v0.4.x store wrote empty placeholders to `HEADER_SCORES` for
main-chain headers. v0.5.0 needs real cumulative scores, so on the
first start with the new binary, a one-shot migration walks
`BEST_CHAIN` from height 1 and computes
`score(h) = score(h-1) + decode_compact_bits(header.n_bits)` into
`HEADER_SCORES`.

**~50 seconds on the laptop for 1.78M headers** (50 000 scores per
batched redb transaction). Progress is logged every 10 000 headers:

```
INFO scores migration: starting (one-time backfill) total=1784795
INFO scores migration: progress done=10000 total=1784795
INFO scores migration: progress done=20000 total=1784795
...
INFO scores migration: complete headers=1784795
INFO scores migration: sentinel written
```

The migration is **resumable** — killing the process mid-walk leaves
the sentinel unwritten, and the next start re-runs from height 1
(every write is idempotent). After completion, the `chain_meta`
sentinel `scores_migrated_v1` ensures the migration runs at most once.

If you ever need to force a re-run, the hidden
`--reset-scores-migration` flag clears the sentinel:

```
sudo systemctl stop ergo-node-rust
sudo -u ergo-node /usr/bin/ergo-node-rust --reset-scores-migration \
    /etc/ergo-node/ergo.toml
sudo systemctl start ergo-node-rust
```

### Added
- **`HeaderChain::restore`** constructor (`enr-chain`).
- **`ModifierStore::best_chain_entries`**, **`put_header_score`**,
  **`put_header_score_batch`**, **`chain_meta_get`**/**`put`**/**`delete`**
  (`enr-store`).
- **`--reset-scores-migration`** CLI flag in the main binary
  (operator tool; not in `--help` output, documented here).
- **Per-step `RedbModifierStore::new` timing logs** so operators see
  where open time goes (`store open: Database::create elapsed_ms=...`
  etc).

### Changed
- **`ModifierStore::put_batch` carries real scores for `type_id=101`**.
  Entry tuple grows from 4 to 5 elements with an `Option<Vec<u8>>`
  score, required for headers and `None` for everything else.
- **`put` (single) with `type_id=101` is rejected** — main-chain
  header writes must use `put_batch` so the score travels alongside
  the data atomically.
- **`install_from_nipopow_proof` returns `Vec<InstalledHeader>`**
  (`{id, height, score_be}`) so the integrator persists installed-
  suffix scores via the store. Chain no longer owns score
  persistence.
- **Chain `scores: Vec<BigUint>` retired** in favour of the
  `ScoreLoader` (~105 MB RSS reduction at full mainnet, contributing
  to the ~500 MB at-tip RSS drop observed against v0.4.4).
- **All redb writes use `set_quick_repair(true)`** in `enr-store`
  (8 write paths) and `enr-state` (6 write paths). Measured per-
  commit overhead at `Durability::None`: ~5 µs. Negligible against
  the recovery-time win.
- **`load_tips`** uses per-type backward range scan instead of full
  `HEIGHT_INDEX` iteration.

## v0.4.5 — 2026-05-13

Operator-reported packaging and startup-visibility fixes. No protocol
or validation changes.

### Fixed
- **systemd unit logged to file while fail2ban watched journald**
  ([#4](https://github.com/mwaddip/ergo-node-rust/issues/4)). The unit
  carried `StandardOutput=append:/var/log/ergo-node/ergo-node.log` and
  `StandardError=append:...` left over from the pre-fail2ban era; the
  v0.4.4 jail uses `backend=systemd` with
  `journalmatch=_SYSTEMD_UNIT=ergo-node-rust.service`, so PENALTY
  lines never reached the journal and the jail was effectively inert.
  Dropped both redirects — stdout/stderr now default to journald.

  **Upgrade note for v0.4.4 operators**: after `apt upgrade` the unit
  reload is automatic, but the daemon must be restarted to pick up the
  new logging target. The existing `/var/log/ergo-node/ergo-node.log`
  file stops growing; switch to `journalctl -u ergo-node-rust -f` for
  live tailing. If you have a systemd drop-in that already overrides
  `StandardOutput`/`StandardError`, remove it so the package default
  applies:

  ```
  sudo systemctl daemon-reload
  sudo systemctl restart ergo-node-rust
  sudo fail2ban-client reload
  journalctl -u ergo-node-rust -f
  ```

  The fail2ban jail itself is unchanged.
- **Packaged files owned by build-runner UID instead of root:root**
  ([#2](https://github.com/mwaddip/ergo-node-rust/issues/2)). The
  v0.4.4 `.deb` shipped `/usr/bin/ergo-node-rust`, `/usr/bin/sharpen`,
  config files, and the systemd unit owned by the build host's
  numeric UID (1001 on the GitHub Actions runner), surfacing as
  `UNKNOWN:UNKNOWN` on hosts without that UID. `build-deb` now passes
  `--root-owner-group` to `dpkg-deb --build`.
- **`preinst` warned about missing home directory**
  ([#3](https://github.com/mwaddip/ergo-node-rust/issues/3)). The
  script ran `adduser --home /var/lib/ergo-node` before the directory
  existed, emitting `info: The home dir /var/lib/ergo-node you
  specified can't be accessed`. Reordered: `mkdir -p` runs first.
- **Manpage example used stale `:9020` mainnet seed peers**
  ([#5](https://github.com/mwaddip/ergo-node-rust/issues/5)). The
  `ergo-node-rust.conf(5)` minimal example referenced
  `213.239.193.208:9020` and `176.9.15.237:9020`, both currently
  unreachable on that port. Refreshed with a 4-peer `:9030` subset
  from the shipped `mainnet.toml`.
- **Silent startup after unclean shutdown**
  ([#6](https://github.com/mwaddip/ergo-node-rust/issues/6)). After a
  power loss mid-sync, the daemon logged only the initial `node
  config` line for many minutes before binding the API/P2P ports —
  the header-chain restore (one redb read per stored header, walked
  backward then replayed forward) and the state.redb open are
  inherently slow on a cold cache and can run for tens of minutes on
  a partially-synced node. Added INFO logs at each boundary
  (`opening modifier store`, `restoring header chain from store
  (walk backward)`, periodic progress every 100k headers,
  `opening UTXO state storage`, etc.) so operators can distinguish a
  long-but-healthy load from a hang.

## v0.4.4 — 2026-05-03

Packaging polish: wire fail2ban via systemd journal so the shipped
jail actually catches PENALTY events; add manpages; ship the
chain-tip rollback tool.

### Added
- **Manpages**: `ergo-node-rust(8)` (daemon, signals, env, log
  format, fail2ban integration), `ergo-node-rust.conf(5)` (every
  config key with defaults and examples), `sharpen(8)` (rollback
  tool). Sources are markdown in `man/`; `man/build` regenerates the
  installed `.gz` artifacts via pandoc.
- **`sharpen` shipped in the `.deb`** at `/usr/bin/sharpen`. The
  recovery tool every operator will eventually need.

### Changed
- **`sharpen` CLI polished**: now uses clap for `--help` /
  `--version` and proper option parsing. No behavior change.
- **postinst NOTE when fail2ban absent**: now explains what PENALTY
  lines are, mentions alternative operator tooling, and prints the
  absolute filter/jail file paths so operators can wire alternative
  log-to-firewall pipelines.
- **Package description**: now describes the full-node reality
  (validates blocks, persistent UTXO state, mempool, REST API,
  mining) instead of "header-validating proxy evolving toward a
  full validating node".

### Fixed
- **fail2ban jail used non-existent log file**: jail had
  `logpath=/var/log/ergo-node/ergo-node.log` but the systemd unit
  writes to journald only. Switched to `backend=systemd` with
  `journalmatch=_SYSTEMD_UNIT=ergo-node-rust.service`.
- **fail2ban silently ignored the jail**: file was named
  `ergo-node.jail` but fail2ban only loads `.conf`/`.local`. Renamed
  to `ergo-node-jail.conf` (matches the proxy package convention).

### Removed
- **`inspect-ext` debugging binary**: one-off tool from the v0.3.1
  voting investigation. Hardcoded data dir and field ID 124, no real
  CLI. The forensic `inspect-state` tool stays in `src/bin/` for
  ad-hoc debugging but is not shipped in the `.deb`.

## v0.4.3 — 2026-05-03

At-tip steady-state corruption fixes. v0.4.x mainnet validators were
hitting "Should never reach this point" panics during apply_state after
hours of at-tip operation, caused by over-deletion in the AVL tree's
`removed_nodes()` path.

### Fixed
- **AVL prover left half-applied on apply failure**: `UtxoValidator::apply_state`
  now wraps an internal helper and rolls the prover back to the pre-block
  digest on any error. Without this, sync's retry re-entered with a dirty
  prover and surfaced a different error on a different op number, burying
  the original cause.
- **Over-deletion via `contains_recursive` LabelOnly miss** (avltree #13):
  with persistent backends, most of the in-memory tree is `LabelOnly`. When
  `removed_nodes()` walked into an unresolved subtree, contains returned
  false and the candidate digest was deleted from storage even though it
  remained reachable. Fail-safed by returning true on unresolvable
  LabelOnly.
- **Over-deletion in `update_internal` write-then-delete overlap** (state):
  refuses to delete a digest that's also in this commit's write set
  (would otherwise destroy a freshly-written content-identical node).
- **Over-deletion in `rollback()` blind inserted-label removal** (state):
  rollback no longer deletes `undo.inserted_labels` — the same digest may
  be referenced from older versions still in the chain.

### Added
- `inspect-state` binary: forensic walker for state.redb (walk by key,
  check single digest presence, full tree scan reporting dangling
  references and orphan counts).

### Diagnostic
- WARN log on every resolver miss (with digest hex and reason) to
  distinguish transient redb errors from genuinely missing digests.

### External
- ergo_avltree_rust PR #13 (`contains_recursive` LabelOnly fail-safe):
  open, awaiting upstream review. Our fork carries the fix.

## v0.4.2 — 2026-04-26

### Deploy
- jemalloc opted out of transparent huge pages via
  `_RJEM_MALLOC_CONF=thp:never` systemd drop-in.

## v0.4.1 — 2026-04-26

### Fixed
- **Snapshot bootstrap parameter mismatch**: validator built from UTXO
  snapshot now recomputes active chain parameters at the snapshot height
  before the first apply_state. Without this, the first post-bootstrap
  block failed with "epoch-boundary parameter mismatch".
- **at-tip handshake fired too early**: defer the storage reopen until
  validator is within 16 blocks of header tip, so the catch-up replay
  doesn't run on the smaller `synced_cache_mb`.
- **at-tip handshake dropped uncommitted state**: flush validator before
  drop in the at-tip handshake. The rebuilt validator was previously
  loading an older on-disk digest while reporting the higher in-memory
  height.

## v0.4.0 — 2026-04-26

At-tip memory tuning: when validator catches up to header tip, switch
to a smaller redb cache and tighter flush cadence to bound steady-state
RSS.

### Added
- **at-tip storage reopen**: on first synced() entry, reopen state.redb
  with `synced_cache_mb` (default 256 MB).
- **at-tip flush switch**: tighter `synced_flush_*` config applied at the
  same handshake.
- **fastsync addon**: imported as a workspace addon.

### Fixed
- **mining proposed_update in candidate extension**: encode active
  proposed update bytes in the candidate's extension and pass them to
  `compute_expected_parameters` — required for v6-activation candidates
  to match the canonical chain.
- **indexer reorg detection**: catch reorgs at or below indexer tip via
  outer-loop tip-canonical check, not just the per-target rollback path.
- **redb steady-state RSS**: `cache_mb` 4096 → 1024,
  `flush_heap_threshold_mb` introduced.
- **script_verified_height drift**: persist on every advance and at
  startup gap-fill (drop the every-100-blocks gate).
- **synced() phase missing delivery_data drain**: dropped Received
  notifications kept tracker entries pending forever, causing
  re-requests of already-stored modifiers (likely the source of peer
  misbehavior flags).
- **proxy-relayed Inv messages**: stop forwarding to all peers; the
  consumed-codes registry handles delivery.
- **paired modifier-store + state flush**: crash-recovery bounds now
  cover both databases.
- **info_wait long-poll early return**: now loops until requested height
  is actually exceeded.
- **voting matchParameters60 tautology**: dropped the redundant
  proposedUpdate self-check (both operands came from the same block).
- **indexer tx JSON parsing**: parse directly instead of round-tripping
  through sigma-rust Transaction.

### Changed
- **sigma-rust pin** bumped to 46e94c21 (JIT costing refactor).
- **chain/p2p/state submodules** bumped during health audit cleanup.

### CI
- Build and attach addon `.deb` and `.tar.gz` bundles on tagged
  releases.

## v0.3.1 — 2026-04-16

### Fixed
- **Voting matchParameters60 enforcement**: enforce proposedUpdate check
  at v4+ epoch boundaries (was previously skipped).
- **Voting block_proposed_update threading**: now flows into
  `compute_expected_parameters` so candidate extensions match canonical.
- **Indexer box tokens deduped**; initial node connection retries on
  failure.

## v0.3.0 — 2026-04-16

Memory + state durability sweep. Peak RSS dropped from 14.95 GB to
11.52 GB.

### Added
- **`/debug/memory` endpoint**: jemalloc + process + component memory
  breakdown.
- **Memory-aware flush dial**: validator commits gated by
  `flush_heap_threshold_mb` with min/max block guardrails.
- **Lazy header store** (chain Phase 2/3): drop in-memory `Vec<Header>`,
  load from store on demand; scores Vec retained.
- **Boot-time fastsync bootstrap** with block-request gate.
- **Validator height persisted inside state.redb** (drop the separate
  `modifiers.redb` hint).

### Fixed
- **Durable state flush + bounded startup memory**.
- **Pipeline sweep rollback + selfBoxIndex gate**; startup optimization.
- **Per-modifier INFO log spam** silenced.

### Build
- **Cargo.lock committed** for reproducible CI release builds.

## v0.2.0 — 2026-04-13

Mainnet support. Validates from genesis without a checkpoint. 271k+
blocks cross-validated against the JVM reference node.

### Added
- **Mainnet support**: genesis boxes, chain config, seed peers.
- **Pipelined validation** with deferred script eval (rayon).
- **Indexer addon** + `/info/wait` long-poll endpoint.
- **Fastsync prerequisites**: `/peers/api-urls` and `/ingest/modifiers`
  endpoints; pipeline penalty attribution threading peer_id through the
  modifier channel.
- **Peer penalty system** with fail2ban integration.
- **UPnP port mapping** + IPv6 auto-detect for declared_address.
- **Validator wrapper** delegating apply_state; split
  validate_block into apply_state + evaluate_scripts.
- **Persisted script_verified_height** + startup gap handling.

### Fixed
- **Voting parameter steps** (chain submodule).
- **NiPoPoW allocation bomb** (sigma-rust 3ca4af0b).

### Tests
- Adversarial wire-data tests for NiPoPoW parsing.

## v0.1.6 — 2026-04-09

Known issues sweep: all 7 issues from v0.1.5 release notes addressed.

### Fixed
- **API fullHeight bug**: `/info` now reports `fullHeight` (last validated
  block) separately from `headersHeight` (chain tip). Previously both showed
  the headers height, masking validator stalls during sync.
- **Stale request tracker entries**: `RequestTracker` now expires entries
  after 60 seconds. Unfulfilled requests (peer disconnect, pipeline reject)
  no longer accumulate indefinitely.
- **Unknown message forwarding waste**: P2P router no longer blindly forwards
  snapshot (76/78/80) and NiPoPoW (90/91) messages to all peers. Consumed
  codes are registered at startup; the event stream handles delivery.
- **Extension header_id discarding**: `recompute_active_parameters_from_storage`
  now validates that the embedded header_id in extension bytes matches the
  expected block at that height. Previously discarded without checking.
- **Stale p2p/facts submodule pointer**: bumped from pruned commit to main.

### Changed
- **Multi-peer NiPoPoW comparison** (KMZ17 sect 4.3): light-client bootstrap now
  broadcasts `GetNipopowProof` to all outbound peers, collects valid proofs
  within a 30s window, and selects the best via `NipopowProof::is_better_than`.
  Previously accepted the first valid proof from a single peer.

### External
- sigma-rust PR #852 (`has_valid_connections` lookback fix): still open,
  awaiting upstream review. Our fork carries the fix.

## v0.1.5 — 2026-04-09

Codebase health audit + JIT costing. See SESSION_CONTEXT.md for details.

## v0.1.1 — 2026-04-09

`--version` flag, standalone testnet config.

## v0.1.0 — Initial release

P2P networking, header chain validation, block validation (digest + UTXO),
UTXO state management, chain sync, UTXO snapshot sync, mempool, mining,
soft-fork voting, NiPoPoW serve/verify.
