# Journal Events Contract

Version: 1.6.0

Stable contract for parseable events in the node's structured log
output. Consumers (e.g. the Ergo Node Doctor adapter) write parsers
against this document; emit sites in the code MUST conform.

## Why

Free-text `tracing::info!()` strings drift with every refactor. A
downstream parser that depends on prose phrasing breaks silently the
first time someone tightens a log message. This contract names a
minimum set of events and pins each one's marker prefix, fields, and
stability. Code may emit additional log lines; only events listed here
are promised stable.

## Versioning

The node advertises this contract's version in `/info`:

```json
{ "journalEventsVersion": "1.0", ... }
```

Consumers refuse to parse on unrecognized major. Additive changes (new
events, new optional fields) are minor bumps.

## Format conventions

Each contract event is a single line emitted via `tracing::info!()`,
`tracing::warn!()`, or `tracing::error!()` with the default
human-readable subscriber. The line carries:

- A **marker prefix**: the leading string literal that identifies the
  event. Stable across versions for a given event name.
- Zero or more **fields**: `key=value` pairs appended via tracing's
  named-field syntax. Keys are snake_case ASCII. String-recorded values
  are surrounded by double quotes in the default formatter (e.g.
  `error_kind="script_eval"`); numeric and `Display`-formatted values
  are not. Parsers MUST tolerate optional surrounding quotes on any
  value.
- Optional **free-text suffix** after the marker, for human
  readability. Parsers MUST tolerate arbitrary suffix content.

Two rules follow from parsers matching on the *prefix*, both learned by
breaking them on 2026-08-12:

- **No marker may be a prefix of another marker.** A parser keyed on the
  shorter one matches both events and then fails looking for fields that only
  the shorter one carries. `"deferred eval backlog at bound …"` began with the
  whole of `"deferred eval backlog"` and did exactly this; it became
  `"eval dispatch gate engaged"`. When adding an event, check its marker
  against every existing one in both directions.
- **Markers are ASCII.** The free-text suffix may contain anything — an em dash
  there is fine and several markers have one — but the matched portion must
  survive being retyped from a report, a terminal, or a grep. The same event
  shipped with a U+2014 inside its marker while this document specified a
  hyphen, so the documented literal matched nothing.

Example emit:

```rust
tracing::info!(
    headers = restored,
    tip = %tip_id,
    "header chain restored"
);
```

Renders as:

```
2026-05-15T14:00:00Z INFO header chain restored headers=1784795 tip=0000abc...
```

Parsers identify the event by matching the marker prefix
(`"header chain restored"`) and extract fields from the trailing
`key=value` segment.

Levels:

- **INFO** — normal lifecycle and progress
- **WARN** — degraded but recoverable
- **ERROR** — stuck, broken, or imminent shutdown

The level is part of the contract for a given event. A consumer that
sees an event at an unexpected level may treat it as schema
mismatch.

## Future format

Operators or adapter authors may prefer JSON-line output from
`tracing_subscriber::fmt().json()`. The set of events and their field
names remain identical between the prose and JSON renderings; only the
on-disk shape differs. A `journal_format` knob in `[logging]` may be
added in a later minor; the contract version does not change when it
ships.

## Event registry

### Lifecycle

#### `node_starting`
- **Level:** INFO
- **Marker:** `"Ergo node starting"`
- **Fields:** `version` (string), `network` (string: `mainnet`|`testnet`)
- **Since:** 1.0
- **Stability:** stable
- **Emitted at:** very first line after subscriber init, before any I/O.

#### `node_ready`
- **Level:** INFO
- **Marker:** `"Ergo node running"`
- **Fields:** `version` (string)
- **Since:** 1.0
- **Stability:** stable
- **Emitted at:** after all subsystems have come up. Marks transition
  from startup to steady-state.

#### `shutdown_signal_received`
- **Level:** INFO
- **Marker:** `"SIGINT received"` or `"SIGTERM received"`
- **Fields:** none
- **Since:** 1.0
- **Stability:** stable

#### `node_shutting_down`
- **Level:** INFO
- **Marker:** `"Shutting down"`
- **Fields:** `reason` (string, optional)
- **Since:** 1.0
- **Stability:** stable

### Startup / recovery phases

