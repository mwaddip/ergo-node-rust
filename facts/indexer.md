# Indexer Addon Contract

Version: 1.2.1

## Component: `addons/indexer/` (standalone crate)

Out-of-process blockchain indexer for `ergo-node-rust`. Polls the
node's REST API, walks blocks forward, persists a queryable mirror
of chain data (blocks, transactions, boxes with spent/unspent
lifecycle, tokens) in a SQLite (default) or PostgreSQL backend,
and serves the result via its own HTTP API on a separate port.

Distinct from the node:

- Runs as a separate process (separate systemd unit, separate
  binary `ergo-indexer`).
- Owns its own storage (`/var/lib/ergo-indexer/index.db` by default).
- Has its own REST API, distinct from the node's (port 9054 vs the
  node's 9052).
- Failures here never compromise consensus or the node's own
  storage.
- Excluded from the workspace via `[workspace].exclude` in the
  repo-root `Cargo.toml`. Built/tested with
  `cargo … --manifest-path addons/indexer/Cargo.toml` and carries
  its own `Cargo.lock`.

Primary consumers: block explorers, wallets needing spent-box
history, validation harnesses that need historical box bytes the
node's UTXO state has discarded (the immediate motivating use case
is the TypeScript-side mainnet-validation harness in the
`ergots` project).

## SPECIAL Profile

```
S6  P8  E8  C7  I6  A6  L8
```

External-facing (P8) — the indexer binds to a non-loopback address
by default in the installed systemd unit, so input validation
matters. Crash recovery is the dominant correctness concern (E8) —
the indexer will be killed mid-batch, mid-reorg, mid-startup, and
must come back without operator intervention beyond what `sharpen
--indexer` provides. Edge cases are dense (L8): reorg handling,
fork-header-recorded-as-canonical, partial-batch crashes, schema
migrations across versions.

## Design Principles

- **Downstream-only.** The indexer reads from the node's REST API;
  it never writes to or controls the node. Killing the indexer or
  corrupting its database has zero effect on consensus state.
- **Single forward walker.** No concurrent writers. One
  `ergo-indexer` process owns the SQLite file. Operator-side
  read replicas are encouraged via standard SQLite tooling, not by
  spawning a second writer.
- **Schema-versioned.** The `indexer_state` table carries a
  `schema_version` row. Code refuses to start against an
  incompatible version rather than silently mis-parsing.
- **Crash-aware composition.** All multi-row inserts run inside a
  single SQLite transaction per block. Either the entire block
  lands (state row, transactions, boxes, tokens, cascade tables)
  or none of it does. Partial writes are impossible at the SQL
  level — the historical "stuck on duplicate" failure mode
  (resolved 2026-05-23) was a fork-handling gap, not a partial-
  write bug.

## Framework

- `axum` — HTTP framework, matches the node's choice for the same
  reasons (tokio-native, tower middleware, minimal).
- `rusqlite` — synchronous SQLite access for the default SQLite
  backend.
- `sqlx` — async SQL access for the PostgreSQL backend only.
- `reqwest` — outbound HTTP to the node's REST API.
- `tikv-jemallocator` — global allocator (default-on via the
  `jemalloc` feature). Daemon-style workloads with sustained
  allocation churn benefit substantially from jemalloc's decommit
  behavior vs glibc malloc; observed 21.8 GB peak RSS under
  harness load was the motivation. Opt out with
  `--no-default-features --features sqlite`.

## Dependencies

- `axum`, `rusqlite`, `sqlx`, `reqwest`, `serde`, `serde_json`,
  `clap`, `anyhow`
- `tikv-jemallocator` + `tikv-jemalloc-ctl` (gated by `jemalloc`
  feature, default-on)
- **No** dependency on `enr-store`, `ergo-validation`, `ergo-mempool`,
  `ergo-p2p`, or any other node-internal crate. The indexer talks to
  the node only via HTTP.
- `sigma-rust` (`ergo-lib`, `ergo-chain-types`) — only as needed for
  parsing canonical bytes returned from node JSON. The indexer does
  NOT run sigma-rust evaluation (no `reduce_to_crypto`); it parses
  JSON directly and trusts node-emitted IDs.

## Configuration

The indexer reads configuration from three sources in increasing
precedence order: built-in defaults < TOML config file < env vars
< CLI flags. Each key resolves independently — setting `--db` on
the CLI does not require setting the others; unset keys fall
through to lower-precedence sources.

