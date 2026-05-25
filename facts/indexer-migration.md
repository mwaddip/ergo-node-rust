# Indexer DB Migration Contract

Version: 1.1.0

## Component: `addons/indexer/src/bin/ergo-indexer-migratedb.rs`

One-shot migrator between the indexer's two backends — SQLite
(default) and PostgreSQL. Same crate as the indexer (shares
schema types, DB modules, and constants); ships as a separate
`ergo-indexer-migratedb` binary because migration is occasional
and bolting it onto the long-running daemon's CLI surface as a
subcommand would muddle that surface.

## Purpose

Operators commonly start on SQLite (zero setup) and move to
PostgreSQL once concurrency or dataset size pressures hit. The
reverse path (PG → SQLite) is rarer but supported for symmetry;
the algorithm is identical in either direction.

Schema is documented in `facts/indexer.md`. The migrator
introduces no new tables — checkpoint state lives in the
target's existing `indexer_state` table.

## SPECIAL Profile

```
S8  P5  E9  C7  I6  A5  L9
```

E9 (crash recovery is the product — interruption mid-multi-hour
migration must always be resumable) and L9 (edge cases dense:
fork-handling debt in source data, schema-version drift, source
mutating between resume attempts). P5 since the migrator is
operator-invoked, not network-facing.

## CLI

```
ergo-indexer-migratedb [--in=<src>] [--out=<tgt>] [--update-config] [--resume] [-y]
```

All flags optional. Behavior is determined by what's explicitly
set, what can be derived from the indexer's config and env, and
whether the target already exists.

| Flag | Effect |
|---|---|
| `--in=<src>` | Source DB. SQLite path (or `sqlite://...`) or `postgres://...` URL. |
| `--out=<tgt>` | Target DB. Same shape as `--in`. |
| `--update-config` | After successful migration, rewrite `[storage].db` in `/etc/ergo-node/indexer.toml` to the new target. |
| `--resume` | Resume an interrupted migration (see Resume Semantics). |
| `-y` | Non-interactive; skip the pre-migration confirmation prompt. |

## Derivation rules

**When `--in` is not set**: read `[storage].db` from
`/etc/ergo-node/indexer.toml`. If the config file doesn't exist
or doesn't set `storage.db`, error with the same diagnostic the
indexer itself uses for missing-storage-db.

**When `--out` is not set**: target type is the *opposite* of
source type. Specifics:

- **Source = SQLite, target = PostgreSQL**: build the target URL
  from libpq env vars (`PGHOST`, `PGPORT`, `PGUSER`,
  `PGDATABASE`, optional `PGSSLMODE`). Missing required env vars
  → error listing exactly what's missing. Password sourced as
  the indexer does (`PGPASSWORD` env or `~/.pgpass`).
- **Source = PostgreSQL, target = SQLite**: target path defaults
  to `/var/lib/ergo-indexer/index.db`. If that file already
  exists, error UNLESS `--resume` is set.

If both resolve to the same backend type, error (probably a
typo; no work to do).

## Confirmation

When stdout is a TTY and `-y` is not set, print the migration
plan and prompt:

```
Migration plan:
  Source:        sqlite:///var/lib/ergo-indexer/index.db
  Target:        postgres://indexer@db.example.com:5432/ergo
  Update config: yes
  Resume:        no (fresh migration)
Proceed? [y/N]:
```

Default answer is `N`. Anything other than `y`/`yes` (case-
insensitive) aborts.

When `-y` is set, skip the prompt and proceed silently.

When stdout is NOT a TTY (piped, cron, etc.) AND `-y` is not
set, refuse to proceed: `non-interactive context but -y not
set; pass -y to proceed without confirmation`. Prevents
accidental yes-by-redirection.

## Direction

Bidirectional. The algorithm is identical in either direction —
the only differences are which driver module is the source
reader vs. the target writer (rusqlite vs. sqlx-postgres).

Primary user path: SQLite → PostgreSQL.

## Resume Semantics

The migrator writes three keys into the target's `indexer_state`
table on the first batch of every migration:

- **`migration_cursor`**: last fully-copied source height.
  Updated per-block in the same transaction as that block's
  data. Resume picks up at `migration_cursor + 1`.
- **`migration_source`**: normalized source URL string (e.g.
  `sqlite:///absolute/path/index.db` after path resolution).
  Catches "operator pointed `--resume` at a different source"
  errors.
- **`migration_source_fingerprint`**: `blake2b256` of the
  source's block content at a fixed fingerprint height (default
  `h=1`). Catches "the source's data changed between resume
  attempts" errors.

### `--resume` preconditions

When `--resume` is set, BEFORE any work:

1. Target exists. If absent → treat as fresh, log a one-line
   warning that `--resume` was unnecessary.
2. Target's `indexer_state.schema_version` matches the
   migrator's expected version.
3. Target has `migration_cursor`, `migration_source`, and
   `migration_source_fingerprint` rows in `indexer_state`.
   Missing any → error (target wasn't created by a prior run of
   this tool).
