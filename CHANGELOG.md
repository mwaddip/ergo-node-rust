# Changelog

## v0.5.0 — 2026-05-14

Fast restart on partially-synced chains. Closes the v0.4.x crash-recovery
silent-loading window from minutes to seconds by completing a deferred
storage migration: header cumulative scores now live in the store
(`HEADER_SCORES`) as real values instead of empty placeholders. Startup
restores the chain by iterating `BEST_CHAIN` once and wiring loaders —
no header replay, no PoW recheck, no difficulty recalculation.

### **Operator notice: one-time scores backfill migration on first start**

The v0.4.x store wrote empty placeholders to `HEADER_SCORES` for
main-chain headers (a documented but deferred-to-someday migration —
see [the chain crate's `scores` field comment](https://github.com/mwaddip/ergo-node-rust/blob/v0.4.4/chain/src/chain.rs)).
v0.5.0 needs real cumulative scores there, so on the first start with
the new binary, a one-shot migration walks `BEST_CHAIN` from height 1
and computes `score(h) = score(h-1) + decode_compact_bits(header.n_bits)`
into `HEADER_SCORES`.

For a 1.76M-header full-mainnet store this is ~5-15 minutes on commodity
SSD. **Progress is logged** every 10 000 headers:

```
INFO scores migration: starting (one-time backfill, may take 5-15 min on full mainnet) total=1784690
INFO scores migration: progress done=10000 total=1784690
INFO scores migration: progress done=20000 total=1784690
...
INFO scores migration: complete headers=1784690
INFO scores migration: sentinel written
```

The migration is **resumable** — killing the process mid-walk leaves
the sentinel unwritten, and the next start re-runs from height 1
(every write is idempotent). After completion, the `chain_meta`
sentinel `scores_migrated_v1` ensures the migration runs at most once.

Subsequent restarts skip the migration and complete header chain
restore in seconds.

### Added
- **`HeaderChain::restore`** constructor (`enr-chain`). Builds a chain
  in O(n) HashMap inserts from a `(height, header_id)` iterator. No
  header parsing, no PoW, no difficulty recalc — store vouches for the
  data. `light_client_mode` is derived from `base_height > 1`.
- **`ModifierStore::best_chain_entries`**, **`put_header_score`**,
  **`chain_meta_get`**/**`chain_meta_put`** (`enr-store`). Building
  blocks for the new restore path and the scores migration.
- **`ModifierStore::put_batch` carries real scores for `type_id=101`**.
  The entry tuple grows from 4 to 5 elements with an `Option<Vec<u8>>`
  score required for headers and `None` for everything else.

### Changed
- **Startup restore replaces the v0.4.x backward-walk-then-replay path**.
  On the reporter's 804k-height test node ([#6](https://github.com/mwaddip/ergo-node-rust/issues/6))
  this was ~28 minutes of silent CPU-bound work after an unclean
  shutdown. v0.5.0 restores the same chain in seconds — single
  sequential `best_chain_entries` read + `HeaderChain::restore`.
- **The chain's `scores: Vec<BigUint>` field is retired** (~105 MB
  RSS reduction at full mainnet). Score lookups go exclusively
  through the `ScoreLoader` wired by the main crate.
- **`install_from_nipopow_proof` returns `Vec<InstalledHeader>`**
  (each `{id, height, score_be}`) so the integrator persists scores
  via the store, matching the new "store is the source of truth"
  invariant. Chain no longer owns score persistence.
- **`put` (single) with `type_id=101` is now rejected** — main-chain
  header writes must use `put_batch` so the score travels alongside
  the data atomically.

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