### Required keys

`storage.db` is REQUIRED. There is no built-in default. If
neither the config file nor a `--db` CLI flag sets it, the
indexer exits non-zero at startup with a clear diagnostic
(`storage.db not configured — set [storage].db in the config
file or pass --db <url-or-path>`). The database location is too
operationally important to silently default to a relative path
that depends on the process's working directory.

All other keys have safe defaults: `node.url =
http://127.0.0.1:9052`, `api.bind = 127.0.0.1:8080`,
`sync.start_height` unset (resume from DB).

### Config file location

Default: **`/etc/ergo-node/indexer.toml`** (sibling to the node's
`ergo.toml` — related services share the directory).

Override via `--config <path>` on the CLI. When the file does not
exist at the resolved path, the indexer logs an info-level line
(`config file not found at <path>, using defaults`) and continues
with built-in defaults + env vars + CLI flags. No hard error —
operators who run all-CLI today continue to work without a config
file.

When the file exists but fails to parse, the indexer exits with a
non-zero status and a diagnostic. Don't silently fall back from a
malformed file — it almost certainly indicates a typo the
operator wants surfaced.

### Schema

```toml
# /etc/ergo-node/indexer.toml

[node]
# URL of the local ergo-node-rust REST API.
url = "http://127.0.0.1:9052"

[storage]
# SQLite path: "sqlite:///var/lib/ergo-indexer/index.db" or a
# bare path treated as SQLite.
# PostgreSQL URL: "postgres://user@host:5432/dbname". Password
# comes from env (PGPASSWORD) or ~/.pgpass — never the config
# file.
db = "sqlite:///var/lib/ergo-indexer/index.db"

[api]
# Bind address for the indexer's HTTP API. Default
# 127.0.0.1:8080 (loopback). The installed systemd unit's
# default sets 0.0.0.0:9054 — non-loopback grants network-wide
# read access to indexed data.
bind = "127.0.0.1:8080"

[sync]
# Optional. When set, sync resumes from this height instead of
# the DB's last-committed height. Used for diagnostics + recovery.
# start_height = 1782320
```

Section names and keys above are stable across minor versions per
the Stability table; additive changes (new optional keys, new
sections) do not bump the major.

### Env vars

- `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`,
  `PGSSLMODE` — standard libpq env vars. Honored when
  `storage.db` resolves to a PostgreSQL backend. `PGPASSWORD` is
  the canonical password source; the config file does NOT carry
  passwords.
- `INDEXER_CONFIG` — alternative to `--config <path>`. CLI
  overrides this if both are set.

### CLI flag → config key mapping

| CLI flag | Config key | Notes |
|---|---|---|
| `--config <path>` | n/a | Path to TOML file; default `/etc/ergo-node/indexer.toml` |
| `--node-url <url>` | `node.url` | |
| `--db <url-or-path>` | `storage.db` | URL form preferred; bare path = SQLite |
| `--bind <addr:port>` | `api.bind` | |
| `--start-height <N>` | `sync.start_height` | Optional in both |
| `sync \| serve` (subcommand) | n/a | Mode is CLI-only |

CLI flags override the config file on a per-key basis. Operators
running the systemd unit can leave `ExecStart` minimal
(`/usr/bin/ergo-indexer --config /etc/ergo-node/indexer.toml`)
and put everything else in the TOML file, or keep CLI flags for
specific overrides during ad-hoc runs.

### Reload behavior

Configuration is loaded once at startup. Changes to the config
file are NOT live-reloaded; restart the service to apply
(`systemctl restart ergo-indexer`). `SIGHUP` is explicitly
ignored — the indexer does not terminate on receipt.

### Behavior when no config exists

Operators upgrading from a pre-config-file indexer can continue
to pass every flag via `ExecStart` — built-in defaults are still
applied first, CLI flags override them, and the missing
`/etc/ergo-node/indexer.toml` produces only a one-line info
log. The pre-config-file deploy is functionally unchanged.

## Storage Schema

SQLite tables (PostgreSQL backend mirrors these exactly):

