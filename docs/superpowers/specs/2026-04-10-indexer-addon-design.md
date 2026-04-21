# Indexer Addon Design

**Date:** 2026-04-10
**Status:** Draft

## Overview

A standalone indexer addon (`addons/indexer/`) that follows the established addon pattern: separate crate excluded from the main workspace, communicates with the node via REST API, packaged independently. Unlike fastsync (which catches up and exits), the indexer is a long-running service that tracks the chain tip and serves rich query endpoints.

## Goals

- Provide query capabilities the core REST API intentionally omits: box lookup by address, transaction history, token metadata, holder lists, ErgoTree queries, network statistics
- Ship as a self-contained binary with SQLite by default, PostgreSQL optional
- Support split deployment: writer and reader can run on separate machines (PG mode)
- Self-documenting API via Swagger UI (utoipa)

## Non-Goals

- Replacing the core node's REST API — the indexer is additive
- Smart contract indexing / event log parsing — out of scope for v1
- Full-text search — standard SQL indexes are sufficient

## Binary & CLI

Single binary: `ergo-indexer`

```
ergo-indexer                  # combined: sync + serve (default)
ergo-indexer sync             # index blocks, no query API
ergo-indexer serve            # query API only, read-only
```

CLI arguments (clap):

| Arg | Default | Description |
|-----|---------|-------------|
| `--node-url` | `http://127.0.0.1:9052` | Node REST API URL |
| `--db` | `./indexer.db` | SQLite path or `postgres://` URL |
| `--bind` | `127.0.0.1:8080` | Query API listen address |
| `--start-height` | auto-detect | Resume from specific height |

The `--db` argument determines the backend: `postgres://` prefix → PG, otherwise SQLite file path. Config file alternative at `indexer.toml` (or `/etc/ergo-node/indexer.toml` when packaged), same fields. CLI overrides config.

## Database Schema

### Core Tables

```sql
-- Indexed blocks (canonical chain only)
blocks (
    height        BIGINT PRIMARY KEY,
    header_id     BYTEA NOT NULL UNIQUE,
    timestamp     BIGINT NOT NULL,
    difficulty    BIGINT NOT NULL,
    miner_pk      BYTEA NOT NULL,
    block_size    INT NOT NULL,
    tx_count      INT NOT NULL
)

-- Every transaction
transactions (
    tx_id         BYTEA PRIMARY KEY,
    header_id     BYTEA NOT NULL REFERENCES blocks(header_id),
    height        BIGINT NOT NULL,
    tx_index      INT NOT NULL,
    size          INT NOT NULL
)

-- Every box ever created
boxes (
    box_id        BYTEA PRIMARY KEY,
    tx_id         BYTEA NOT NULL REFERENCES transactions(tx_id),
    header_id     BYTEA NOT NULL,
    height        BIGINT NOT NULL,
    output_index  INT NOT NULL,
    ergo_tree     BYTEA NOT NULL,
    ergo_tree_hash BYTEA NOT NULL,        -- blake2b256(ergo_tree), fixed-width for indexing
    address       TEXT NOT NULL,
    value         BIGINT NOT NULL,
    spent_tx_id   BYTEA,
    spent_height  BIGINT
)

-- Token amounts per box
box_tokens (
    box_id        BYTEA NOT NULL REFERENCES boxes(box_id),
    token_id      BYTEA NOT NULL,
    amount        BIGINT NOT NULL,
    PRIMARY KEY (box_id, token_id)
)

-- Box registers R4-R9 (non-empty only)
box_registers (
    box_id        BYTEA NOT NULL REFERENCES boxes(box_id),
    register_id   SMALLINT NOT NULL,
    serialized    BYTEA NOT NULL,
    PRIMARY KEY (box_id, register_id)
)

-- Token metadata (from minting tx's first output R4-R6, per EIP-4)
tokens (
    token_id      BYTEA PRIMARY KEY,
    minting_tx_id BYTEA NOT NULL,
    minting_height BIGINT NOT NULL,
    name          TEXT,
    description   TEXT,
    decimals      INT
)

-- Indexer state tracking
indexer_state (
    key           TEXT PRIMARY KEY,
    value         TEXT NOT NULL
)
```