4. Stored `migration_source` equals the current source URL,
   normalized. Mismatch → error.
5. Stored `migration_source_fingerprint` equals the source's
   fingerprint computed now. Mismatch → error (source data
   changed between runs).
6. Spot-check: re-hash the source's block at `migration_cursor`
   and compare against the target's block at the same height.
   Mismatch → error.

All 6 checks must pass before resume continues. Any failure
exits with a diagnostic naming the specific check that tripped.

### Without `--resume`

If the target exists and contains data → error with diagnostic:
`target already exists at <path>; pass --resume to continue an
interrupted migration, or move/delete the existing target`.

Exception: if the target was created by THIS migrator but never
got past the schema-init step (the `indexer_state` table is
present but lacks `migration_cursor`), allow overwrite. This
handles the case where a prior run errored before any block was
copied.

## Per-block migration unit

The migrator walks source heights from `1` (or
`migration_cursor + 1` on resume) to `source.max_height`.

For each height `H`, in one target-side transaction:

1. INSERT new rows created at H: `blocks` row, `transactions`
   rows where `height = H`, `boxes` rows where `height = H`,
   plus their `box_registers` / `box_tokens` rows and `tokens`
   rows where `minting_height = H`.
2. UPDATE existing `boxes` rows where `spent_height = H` (these
   were created at prior heights, spent at H). Sets
   `spent_tx_id` and `spent_height`.
3. UPDATE `indexer_state.migration_cursor = H`.
4. Commit.

If interrupted at any step, the transaction rolls back. Resume
picks up at H. No half-committed state.

Foreign keys are deferred (`PRAGMA foreign_keys = OFF` on SQLite
target, `SET CONSTRAINTS ALL DEFERRED` on PG target) during the
entire migration and re-enabled at the end. Belt-and-suspenders
against any ordering surprise.

## Progress output

Migrations involve 50+ GB of data and run for tens of minutes to
hours. The migrator emits a progress trail to stdout to make
that visible:

- One `.` per 1000 source blocks fully committed (post-hash-
  verification, post-cursor-update).
- At each percentage-point boundary, terminate the current line
  with `(N%)` and a newline. Resume dots on the next line.

Example for a 1.79M block chain (~17.9 dots per 1% boundary):

```
.................. (1%)
.................. (2%)
.................. (3%)
...
.................. (100%)
Migration complete: 1,790,128 blocks copied in 47m22s (629 blk/s avg).
```

Progress flushes after every dot so operators see real-time
movement under TTY. Under non-TTY (systemd-journal, piped to a
file) progress will appear line-buffered, one line per 1% — still
useful.

On `--resume`, the dot stream starts at `migration_cursor + 1`
and the leading `(N%)` reflects that — first line may begin with
`(36%)` (or whatever) if 35% had already been copied.

The progress line is the only thing on stdout. All other
diagnostics (warnings, errors, info logs) go to stderr.

## Hash verification

After each block's INSERT/UPDATE in target (step 3 above), the
migrator computes a `blake2b256` digest of the block's content
from BOTH source and target and compares.

Hash input is canonical row-by-row encoding of all data at H, in
this order:

- `blocks` row at H (header_id, height, ...)
- `transactions` rows where `height = H`, sorted by `tx_index`
- `boxes` rows where `height = H`, sorted by `box_id`
- `box_registers` + `box_tokens` for those boxes, in `box_id`
  order
- `tokens` rows where `minting_height = H`, sorted by `token_id`
- `boxes` rows where `spent_height = H` (post-update state),
  sorted by `box_id`

Encoding: each column emitted in a fixed order, separated by a
sentinel byte; row boundary marked by a different sentinel byte.
Detailed encoding is the implementation's choice as long as it
is fully deterministic and identical on both sides.

If hashes disagree at any height, the migrator aborts: prints a
diagnostic identifying the height and direction of the mismatch,
exits non-zero. The current block's transaction has already
committed (see implementation note below), so the target's
`migration_cursor` equals the failing height. The operator
investigates and re-runs with `--resume`; the resume precondition
check #6 (spot-check the cursor-height block) will then fail at
the same height, surfacing the divergence cleanly.

**Implementation note** — the per-block transaction commits BEFORE
the hash comparison. The original spec called for rollback on
mismatch, but the read-back step that supplies the target's row
content for hashing operates on committed data. Rolling back
would require either (a) reading rows back inside the still-open
transaction (changes the Backend API significantly), or (b)
double-buffering the writes (significant memory cost on large
blocks). The chosen behavior — commit then hash then error —
preserves diagnostic state on disk for inspection, and the
spot-check re-run on `--resume` re-detects the divergence
deterministically. Worst-case waste on hash mismatch: one
committed block of incorrect data the operator can investigate
in-place before re-running.

## `--update-config` behavior

When `--update-config` is set AND the migration completes
successfully, the migrator rewrites `[storage].db` in
`/etc/ergo-node/indexer.toml` to point at `--out`. The previous
value goes into a comment line immediately above the new key
for rollback reference. The migrator does NOT restart the
indexer service — the operator decides when to switch over.