```sql
CREATE TABLE indexer_state (
    key   TEXT PRIMARY KEY,
    value TEXT
);
-- Reserved keys: 'height', 'schema_version'.

CREATE TABLE blocks (
    header_id BLOB PRIMARY KEY,
    height    INTEGER NOT NULL,
    ...
);

CREATE TABLE transactions (
    tx_id     BLOB PRIMARY KEY,
    header_id BLOB NOT NULL REFERENCES blocks(header_id),
    height    INTEGER NOT NULL,
    tx_index  INTEGER NOT NULL,
    size      INTEGER NOT NULL
);

CREATE TABLE boxes (
    box_id        BLOB PRIMARY KEY,
    creation_tx_id BLOB NOT NULL REFERENCES transactions(tx_id),
    height        INTEGER NOT NULL,
    spent_tx_id   BLOB,            -- NULL = unspent
    spent_height  INTEGER,
    ...
);

CREATE TABLE box_registers (
    box_id BLOB NOT NULL REFERENCES boxes(box_id),
    ...
);

CREATE TABLE box_tokens (
    box_id BLOB NOT NULL REFERENCES boxes(box_id),
    ...
);

CREATE TABLE tokens (
    token_id       BLOB PRIMARY KEY,
    minting_height INTEGER NOT NULL,
    ...
);
```

Full column lists in `addons/indexer/src/db/sqlite.rs`; this
contract documents the invariants, not the field-by-field schema.

### Invariants

- `indexer_state.value WHERE key='height'` is the highest fully-
  committed block height. Every row in every table with a `height`
  column has `height <= indexer_state.height`.
- `blocks.header_id` at any given `height` must match the node's
  canonical `header_id` at that height. See **Sync Semantics**
  below — this is the historical gap.
- All foreign keys are CASCADE-safe to delete via the cascade in
  `sharpen --indexer`:

```sql
-- Cascade order (must be preserved on truncation):
UPDATE boxes SET spent_tx_id = NULL, spent_height = NULL WHERE spent_height > H;
DELETE FROM box_registers WHERE box_id IN (SELECT box_id FROM boxes WHERE height > H);
DELETE FROM box_tokens     WHERE box_id IN (SELECT box_id FROM boxes WHERE height > H);
DELETE FROM boxes          WHERE height > H;
DELETE FROM tokens         WHERE minting_height > H;
DELETE FROM transactions   WHERE height > H;
DELETE FROM blocks         WHERE height > H;
INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('height', '<H>');
```

The cascade is the authoritative truncation procedure. `sharpen.rs`
contains the canonical SQL; any future migration must update both.

### SQLite pragmas

Applied at connection initialization on both writer and reader
connections:

```
PRAGMA cache_size = -65536;     -- 64 MiB per connection (negative = KiB form)
PRAGMA mmap_size = 268435456;   -- 256 MiB
PRAGMA temp_store = MEMORY;
PRAGMA busy_timeout = 5000;     -- ms
```

`journal_mode=WAL` remains enabled. The `cache_size` negative form
is rusqlite/SQLite's KiB shorthand (positive values are page
count). `mmap_size` enables read-side mmap to reduce contention
between the writer transaction and parallel-fetch read traffic.

## Sync Semantics

The sync loop polls `GET /info` on the node to learn `fullHeight`,
then walks forward block by block via `GET /blocks/at/{h}` →
`GET /blocks/{id}` → batch-insert in a single SQLite transaction →
update `indexer_state.height`.

### Reorg handling

The indexer detects reorgs via **parent-linkage verification** on
every block it indexes. After fetching the target block's header
but before inserting its data, the indexer compares the target's
`parent_id` against the stored `header_id` at `last_indexed`. If
they match, indexing proceeds. If they don't, the indexer's chain
has been orphaned at or before `last_indexed`; it walks back via
canonical-id comparison to find the actual fork point and rolls
back the DB to that height.

This catches both single-block reorgs (the common case — the
indexer briefly indexed the orphan tip and the chain reorged a
moment later) and multi-block reorgs (the walk-back finds the
deeper fork point).

Operators see this as a WARN-level log line:

```
WARN parent linkage mismatch — rolled back
     prev_tip=<orphaned-height> fork_point=<rollback-target>
     target=<height-being-indexed>
     target_parent_id=<canonical-parent>
```

After the rollback, the indexer re-indexes from `fork_point + 1`
upward against the canonical chain. No operator intervention is
required — the recovery is automatic.