### Indexes

```sql
boxes(address) WHERE spent_tx_id IS NULL   -- unspent boxes by address
boxes(ergo_tree_hash) WHERE spent_tx_id IS NULL -- unspent boxes by ErgoTree hash
boxes(address)                              -- full history by address
box_tokens(token_id)                        -- token holder queries
transactions(height)                        -- block transaction listing
```

### Reorg Handling

On reorg detection (new block at already-indexed height with different header_id):
1. `DELETE` from all tables `WHERE height >= fork_point`
2. Reset `spent_tx_id`/`spent_height` to `NULL` on boxes spent by rolled-back blocks
3. All within a single DB transaction (atomic rollback)
4. Re-index from fork point

## Database Backend Abstraction

```rust
#[async_trait]
trait IndexerDb: Send + Sync {
    // Write path
    async fn get_indexed_height(&self) -> Result<Option<u64>>;
    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>>;
    async fn insert_block(&self, block: &IndexedBlock) -> Result<()>;
    async fn rollback_to(&self, height: u64) -> Result<()>;

    // Read path
    async fn get_block_by_height(&self, height: u64) -> Result<Option<BlockRow>>;
    async fn get_block_by_id(&self, id: &[u8]) -> Result<Option<BlockRow>>;
    async fn get_transactions(&self, header_id: &[u8]) -> Result<Vec<TxRow>>;
    async fn get_transaction(&self, tx_id: &[u8]) -> Result<Option<TxRow>>;
    async fn get_box(&self, box_id: &[u8]) -> Result<Option<BoxRow>>;
    async fn get_unspent_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_boxes_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_balance(&self, addr: &str) -> Result<Balance>;
    async fn get_txs_by_address(&self, addr: &str, offset: u64, limit: u64) -> Result<Page<TxRow>>;
    async fn get_unspent_by_ergo_tree(&self, hash: &[u8], offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_token(&self, token_id: &[u8]) -> Result<Option<TokenRow>>;
    async fn get_token_holders(&self, token_id: &[u8], offset: u64, limit: u64) -> Result<Page<HolderRow>>;
    async fn get_token_boxes(&self, token_id: &[u8], offset: u64, limit: u64) -> Result<Page<BoxRow>>;
    async fn get_tokens(&self, offset: u64, limit: u64) -> Result<Page<TokenRow>>;
    async fn get_stats(&self) -> Result<NetworkStats>;
    async fn get_daily_stats(&self, days: u32) -> Result<Vec<DailyStats>>;
}
```

Two implementations:
- `SqliteDb` — `rusqlite` + `tokio::task::spawn_blocking`, WAL mode, single writer + read pool
- `PgDb` — `sqlx::PgPool`, native async

Selected at startup by `--db` URL. Rest of codebase sees `Arc<dyn IndexerDb>`.

Schema versioning via `indexer_state` table (`schema_version` key). Migrations run on startup. Separate `.sql` files per backend where dialect diverges.

## Sync Pipeline

```
1. Startup:
   - Read indexer_state.height → last_indexed
   - If NULL → start from height 1 (Ergo genesis)

2. Steady state loop:
   - GET /info/wait?after={last_indexed}
   - Returns immediately if node is ahead, blocks otherwise

3. For each height (last_indexed + 1) to fullHeight:
   - Reorg check: if blocks table has this height with different header_id → rollback
   - GET /blocks/at/{height} → header_id
   - GET /blocks/{header_id}/header → header data
   - GET /blocks/{header_id}/transactions → tx list

4. Parse block:
   - Block metadata → blocks row
   - Per transaction:
     - transactions row
     - Per input: mark box as spent (spent_tx_id, spent_height)
     - Per output: boxes row, box_tokens rows, box_registers rows
     - Token minting detection (token_id == first_input_box_id) → tokens row

5. Write entire block in single DB transaction
   - Update indexer_state.height
   - Commit
```

Error handling: exponential backoff on node unreachable (1s → 2s → 4s, cap 30s). Log and retry, never panic.

## Query API

Axum + utoipa. Swagger UI at `/swagger`. All endpoints under `/api/v1/`.

