# Journal Events Contract

Version: 1.1.0

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
  named-field syntax. Keys are snake_case ASCII. Values that contain
  whitespace are surrounded by double quotes in the default
  formatter.
- Optional **free-text suffix** after the marker, for human
  readability. Parsers MUST tolerate arbitrary suffix content.

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
- **Since:** 1.1
- **Stability:** stable
- **Emitted:** when `apply_state` fails the same way on the same
  height for `attempts >= 5` consecutive sweeps. Surfaces the silent
  retry loop that previously buried this kind of state-DB
  inconsistency in INFO-level logs. The Doctor adapter treats this
  as a primary "node stuck" signal. Emitted at most once per height;
  re-emits when the height changes or after a recovery resets the
  counter.

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
  as `header_parse_failed`, `invalid_pow`, `address_sanity`), `detail`
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