**The pre-fix failure mode**, preserved here for historical
diagnosis: indexers running v0.2.0 and earlier did not perform
this check, and would crash with `UNIQUE constraint failed:
transactions.tx_id` when a canonical block re-included
transactions from an orphaned predecessor. Recovery for those
versions required manually deleting the orphaned rows and rolling
back `indexer_state.height`.

## Crash Recovery

`Restart=on-failure` with `StartLimitBurst=3` /
`StartLimitIntervalSec=60` in the systemd unit. After three
failures in 60 s the unit stops and requires operator
intervention. This is intentional: a third failure in 15 s is
almost certainly deterministic (constraint violation, schema
mismatch, file permissions) and infinite-looping past it just
fills the journal without fixing anything.

Recovery procedure for the deterministic-failure case:

```
systemctl stop ergo-indexer
sharpen <H> --indexer            # node-coupled; rolls state.redb too
# OR for indexer-only truncation:
sqlite3 /var/lib/ergo-indexer/index.db < <cascade-SQL-above>
systemctl reset-failed ergo-indexer
systemctl start ergo-indexer
```

`sharpen --indexer` always rolls the node's state and modifier
stores to `<H>` in lockstep. For repairs that don't require
walking the node back (the more common case — node is fine, only
the indexer is stuck), run the cascade SQL directly.

## Shutdown Semantics

`SIGTERM` and `SIGINT` both trigger graceful shutdown via a
`tokio::sync::watch` channel:

1. The signal handler fires the watch sender.
2. The sync task's `info_wait` long-poll is wrapped in a
   `tokio::select!` against the watch receiver. On signal, sync
   exits *between* blocks — any in-flight `insert_block`
   transaction commits atomically under WAL before the loop
   exits. No half-validated block is observable post-shutdown.
3. The API task uses `axum`'s `with_graceful_shutdown` to drain
   in-flight requests.
4. `main()` awaits both task handles with a 30 s bounded timeout,
   matching the node's `SHUTDOWN_GRACE`.

Measured: ~4 ms clean exit in combined-mode locally after a
1543-block ingest run. Partial writes are impossible at the SQL
level (block commits are atomic), and the watch + select pattern
guarantees no straddled state across the signal.

## Endpoints

All endpoints live under `/api/v1/`. The path prefix is part of the
contract — clients that hard-code `/blocks/...` (without `/api/v1`)
are out of contract.

### Catalog (retroactive, stubbed)

The existing endpoint set is sourced from `addons/indexer/src/api/mod.rs`.
This contract enumerates them by name; per-endpoint response shapes
are TBD-from-source (this file describes the contract for the
addon; details below are stubs to be filled in incrementally as
each endpoint is reviewed).

```
GET  /api/v1/info                              → indexer + node-tip status
GET  /api/v1/stats                             → totals (blocks, txs, boxes, tokens)
GET  /api/v1/stats/daily                       → time-bucketed activity
GET  /api/v1/debug/memory                      → process RSS + jemalloc stats + component breakdown

GET  /api/v1/blocks                            → list, by height descending
GET  /api/v1/blocks/height/{height}            → block by height
GET  /api/v1/blocks/{header_id}                → block by id
GET  /api/v1/blocks/{header_id}/transactions   → txs in a block

GET  /api/v1/transactions/{tx_id}              → tx by id
GET  /api/v1/addresses/{addr}/transactions     → txs touching an address

GET  /api/v1/boxes/{box_id}                    → box by id (JSON)
GET  /api/v1/addresses/{addr}/balance          → confirmed balance
GET  /api/v1/addresses/{addr}/unspent          → unspent boxes for address
GET  /api/v1/addresses/{addr}/boxes            → all boxes for address
GET  /api/v1/ergoTree/{ergo_tree}/unspent      → unspent boxes by script

GET  /api/v1/tokens                            → token list
GET  /api/v1/tokens/{token_id}                 → token metadata
GET  /api/v1/tokens/{token_id}/holders         → holder list
GET  /api/v1/tokens/{token_id}/boxes           → boxes containing token
```

### `GET /api/v1/boxes/{box_id}/bytes`

Canonical `ErgoBox::sigma_serialize` bytes for any box the indexer
has ever indexed — spent or unspent. Returns the same byte sequence
that lives in the originating tx's outputs section on the wire,
hex-encoded.

