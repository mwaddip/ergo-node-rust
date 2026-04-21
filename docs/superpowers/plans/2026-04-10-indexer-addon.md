# Indexer Addon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a standalone indexer addon that indexes Ergo blockchain data into SQL and serves rich query endpoints via REST API with Swagger UI.

**Architecture:** Separate crate at `addons/indexer/`, excluded from main workspace. Communicates with the node via REST API. SQLite by default, PostgreSQL optional via feature flag. Three CLI modes: `sync` (writer), `serve` (reader), combined (both). One small change to the core node: a long-poll `/info/wait` endpoint.

**Tech Stack:** Rust, axum 0.8, utoipa (Swagger), rusqlite (SQLite), sqlx (PostgreSQL), reqwest + rustls, clap 4, tokio, ergo-lib/ergo-chain-types (address derivation, type parsing)

**Spec:** `docs/superpowers/specs/2026-04-10-indexer-addon-design.md`

---

## File Map

### Core node change (1 file modified, 2 files modified)

| File | Action | Purpose |
|------|--------|---------|
| `api/src/lib.rs` | Modify | Add `height_watch: tokio::sync::watch::Receiver<u32>` to `ApiState`, add `/info/wait` route |
| `api/src/handlers.rs` | Modify | Add `info_wait` handler |
| `src/main.rs` | Modify | Create `watch::channel`, wire sender into `Validator`, receiver into `ApiState` |

### Indexer addon (new crate)

| File | Action | Purpose |
|------|--------|---------|
| `addons/indexer/Cargo.toml` | Create | Standalone workspace, feature flags for sqlite/postgres |
| `addons/indexer/src/main.rs` | Create | CLI (clap), mode dispatch, startup |
| `addons/indexer/src/types.rs` | Create | Shared types: `IndexedBlock`, `BoxRow`, `TxRow`, `TokenRow`, `Balance`, `Page<T>`, API response types |
| `addons/indexer/src/node_client.rs` | Create | HTTP client for node REST API (`/info/wait`, `/blocks/at`, `/blocks/{id}/header`, `/blocks/{id}/transactions`) |
| `addons/indexer/src/parser.rs` | Create | Block JSON -> `IndexedBlock` conversion (address derivation, token minting detection, register extraction) |
| `addons/indexer/src/db/mod.rs` | Create | `IndexerDb` trait, `Page<T>`, open_db() dispatch |
| `addons/indexer/src/db/sqlite.rs` | Create | `SqliteDb` — schema creation, migrations, all trait methods |
| `addons/indexer/src/db/postgres.rs` | Create | `PgDb` — same trait, sqlx-based (behind `postgres` feature) |
| `addons/indexer/src/sync.rs` | Create | Sync loop: long-poll, fetch, parse, write, reorg detection |
| `addons/indexer/src/api/mod.rs` | Create | Axum router, Swagger UI setup, shared query param types |
| `addons/indexer/src/api/blocks.rs` | Create | Block query endpoints |
| `addons/indexer/src/api/transactions.rs` | Create | Transaction query endpoints |
| `addons/indexer/src/api/boxes.rs` | Create | Box/UTXO/address query endpoints |
| `addons/indexer/src/api/tokens.rs` | Create | Token query endpoints |
| `addons/indexer/src/api/stats.rs` | Create | Stats + info endpoints |

### Build & packaging

| File | Action | Purpose |
|------|--------|---------|
| `Cargo.toml` (root) | Modify | Add `"addons/indexer"` to workspace `exclude` list |
| `deploy/ergo-indexer.service` | Create | Systemd unit file |
| `build-deb` | Modify | Add `--with-indexer` flag |

---

## Task 1: Long-poll endpoint in core node

**Files:**
- Modify: `api/src/lib.rs`
- Modify: `api/src/handlers.rs`
- Modify: `src/main.rs`

This is the only change to the core node. Add a `watch` channel for validated height and a `/info/wait` endpoint that long-polls on it.

- [ ] **Step 1: Add watch receiver to ApiState**

In `api/src/lib.rs`, add the field to `ApiState`:

```rust
// In ApiState struct, after validated_height field:
    /// Watch channel for block height changes — used by /info/wait long-poll.
    pub height_watch: tokio::sync::watch::Receiver<u32>,
```

- [ ] **Step 2: Add the /info/wait route**

In `api/src/lib.rs`, in the `router()` function, add the route after the `/info` route:

```rust
        .route("/info/wait", get(handlers::info_wait))
```

- [ ] **Step 3: Add query param type and handler**

In `api/src/handlers.rs`, add the query param struct and handler:

```rust
#[derive(Deserialize)]
pub struct WaitQuery {
    after: u32,
}

// ---------------------------------------------------------------------------
// GET /info/wait?after={height}
// ---------------------------------------------------------------------------

pub async fn info_wait(
    State(state): State<ApiState>,
    Query(params): Query<WaitQuery>,
) -> Result<Json<NodeInfo>, StatusCode> {
    let current = state.validated_height.load(std::sync::atomic::Ordering::Relaxed);
    if current > params.after {
        return Ok(get_info(State(state)).await);
    }

    let mut rx = state.height_watch.clone();
    tokio::select! {
        _ = rx.changed() => Ok(get_info(State(state)).await),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => Err(StatusCode::NO_CONTENT),
    }
}
```

- [ ] **Step 4: Add `tokio::time` dependency to api crate**

In `api/Cargo.toml`, update the tokio dependency:

```toml
tokio = { version = "1", features = ["sync", "time"] }
```

- [ ] **Step 5: Create watch channel in main.rs and wire it in**

In `src/main.rs`, find where `shared_validated_height` is created (around line 1155) and add a watch channel alongside it:

```rust
let (height_watch_tx, height_watch_rx) = tokio::sync::watch::channel(0u32);
```

Wire `height_watch_rx` into the `ApiState` construction (search for `ApiState {`):

```rust
        height_watch: height_watch_rx,
```

- [ ] **Step 6: Send on height_watch_tx when a block is validated**

In `src/main.rs`, in the `Validator` struct, add a `height_watch_tx` field:

```rust
    height_watch_tx: tokio::sync::watch::Sender<u32>,
```

Add it to the `Validator::new()` constructor signature and body. In `validate_block()`, after the `self.shared_height.store(...)` line (line ~279), add:

```rust
            let _ = self.height_watch_tx.send(self.validated_height());
```

Also add it after `reset_to()` (line ~332):

```rust
        let _ = self.height_watch_tx.send(self.validated_height());
```

Thread the sender through all `Validator::new()` call sites in `main.rs` (there are 4-5 of them — search for `Validator::new(`).

- [ ] **Step 7: Build and test the core node compiles**

```bash
cargo build 2>&1 | tail -5
```

Expected: compiles without errors.

- [ ] **Step 8: Commit**

```bash
git add api/src/lib.rs api/src/handlers.rs api/Cargo.toml src/main.rs
git commit -m "feat: add /info/wait long-poll endpoint for indexer"
```

