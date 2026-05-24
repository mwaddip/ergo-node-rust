# Indexer Addon Contract

Version: 1.0.0

## Component: `addons/indexer/` (workspace addon)

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
- `sqlx` — async SQL access, supports both SQLite and PostgreSQL
  backends with the same query surface.
- `reqwest` — outbound HTTP to the node's REST API.

## Dependencies

- `axum`, `sqlx`, `reqwest`, `serde`, `serde_json`, `clap`, `anyhow`
- **No** dependency on `enr-store`, `ergo-validation`, `ergo-mempool`,
  `ergo-p2p`, or any other node-internal crate. The indexer talks to
  the node only via HTTP.
- `sigma-rust` (`ergo-lib`, `ergo-chain-types`) — only as needed for
  parsing canonical bytes returned from node JSON. The indexer does
  NOT run sigma-rust evaluation (no `reduce_to_crypto`); it parses
  JSON directly and trusts node-emitted IDs.

## Configuration

CLI flags only (no config file):

```
ergo-indexer
  --node-url   http://127.0.0.1:9052     # default
  --db         ./indexer.db              # SQLite path or postgres://... URL
  --bind       127.0.0.1:8080            # CLI default; systemd unit overrides
  --start-height N                       # optional, default = resume from DB
  [sync|serve]                            # optional mode subcommand; default = both
```

The installed systemd unit at `/usr/lib/systemd/system/ergo-indexer.service`
sets `--bind 0.0.0.0:9054`. Operators who want loopback-only access
should override via a drop-in or edit the unit.

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

## Sync Semantics

The sync loop polls `GET /info` on the node to learn `fullHeight`,
then walks forward block by block via `GET /blocks/at/{h}` →
`GET /blocks/{id}` → batch-insert in a single SQLite transaction →
update `indexer_state.height`.

### Known gap: reorg handling

**The indexer does not detect reorgs.** On every block-insert, it
trusts that the canonical header_id at `height = indexer.height + 1`
hasn't changed since it was first committed. If the node reorgs
across a height the indexer has already written, the indexer keeps
its old (now-fork) `header_id` row and never reconciles.

In practice this surfaces as a `UNIQUE constraint failed:
transactions.tx_id` on a future block, because some tx_ids
in the canonical block are already in the table under the fork's
header_id. The recovery path today is operator-driven:

1. Diagnose with `sqlite3 ... "SELECT lower(hex(header_id)) FROM
   blocks WHERE height = X"` vs `GET /blocks/at/X` from the node.
2. Truncate above the last matching height via the cascade above.
3. Restart the indexer.

A proper fix (reorg detection on the indexer side) is a future
contract change. Until then, this contract documents the gap so
operators expect it.

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

## Stability

| Aspect | Stability |
|---|---|
| `/api/v1/` path prefix | Stable across minor versions |
| Existing endpoint paths | Stable across major versions |
| `bytes` field is hex (not base64, base16-prefixed, etc.) | Stable across major versions |
| `Restart=on-failure` + StartLimit policy | Stable; tuning may change |
| SQLite schema column adds | Additive only; removals bump major |
| Reorg detection added | Future minor — clients must not depend on its absence |
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