Each phase has a `_started` and `_complete` event. Adapters compute
phase durations as the wall-clock delta. Phases are emitted in
deterministic order; a phase's `_complete` always precedes the next
phase's `_started`.

#### `scores_migration_progress`
- **Level:** INFO
- **Marker:** `"scores migration: progress"`
- **Fields:** `done` (u64), `total` (u64)
- **Since:** 1.0
- **Stability:** stable
- **Emitted:** periodically during migration. Final emission has
  `done == total`.

#### `header_chain_restore_started`
- **Level:** INFO
- **Marker:** `"restoring header chain from store"`
- **Fields:** none
- **Since:** 1.0
- **Stability:** stable

#### `header_chain_restore_complete`
- **Level:** INFO
- **Marker:** `"header chain restored"`
- **Fields:** `headers` (u64), `tip` (string: 32-byte hex)
- **Since:** 1.0
- **Stability:** stable

#### `state_storage_open_started`
- **Level:** INFO
- **Marker:** `"opening UTXO state storage"`
- **Fields:** `path` (string)
- **Since:** 1.0
- **Stability:** stable

#### `state_storage_open_complete`
- **Level:** INFO
- **Marker:** `"UTXO state storage opened"`
- **Fields:** none required; implementation may add `digest` (string).
- **Since:** 1.0
- **Stability:** stable

#### `validated_height_drift`
- **Level:** WARN
- **Marker:** `"validated_height drift detected"`
- **Fields:** `state_height` (u64), `store_height` (u64), `mode`
  (string: `forward`|`rollback`|`rollback_failed`|`forced_trust`|
  `regressed`), `gap` (u64: `|state_height - store_height|`), `error`
  (string, only with mode `rollback_failed`)
- **Since:** 1.2 (`rollback_failed` added 2026-06-12 — additive;
  consumers must tolerate unknown modes)
- **Stability:** stable
- **Emitted:** at most once at startup, after `state.redb` is opened
  and the modifier store's `chain_meta[b"validated_height"]` is read,
  when state's `META_BLOCK_HEIGHT` does not match the recorded value.
  Surfaces cross-DB drift that crossed an unclean shutdown. Modes:
  `forward` (state ran ahead, brought store forward); `rollback`
  (state ran ahead beyond trust threshold, rolled state back to store);
  `rollback_failed` (rollback attempted and FAILED — validator state
  unchanged at `state_height`, store brought forward to match: the
  forced-trust outcome with the rollback error attached; `reset_to →
  Result`, 2026-06-12); `forced_trust` (state ran ahead, rollback
  impossible — store brought forward with loud warning); `regressed`
  (state below store, sync will re-validate forward).  Absence of this
  event at startup means the two databases agreed.

#### `peerdb_initialised`
- **Level:** INFO
- **Marker:** `"PeerDb initialised"`
- **Fields:** `loaded_peers` (u64)
- **Since:** 1.0
- **Stability:** stable

#### `api_listening`
- **Level:** INFO
- **Marker:** `"REST API listening"`
- **Fields:** `bind` (string: `host:port`)
- **Since:** 1.0
- **Stability:** stable
- **Emitted:** after the public REST API has bound its socket.
  Used by the Doctor adapter as the canonical "API is up" signal.

### Validation and sync

#### `validation_sweep_started`
- **Level:** INFO
- **Marker:** `"VALIDATION SWEEP STARTED"`
- **Fields:** `from` (u64), `to` (u64)
- **Since:** 1.0
- **Stability:** stable

#### `validation_sweep_complete`
- **Level:** INFO
- **Marker:** `"VALIDATION SWEEP COMPLETE"`
- **Fields:** `from` (u64), `to` (u64), `blocks` (u64)
- **Since:** 1.0
- **Stability:** stable
- **Emitted:** once per sweep window. Adapter derives sync rate as
  `blocks / wall_delta_between_started_and_complete`.

#### `block_applied`
- **Level:** INFO
- **Marker:** `"block applied"`
- **Fields:** `height` (u64), `id` (string: 32-byte hex)
- **Since:** 1.0
- **Stability:** stable
- **Emitted:** for every block that advances the validated tip. High
  frequency during sync, low frequency at tip. Adapters may sample
  rather than process every line.