---

## Task 2: Indexer crate scaffold + CLI

**Files:**
- Create: `addons/indexer/Cargo.toml`
- Create: `addons/indexer/src/main.rs`
- Create: `addons/indexer/src/types.rs`
- Modify: `Cargo.toml` (root workspace exclude)

- [ ] **Step 1: Add indexer to workspace exclude list**

In root `Cargo.toml`, change:

```toml
exclude = ["addons/fastsync"]
```

to:

```toml
exclude = ["addons/fastsync", "addons/indexer"]
```

- [ ] **Step 2: Create Cargo.toml**

Create `addons/indexer/Cargo.toml`:

```toml
[workspace]

[package]
name = "ergo-indexer"
version = "0.1.0"
edition = "2021"
description = "Blockchain indexer for ergo-node-rust — rich query API over SQLite or PostgreSQL"

[features]
default = ["sqlite"]
sqlite = ["dep:rusqlite"]
postgres = ["dep:sqlx"]

[dependencies]
# Ergo types — address derivation, register decoding
ergo-chain-types = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "3ca4af0b" }
ergo-lib = { git = "https://github.com/mwaddip/sigma-rust.git", rev = "3ca4af0b" }

# HTTP client — rustls for static binary
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "signal"] }

# Query API
axum = "0.8"
utoipa = { version = "5", features = ["axum_extras"] }
utoipa-swagger-ui = { version = "9", features = ["axum"] }

# Database backends
rusqlite = { version = "0.33", features = ["bundled"], optional = true }
sqlx = { version = "0.8", features = ["runtime-tokio-rustls", "postgres"], optional = true }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["arbitrary_precision"] }
hex = "0.4"

# CLI
clap = { version = "4", features = ["derive"] }

# Crypto (blake2b256 for ergo_tree_hash)
blake2 = "0.10"

# Misc
anyhow = "1"
async-trait = "0.1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 3: Create types.rs**

Create `addons/indexer/src/types.rs`:

```rust
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Node REST API response types (deserialization)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub full_height: u32,
    pub headers_height: u32,
    pub best_full_header_id: String,
    pub network: String,
}