The config rewrite is best-effort: if the file doesn't exist or
isn't writable, the migrator logs a warning and exits `0` (the
migration itself succeeded).

Without `--update-config`, the migrator prints a hint on
successful completion:

```
Migration complete. To switch the indexer to the new database,
update [storage].db in /etc/ergo-node/indexer.toml or pass
--db <new-target> to the indexer service.
```

## Schema version check

Before any work (resume or fresh), check both sides'
`indexer_state.schema_version`:

- Source `schema_version` must match the migrator's expected
  version. Mismatch → error.
- Target empty (or just initialized): migrator writes the
  expected `schema_version` row as part of schema init.
- Target has any rows: existing `schema_version` must match.
  Mismatch → error.

## Signal handling

`SIGINT` / `SIGTERM` during migration: the migrator finishes
the current block's transaction commit (if mid-step-4) or
rolls back (if mid-step-1/2/3), writes nothing additional,
exits non-zero. Resume picks up at the last fully-committed
block. Worst case waste: one block of work.

## Liveness checks

The migrator actively refuses to proceed if it detects a running
indexer that could be writing to the source. Two checks, run
before any migration work:

### 1. API ping (always)

The migrator attempts a `GET /api/v1/info` against the bind
address resolved from `/etc/ergo-node/indexer.toml`'s `[api].bind`
(falling back to `127.0.0.1:8080` if the file or key is absent).
Timeout: 2 seconds.

- If the request succeeds with a 2xx response → the indexer is
  running. Refuse with diagnostic:

  ```
  indexer appears to be running (got 2xx from http://<bind>/api/v1/info).
  Stop the indexer (e.g. systemctl stop ergo-indexer) and re-run.
  ```

- If the request fails (connection refused, timeout, DNS) → no
  responder, proceed.
- If the request succeeds with non-2xx (5xx, 4xx) → log a one-
  line warning that something's listening on the bind but
  doesn't look like a healthy indexer. Proceed.

### 2. SQLite exclusive lock (SQLite source only)

When source is SQLite, the migrator also attempts to acquire an
EXCLUSIVE lock on the source database file. SQLite's lock
mechanism blocks if any other process is currently writing.

- Lock acquired → no concurrent writer, proceed.
- Lock blocked (SQLITE_BUSY) → another process is writing to
  the source. Refuse with diagnostic:

  ```
  source SQLite database is locked by another process. Stop the
  writer (likely the indexer) and re-run.
  ```

### PG source caveat

The migrator does NOT have a clean cross-process "is the
indexer using this PG database?" signal. The API ping is the
only check for PG source. Operators running multiple indexer
instances against the same PG database (unusual but not
impossible) must ensure all of them are stopped before
migration.

The migrator releases the source lock and the API check on
exit; the next run starts fresh.

## Operator prerequisites

1. The source indexer process MUST be stopped (the liveness
   checks above enforce this, but the operator is still
   responsible for stopping it cleanly to avoid mid-write
   inconsistency the migrator can't detect after the fact).
2. The target must be either absent, initialized by a prior
   run of this migrator, or explicitly deleted for fresh.
3. For PG target: the database must exist, the user must have
   `CREATE TABLE` / `INSERT` / `UPDATE` permissions. The
   migrator creates the schema on first batch.
4. Sufficient disk space at the target. The migrator does not
   pre-check; out-of-disk surfaces as a regular SQL error and
   the operator resumes after freeing space.

## Exit codes

- `0` — success
- non-zero — error with diagnostic on stderr

The CLI library may surface its own usage-error code (clap
typically uses `2`); that's library behavior, not part of this
contract.

## Stability

| Aspect | Stability |
|---|---|
| Binary name `ergo-indexer-migratedb` | Stable across major versions |
| CLI flag names | Stable across major versions |
| `migration_cursor` / `migration_source` / `migration_source_fingerprint` key names | Stable across major versions (rename = major bump) |
| Schema-version-must-match policy | Stable |
| Per-block atomicity | Stable |
| Hash verification algorithm and canonical encoding | Stable across major versions; encoding changes require major bump |
| `--update-config` behavior on missing/unwritable file | Stable in direction (best-effort); details may evolve additively |

## Out of scope

- **Online migration** (source running, target catching up via
  replication-style streaming). Operators must stop the indexer
  before migrating.
- **Cross-schema-version migration**. This tool migrates between
  backends at the same schema_version. Schema upgrades are a
  separate concern (handled by the indexer's own startup
  migrations, or future dedicated tooling).
- **Partial migration** (migrate only some heights). Would
  break the indexer's continuity assumptions.
- **Automatic switchover**. Even with `--update-config`, the
  migrator does not restart the indexer service. The operator
  controls the switchover window.

## See also

- `facts/indexer.md` — the indexer's contract; schema, sync
  semantics, and config-file shape that this migrator depends
  on.