#### `validation_stuck`
- **Level:** WARN
- **Marker:** `"validation stuck"`
- **Fields:** `height` (u64), `attempts` (u64), `error_kind` (string),
  `missing_key` (string: hex, optional — present when `error_kind`
  is `missing_key`)
- **Since:** 1.1 (precondition broadened in 1.3 — see Emitted)
- **Stability:** stable
- **Emitted:** when the validated frontier (`validated_height`) fails
  to advance past the same height for `attempts >= 5` consecutive
  sweeps — the next block is failing deterministically. Covers both
  failure modes: an `apply_state` error (state-DB inconsistency such
  as `missing_key`) and a deferred script-eval rejection (the
  evaluator refusing a transaction). `error_kind` names the mode — an
  `apply_state` error kind, or `script_eval` for an eval-failure
  stall; `missing_key` is present only for the `apply_state`
  `missing_key` case. Surfaces the silent retry loop that previously
  buried a stuck frontier in INFO-level logs. The Doctor adapter
  treats this as a primary "node stuck" signal. Emitted at most once
  per height; re-emits when the height changes or after real progress
  resets the counter. **Before 1.3** this fired only on the
  `apply_state` path — a deferred eval-failure stall (the loop that
  hammered on a wrongly-rejected script) did not emit it.

#### `deferred_eval_backlog`
- **Level:** INFO
- **Marker:** `"deferred eval backlog"`
- **Fields:** `evals_in_flight` (u64: script evaluations dispatched to Rayon
  and not yet drained), `state_applied_height` (u64: the **applied tip**, read
  from the validator — see the warning below), `script_verified_height` (u64),
  `eval_lag` (u64: applied tip − script_verified_height),
  `jemalloc_allocated` (u64, **omitted entirely** when no heap probe is wired,
  same convention as `validation_stuck`'s optional `missing_key`),
  `eval_bytes_in_flight` (u64: Σ `approx_heap_bytes` of dispatched-not-drained
  evals — the accumulator the byte bound gates on; **added 1.6**)
- **Since:** 1.5 (added 2026-08-12); `eval_bytes_in_flight` since 1.6
- **Stability:** stable
- **Emitted:** at most once per 5 s during catch-up, from the sweep loop after
  the non-blocking drain and before the flush — so the count is post-drain and
  the heap reading is pre-flush. **Not emitted at chain tip**, where a
  single-block sweep drains synchronously every time and the record would be
  noise.
- **Why it exists:** the queue was unbounded until 2026-08-12. During catch-up
  the sweep-end drain never blocked, so if script verification was slower than
  state application the queue grew across sweeps without limit — which is how a
  4-thread host reached 10.62 GiB of anonymous RSS and was OOM-killed on
  2026-08-12. It is now bounded by `eval_backlog_max_mb` /
  `eval_backlog_max_blocks`; the record remains the way an operator sees depth
  approaching those bounds. (An earlier revision of this entry derived an eval
  count from a ~410 KB per-item figure that was 6–8× too low. Quote the
  anon-rss; see `facts/sync.md`.)
- ⚠ **`eval_lag` was meaningless before 1.6** — it derives from
  `script_verified_height`, which used to freeze on the first out-of-order eval
  result. Measured live: `eval_lag=187711` while `evals_in_flight=1` and
  jemalloc `allocated` flat at ~1.14 GB across 190,000 blocks. The watermark is
  fixed and the field now tracks reality, **except** on a node configured with
  `checkpoint_height` above its start height, where heights at or below the
  checkpoint never dispatch an eval and `eval_lag` is permanently large by that
  offset. Anything quoting `eval_lag` from a pre-1.6 run is quoting a frozen
  number.
- ⚠ **`state_applied_height` is the validator's `validated_height()`, not the
  sync struct's field of the same name.** That field is a cache reconciled only
  after the sweep loop; mid-sweep it is frozen at the pre-sweep tip, which
  would peg `eval_lag` at 0 by saturation and report a healthy system while the
  queue grew.

#### `deferred_eval_gate_engaged`
- **Level:** DEBUG
- **Marker:** `"eval dispatch gate engaged"`
- **Fields:** `evals_in_flight` (u64), `eval_bytes_in_flight` (u64),
  `incoming_bytes` (u64: `approx_heap_bytes` of the eval that did not fit —
  explains why *this* block tripped it), `bound` (string:
  `bytes`|`blocks`|`both` — which bound tripped, named rather than left to be
  inferred from the two numbers, since the operator re-tuning one needs to know
  which regime the host is in)
- **Since:** 1.6 (added 2026-08-12)
- **Stability:** stable
- **Emitted:** when the dispatch gate blocks because adding an eval would
  exceed the byte bound or the count bound, immediately before it drains to
  half of each enabled bound.
- *The marker was `"deferred eval backlog at bound — draining to half"` in the
  first draft of this entry. Changed before release for two reasons: it began
  with `deferred_eval_backlog`'s entire marker, so a prefix parser matched both
  events and then failed looking for `eval_lag`; and it carried a U+2014 em
  dash inside the matched portion. The configured limits were also documented
  as fields — dropped, because they are static and the operator already has
  them; the emitted set above is the one that varies.*
- **Why it exists:** the gate blocking is the node deliberately trading sync
  throughput for bounded memory, and it is otherwise invisible — the operator
  sees only that catch-up got slower. DEBUG rather than INFO because on a host
  that genuinely cannot keep up this fires continuously; it is a diagnostic for
  "why is catch-up slow", not a health signal. Sustained engagement means the
  host is verification-bound, which is the condition the bound exists for and
  not in itself a fault.

#### `eval_frontier_hole`
- **Level:** WARN
- **Marker:** `"no eval outstanding below the script frontier"`
- **Fields:** `script_verified_height` (u64: where the frontier is stuck),
  `hole` (u64: the height it cannot pass, `script_verified_height + 1`),
  `buffered` (u64: reorder-buffer entries discarded at this point — the number
  that would otherwise have grown by one per block forever),
  `state_applied_height` (u64: how far state has run ahead, i.e. the size of
  the span this leaves unattested)
- **Since:** 1.6 (added 2026-08-12)
- **Stability:** stable
- **Emitted:** when a drain finds nothing in flight below the frontier — i.e.
  no result can ever arrive to advance it. **Latched: once per distinct hole,
  not once per occurrence**, re-arming when the frontier moves; otherwise this
  is a WARN on every block for the life of the process. The reorder buffer is
  dropped at the same point so it stops accumulating one `u32` per block.
- **Why it exists:** the expected cause is a `checkpoint_height` configured
  above the node's start height. Heights at or below the checkpoint never
  dispatch an eval, so the frontier can never reach it and `eval_lag` is
  permanently large by that offset. **This is configuration, not backlog** —
  read `evals_in_flight` and `eval_bytes_in_flight` for real depth. If it fires
  on a node with **no** checkpoint configured, that is a genuine defect in
  frontier accounting and should be reported. See `facts/sync.md`
  § "Checkpoint frontier hole", which records the open question of whether the
  frontier should advance over deliberately-skipped heights.

#### `validation_rollback_failed`
- **Level:** ERROR
- **Marker:** `"validation rollback failed"`
- **Fields:** `height` (u64: the rollback TARGET height), `path`
  (string: `eval_failure`|`reorg`), `error` (string: the underlying
  storage/rollback error, Display-formatted)
- **Since:** 1.4 (added 2026-06-12 with `reset_to → Result`)
- **Stability:** stable
- **Emitted:** when `BlockValidator::reset_to` returns Err on the
  deferred-eval-failure path or the reorg-control path — the underlying
  state rollback failed and the validator did NOT move (its height and
  digest are unchanged; facts/validation.md `reset_to` Err
  postcondition). Sync holds every watermark in place rather than
  retreating onto un-rolled state; recovery rides the sweep/backoff
  machinery. Previously this failure was logged-and-swallowed inside
  the validator while its cache advanced — the gap-wedge latent hole.
  The startup-reconciliation variant of the same failure surfaces as
  `validated_height_drift` with `mode = "rollback_failed"` instead.
  The Doctor adapter should treat repeated emissions at the same
  height as a stuck-frontier signal (it will co-occur with
  `validation_stuck` once attempts accumulate).

#### `deep_reorg_succeeded`
- **Level:** INFO
- **Marker:** `"deep reorg succeeded"`
- **Fields:** `fork_point` (u64: height), `demoted` (u64), `old_tip`
  (string), `new_tip` (string)
- **Since:** 1.0
- **Stability:** stable

#### `chain_tip_reached`
- **Level:** INFO
- **Marker:** `"chain tip reached"`
- **Fields:** `height` (u64)
- **Since:** 1.0
- **Stability:** stable
- **Emitted:** the first time `synced() == true` after startup or
  after dropping out of sync.

### UTXO snapshot bootstrap

#### `utxo_bootstrap_initiated`
- **Level:** INFO
- **Marker:** `"UTXO state empty, will bootstrap from peer snapshot"`
- **Fields:** none
- **Since:** 1.0
- **Stability:** stable

#### `utxo_snapshot_stored`
- **Level:** INFO
- **Marker:** `"UTXO snapshot created and stored"`
- **Fields:** `height` (u64)
- **Since:** 1.0
- **Stability:** stable

### Peer lifecycle

#### `peer_active`
- **Level:** INFO
- **Marker:** `"Peer active"`
- **Fields:** `peer` (string: connection id), `name` (string),
  `agent` (string), `version` (string), `direction` (string:
  `inbound`|`outbound`)
- **Since:** 1.0
- **Stability:** stable
- **Emitted:** when handshake completes and a peer becomes routable.

#### `peer_disconnected`
- **Level:** INFO
- **Marker:** `"Peer removed"`
- **Fields:** `peer` (string), `reason` (string)
- **Since:** 1.0
- **Stability:** stable

#### `peer_penalised`
- **Level:** WARN
- **Marker:** `"PENALTY"`
- **Fields:** `peer` (string), `kind` (string: short identifier such
  as `header_parse_failed`, `invalid_pow`, `malformed_peers`), `detail`
  (string, optional)
- **Since:** 1.0
- **Stability:** stable
- **Note:** existing emissions in v0.5.x use marker prefix `"PENALTY"`
  followed by a free-text reason. Implementation MUST move to named
  fields at the next minor; consumers SHOULD parse both forms during
  the 1.0 line.

### Network plumbing

#### `ipv4_listener_started`
- **Level:** INFO
- **Marker:** `"IPv4 listener started"`
- **Fields:** `bind` (string)
- **Since:** 1.0
- **Stability:** stable

#### `ipv6_listener_started`
- **Level:** INFO
- **Marker:** `"IPv6 listener started"`
- **Fields:** `bind` (string)
- **Since:** 1.0
- **Stability:** stable

#### `upnp_gateway_found`
- **Level:** INFO
- **Marker:** `"UPnP: gateway found"`
- **Fields:** `address` (string)
- **Since:** 1.0
- **Stability:** stable

#### `upnp_port_mapping_added`
- **Level:** INFO
- **Marker:** `"UPnP: port mapping added"`
- **Fields:** `port` (u16), `protocol` (string)
- **Since:** 1.0
- **Stability:** stable

### Mining (only when mining enabled)

#### `mining_block_found`
- **Level:** INFO
- **Marker:** `"mining: block found"`
- **Fields:** `height` (u64), `id` (string)
- **Since:** 1.0
- **Stability:** stable

## Stability rules

- **stable** events: marker prefix, field names, field types, and
  emission preconditions are frozen across this major. Removal or
  semantic change requires a major-version bump and a deprecation
  release.
- **experimental** events: may change without major bump. Adapters
  should treat them as best-effort. No experimental events in v1.0.
- **internal** events: not part of the contract; not enumerated here.
  Default for any tracing emission not in this document.

## What this contract is NOT

- A complete list of log lines. The node emits many lines that aren't
  listed here. They are internal and may move freely.
- A guarantee of emission timing within a phase. Phase markers are
  ordered with respect to other phase markers; secondary lines within
  a phase are not.
- A serialization format spec. The wire shape of the log line depends
  on the active `tracing_subscriber` formatter. Consumers extract
  events by marker-prefix matching, not by full-line regex.

## Open follow-ups

- JSON-line opt-in formatter: design and ship under `[logging]`
  config section in a later minor.
- Penalty event field migration: rename free-text reasons to
  `kind` enum values during 1.x.
- `block_applied` may not exist yet in all sync paths under that
  exact prefix; per-crate audit needed during implementation.
- Mining events: enumerate the remaining lifecycle (`template_built`,
  `solution_received`) once mining log lines are audited.