// ---------------------------------------------------------------------------
// Database row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlockRow {
    pub height: u64,
    pub header_id: String,
    pub timestamp: u64,
    pub difficulty: u64,
    pub miner_pk: String,
    pub block_size: u32,
    pub tx_count: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TxRow {
    pub tx_id: String,
    pub header_id: String,
    pub height: u64,
    pub tx_index: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoxRow {
    pub box_id: String,
    pub tx_id: String,
    pub header_id: String,
    pub height: u64,
    pub output_index: u32,
    pub ergo_tree: String,
    pub ergo_tree_hash: String,
    pub address: String,
    pub value: u64,
    pub spent_tx_id: Option<String>,
    pub spent_height: Option<u64>,
    pub tokens: Vec<BoxTokenRow>,
    pub registers: Vec<RegisterRow>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoxTokenRow {
    pub token_id: String,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRow {
    pub register_id: u8,
    pub serialized: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenRow {
    pub token_id: String,
    pub minting_tx_id: String,
    pub minting_height: u64,
    pub name: Option<String>,
    pub description: Option<String>,
    pub decimals: Option<i32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HolderRow {
    pub address: String,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub nano_ergs: u64,
    pub tokens: Vec<TokenBalance>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenBalance {
    pub token_id: String,
    pub amount: u64,
    pub name: Option<String>,
    pub decimals: Option<i32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStats {
    pub indexed_height: u64,
    pub total_blocks: u64,
    pub total_transactions: u64,
    pub total_boxes: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DailyStats {
    pub date: String,
    pub tx_count: u64,
    pub block_count: u64,
    pub volume: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexerInfo {
    pub indexed_height: u64,
    pub node_height: u32,
    pub backend: String,
    pub uptime_secs: u64,
}

/// Paginated response wrapper.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Page<T: Serialize + ToSchema> {
    pub items: Vec<T>,
    pub total: u64,
}

// ---------------------------------------------------------------------------
// Intermediate types for the sync pipeline (not API-facing)
// ---------------------------------------------------------------------------

/// A fully parsed block ready for DB insertion.
pub struct IndexedBlock {
    pub height: u64,
    pub header_id: [u8; 32],
    pub timestamp: u64,
    pub difficulty: u64,
    pub miner_pk: Vec<u8>,
    pub block_size: u32,
    pub transactions: Vec<IndexedTx>,
}

pub struct IndexedTx {
    pub tx_id: [u8; 32],
    pub tx_index: u32,
    pub size: u32,
    pub inputs: Vec<InputRef>,
    pub outputs: Vec<IndexedBox>,
}

pub struct InputRef {
    pub box_id: [u8; 32],
}

pub struct IndexedBox {
    pub box_id: [u8; 32],
    pub output_index: u32,
    pub ergo_tree: Vec<u8>,
    pub ergo_tree_hash: [u8; 32],
    pub address: String,
    pub value: u64,
    pub tokens: Vec<IndexedToken>,
    pub registers: Vec<IndexedRegister>,
    /// If this output mints a new token, this holds the EIP-4 metadata.
    pub minted_token: Option<MintedToken>,
}

pub struct IndexedToken {
    pub token_id: [u8; 32],
    pub amount: u64,
}

pub struct IndexedRegister {
    pub register_id: u8,
    pub serialized: Vec<u8>,
}

pub struct MintedToken {
    pub token_id: [u8; 32],
    pub name: Option<String>,
    pub description: Option<String>,
    pub decimals: Option<i32>,
}
```

- [ ] **Step 4: Create main.rs with CLI parsing and mode dispatch**

Create `addons/indexer/src/main.rs`:

```rust
mod api;
mod db;
mod node_client;
mod parser;
mod sync;
mod types;

use std::net::SocketAddr;
use std::time::Instant;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ergo-indexer", version, about = "Blockchain indexer for ergo-node-rust")]
struct Cli {
    /// Node REST API URL.
    #[arg(long, default_value = "http://127.0.0.1:9052")]
    node_url: String,

    /// Database path (SQLite) or URL (postgres://...).
    #[arg(long, default_value = "./indexer.db")]
    db: String,

    /// Query API bind address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Resume from specific height (default: auto-detect from DB).
    #[arg(long)]
    start_height: Option<u64>,

    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// Index blocks only — no query API.
    Sync,
    /// Query API only — read-only database access.
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ergo_indexer=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let start = Instant::now();

    let db = db::open_db(&cli.db).await?;

    match cli.mode {
        Some(Mode::Sync) => {
            tracing::info!("starting in sync-only mode");
            sync::run(db, &cli.node_url, cli.start_height).await
        }
        Some(Mode::Serve) => {
            tracing::info!(%cli.bind, "starting in serve-only mode");
            api::serve(db, cli.bind, start).await
        }
        None => {
            tracing::info!(%cli.bind, "starting in combined mode (sync + serve)");
            let db2 = db::open_db(&cli.db).await?;
            let sync_handle = tokio::spawn({
                let node_url = cli.node_url.clone();
                let start_height = cli.start_height;
                async move { sync::run(db, &node_url, start_height).await }
            });
            let api_handle = tokio::spawn(api::serve(db2, cli.bind, start));
            tokio::select! {
                r = sync_handle => r?,
                r = api_handle => r?,
            }
        }
    }
}
```

- [ ] **Step 5: Create stub modules so it compiles**

Create `addons/indexer/src/node_client.rs`:

```rust
// Node REST API client — implemented in Task 3
```

Create `addons/indexer/src/parser.rs`:

```rust
// Block JSON → IndexedBlock parser — implemented in Task 4
```

Create `addons/indexer/src/db/mod.rs`:

```rust
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::types::*;

#[async_trait]
pub trait IndexerDb: Send + Sync {
    // Write path
    async fn get_indexed_height(&self) -> Result<Option<u64>>;
    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>>;
    async fn insert_block(&self, block: &IndexedBlock) -> Result<()>;
    async fn rollback_to(&self, height: u64) -> Result<()>;

    // Read path
    async fn get_block_by_height(&self, height: u64) -> Result<Option<BlockRow>>;
    async fn get_block_by_id(&self, id: &[u8]) -> Result<Option<BlockRow>>;
    async fn get_blocks(&self, offset: u64, limit: u64) -> Result<Page<BlockRow>>;
    async fn get_transactions_for_block(&self, header_id: &[u8]) -> Result<Vec<TxRow>>;
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

/// Open database based on URL scheme. postgres:// → PgDb, else → SqliteDb.
pub async fn open_db(url: &str) -> Result<Arc<dyn IndexerDb>> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        #[cfg(feature = "postgres")]
        {
            let db = postgres::PgDb::connect(url).await?;
            return Ok(Arc::new(db));
        }
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!("PostgreSQL support requires --features postgres");
    }

    #[cfg(feature = "sqlite")]
    {
        let db = sqlite::SqliteDb::open(url)?;
        return Ok(Arc::new(db));
    }
    #[cfg(not(feature = "sqlite"))]
    anyhow::bail!("No database backend enabled — build with --features sqlite or --features postgres");
}

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;
```

Create `addons/indexer/src/db/sqlite.rs`:

```rust
// SQLite backend — implemented in Task 5
```

Create `addons/indexer/src/sync.rs`:

```rust
use std::sync::Arc;
use crate::db::IndexerDb;

pub async fn run(
    _db: Arc<dyn IndexerDb>,
    _node_url: &str,
    _start_height: Option<u64>,
) -> anyhow::Result<()> {
    // Implemented in Task 6
    todo!()
}
```

Create `addons/indexer/src/api/mod.rs`:

```rust
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use crate::db::IndexerDb;

pub mod blocks;
pub mod boxes;
pub mod stats;
pub mod tokens;
pub mod transactions;

pub async fn serve(
    _db: Arc<dyn IndexerDb>,
    _bind: SocketAddr,
    _start: Instant,
) -> anyhow::Result<()> {
    // Implemented in Task 7
    todo!()
}
```

Create stub files for each API module:

`addons/indexer/src/api/blocks.rs`:
```rust
// Block query endpoints — implemented in Task 7
```

`addons/indexer/src/api/transactions.rs`:
```rust
// Transaction query endpoints — implemented in Task 7
```

`addons/indexer/src/api/boxes.rs`:
```rust
// Box/UTXO/address query endpoints — implemented in Task 7
```

`addons/indexer/src/api/tokens.rs`:
```rust
// Token query endpoints — implemented in Task 7
```

`addons/indexer/src/api/stats.rs`:
```rust
// Stats endpoints — implemented in Task 7
```

- [ ] **Step 6: Verify it compiles**

```bash
cd addons/indexer && cargo check 2>&1 | tail -5
```

Expected: compiles (with warnings about dead code and todo!).

- [ ] **Step 7: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add Cargo.toml addons/indexer/
git commit -m "feat: scaffold indexer addon crate with CLI and types"
```

---

## Task 3: Node REST client

**Files:**
- Create: `addons/indexer/src/node_client.rs`

HTTP client that talks to the ergo-node-rust REST API.

- [ ] **Step 1: Implement NodeClient**

Replace `addons/indexer/src/node_client.rs` with:

```rust
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::types::NodeInfo;

pub struct NodeClient {
    client: Client,
    base_url: String,
}

/// Raw header JSON from the node — we keep it as serde_json::Value
/// because ergo-chain-types::Header's serde format may differ from
/// what we need for indexing. We extract fields manually.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderJson {
    pub id: String,
    pub height: u64,
    pub timestamp: u64,
    #[serde(alias = "nBits")]
    pub n_bits: u64,
    #[serde(default)]
    pub difficulty: Option<u64>,
    pub miner_pk: String,
    #[serde(default)]
    pub size: Option<u32>,
}

/// Block transactions response from GET /blocks/{id}/transactions.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTransactionsJson {
    pub header_id: String,
    pub transactions: Vec<serde_json::Value>,
}

impl NodeClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("failed to build HTTP client")?;
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self { client, base_url })
    }

    /// GET /info — node status.
    pub async fn info(&self) -> Result<NodeInfo> {
        let resp = self.client
            .get(format!("{}/info", self.base_url))
            .send().await
            .context("GET /info failed")?
            .error_for_status()
            .context("GET /info returned error")?;
        resp.json().await.context("GET /info parse failed")
    }

    /// GET /info/wait?after={height} — long-poll for new blocks.
    /// Returns None on 204 (timeout), Some(info) on new block.
    pub async fn info_wait(&self, after: u64) -> Result<Option<NodeInfo>> {
        let resp = self.client
            .get(format!("{}/info/wait?after={}", self.base_url, after))
            .timeout(std::time::Duration::from_secs(35))
            .send().await
            .context("GET /info/wait failed")?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let info = resp.error_for_status()
            .context("GET /info/wait error")?
            .json().await
            .context("GET /info/wait parse failed")?;
        Ok(Some(info))
    }

    /// GET /blocks/at/{height} — block IDs at height.
    pub async fn block_ids_at(&self, height: u64) -> Result<Vec<String>> {
        let resp = self.client
            .get(format!("{}/blocks/at/{}", self.base_url, height))
            .send().await
            .context("GET /blocks/at failed")?
            .error_for_status()
            .context("GET /blocks/at error")?;
        resp.json().await.context("GET /blocks/at parse failed")
    }

    /// GET /blocks/{header_id}/header — full header.
    pub async fn header(&self, header_id: &str) -> Result<HeaderJson> {
        let resp = self.client
            .get(format!("{}/blocks/{}/header", self.base_url, header_id))
            .send().await
            .context("GET header failed")?
            .error_for_status()
            .context("GET header error")?;
        resp.json().await.context("GET header parse failed")
    }

    /// GET /blocks/{header_id}/transactions — block transactions.
    pub async fn transactions(&self, header_id: &str) -> Result<BlockTransactionsJson> {
        let resp = self.client
            .get(format!("{}/blocks/{}/transactions", self.base_url, header_id))
            .send().await
            .context("GET transactions failed")?
            .error_for_status()
            .context("GET transactions error")?;
        resp.json().await.context("GET transactions parse failed")
    }
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd addons/indexer && cargo check 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add addons/indexer/src/node_client.rs
git commit -m "feat(indexer): node REST client"
```

---

## Task 4: Block parser

**Files:**
- Create: `addons/indexer/src/parser.rs`

Converts JSON from the node REST API into `IndexedBlock` for DB insertion. Handles address derivation from ErgoTree, token minting detection, and register extraction.

- [ ] **Step 1: Implement the parser**

Replace `addons/indexer/src/parser.rs` with:

```rust
use anyhow::{Context, Result};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

use crate::node_client::{BlockTransactionsJson, HeaderJson};
use crate::types::*;

type Blake2b256 = Blake2b<U32>;

fn blake2b256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Parse a header + transactions response into an IndexedBlock.
pub fn parse_block(
    header: &HeaderJson,
    txs_json: &BlockTransactionsJson,
    network: NetworkPrefix,
) -> Result<IndexedBlock> {
    let header_id = hex_to_bytes32(&header.id)
        .context("invalid header ID hex")?;
    let miner_pk = hex::decode(&header.miner_pk)
        .context("invalid miner_pk hex")?;

    let encoder = AddressEncoder::new(network);

    let mut transactions = Vec::with_capacity(txs_json.transactions.len());
    let mut block_size: u32 = 0;

    for (tx_index, tx_val) in txs_json.transactions.iter().enumerate() {
        let tx: Transaction = serde_json::from_value(tx_val.clone())
            .with_context(|| format!("failed to parse tx at index {tx_index}"))?;

        let tx_bytes = tx.sigma_serialize_bytes()
            .with_context(|| format!("failed to serialize tx at index {tx_index}"))?;
        let tx_size = tx_bytes.len() as u32;
        block_size += tx_size;

        let tx_id_bytes: [u8; 32] = tx.id().into();

        // Parse inputs
        let inputs: Vec<InputRef> = tx.inputs.iter().map(|input| {
            let box_id: [u8; 32] = input.box_id.into();
            InputRef { box_id }
        }).collect();

        // Parse outputs
        let first_input_id: [u8; 32] = if !tx.inputs.is_empty() {
            tx.inputs[0].box_id.into()
        } else {
            [0u8; 32]
        };

        let mut outputs = Vec::with_capacity(tx.outputs.len());
        for (out_idx, ergo_box) in tx.outputs.iter().enumerate() {
            let box_id: [u8; 32] = ergo_box.box_id().into();
            let ergo_tree_bytes = ergo_box.ergo_tree.sigma_serialize_bytes()
                .unwrap_or_default();
            let ergo_tree_hash = blake2b256(&ergo_tree_bytes);

            let address = match Address::recreate_from_ergo_tree(&ergo_box.ergo_tree) {
                Ok(addr) => encoder.address_to_str(&addr),
                Err(_) => hex::encode(&ergo_tree_bytes),
            };

            let tokens: Vec<IndexedToken> = ergo_box.tokens.as_ref()
                .map(|toks| toks.iter().map(|t| {
                    let tid: [u8; 32] = t.token_id.into();
                    IndexedToken { token_id: tid, amount: *t.amount.as_u64() }
                }).collect())
                .unwrap_or_default();

            // Token minting detection: token_id == first input's box_id
            let minted_token = tokens.iter().find(|t| t.token_id == first_input_id).map(|_| {
                let name = extract_register_string(ergo_box, 4);
                let description = extract_register_string(ergo_box, 5);
                let decimals = extract_register_int(ergo_box, 6);
                MintedToken {
                    token_id: first_input_id,
                    name,
                    description,
                    decimals,
                }
            });

            let registers = extract_registers(ergo_box);

            outputs.push(IndexedBox {
                box_id,
                output_index: out_idx as u32,
                ergo_tree: ergo_tree_bytes,
                ergo_tree_hash,
                address,
                value: *ergo_box.value.as_u64(),
                tokens,
                registers,
                minted_token,
            });
        }

        transactions.push(IndexedTx {
            tx_id: tx_id_bytes,
            tx_index: tx_index as u32,
            size: tx_size,
            inputs,
            outputs,
        });
    }

    Ok(IndexedBlock {
        height: header.height,
        header_id,
        timestamp: header.timestamp,
        difficulty: header.difficulty.unwrap_or(header.n_bits),
        miner_pk,
        block_size,
        transactions,
    })
}

fn hex_to_bytes32(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("expected 32 bytes"))?;
    Ok(arr)
}

/// Try to extract a UTF-8 string from register R{idx} (Coll[Byte]).
fn extract_register_string(ergo_box: &ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox, idx: usize) -> Option<String> {
    use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
    use ergo_lib::ergotree_ir::mir::constant::Constant;

    let reg_id = NonMandatoryRegisterId::try_from(idx as u8).ok()?;
    let constant: &Constant = ergo_box.additional_registers.get(reg_id)?;

    // Try to extract as Coll[Byte] → UTF-8 string
    let bytes: Vec<u8> = constant.clone().try_extract_into().ok()?;
    String::from_utf8(bytes).ok()
}

/// Try to extract an i32 from register R{idx}.
fn extract_register_int(ergo_box: &ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox, idx: usize) -> Option<i32> {
    use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
    use ergo_lib::ergotree_ir::mir::constant::Constant;

    let reg_id = NonMandatoryRegisterId::try_from(idx as u8).ok()?;
    let constant: &Constant = ergo_box.additional_registers.get(reg_id)?;
    constant.clone().try_extract_into::<i32>().ok()
}

/// Extract non-empty registers R4-R9 as serialized bytes.
fn extract_registers(ergo_box: &ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox) -> Vec<IndexedRegister> {
    use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

    let mut regs = Vec::new();
    for i in 4u8..=9 {
        if let Ok(reg_id) = NonMandatoryRegisterId::try_from(i) {
            if let Some(constant) = ergo_box.additional_registers.get(reg_id) {
                if let Ok(bytes) = constant.sigma_serialize_bytes() {
                    regs.push(IndexedRegister {
                        register_id: i,
                        serialized: bytes,
                    });
                }
            }
        }
    }
    regs
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd addons/indexer && cargo check 2>&1 | tail -5
```

Note: The exact ergo-lib API for `tx.inputs`, `ergo_box.tokens`, `box_id.into()`, `NonMandatoryRegisterId`, and `try_extract_into` needs to be verified against the sigma-rust fork at rev `3ca4af0b`. If the API differs, adjust the field access accordingly — the structure is correct, only the accessor names may vary.

- [ ] **Step 3: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add addons/indexer/src/parser.rs
git commit -m "feat(indexer): block parser — JSON to IndexedBlock"
```

---

## Task 5: SQLite backend

**Files:**
- Create: `addons/indexer/src/db/sqlite.rs`

Full `IndexerDb` implementation over rusqlite with WAL mode.

- [ ] **Step 1: Schema and connection setup**

Replace `addons/indexer/src/db/sqlite.rs` with the full implementation. This is a large file — implement it in stages. Start with the schema and write path:

```rust
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::db::IndexerDb;
use crate::types::*;

const SCHEMA_VERSION: u32 = 1;

const CREATE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS indexer_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS blocks (
    height     INTEGER PRIMARY KEY,
    header_id  BLOB NOT NULL UNIQUE,
    timestamp  INTEGER NOT NULL,
    difficulty INTEGER NOT NULL,
    miner_pk   BLOB NOT NULL,
    block_size INTEGER NOT NULL,
    tx_count   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS transactions (
    tx_id     BLOB PRIMARY KEY,
    header_id BLOB NOT NULL REFERENCES blocks(header_id),
    height    INTEGER NOT NULL,
    tx_index  INTEGER NOT NULL,
    size      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS boxes (
    box_id         BLOB PRIMARY KEY,
    tx_id          BLOB NOT NULL REFERENCES transactions(tx_id),
    header_id      BLOB NOT NULL,
    height         INTEGER NOT NULL,
    output_index   INTEGER NOT NULL,
    ergo_tree      BLOB NOT NULL,
    ergo_tree_hash BLOB NOT NULL,
    address        TEXT NOT NULL,
    value          INTEGER NOT NULL,
    spent_tx_id    BLOB,
    spent_height   INTEGER
);

CREATE TABLE IF NOT EXISTS box_tokens (
    box_id   BLOB NOT NULL REFERENCES boxes(box_id),
    token_id BLOB NOT NULL,
    amount   INTEGER NOT NULL,
    PRIMARY KEY (box_id, token_id)
);

CREATE TABLE IF NOT EXISTS box_registers (
    box_id      BLOB NOT NULL REFERENCES boxes(box_id),
    register_id INTEGER NOT NULL,
    serialized  BLOB NOT NULL,
    PRIMARY KEY (box_id, register_id)
);

CREATE TABLE IF NOT EXISTS tokens (
    token_id       BLOB PRIMARY KEY,
    minting_tx_id  BLOB NOT NULL,
    minting_height INTEGER NOT NULL,
    name           TEXT,
    description    TEXT,
    decimals       INTEGER
);

CREATE INDEX IF NOT EXISTS idx_boxes_address_unspent ON boxes(address) WHERE spent_tx_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_boxes_ergo_tree_hash_unspent ON boxes(ergo_tree_hash) WHERE spent_tx_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_boxes_address ON boxes(address);
CREATE INDEX IF NOT EXISTS idx_box_tokens_token_id ON box_tokens(token_id);
CREATE INDEX IF NOT EXISTS idx_transactions_height ON transactions(height);
CREATE INDEX IF NOT EXISTS idx_boxes_height ON boxes(height);
";

pub struct SqliteDb {
    /// Single writer connection (serialized via Mutex).
    writer: Arc<Mutex<Connection>>,
    /// Read-only connection (WAL allows concurrent reads).
    reader: Connection,
}

impl SqliteDb {
    pub fn open(path: &str) -> Result<Self> {
        let writer = Connection::open(path)
            .with_context(|| format!("failed to open SQLite at {path}"))?;
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        writer.execute_batch(CREATE_TABLES)?;

        // Initialize schema version if not present
        writer.execute(
            "INSERT OR IGNORE INTO indexer_state (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;

        let reader = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).with_context(|| format!("failed to open read-only SQLite at {path}"))?;
        reader.execute_batch("PRAGMA query_only=ON;")?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            reader,
        })
    }
}
```

- [ ] **Step 2: Implement write-path trait methods**

Add the write-path methods to the `#[async_trait] impl IndexerDb for SqliteDb` block. Use `tokio::task::spawn_blocking` for all rusqlite calls since they're synchronous:

```rust
#[async_trait]
impl IndexerDb for SqliteDb {
    async fn get_indexed_height(&self) -> Result<Option<u64>> {
        let conn = self.writer.lock().await;
        let height: Option<String> = conn.query_row(
            "SELECT value FROM indexer_state WHERE key = 'height'",
            [],
            |row| row.get(0),
        ).optional()?;
        Ok(height.and_then(|s| s.parse().ok()))
    }

    async fn get_block_id_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let conn = self.writer.lock().await;
        let id: Option<Vec<u8>> = conn.query_row(
            "SELECT header_id FROM blocks WHERE height = ?1",
            params![height as i64],
            |row| row.get(0),
        ).optional()?;
        Ok(id)
    }

    async fn insert_block(&self, block: &IndexedBlock) -> Result<()> {
        let conn = self.writer.lock().await;
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO blocks (height, header_id, timestamp, difficulty, miner_pk, block_size, tx_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                block.height as i64,
                block.header_id.as_slice(),
                block.timestamp as i64,
                block.difficulty as i64,
                block.miner_pk.as_slice(),
                block.block_size,
                block.transactions.len() as u32,
            ],
        )?;

        for itx in &block.transactions {
            tx.execute(
                "INSERT INTO transactions (tx_id, header_id, height, tx_index, size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    itx.tx_id.as_slice(),
                    block.header_id.as_slice(),
                    block.height as i64,
                    itx.tx_index,
                    itx.size,
                ],
            )?;

            // Mark spent inputs
            for input in &itx.inputs {
                tx.execute(
                    "UPDATE boxes SET spent_tx_id = ?1, spent_height = ?2 WHERE box_id = ?3",
                    params![itx.tx_id.as_slice(), block.height as i64, input.box_id.as_slice()],
                )?;
            }

            // Insert outputs
            for out in &itx.outputs {
                tx.execute(
                    "INSERT INTO boxes (box_id, tx_id, header_id, height, output_index, ergo_tree, ergo_tree_hash, address, value, spent_tx_id, spent_height)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)",
                    params![
                        out.box_id.as_slice(),
                        itx.tx_id.as_slice(),
                        block.header_id.as_slice(),
                        block.height as i64,
                        out.output_index,
                        out.ergo_tree.as_slice(),
                        out.ergo_tree_hash.as_slice(),
                        out.address,
                        out.value as i64,
                    ],
                )?;

                for tok in &out.tokens {
                    tx.execute(
                        "INSERT INTO box_tokens (box_id, token_id, amount) VALUES (?1, ?2, ?3)",
                        params![out.box_id.as_slice(), tok.token_id.as_slice(), tok.amount as i64],
                    )?;
                }

                for reg in &out.registers {
                    tx.execute(
                        "INSERT INTO box_registers (box_id, register_id, serialized) VALUES (?1, ?2, ?3)",
                        params![out.box_id.as_slice(), reg.register_id as i32, reg.serialized.as_slice()],
                    )?;
                }

                if let Some(minted) = &out.minted_token {
                    tx.execute(
                        "INSERT OR IGNORE INTO tokens (token_id, minting_tx_id, minting_height, name, description, decimals)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            minted.token_id.as_slice(),
                            itx.tx_id.as_slice(),
                            block.height as i64,
                            minted.name,
                            minted.description,
                            minted.decimals,
                        ],
                    )?;
                }
            }
        }

        // Update indexed height
        tx.execute(
            "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('height', ?1)",
            params![(block.height as i64).to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    async fn rollback_to(&self, height: u64) -> Result<()> {
        let conn = self.writer.lock().await;
        let tx = conn.unchecked_transaction()?;
        let h = height as i64;

        // Reset spent markers on boxes spent by blocks being rolled back
        tx.execute_batch(&format!(
            "UPDATE boxes SET spent_tx_id = NULL, spent_height = NULL
             WHERE spent_height > {h};"
        ))?;

        // Delete in reverse dependency order
        tx.execute("DELETE FROM box_registers WHERE box_id IN (SELECT box_id FROM boxes WHERE height > ?1)", params![h])?;
        tx.execute("DELETE FROM box_tokens WHERE box_id IN (SELECT box_id FROM boxes WHERE height > ?1)", params![h])?;
        tx.execute("DELETE FROM boxes WHERE height > ?1", params![h])?;
        tx.execute("DELETE FROM tokens WHERE minting_height > ?1", params![h])?;
        tx.execute("DELETE FROM transactions WHERE height > ?1", params![h])?;
        tx.execute("DELETE FROM blocks WHERE height > ?1", params![h])?;

        tx.execute(
            "INSERT OR REPLACE INTO indexer_state (key, value) VALUES ('height', ?1)",
            params![h.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ... read-path methods follow in Step 3
```

- [ ] **Step 3: Implement read-path trait methods**

Continue the `impl IndexerDb for SqliteDb` block with all the read-path queries. These are straightforward SQL queries over the reader connection. Each method follows the same pattern: query with params, map rows into types, handle pagination with `LIMIT/OFFSET` and a `COUNT(*)` subquery for `total`.

Key implementation notes:
- Use `self.reader` (not `self.writer`) for all read methods
- `BoxRow` queries need LEFT JOINs to `box_tokens` and `box_registers` — or do a primary query + secondary queries per box. For simplicity, do the primary query then batch-fetch tokens/registers for the result set.
- `get_balance` aggregates `SUM(value)` on unspent boxes and `SUM(amount) GROUP BY token_id` on their tokens
- `get_daily_stats` groups by `date(timestamp/1000, 'unixepoch')` (Ergo timestamps are milliseconds)
- `get_stats` uses `SELECT COUNT(*) FROM` each table + max height from indexer_state

Add `use rusqlite::OptionalExtension;` at the top of the file for the `.optional()` method.

Implement each method. The SQL is mechanical — the types and schema are already defined. The agent implementing this task should write the full SQL for each method rather than leaving placeholders.

- [ ] **Step 4: Verify it compiles**

```bash
cd addons/indexer && cargo check 2>&1 | tail -10
```

- [ ] **Step 5: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add addons/indexer/src/db/sqlite.rs
git commit -m "feat(indexer): SQLite backend — full IndexerDb implementation"
```

---

## Task 6: Sync pipeline

**Files:**
- Modify: `addons/indexer/src/sync.rs`

The main sync loop: long-poll, fetch, parse, write, reorg detection.

- [ ] **Step 1: Implement the sync loop**

Replace `addons/indexer/src/sync.rs` with:

```rust
use std::sync::Arc;

use anyhow::{Context, Result};
use ergo_lib::ergotree_ir::chain::address::NetworkPrefix;

use crate::db::IndexerDb;
use crate::node_client::NodeClient;
use crate::parser;

pub async fn run(
    db: Arc<dyn IndexerDb>,
    node_url: &str,
    start_height: Option<u64>,
) -> Result<()> {
    let client = NodeClient::new(node_url)?;

    // Determine starting height
    let mut last_indexed = match start_height {
        Some(h) => {
            tracing::info!(height = h, "starting from specified height");
            h.saturating_sub(1) // will index from h
        }
        None => {
            let h = db.get_indexed_height().await?;
            match h {
                Some(h) => {
                    tracing::info!(height = h, "resuming from last indexed height");
                    h
                }
                None => {
                    tracing::info!("fresh database — starting from genesis (height 1)");
                    0
                }
            }
        }
    };

    // Detect network from node info
    let info = client.info().await.context("initial /info call failed")?;
    let network = match info.network.as_str() {
        "mainnet" => NetworkPrefix::Mainnet,
        _ => NetworkPrefix::Testnet,
    };
    tracing::info!(
        network = %info.network,
        node_height = info.full_height,
        indexed_height = last_indexed,
        "sync starting"
    );

    let mut backoff = std::time::Duration::from_secs(1);
    let max_backoff = std::time::Duration::from_secs(30);

    loop {
        // Wait for new blocks via long-poll
        let node_height = match client.info_wait(last_indexed).await {
            Ok(Some(info)) => info.full_height as u64,
            Ok(None) => continue, // 204 timeout, retry
            Err(e) => {
                tracing::warn!(error = %e, "node unreachable, retrying in {:?}", backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };
        backoff = std::time::Duration::from_secs(1); // reset on success

        // Index all new blocks
        while last_indexed < node_height {
            let target = last_indexed + 1;

            // Reorg detection
            if let Some(existing_id) = db.get_block_id_at(target).await? {
                let block_ids = client.block_ids_at(target).await?;
                if let Some(canonical_id) = block_ids.first() {
                    let canonical_bytes = hex::decode(canonical_id)?;
                    if canonical_bytes != existing_id {
                        tracing::warn!(height = target, "reorg detected — rolling back");
                        // Find fork point: walk back until IDs match
                        let mut fork = target;
                        while fork > 0 {
                            fork -= 1;
                            if let Some(db_id) = db.get_block_id_at(fork).await? {
                                let ids = client.block_ids_at(fork).await?;
                                if let Some(node_id) = ids.first() {
                                    if hex::decode(node_id)? == db_id {
                                        break;
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                        tracing::info!(fork_point = fork, "rolling back to fork point");
                        db.rollback_to(fork).await?;
                        last_indexed = fork;
                        continue;
                    }
                }
            }

            // Fetch block data
            let block_ids = client.block_ids_at(target).await
                .with_context(|| format!("failed to fetch block IDs at height {target}"))?;
            let header_id = block_ids.first()
                .with_context(|| format!("no block at height {target}"))?;

            let header = client.header(header_id).await
                .with_context(|| format!("failed to fetch header {header_id}"))?;
            let txs = client.transactions(header_id).await
                .with_context(|| format!("failed to fetch transactions for {header_id}"))?;

            // Parse and store
            let indexed = parser::parse_block(&header, &txs, network)
                .with_context(|| format!("failed to parse block at height {target}"))?;
            db.insert_block(&indexed).await
                .with_context(|| format!("failed to insert block at height {target}"))?;

            last_indexed = target;

            if last_indexed % 1000 == 0 {
                tracing::info!(height = last_indexed, "indexed");
            }
        }
    }
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd addons/indexer && cargo check 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add addons/indexer/src/sync.rs
git commit -m "feat(indexer): sync pipeline — long-poll, fetch, parse, reorg"
```

---

## Task 7: Query API with Swagger UI

**Files:**
- Modify: `addons/indexer/src/api/mod.rs`
- Create: `addons/indexer/src/api/blocks.rs`
- Create: `addons/indexer/src/api/transactions.rs`
- Create: `addons/indexer/src/api/boxes.rs`
- Create: `addons/indexer/src/api/tokens.rs`
- Create: `addons/indexer/src/api/stats.rs`

Axum router with utoipa OpenAPI spec generation and Swagger UI.

- [ ] **Step 1: Implement API router and shared state**

Replace `addons/indexer/src/api/mod.rs`:

```rust
pub mod blocks;
pub mod boxes;
pub mod stats;
pub mod tokens;
pub mod transactions;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::db::IndexerDb;

#[derive(Clone)]
pub struct ApiContext {
    pub db: Arc<dyn IndexerDb>,
    pub start_time: Instant,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Ergo Indexer API", version = "1.0.0"),
    paths(
        blocks::get_blocks,
        blocks::get_block_by_height,
        blocks::get_block_by_id,
        blocks::get_block_transactions,
        transactions::get_transaction,
        transactions::get_address_transactions,
        boxes::get_box,
        boxes::get_address_balance,
        boxes::get_address_unspent,
        boxes::get_address_boxes,
        boxes::get_ergo_tree_unspent,
        tokens::get_tokens,
        tokens::get_token,
        tokens::get_token_holders,
        tokens::get_token_boxes,
        stats::get_stats,
        stats::get_daily_stats,
        stats::get_info,
    ),
    components(schemas(
        crate::types::BlockRow,
        crate::types::TxRow,
        crate::types::BoxRow,
        crate::types::BoxTokenRow,
        crate::types::RegisterRow,
        crate::types::TokenRow,
        crate::types::HolderRow,
        crate::types::Balance,
        crate::types::TokenBalance,
        crate::types::NetworkStats,
        crate::types::DailyStats,
        crate::types::IndexerInfo,
    ))
)]
struct ApiDoc;

pub async fn serve(
    db: Arc<dyn IndexerDb>,
    bind: SocketAddr,
    start_time: Instant,
) -> anyhow::Result<()> {
    let ctx = ApiContext { db, start_time };

    let app = Router::new()
        .merge(SwaggerUi::new("/swagger").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .nest("/api/v1", api_routes())
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "indexer API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn api_routes() -> Router<ApiContext> {
    use axum::routing::get;

    Router::new()
        // Blocks
        .route("/blocks", get(blocks::get_blocks))
        .route("/blocks/height/{height}", get(blocks::get_block_by_height))
        .route("/blocks/{header_id}", get(blocks::get_block_by_id))
        .route("/blocks/{header_id}/transactions", get(blocks::get_block_transactions))
        // Transactions
        .route("/transactions/{tx_id}", get(transactions::get_transaction))
        .route("/addresses/{address}/transactions", get(transactions::get_address_transactions))
        // Boxes
        .route("/boxes/{box_id}", get(boxes::get_box))
        .route("/addresses/{address}/balance", get(boxes::get_address_balance))
        .route("/addresses/{address}/unspent", get(boxes::get_address_unspent))
        .route("/addresses/{address}/boxes", get(boxes::get_address_boxes))
        .route("/ergo-tree/{hash}/unspent", get(boxes::get_ergo_tree_unspent))
        // Tokens
        .route("/tokens", get(tokens::get_tokens))
        .route("/tokens/{token_id}", get(tokens::get_token))
        .route("/tokens/{token_id}/holders", get(tokens::get_token_holders))
        .route("/tokens/{token_id}/boxes", get(tokens::get_token_boxes))
        // Stats
        .route("/stats", get(stats::get_stats))
        .route("/stats/daily", get(stats::get_daily_stats))
        .route("/info", get(stats::get_info))
}
```

- [ ] **Step 2: Implement shared pagination query param**

Add a `Pagination` struct used by all list endpoints. Put it at the bottom of `api/mod.rs`:

```rust
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Pagination {
    #[serde(default = "default_offset")]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_offset() -> u64 { 0 }
fn default_limit() -> u64 { 50 }

impl Pagination {
    pub fn clamped(&self) -> (u64, u64) {
        (self.offset, self.limit.min(100))
    }
}
```

- [ ] **Step 3: Implement block endpoints**

Replace `addons/indexer/src/api/blocks.rs` — each handler extracts state, calls `db` methods, returns JSON. Use `#[utoipa::path(...)]` attributes on each handler for OpenAPI generation. Follow the pattern:

```rust
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;

use super::{ApiContext, Pagination};
use crate::types::*;

#[utoipa::path(
    get, path = "/api/v1/blocks",
    params(("offset" = u64, Query, description = "Offset"), ("limit" = u64, Query, description = "Limit")),
    responses((status = 200, body = Page<BlockRow>))
)]
pub async fn get_blocks(
    State(ctx): State<ApiContext>,
    Query(page): Query<Pagination>,
) -> Result<Json<Page<BlockRow>>, StatusCode> {
    let (offset, limit) = page.clamped();
    ctx.db.get_blocks(offset, limit).await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
```

Implement `get_block_by_height`, `get_block_by_id`, `get_block_transactions` following the same pattern. For ID-based lookups, use `hex::decode` on the path param and return 400 on invalid hex, 404 when not found.

- [ ] **Step 4: Implement transaction endpoints**

Replace `addons/indexer/src/api/transactions.rs` — `get_transaction` (by tx_id) and `get_address_transactions` (paginated by address).

- [ ] **Step 5: Implement box/address endpoints**

Replace `addons/indexer/src/api/boxes.rs` — `get_box`, `get_address_balance`, `get_address_unspent`, `get_address_boxes`, `get_ergo_tree_unspent`.

- [ ] **Step 6: Implement token endpoints**

Replace `addons/indexer/src/api/tokens.rs` — `get_tokens` (paginated list), `get_token` (by ID), `get_token_holders`, `get_token_boxes`.

- [ ] **Step 7: Implement stats endpoints**

Replace `addons/indexer/src/api/stats.rs` — `get_stats`, `get_daily_stats` (with `days` query param), `get_info` (indexer status with uptime).

```rust
#[derive(Deserialize)]
pub struct DailyQuery {
    #[serde(default = "default_days")]
    pub days: u32,
}
fn default_days() -> u32 { 30 }
```

- [ ] **Step 8: Verify it compiles**

```bash
cd addons/indexer && cargo check 2>&1 | tail -10
```

- [ ] **Step 9: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add addons/indexer/src/api/
git commit -m "feat(indexer): query API with Swagger UI — all endpoints"
```

---

## Task 8: PostgreSQL backend (optional feature)

**Files:**
- Create: `addons/indexer/src/db/postgres.rs`

Same trait implementation as SQLite, but using sqlx with native async. Behind `postgres` feature flag.

- [ ] **Step 1: Implement PgDb**

Create `addons/indexer/src/db/postgres.rs` with the full `IndexerDb` implementation using sqlx. The schema creation SQL is the same as SQLite with minor PG syntax differences (e.g., `BYTEA` instead of `BLOB`, `BIGINT` instead of `INTEGER` for heights). Use `sqlx::PgPool` for connection pooling.

Key differences from SQLite:
- `sqlx::query!` or `sqlx::query_as!` macros for compile-time checked queries (or `sqlx::query` with runtime binding if compile-time DB isn't available)
- All methods are natively async (no `spawn_blocking` needed)
- Schema creation uses `CREATE TABLE IF NOT EXISTS` same as SQLite
- Partial indexes use same syntax: `CREATE INDEX ... WHERE spent_tx_id IS NULL`

```rust
use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::PgPool;

use crate::db::IndexerDb;
use crate::types::*;

pub struct PgDb {
    pool: PgPool,
}

impl PgDb {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPool::connect(url).await
            .context("failed to connect to PostgreSQL")?;
        let db = Self { pool };
        db.run_migrations().await?;
        Ok(db)
    }

    async fn run_migrations(&self) -> Result<()> {
        sqlx::query(CREATE_TABLES_PG)
            .execute(&self.pool).await
            .context("failed to create tables")?;
        Ok(())
    }
}
```

Implement all `IndexerDb` trait methods using `sqlx::query` / `sqlx::query_as` with the PG pool. The SQL is identical to SQLite for most queries — parameter binding syntax differs (`$1` vs `?1`).

- [ ] **Step 2: Verify it compiles with the postgres feature**

```bash
cd addons/indexer && cargo check --features postgres 2>&1 | tail -10
```

- [ ] **Step 3: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add addons/indexer/src/db/postgres.rs
git commit -m "feat(indexer): PostgreSQL backend behind 'postgres' feature flag"
```

---

## Task 9: Build script and systemd service

**Files:**
- Create: `deploy/ergo-indexer.service`
- Modify: `build-deb`

- [ ] **Step 1: Create systemd service file**

Create `deploy/ergo-indexer.service`:

```ini
[Unit]
Description=Ergo Blockchain Indexer
After=ergo-node-rust.service
Wants=ergo-node-rust.service

[Service]
Type=simple
User=ergo
ExecStart=/usr/bin/ergo-indexer --node-url http://127.0.0.1:9052 --db /var/lib/ergo-indexer/index.db --bind 0.0.0.0:8080
Restart=always
RestartSec=5
Environment=RUST_LOG=ergo_indexer=info

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 2: Add --with-indexer flag to build-deb**

Modify `build-deb` to add an optional `--with-indexer` flag that builds the indexer addon and packages it as a separate .deb. Add after the existing `dpkg-deb` line:

```bash
# --- Optional: indexer addon ---
if [[ "${1:-}" == "--with-indexer" ]]; then
    echo ""
    echo "Building ergo-indexer..."
    (cd addons/indexer && cargo build --release)

    IDX_PKG="ergo-node-rust-indexer_${VERSION}_${ARCH}"
    rm -rf "target/${IDX_PKG}"
    mkdir -p "target/${IDX_PKG}/DEBIAN"
    mkdir -p "target/${IDX_PKG}/usr/bin"
    mkdir -p "target/${IDX_PKG}/usr/lib/systemd/system"
    mkdir -p "target/${IDX_PKG}/var/lib/ergo-indexer"

    cp addons/indexer/target/release/ergo-indexer "target/${IDX_PKG}/usr/bin/"
    strip "target/${IDX_PKG}/usr/bin/ergo-indexer"
    cp deploy/ergo-indexer.service "target/${IDX_PKG}/usr/lib/systemd/system/"

    cat > "target/${IDX_PKG}/DEBIAN/control" <<CTRL
Package: ergo-node-rust-indexer
Version: ${VERSION}
Architecture: ${ARCH}
Depends: ergo-node-rust (>= ${VERSION})
Maintainer: mwaddip
Description: Blockchain indexer for ergo-node-rust
 Rich query API over SQLite — box lookup by address, transaction history,
 token metadata, and network statistics.
CTRL
    SIZE=$(du -sk "target/${IDX_PKG}" | cut -f1)
    echo "Installed-Size: ${SIZE}" >> "target/${IDX_PKG}/DEBIAN/control"

    dpkg-deb --build "target/${IDX_PKG}"
    rm -rf "target/${IDX_PKG}"

    echo ""
    echo "Built: target/${IDX_PKG}.deb"
    ls -lh "target/${IDX_PKG}.deb"
fi
```

- [ ] **Step 3: Commit**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add deploy/ergo-indexer.service build-deb
git commit -m "feat(indexer): systemd service + build-deb --with-indexer"
```

---

## Task 10: Integration test — end to end

**Files:**
- No new files — manual verification against running node

- [ ] **Step 1: Build the indexer**

```bash
cd addons/indexer && cargo build --release 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 2: Run against the node (if available)**

```bash
cd addons/indexer && RUST_LOG=ergo_indexer=debug cargo run -- --node-url http://127.0.0.1:9052 --db /tmp/test-indexer.db --bind 127.0.0.1:8080
```

If the node is running, verify:
- Indexer connects and starts syncing from genesis
- Blocks are indexed (check log output)
- Swagger UI is accessible at `http://127.0.0.1:8080/swagger`
- Query endpoints return data: `curl http://127.0.0.1:8080/api/v1/info`

- [ ] **Step 3: Test sync-only and serve-only modes**

```bash
# Sync only (no API)
cargo run -- sync --node-url http://127.0.0.1:9052 --db /tmp/test-indexer.db

# Serve only (read-only, assumes DB was populated by sync)
cargo run -- serve --db /tmp/test-indexer.db --bind 127.0.0.1:8080
```

- [ ] **Step 4: Verify query endpoints**

```bash
curl -s http://127.0.0.1:8080/api/v1/blocks?limit=5 | python3 -m json.tool | head -20
curl -s http://127.0.0.1:8080/api/v1/stats | python3 -m json.tool
curl -s http://127.0.0.1:8080/api/v1/info | python3 -m json.tool
```

- [ ] **Step 5: Final commit if any fixes were needed**

```bash
cd /home/mwaddip/projects/ergo-node-rust
git add -A addons/indexer/
git commit -m "fix(indexer): integration test fixes"
```
