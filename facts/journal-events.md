# Journal Events Contract

Version: 2.0.0

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

### 2.0.0 — deferred script evaluation removed (v0.8.0)

A major bump, because three **stable** events were removed outright and two
field domains narrowed. Per the stability rules below, that is exactly what a
major is for.

| Change | Was |
|---|---|
| `deferred_eval_backlog` removed | 1.5, replaced by `catchup_progress` |
| `deferred_eval_gate_engaged` removed | 1.6 |
| `eval_frontier_hole` removed | 1.6 |
| `validation_rollback_failed.path` loses `eval_failure` | now only `reorg` |
| `validation_stuck.error_kind` loses `script_eval` | replaced by `transaction_invalid` |

The first four describe deferred script evaluation, which no longer exists:
`apply_state` evaluates before persisting, so there is no queue to report on,
no dispatch gate to engage, and no verification frontier to fall behind.

⚠ **The stability rules below also require a deprecation release, and this did
not get one.** The events went in the same release that removed the machinery
behind them, because emitting a deprecated `eval_frontier_hole` from a node
with no frontier would mean fabricating the fields. Consumers pinned to major 1
— the Doctor adapter among them — will refuse to parse a v0.8.0 node until they
are updated. That is a real cost and it was accepted knowingly rather than
overlooked.

## Format conventions

Each contract event is a single line emitted via `tracing::info!()`,
`tracing::warn!()`, or `tracing::error!()` with the default
human-readable subscriber. The line carries:

- A **marker prefix**: the leading string literal that identifies the
  event. Stable across versions for a given event name.
- Zero or more **fields**: `key=value` pairs appended via tracing's
  named-field syntax. Keys are snake_case ASCII. String-recorded values
  are surrounded by double quotes in the default formatter (e.g.
  `error_kind="missing_key"`); numeric and `Display`-formatted values
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

One field-level convention, which applies to **every** event that carries it:

- **`state_applied_height` is always `validator.validated_height()`, never
  `HeaderSync`'s struct field of the same name.** That field is a cache
  reconciled at sweep *end*; any event emitted mid-sweep reads it frozen at the
  pre-sweep tip. This is stated once here rather than per entry because it has
  caught two events before either was retired in 2.0 — `deferred_eval_backlog`,
  where the stale read would peg `eval_lag` at 0 by saturation and report a
  healthy node while the queue grew, and `eval_frontier_hole`, reached from the
  dispatch gate's mid-sweep drain. Both are gone; the trap is not. `catchup_progress`
  carries the field today and reads it from the validator for exactly this
  reason. Assume the next event that carries it has the same trap.

A parser consuming these cannot tell a stale value from a fresh one, which is
why the rule lives in the contract rather than in a comment at each emit site.

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
  failure modes, though since 2.0 they arrive by one route: an
  `apply_state` error (state-DB inconsistency such as `missing_key`)
  and a script rejection, which `apply_state` now returns directly
  because evaluation happens inside it. `error_kind` names the mode
  via `classify_apply_state_error`; `missing_key` is present only for
  the `apply_state` `missing_key` case.

  **`error_kind` domain as of 2.0:** `missing_key` (the AVL tree lacks a key
  the block spends), `transaction_invalid` (the block was rejected because a
  transaction in it did not validate — a consensus problem), `other`
  (everything else). `missing_key` is accompanied by the `missing_key` field;
  the others are not.

  ⚠ **`missing_key` arrives through two variants, not one, and means
  different things in each.** The prose is raised once in
  `ergo_avltree_rust`'s shared `operation.rs`, which both the prover and the
  verifier use — so it surfaces as `StateOperationFailed` in UTXO mode and as
  `ProofVerificationFailed` in digest mode. The classifier matches both.
  In UTXO mode it means the node's own state store lacks the key, which is a
  local storage problem. In digest mode the node holds no UTXO set at all, so
  it means the *proof* did not contain the key it needed — closer to a bad
  block or a bad peer. An adapter that reads `missing_key` as "this node's
  database is damaged" is right only for a UTXO-mode node, and should consult
  `stateType` from `/info` before recommending a resync.

  *An earlier draft of this section described a single `String`-carrying
  variant and called the kind a state-DB problem outright. Both were wrong,
  and matching only the UTXO arm would have dropped `missing_key` on every
  digest-mode node — silently, which is the exact failure this rewrite exists
  to prevent.*

  *`script_eval` is gone and `transaction_invalid` is not a rename of it.*
  The old value named the deferred eval-failure **path**, which no longer
  exists. The new one names the **fact** — `ValidationError::TransactionInvalid`
  — which covers a failed script and equally a failed ERG or token
  preservation check. That is the distinction the Doctor actually needs: is
  this node stuck because its state store is damaged, or because it keeps
  refusing a block the network accepted?

  ⚠ **Classification is on the error variant, not on its Display string.**
  `classify_apply_state_error` took a `&str` and grepped it for
  `"does not exist"`, while its caller held the typed `ValidationError` and
  stringified it one line earlier. That is why the distinction disappeared
  silently when deferred evaluation was removed — nothing referenced the
  variant, so nothing broke. It now takes `&ValidationError`. The one place
  a string is still parsed is inside `missing_key`, where the key really
  does arrive as prose from the storage layer. Surfaces the silent retry loop that previously
  buried a stuck frontier in INFO-level logs. The Doctor adapter
  treats this as a primary "node stuck" signal. Emitted at most once
  per height; re-emits when the height changes or after real progress
  resets the counter. **Before 1.3** this fired only on the
  `apply_state` path — a deferred eval-failure stall (the loop that
  hammered on a wrongly-rejected script) did not emit it.

#### `catchup_progress`
- **Level:** INFO
- **Marker:** `"catch-up progress"`
- **Fields:** `state_applied_height` (u64: the **applied tip**, read from the
  validator rather than from sync's struct field), `jemalloc_allocated`
  (u64, **omitted entirely** when no heap probe is wired — an absent field
  rather than a rendered `None`, same convention as `validation_stuck`'s
  optional `missing_key`)
- **Since:** 2.0 (replaces `deferred_eval_backlog`, 1.5–1.6)
- **Stability:** stable
- **Emitted:** at most once per 5 s during catch-up, from the sweep loop. Not
  emitted at tip.
- **Why it exists:** catch-up is otherwise silent between flushes, and an
  operator watching a long sync needs to know it is moving. The applied tip is
  read from the validator because sync's own field lags it within a sweep.

*Replaced `deferred_eval_backlog` in 2.0. That event carried four more fields
— `evals_in_flight`, `script_verified_height`, `eval_lag`, and
`eval_bytes_in_flight` — all describing the deferred-eval queue, which no
longer exists. Anything quoting `eval_lag` from a v0.7.x journal is quoting a
number that was frozen by the reorder-buffer bug until 2026-08-12 anyway; see
facts/sync.md § "Block Assembly".*

#### `validation_rollback_failed`
- **Level:** ERROR
- **Marker:** `"validation rollback failed"`
- **Fields:** `height` (u64: the rollback TARGET height), `path`
  (string: `reorg` — the only value since 2.0; see below), `error` (string: the underlying
  storage/rollback error, Display-formatted)
- **Since:** 1.4 (added 2026-06-12 with `reset_to → Result`)
- **Stability:** stable
- **Emitted:** when `BlockValidator::reset_to` returns Err on the
  reorg-control path — the underlying
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