### Endpoints

**Blocks:**
```
GET /api/v1/blocks?offset=0&limit=20
GET /api/v1/blocks/height/{height}
GET /api/v1/blocks/{header_id}
GET /api/v1/blocks/{header_id}/transactions
```

**Transactions:**
```
GET /api/v1/transactions/{tx_id}
GET /api/v1/addresses/{address}/transactions?offset=0&limit=50
```

**Boxes:**
```
GET /api/v1/boxes/{box_id}
GET /api/v1/addresses/{address}/balance
GET /api/v1/addresses/{address}/unspent?offset=0&limit=50
GET /api/v1/addresses/{address}/boxes?offset=0&limit=50
GET /api/v1/ergo-tree/{hash}/unspent?offset=0&limit=50
```

**Tokens:**
```
GET /api/v1/tokens?offset=0&limit=50
GET /api/v1/tokens/{token_id}
GET /api/v1/tokens/{token_id}/holders?offset=0&limit=50
GET /api/v1/tokens/{token_id}/boxes?offset=0&limit=50
```

**Stats:**
```
GET /api/v1/stats
GET /api/v1/stats/daily?days=30
```

**Info:**
```
GET /api/v1/info
```

**Common patterns:**
- Pagination: `offset`/`limit` on all list endpoints (default 50, max 100)
- Responses include `total` count
- Box responses resolve token names/decimals from `tokens` table
- Transaction responses include input box IDs and output boxes inline

## Node-Side Change: Long-Poll Endpoint

One new endpoint in the core API crate:

```
GET /info/wait?after={height}
```

- If `fullHeight > after` → returns immediately with standard `/info` response
- If `fullHeight <= after` → holds until new block validated, then returns
- 30-second timeout → 204 No Content (client re-requests)

Implementation: `tokio::sync::watch` channel in `ApiState`. Validation pipeline sends `tx.send(new_height)` after committing a block. Handler clones the receiver and `select!`s on `rx.changed()` vs timeout.

This is the only change to the core node.

## Crate Layout

```
addons/indexer/
  Cargo.toml
  src/
    main.rs             # CLI (clap), mode dispatch
    sync.rs             # block poller + parser pipeline
    db/
      mod.rs            # IndexerDb trait, types, Page<T>
      sqlite.rs         # SqliteDb implementation
      postgres.rs       # PgDb implementation (behind feature flag)
      migrations/       # numbered .sql files
    api/
      mod.rs            # axum router, swagger setup
      blocks.rs         # block endpoints
      transactions.rs   # tx endpoints
      boxes.rs          # box/UTXO endpoints
      tokens.rs         # token endpoints
      stats.rs          # stats endpoints
    parser.rs           # block JSON → IndexedBlock conversion
    types.rs            # shared types
```

## Feature Flags

```toml
[features]
default = ["sqlite"]
sqlite = ["rusqlite"]
postgres = ["sqlx"]
```

Both can be compiled together. Runtime `--db` URL determines which backend is used.

## Build & Packaging

- Main workspace excludes `addons/indexer/`
- Standalone build: `cd addons/indexer && cargo build --release`
- PG support: `cargo build --release --features postgres`
- `build-deb --with-indexer` produces `ergo-node-rust-indexer_X.Y.Z_amd64.deb`
- Systemd service: `ergo-indexer.service` (After=ergo-node-rust.service)
- Config: `/etc/ergo-node/indexer.toml`
- Binary: `/usr/bin/ergo-indexer`
- Data: `/var/lib/ergo-indexer/` (SQLite default path)

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI parsing |
| `tokio` | async runtime |
| `axum` | query API |
| `utoipa` + `utoipa-swagger-ui` | OpenAPI + Swagger |
| `reqwest` + `rustls` | node REST client |
| `rusqlite` | SQLite backend |
| `sqlx` | PostgreSQL backend (optional) |
| `serde` / `serde_json` | serialization |
| `tracing` / `tracing-subscriber` | logging |
| `anyhow` | error handling |
| `ergo-lib` / `ergo-chain-types` | Ergo type parsing (address derivation, register decoding) |