**Path param:** `box_id` — 32-byte box id, hex-encoded.

**Response 200 (`Content-Type: application/json`):**

```json
{
  "bytes": "<hex>"
}
```

**Response 404:** box id not present in the indexer's `boxes` table.
Distinguishable from "exists but unreadable" by status code only —
404 body is identical.

```json
{
  "error": "box-not-found",
  "boxId": "<hex>"
}
```

**Response 500:** internal failure (e.g., stored creating-tx row
has a corrupted output index). Body:

```json
{
  "error": "<short-code>",
  "boxId": "<hex>",
  "message": "<diagnostic>"
}
```

### Invariants

- Must serve both spent and unspent boxes uniformly. A client cannot
  tell from the response whether the box is currently spent.
- The returned bytes MUST satisfy
  `blake2b256(bytes) == box_id`. If this invariant cannot be
  established, return 500 — never serve bytes whose hash disagrees
  with the requested id.
- Composition path: look up `boxes.creation_tx_id` for the requested
  `box_id` → fetch the creating tx's canonical bytes (from the
  node's `/blocks/{id}/transactions` or a local cache) → extract
  output at the recorded index → return.

### Performance

Each lookup is O(log n) box-id index probe + one creating-tx fetch.
A 100-input bundle (the harness's hot path) is 100 parallel calls
into this endpoint. The harness MUST issue them in parallel; the
server holds no per-request state and SQLite + HTTP/1.1 keep-alive
will handle ~1k concurrent reads against a single SQLite file
without contention.

### `GET /api/v1/debug/memory`

Operator-facing memory diagnostics. Mirrors the node's
`/debug/memory` endpoint shape, with indexer-specific components.

**Response 200 (`Content-Type: application/json`):**

```jsonc
{
  "process": {
    "rss_bytes": N,                       // resident set size from /proc
    "...": "..."                           // node-mirror fields
  },
  "jemalloc": {                            // present when feature `jemalloc` is enabled
    "allocated": N,
    "resident": N,
    "...": "..."                           // standard jemalloc.stats keys
  },
  "components": {
    "indexedHeight":         N,
    "dbBackend":             "sqlite" | "postgres",
    "dbOnDiskBytes":         N,            // SQLite file size; postgres reports N/A
    "dbCacheBytesPerConn":   N,            // SQLite cache_size derived; postgres N/A
    "dbConnections":         N
  }
}
```

**Response 200 with `jemalloc` absent**: build was
`--no-default-features --features sqlite`. The `jemalloc` block
is omitted; consumers must tolerate its absence.

**No 4xx/5xx responses expected** in normal operation. Failure
to read `/proc/self/status` for RSS produces 500 with a
diagnostic body.

## Stability

| Aspect | Stability |
|---|---|
| `/api/v1/` path prefix | Stable across minor versions |
| Existing endpoint paths | Stable across major versions |
| `bytes` field is hex (not base64, base16-prefixed, etc.) | Stable across major versions |
| `Restart=on-failure` + StartLimit policy | Stable; tuning may change |
| SQLite schema column adds | Additive only; removals bump major |
| Reorg detection | Shipped via parent-linkage check; clients should not depend on a specific reorg-window depth |
| PostgreSQL backend parity with SQLite | Stable across major versions |

## Out of scope

- Authentication on `/api/v1/*` — there is none. Loopback bind is
  the security mechanism for sensitive deployments.
- Cross-DB transactions — the indexer never coordinates with the
  node's redb files. Atomicity ends at the SQLite transaction
  boundary.
- Box bytes for boxes the indexer has not yet indexed — the endpoint
  returns 404 for any box at `height > indexer_state.height`, even
  if it exists on the node's chain.
- Box bytes for genesis boxes that were never the output of any
  tx the indexer ingested — the seed-state genesis boxes (emission,
  no_premine, founders) exist at `height = 1` without a creating
  tx. Behavior here is TBD; if the harness needs them, this contract
  should explicitly add them to the `boxes` table at height 1 with
  a synthetic `creation_tx_id` sentinel.

## See also

- `facts/indexer-migration.md` — contract for the
  `ergo-indexer-migratedb` one-shot migrator between the SQLite
  and PostgreSQL backends. Operators scaling up (or down) between
  backends use this; the contract there depends on the schema and
  config-file shape documented here.
