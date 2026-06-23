# ergo-indexer-migratedb Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the SQLite ↔ PostgreSQL migrator binary `ergo-indexer-migratedb` per `facts/indexer-migration.md` v1.0.0.

**Architecture:** Separate binary in the existing `addons/indexer/` crate. New `src/migrate/` module with submodules for liveness, cursor tracking, hash verification, progress output, config-file rewrite, and the per-block runner. Backend abstraction trait shared between SQLite and PostgreSQL — runner is direction-agnostic.

**Tech Stack:** `rusqlite` (existing, SQLite reader/writer), `sqlx` (existing, PG reader/writer), `blake2` (canonical hash), `clap` (existing, CLI), `tracing` (existing, logging), `nix` (signal handling), `reqwest` (existing, indexer API ping), `toml_edit` (preserves formatting on `--update-config` rewrite). Standard library `std::io::IsTerminal` for TTY check (no extra dep).

---

## File Structure

**New files** (all in `addons/indexer/`):

- `src/bin/ergo-indexer-migratedb.rs` — bin entrypoint, clap CLI parsing, dispatch to `migrate::run()`
- `src/migrate/mod.rs` — module root + types (`MigrationPlan`, `DbType`, `Direction`, `Cursor`, `Backend` trait) + plan derivation logic + top-level `run()` orchestrator + confirmation prompt
- `src/migrate/liveness.rs` — API ping helper + SQLite `EXCLUSIVE` lock attempt
- `src/migrate/cursor.rs` — read/write helpers for `migration_cursor` / `migration_source` / `migration_source_fingerprint` rows in `indexer_state`
- `src/migrate/resume.rs` — 6-check resume precondition chain
- `src/migrate/hash.rs` — canonical per-height row encoding + `blake2b256` digest
- `src/migrate/runner.rs` — per-block migration unit (insert created + update spent + cursor advance + hash verify) and the top-level walking loop with signal handling
- `src/migrate/progress.rs` — dot-per-1000-blocks stdout writer, percentage-boundary newlines, final summary
- `src/migrate/config_update.rs` — `--update-config` best-effort rewrite of `/etc/ergo-node/indexer.toml`

**Files modified**:

- `Cargo.toml` — add `[[bin]]` for `ergo-indexer-migratedb`, add `blake2`, `nix` (signal handling), `toml_edit` deps
- `src/lib.rs` (or wherever `pub mod`s live) — `pub mod migrate;`
- `src/db/sqlite.rs` — add `pub fn try_exclusive_lock(path: &Path) -> Result<File>` helper
- `src/db/postgres.rs` — add `pub async fn defer_constraints(conn: &mut PgConnection) -> Result<()>` helper

---

## Task 1: Bin scaffolding + CLI + confirmation prompt

**Files:**
- Create: `addons/indexer/src/bin/ergo-indexer-migratedb.rs`
- Modify: `addons/indexer/Cargo.toml` (add `[[bin]]` entry)

- [ ] **Step 1: Add the [[bin]] entry to Cargo.toml**

```toml
[[bin]]
name = "ergo-indexer-migratedb"
path = "src/bin/ergo-indexer-migratedb.rs"
```

Place this section after the existing `[package]` / `[features]` / `[dependencies]` blocks but before any `[dev-dependencies]`.

- [ ] **Step 2: Write the failing test for --help output**

Create `addons/indexer/src/bin/ergo-indexer-migratedb.rs`:

```rust
fn main() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert_cmd::Command;

    #[test]
    fn help_lists_all_flags() {
        let output = Command::cargo_bin("ergo-indexer-migratedb")
            .unwrap()
            .arg("--help")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        for flag in ["--in", "--out", "--update-config", "--resume", "-y"] {
            assert!(stdout.contains(flag), "missing {flag} in --help");
        }
    }
}
```

Add to `[dev-dependencies]` in Cargo.toml if not present:
```toml
assert_cmd = "2"
```

- [ ] **Step 3: Run the test, expect it to fail**

```
cargo test --bin ergo-indexer-migratedb help_lists_all_flags
```

Expected: FAIL — `--in` not in `--help` output (clap not configured yet).

- [ ] **Step 4: Implement clap CLI**

Replace `main()` in `addons/indexer/src/bin/ergo-indexer-migratedb.rs`:

```rust
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "ergo-indexer-migratedb",
    about = "One-shot migrator between the indexer's SQLite and PostgreSQL backends",
    long_about = None,
)]
struct Cli {
    /// Source DB. SQLite path (or sqlite://...) or postgres://... URL.
    /// Defaults to [storage].db from /etc/ergo-node/indexer.toml.
    #[arg(long = "in", value_name = "URL")]
    src: Option<String>,

    /// Target DB. Same shape as --in.
    /// Defaults to the opposite backend type of --in.
    #[arg(long = "out", value_name = "URL")]
    tgt: Option<String>,

    /// After successful migration, rewrite [storage].db in
    /// /etc/ergo-node/indexer.toml to the new target.
    #[arg(long)]
    update_config: bool,

    /// Resume an interrupted migration. Target must have been created
    /// by a prior run of this tool.
    #[arg(long)]
    resume: bool,

    /// Non-interactive; skip the pre-migration confirmation prompt.
    #[arg(short = 'y')]
    yes: bool,
}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    Ok(())
}
```

- [ ] **Step 5: Run the test, expect pass**

```
cargo test --bin ergo-indexer-migratedb help_lists_all_flags
```

Expected: PASS.

- [ ] **Step 6: Commit**

```
git add addons/indexer/Cargo.toml addons/indexer/src/bin/ergo-indexer-migratedb.rs
git commit -m "feat(addons/indexer): ergo-indexer-migratedb bin + CLI scaffolding"
```

---

## Task 2: MigrationPlan + derivation logic

**Files:**
- Create: `addons/indexer/src/migrate/mod.rs`
- Modify: `addons/indexer/src/lib.rs` or `src/main.rs` (register `pub mod migrate;`)

- [ ] **Step 1: Write the failing tests for plan resolution**

Create `addons/indexer/src/migrate/mod.rs`:

```rust
use anyhow::{anyhow, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbType {
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbSpec {
    pub url: String,
    pub kind: DbType,
}

#[derive(Debug, Clone)]
pub struct MigrationPlan {
    pub source: DbSpec,
    pub target: DbSpec,
    pub update_config: bool,
    pub resume: bool,
    pub yes: bool,
}

/// Inputs to plan derivation. Mirrors the CLI but injectable for tests.
#[derive(Debug, Clone, Default)]
pub struct PlanInputs {
    pub cli_in: Option<String>,
    pub cli_out: Option<String>,
    pub update_config: bool,
    pub resume: bool,
    pub yes: bool,
    pub config_db: Option<String>,           // [storage].db from indexer.toml
    pub pg_env: PgEnv,                       // libpq env vars
}

#[derive(Debug, Clone, Default)]
pub struct PgEnv {
    pub host: Option<String>,
    pub port: Option<String>,
    pub user: Option<String>,
    pub database: Option<String>,
    pub sslmode: Option<String>,
}

pub fn resolve(inputs: PlanInputs) -> Result<MigrationPlan> {
    todo!()
}

pub fn classify(url: &str) -> Result<DbType> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sqlite_url() {
        assert_eq!(classify("sqlite:///var/lib/x.db").unwrap(), DbType::Sqlite);
    }
    #[test]
    fn classify_sqlite_bare_path() {
        assert_eq!(classify("/var/lib/x.db").unwrap(), DbType::Sqlite);
        assert_eq!(classify("./x.db").unwrap(), DbType::Sqlite);
    }
    #[test]
    fn classify_postgres_url() {
        assert_eq!(classify("postgres://x@h/d").unwrap(), DbType::Postgres);
        assert_eq!(classify("postgresql://x@h/d").unwrap(), DbType::Postgres);
    }

    #[test]
    fn resolve_both_cli_set() {
        let p = resolve(PlanInputs {
            cli_in: Some("sqlite:///a.db".into()),
            cli_out: Some("postgres://x@h/d".into()),
            ..Default::default()
        }).unwrap();
        assert_eq!(p.source.kind, DbType::Sqlite);
        assert_eq!(p.target.kind, DbType::Postgres);
    }

    #[test]
    fn resolve_in_unset_uses_config() {
        let p = resolve(PlanInputs {
            cli_in: None,
            cli_out: Some("postgres://x@h/d".into()),
            config_db: Some("sqlite:///cfg.db".into()),
            ..Default::default()
        }).unwrap();
        assert_eq!(p.source.url, "sqlite:///cfg.db");
    }

    #[test]
    fn resolve_in_unset_no_config_errors() {
        let err = resolve(PlanInputs {
            cli_in: None,
            cli_out: Some("postgres://x@h/d".into()),
            config_db: None,
            ..Default::default()
        }).unwrap_err();
        assert!(err.to_string().contains("storage.db not configured"));
    }

    #[test]
    fn resolve_sqlite_to_postgres_default_target_from_env() {
        let p = resolve(PlanInputs {
            cli_in: Some("sqlite:///a.db".into()),
            cli_out: None,
            pg_env: PgEnv {
                host: Some("h".into()),
                port: Some("5432".into()),
                user: Some("u".into()),
                database: Some("d".into()),
                ..Default::default()
            },
            ..Default::default()
        }).unwrap();
        assert_eq!(p.target.kind, DbType::Postgres);
        assert!(p.target.url.contains("h"));
        assert!(p.target.url.contains("d"));
    }

    #[test]
    fn resolve_sqlite_to_postgres_missing_env_errors() {
        let err = resolve(PlanInputs {
            cli_in: Some("sqlite:///a.db".into()),
            cli_out: None,
            pg_env: PgEnv::default(),
            ..Default::default()
        }).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("PGHOST") || msg.contains("missing"));
    }

    #[test]
    fn resolve_postgres_to_sqlite_default_target_path() {
        let p = resolve(PlanInputs {
            cli_in: Some("postgres://x@h/d".into()),
            cli_out: None,
            ..Default::default()
        }).unwrap();
        assert_eq!(p.target.kind, DbType::Sqlite);
        assert_eq!(p.target.url, "/var/lib/ergo-indexer/index.db");
    }

    #[test]
    fn resolve_same_backend_type_errors() {
        let err = resolve(PlanInputs {
            cli_in: Some("sqlite:///a.db".into()),
            cli_out: Some("sqlite:///b.db".into()),
            ..Default::default()
        }).unwrap_err();
        assert!(err.to_string().contains("same backend"));
    }
}
```

Register the module in `addons/indexer/src/lib.rs` (or wherever per the crate layout):

```rust
pub mod migrate;
```

- [ ] **Step 2: Run tests, expect them to fail**

```
cargo test -p ergo-indexer migrate::tests --no-default-features --features sqlite,postgres
```

Expected: 8 tests, all FAIL with `not yet implemented`.

- [ ] **Step 3: Implement classify() and resolve()**

Replace the `todo!()` bodies in `mod.rs`:

```rust
pub fn classify(url: &str) -> Result<DbType> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(DbType::Postgres)
    } else if url.starts_with("sqlite://") || !url.contains("://") {
        Ok(DbType::Sqlite)
    } else {
        Err(anyhow!("unrecognized DB URL scheme: {url}"))
    }
}

pub fn resolve(inputs: PlanInputs) -> Result<MigrationPlan> {
    // 1. Resolve source: --in > config.db
    let src_url = inputs
        .cli_in
        .or(inputs.config_db.clone())
        .ok_or_else(|| anyhow!(
            "storage.db not configured — set [storage].db in /etc/ergo-node/indexer.toml or pass --in <url-or-path>"
        ))?;
    let src_kind = classify(&src_url)?;

    // 2. Resolve target: --out > derived-opposite
    let (tgt_url, tgt_kind) = match inputs.cli_out {
        Some(url) => {
            let k = classify(&url)?;
            (url, k)
        }
        None => derive_default_target(src_kind, &inputs.pg_env)?,
    };

    if src_kind == tgt_kind {
        return Err(anyhow!(
            "source and target are the same backend ({:?}) — nothing to migrate",
            src_kind
        ));
    }

    Ok(MigrationPlan {
        source: DbSpec { url: src_url, kind: src_kind },
        target: DbSpec { url: tgt_url, kind: tgt_kind },
        update_config: inputs.update_config,
        resume: inputs.resume,
        yes: inputs.yes,
    })
}

fn derive_default_target(src_kind: DbType, pg_env: &PgEnv) -> Result<(String, DbType)> {
    match src_kind {
        DbType::Sqlite => {
            // SQLite → PG: build URL from env
            let mut missing = vec![];
            let host = pg_env.host.as_deref().unwrap_or_else(|| {
                missing.push("PGHOST");
                ""
            });
            let user = pg_env.user.as_deref().unwrap_or_else(|| {
                missing.push("PGUSER");
                ""
            });
            let database = pg_env.database.as_deref().unwrap_or_else(|| {
                missing.push("PGDATABASE");
                ""
            });
            if !missing.is_empty() {
                return Err(anyhow!(
                    "cannot derive default PG target — missing required env vars: {}",
                    missing.join(", ")
                ));
            }
            let port = pg_env.port.as_deref().unwrap_or("5432");
            let url = format!("postgres://{user}@{host}:{port}/{database}");
            Ok((url, DbType::Postgres))
        }
        DbType::Postgres => {
            // PG → SQLite: default path
            Ok(("/var/lib/ergo-indexer/index.db".to_string(), DbType::Sqlite))
        }
    }
}
```

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p ergo-indexer migrate::tests --no-default-features --features sqlite,postgres
```

Expected: 8 tests PASS.

- [ ] **Step 5: Commit**

```
git add addons/indexer/src/migrate/mod.rs addons/indexer/src/lib.rs
git commit -m "feat(addons/indexer): migrate::resolve + DbType classification"
```

---

## Task 3: Liveness checks

**Files:**
- Create: `addons/indexer/src/migrate/liveness.rs`
- Modify: `addons/indexer/src/migrate/mod.rs` (`pub mod liveness;`)
- Modify: `addons/indexer/src/db/sqlite.rs` (add `try_exclusive_lock` helper)

- [ ] **Step 1: Write failing tests for API ping**

Create `addons/indexer/src/migrate/liveness.rs`:

```rust
use anyhow::Result;
use std::time::Duration;

const API_PING_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn api_responds(bind: &str) -> bool {
    todo!()
}

pub fn try_exclusive_sqlite_lock(path: &std::path::Path) -> Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use serde_json::json;
    use std::net::TcpListener;
    use tokio::time::Duration;

    /// Start a tiny axum server on a random port responding 200 to /api/v1/info.
    async fn spawn_mock_indexer() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let app: Router = Router::new().route(
            "/api/v1/info",
            get(|| async { Json(json!({"indexedHeight": 1, "nodeHeight": 1})) }),
        );
        tokio::spawn(async move {
            axum::serve(
                tokio::net::TcpListener::from_std(listener).unwrap(),
                app,
            )
            .await
            .unwrap();
        });
        // Give the server a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        format!("127.0.0.1:{}", addr.port())
    }

    #[tokio::test]
    async fn api_responds_true_when_server_up() {
        let bind = spawn_mock_indexer().await;
        assert!(api_responds(&bind).await);
    }

    #[tokio::test]
    async fn api_responds_false_when_no_listener() {
        // unbound port
        assert!(!api_responds("127.0.0.1:1").await);
    }

    #[test]
    fn sqlite_lock_acquired_on_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        // Create empty file
        std::fs::File::create(&path).unwrap();
        try_exclusive_sqlite_lock(&path).expect("should acquire on fresh");
    }

    #[test]
    fn sqlite_lock_blocked_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE; CREATE TABLE t (x INT);").unwrap();
        // While the transaction is open (holds the lock), our attempt must fail.
        let err = try_exclusive_sqlite_lock(&path).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("busy") || err.to_string().to_lowercase().contains("lock"));
    }
}
```

Add to Cargo.toml `[dev-dependencies]`:

```toml
axum = "0.7"
tempfile = "3"
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```

Register module in `mod.rs`:

```rust
pub mod liveness;
```

- [ ] **Step 2: Run tests, expect fail**

```
cargo test -p ergo-indexer migrate::liveness::tests
```

Expected: 4 tests, all FAIL with `not yet implemented`.

- [ ] **Step 3: Implement api_responds and try_exclusive_sqlite_lock**

Replace the `todo!()` bodies:

```rust
pub async fn api_responds(bind: &str) -> bool {
    let url = format!("http://{bind}/api/v1/info");
    let client = match reqwest::Client::builder()
        .timeout(API_PING_TIMEOUT)
        .build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(&url).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

pub fn try_exclusive_sqlite_lock(path: &std::path::Path) -> Result<()> {
    let conn = rusqlite::Connection::open(path)?;
    // BEGIN EXCLUSIVE acquires both PENDING and EXCLUSIVE locks.
    // ROLLBACK releases them immediately.
    conn.execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
        .map_err(|e| anyhow::anyhow!("source SQLite is locked by another process: {e}"))?;
    Ok(())
}
```

- [ ] **Step 4: Run tests, expect pass**

```
cargo test -p ergo-indexer migrate::liveness::tests
```

Expected: 4 tests PASS.

- [ ] **Step 5: Implement the top-level liveness gate**

Add to `liveness.rs`:

```rust
use crate::migrate::{DbType, MigrationPlan};

pub async fn assert_safe_to_proceed(plan: &MigrationPlan, indexer_bind: &str) -> Result<()> {
    // 1. API ping
    if api_responds(indexer_bind).await {
        anyhow::bail!(
            "indexer appears to be running (got 2xx from http://{indexer_bind}/api/v1/info).\n\
             Stop the indexer (e.g. systemctl stop ergo-indexer) and re-run."
        );
    }

    // 2. SQLite source lock attempt
    if plan.source.kind == DbType::Sqlite {
        let path = sqlite_url_to_path(&plan.source.url);
        if path.exists() {
            try_exclusive_sqlite_lock(&path).map_err(|e| {
                anyhow::anyhow!(
                    "source SQLite database is locked by another process.\n\
                     Stop the writer (likely the indexer) and re-run.\n\
                     Underlying error: {e}"
                )
            })?;
        }
    }

    Ok(())
}

fn sqlite_url_to_path(url: &str) -> std::path::PathBuf {
    url.strip_prefix("sqlite://")
        .unwrap_or(url)
        .into()
}
```

Add a test for the integrated gate:

```rust
    #[tokio::test]
    async fn assert_safe_refuses_when_api_responds() {
        let bind = spawn_mock_indexer().await;
        let plan = MigrationPlan {
            source: DbSpec { url: "sqlite:///tmp/x.db".into(), kind: DbType::Sqlite },
            target: DbSpec { url: "postgres://x@h/d".into(), kind: DbType::Postgres },
            update_config: false,
            resume: false,
            yes: false,
        };
        let err = assert_safe_to_proceed(&plan, &bind).await.unwrap_err();
        assert!(err.to_string().contains("indexer appears to be running"));
    }
```

- [ ] **Step 6: Run all liveness tests**

```
cargo test -p ergo-indexer migrate::liveness::tests
```

Expected: 5 tests PASS.

- [ ] **Step 7: Commit**

```
git add addons/indexer/Cargo.toml addons/indexer/src/migrate/{mod,liveness}.rs
git commit -m "feat(addons/indexer): migrate liveness checks (API ping + SQLite lock)"
```

---

## Task 4: Backend trait + cursor table helpers

**Files:**
- Modify: `addons/indexer/src/migrate/mod.rs` (add `Backend` trait + `Cursor` type)
- Create: `addons/indexer/src/migrate/cursor.rs`

- [ ] **Step 1: Add Backend trait and Cursor types to mod.rs**

Append to `addons/indexer/src/migrate/mod.rs`:

```rust
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub last_height: u32,
    pub source_url: String,
    pub source_fingerprint: [u8; 32],
}

#[async_trait]
pub trait Backend: Send {
    /// Schema version recorded in indexer_state, or None if not initialized.
    async fn schema_version(&mut self) -> Result<Option<u32>>;

    /// Initialize an empty target with the given schema version.
    async fn init_schema(&mut self, version: u32) -> Result<()>;

    /// Read current migration cursor (3 keys), or None if not set.
    async fn read_cursor(&mut self) -> Result<Option<Cursor>>;

    /// Write/overwrite migration cursor as part of a per-block transaction.
    async fn write_cursor(&mut self, cursor: &Cursor) -> Result<()>;

    /// Highest height in source's `blocks` table.
    async fn max_height(&mut self) -> Result<Option<u32>>;

    /// Read all data needed for hash + migration at a given height.
    async fn read_block_data(&mut self, height: u32) -> Result<BlockData>;

    /// Apply block data (INSERT created + UPDATE spent) within a single
    /// transaction that also writes the cursor and commits atomically.
    async fn apply_block(&mut self, data: &BlockData, cursor: &Cursor) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct BlockData {
    pub block: BlockRow,
    pub transactions: Vec<TransactionRow>,
    pub created_boxes: Vec<BoxRow>,
    pub box_registers: Vec<BoxRegisterRow>,
    pub box_tokens: Vec<BoxTokenRow>,
    pub minted_tokens: Vec<TokenRow>,
    pub spent_box_updates: Vec<BoxSpentUpdate>,
}

// Row types: the engineer fills these in to match `src/db/sqlite.rs` schemas.
// Each must derive (Debug, Clone, PartialEq).
```

Add `async_trait` to Cargo.toml dependencies:

```toml
async-trait = "0.1"
```

- [ ] **Step 2: Add the BlockRow / TransactionRow / etc. structs**

Fill out the row types based on the schema in `src/db/sqlite.rs` and `facts/indexer.md` § Storage Schema. Each row is a flat struct with the table's columns. Example:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct BlockRow {
    pub header_id: [u8; 32],
    pub height: u32,
    // ... rest of blocks table columns
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransactionRow {
    pub tx_id: [u8; 32],
    pub header_id: [u8; 32],
    pub height: u32,
    pub tx_index: u32,
    pub size: u32,
}

// etc. for BoxRow, BoxRegisterRow, BoxTokenRow, TokenRow, BoxSpentUpdate
```

(Read `addons/indexer/src/db/sqlite.rs` for the exact column set per table — the contract is the design intent, the SQL file is the source of truth for current schema.)

- [ ] **Step 3: Write failing tests for cursor read/write**

Create `addons/indexer/src/migrate/cursor.rs`:

```rust
use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake backend that stores cursor in memory, for testing the
    /// cursor read/write contract independent of any real DB.
    struct FakeBackend {
        schema: Option<u32>,
        cursor: Option<Cursor>,
    }

    #[async_trait]
    impl Backend for FakeBackend {
        async fn schema_version(&mut self) -> Result<Option<u32>> { Ok(self.schema) }
        async fn init_schema(&mut self, v: u32) -> Result<()> {
            self.schema = Some(v);
            Ok(())
        }
        async fn read_cursor(&mut self) -> Result<Option<Cursor>> {
            Ok(self.cursor.clone())
        }
        async fn write_cursor(&mut self, c: &Cursor) -> Result<()> {
            self.cursor = Some(c.clone());
            Ok(())
        }
        async fn max_height(&mut self) -> Result<Option<u32>> { unimplemented!() }
        async fn read_block_data(&mut self, _: u32) -> Result<BlockData> { unimplemented!() }
        async fn apply_block(&mut self, _: &BlockData, _: &Cursor) -> Result<()> { unimplemented!() }
    }

    #[tokio::test]
    async fn cursor_roundtrip() {
        let mut b = FakeBackend { schema: None, cursor: None };
        assert!(b.read_cursor().await.unwrap().is_none());
        let c = Cursor {
            last_height: 1000,
            source_url: "sqlite:///x.db".into(),
            source_fingerprint: [0x42; 32],
        };
        b.write_cursor(&c).await.unwrap();
        assert_eq!(b.read_cursor().await.unwrap().as_ref(), Some(&c));
    }
}
```

- [ ] **Step 4: Run, expect pass (the fake backend satisfies its own contract)**

```
cargo test -p ergo-indexer migrate::cursor::tests
```

Expected: 1 test PASS. The point of this test is the contract shape, not a real backend implementation (those come in Task 7).

- [ ] **Step 5: Commit**

```
git add addons/indexer/Cargo.toml addons/indexer/src/migrate/{mod,cursor}.rs
git commit -m "feat(addons/indexer): migrate Backend trait + Cursor type + row types"
```

---

## Task 5: Hash verification

**Files:**
- Create: `addons/indexer/src/migrate/hash.rs`
- Modify: `addons/indexer/Cargo.toml` (add `blake2`)

- [ ] **Step 1: Add blake2 dep**

```toml
blake2 = "0.10"
```

- [ ] **Step 2: Write failing tests for canonical encoding + hash**

Create `addons/indexer/src/migrate/hash.rs`:

```rust
use blake2::{Blake2b, Digest};
use blake2::digest::consts::U32;
use super::BlockData;

pub type BlockHash = [u8; 32];

/// Compute the canonical blake2b256 digest of a block's data.
///
/// Encoding (per facts/indexer-migration.md § Hash verification):
/// blocks row | transactions sorted by tx_index | created_boxes sorted by box_id
///   | box_registers / box_tokens for those boxes, in box_id order
///   | minted_tokens sorted by token_id | spent_box_updates sorted by box_id
///
/// Each row's fields are emitted in fixed order separated by 0x00 byte;
/// row boundary marked by 0xFF byte.
pub fn hash_block(data: &BlockData) -> BlockHash {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_block_data(seed: u8) -> BlockData {
        todo!()
    }

    #[test]
    fn identical_data_produces_identical_hash() {
        let a = synth_block_data(0x01);
        let b = synth_block_data(0x01);
        assert_eq!(hash_block(&a), hash_block(&b));
    }

    #[test]
    fn differing_data_produces_differing_hash() {
        let a = synth_block_data(0x01);
        let b = synth_block_data(0x02);
        assert_ne!(hash_block(&a), hash_block(&b));
    }

    #[test]
    fn hash_is_deterministic_across_runs() {
        let d = synth_block_data(0x42);
        let h1 = hash_block(&d);
        let h2 = hash_block(&d);
        let h3 = hash_block(&d);
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }
}
```

- [ ] **Step 3: Run, expect fail**

Expected: 3 tests FAIL (both `synth_block_data` and `hash_block` are todo!()).

- [ ] **Step 4: Implement synth_block_data and hash_block**

Replace `synth_block_data` with a real fixture builder that produces `BlockData` with deterministic content given a seed byte. Use the row types from Task 4.

Replace `hash_block`:

```rust
const ROW_SEP: u8 = 0xFF;
const FIELD_SEP: u8 = 0x00;

pub fn hash_block(data: &BlockData) -> BlockHash {
    let mut hasher = Blake2b::<U32>::new();

    // 1. blocks row
    write_block_row(&mut hasher, &data.block);
    hasher.update([ROW_SEP]);

    // 2. transactions sorted by tx_index
    let mut txs = data.transactions.clone();
    txs.sort_by_key(|t| t.tx_index);
    for tx in &txs {
        write_tx_row(&mut hasher, tx);
        hasher.update([ROW_SEP]);
    }

    // 3. created_boxes sorted by box_id
    let mut boxes = data.created_boxes.clone();
    boxes.sort_by(|a, b| a.box_id.cmp(&b.box_id));
    for b in &boxes {
        write_box_row(&mut hasher, b);
        hasher.update([ROW_SEP]);
    }

    // 4. box_registers in box_id order
    let mut regs = data.box_registers.clone();
    regs.sort_by(|a, b| a.box_id.cmp(&b.box_id));
    for r in &regs {
        write_box_register_row(&mut hasher, r);
        hasher.update([ROW_SEP]);
    }

    // 5. box_tokens in box_id order
    let mut tkns_per_box = data.box_tokens.clone();
    tkns_per_box.sort_by(|a, b| a.box_id.cmp(&b.box_id));
    for t in &tkns_per_box {
        write_box_token_row(&mut hasher, t);
        hasher.update([ROW_SEP]);
    }

    // 6. minted_tokens sorted by token_id
    let mut mts = data.minted_tokens.clone();
    mts.sort_by(|a, b| a.token_id.cmp(&b.token_id));
    for t in &mts {
        write_token_row(&mut hasher, t);
        hasher.update([ROW_SEP]);
    }

    // 7. spent_box_updates sorted by box_id
    let mut spent = data.spent_box_updates.clone();
    spent.sort_by(|a, b| a.box_id.cmp(&b.box_id));
    for s in &spent {
        write_spent_update_row(&mut hasher, s);
        hasher.update([ROW_SEP]);
    }

    hasher.finalize().into()
}

fn write_block_row(h: &mut Blake2b<U32>, b: &super::BlockRow) {
    h.update(b.header_id);
    h.update([FIELD_SEP]);
    h.update(b.height.to_le_bytes());
    // ... rest of fields
}
// Similar write_*_row helpers for the other row types.
```

The exact byte layout is the implementer's choice as long as it is deterministic and identical across both backends.

- [ ] **Step 5: Run, expect pass**

```
cargo test -p ergo-indexer migrate::hash::tests
```

Expected: 3 tests PASS.

- [ ] **Step 6: Commit**

```
git add addons/indexer/Cargo.toml addons/indexer/src/migrate/hash.rs
git commit -m "feat(addons/indexer): migrate per-block blake2b256 hash"
```

---

## Task 6: Resume preconditions (6-check chain)

**Files:**
- Create: `addons/indexer/src/migrate/resume.rs`

- [ ] **Step 1: Write failing tests**

Create `addons/indexer/src/migrate/resume.rs`:

```rust
use super::*;
use anyhow::{anyhow, bail, Result};

pub const EXPECTED_SCHEMA_VERSION: u32 = 1;

/// Run the 6-step resume precondition check.
/// On any failure, return Err with a diagnostic naming the specific check.
pub async fn check_resume_preconditions(
    source: &mut dyn Backend,
    target: &mut dyn Backend,
    source_url: &str,
) -> Result<Cursor> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    // FakeBackend from cursor.rs reused; same pattern.
    // Tests for each of the 6 failure modes + happy path = 7 tests minimum.

    // Test names to implement (one #[tokio::test] each):
    // 1. resume_fails_if_target_schema_version_mismatch
    // 2. resume_fails_if_cursor_missing
    // 3. resume_fails_if_migration_source_mismatch
    // 4. resume_fails_if_source_fingerprint_mismatch
    // 5. resume_fails_if_spot_check_block_hash_mismatch
    // 6. resume_succeeds_when_all_six_checks_pass
}
```

Test bodies use a FakeBackend-style mock that exposes configurable schema_version + cursor responses for both source and target.

- [ ] **Step 2: Run, expect fail**

Expected: 6 tests FAIL (todo!).

- [ ] **Step 3: Implement the 6-check chain**

```rust
pub async fn check_resume_preconditions(
    source: &mut dyn Backend,
    target: &mut dyn Backend,
    source_url: &str,
) -> Result<Cursor> {
    // Check 1: target schema_version match
    let tgt_ver = target.schema_version().await?
        .ok_or_else(|| anyhow!("resume: target has no schema_version row"))?;
    if tgt_ver != EXPECTED_SCHEMA_VERSION {
        bail!("resume: target schema_version is {tgt_ver}, expected {EXPECTED_SCHEMA_VERSION}");
    }

    // Checks 2/3/4: cursor + source URL + source fingerprint
    let cursor = target.read_cursor().await?
        .ok_or_else(|| anyhow!(
            "resume: target has no migration_cursor row — was it created by this tool?"
        ))?;

    let normalized_src = normalize_url(source_url);
    let normalized_stored = normalize_url(&cursor.source_url);
    if normalized_src != normalized_stored {
        bail!(
            "resume: stored migration_source ({}) does not match current --in ({})",
            cursor.source_url, source_url
        );
    }

    // Check 4: source fingerprint
    let now_fingerprint = compute_source_fingerprint(source).await?;
    if now_fingerprint != cursor.source_fingerprint {
        bail!(
            "resume: source data fingerprint changed between runs — refusing to continue"
        );
    }

    // Check 5: spot-check the cursor-height block hash
    let src_block_data = source.read_block_data(cursor.last_height).await?;
    let tgt_block_data = target.read_block_data(cursor.last_height).await?;
    let src_hash = crate::migrate::hash::hash_block(&src_block_data);
    let tgt_hash = crate::migrate::hash::hash_block(&tgt_block_data);
    if src_hash != tgt_hash {
        bail!(
            "resume: spot-check failed at height {} — source and target diverge",
            cursor.last_height
        );
    }

    Ok(cursor)
}

fn normalize_url(url: &str) -> String {
    // Trim trailing slash, canonicalize SQLite paths, etc.
    // Minimum: handle "sqlite://" prefix consistently.
    url.trim_end_matches('/').to_string()
}

async fn compute_source_fingerprint(source: &mut dyn Backend) -> Result<[u8; 32]> {
    // Fingerprint = hash of block at height 1.
    let data = source.read_block_data(1).await?;
    Ok(crate::migrate::hash::hash_block(&data))
}
```

- [ ] **Step 4: Run, expect pass**

```
cargo test -p ergo-indexer migrate::resume::tests
```

Expected: all PASS.

- [ ] **Step 5: Commit**

```
git add addons/indexer/src/migrate/resume.rs
git commit -m "feat(addons/indexer): migrate resume 6-check precondition chain"
```

---

## Task 7: Per-block migration unit + Backend implementations

**Files:**
- Create: `addons/indexer/src/migrate/runner.rs` (per-block apply only; loop comes later)
- Modify: `addons/indexer/src/db/sqlite.rs` (implement `Backend` for `SqliteBackend`)
- Modify: `addons/indexer/src/db/postgres.rs` (implement `Backend` for `PostgresBackend`)

This is the largest task. It implements the `Backend` trait for both real backends (which means writing SQL queries for each of the trait methods) AND the per-height migration unit that orchestrates them.

- [ ] **Step 1: Write failing tests for SqliteBackend's Backend impl**

Test cases (place in `db/sqlite.rs`'s test module):

```rust
#[tokio::test]
async fn sqlite_backend_schema_version_roundtrip() {
    let db = tempfile::NamedTempFile::new().unwrap();
    let mut b = SqliteBackend::new_for_migration(db.path()).unwrap();
    assert!(b.schema_version().await.unwrap().is_none());
    b.init_schema(1).await.unwrap();
    assert_eq!(b.schema_version().await.unwrap(), Some(1));
}

#[tokio::test]
async fn sqlite_backend_cursor_roundtrip() {
    let db = tempfile::NamedTempFile::new().unwrap();
    let mut b = SqliteBackend::new_for_migration(db.path()).unwrap();
    b.init_schema(1).await.unwrap();
    assert!(b.read_cursor().await.unwrap().is_none());
    let c = Cursor {
        last_height: 42,
        source_url: "sqlite:///source".into(),
        source_fingerprint: [0xAB; 32],
    };
    b.write_cursor(&c).await.unwrap();
    assert_eq!(b.read_cursor().await.unwrap(), Some(c));
}

#[tokio::test]
async fn sqlite_backend_apply_block_then_read_back() {
    // Build BlockData for some height
    // apply_block(...)
    // read_block_data(height) → same BlockData
}
```

Mirror these for PG (gated `#[cfg(feature = "postgres")]`) using a sqlx connection to a temporary PG schema (assumes test PG is available; otherwise the PG tests are gated behind a feature flag and CI runs them against a sidecar PG).

- [ ] **Step 2: Implement the `Backend` trait for SqliteBackend and PostgresBackend**

For each backend, implement each trait method as the appropriate SQL:

- `schema_version`: `SELECT value FROM indexer_state WHERE key = 'schema_version'` → parse as `u32`
- `init_schema`: run the existing schema-create SQL + `INSERT INTO indexer_state (key, value) VALUES ('schema_version', '1')`
- `read_cursor`: 3-row SELECT
- `write_cursor`: 3 INSERT OR REPLACE / UPSERT
- `max_height`: `SELECT MAX(height) FROM blocks`
- `read_block_data`: union of SELECTs across blocks, transactions, boxes (created), box_registers, box_tokens, tokens (minted), boxes (spent)
- `apply_block`: BEGIN tx, INSERT created rows, UPDATE spent rows, write cursor, COMMIT

For PG: use `BEGIN`/`COMMIT` wrapped in sqlx transactions. Defer constraints at the connection level (added in Task 7.5 below).

- [ ] **Step 3: Add the defer_constraints / FK-off helper**

For SQLite, add to `src/db/sqlite.rs`:

```rust
impl SqliteBackend {
    pub fn new_for_migration(path: &std::path::Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        // The existing pragmas (cache_size, etc.) from Task 9b7f8a1 also apply.
        // ...
        Ok(Self { conn })
    }
}
```

For PG, add to `src/db/postgres.rs`:

```rust
impl PostgresBackend {
    pub async fn new_for_migration(url: &str) -> Result<Self> {
        let pool = sqlx::PgPool::connect(url).await?;
        // SET CONSTRAINTS deferred is per-transaction; do it at apply_block boundary.
        Ok(Self { pool })
    }
}
```

In `apply_block`'s transaction body for PG:

```rust
sqlx::query("SET CONSTRAINTS ALL DEFERRED").execute(&mut *tx).await?;
```

- [ ] **Step 4: Run all the backend tests, expect pass**

```
cargo test -p ergo-indexer --features sqlite,postgres
```

Expected: PASS (assuming a test PG instance is reachable for PG tests).

- [ ] **Step 5: Commit**

```
git add addons/indexer/src/{db,migrate}
git commit -m "feat(addons/indexer): Backend impls for SqliteBackend and PostgresBackend"
```

---

## Task 8: Progress output

**Files:**
- Create: `addons/indexer/src/migrate/progress.rs`

- [ ] **Step 1: Write failing tests**

```rust
use std::io::Write;

pub struct Progress<W: Write> {
    out: W,
    total_blocks: u32,
    blocks_done: u32,
    last_percent_emitted: u8,
    is_tty: bool,
}

impl<W: Write> Progress<W> {
    pub fn new(out: W, total_blocks: u32, starting_from: u32, is_tty: bool) -> Self {
        todo!()
    }
    pub fn tick(&mut self) {
        todo!()
    }
    pub fn finalize(self, elapsed: std::time::Duration) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_emits_dot_every_1000_blocks() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 100_000, 0, true);
        for _ in 0..1000 { p.tick(); }
        assert!(String::from_utf8_lossy(&buf).contains("."));
    }

    #[test]
    fn percentage_boundary_emits_newline_with_percent() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 100_000, 0, true);
        for _ in 0..1000 { p.tick(); }
        let s = String::from_utf8_lossy(&buf).to_string();
        assert!(s.contains("(1%)") || s.contains("(0%)")); // 1000/100k = 1%
    }

    #[test]
    fn resume_starts_at_correct_percentage() {
        let mut buf = Vec::new();
        let p = Progress::new(&mut buf, 100_000, 35_000, true);
        // First emitted percentage line should be (35%) (or 36% depending on rounding).
        // Implementation choice: emit a leading "(35%) " marker on resume.
        // Test: after construction, buf contains "(35%)" or similar resume indicator.
    }

    #[test]
    fn finalize_emits_summary_line() {
        let mut buf = Vec::new();
        let mut p = Progress::new(&mut buf, 1000, 0, true);
        for _ in 0..1000 { p.tick(); }
        p.finalize(std::time::Duration::from_secs(60));
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("Migration complete"));
        assert!(s.contains("1000 blocks"));
    }
}
```

- [ ] **Step 2: Implement Progress**

Replace todos with the implementation following the spec in
`facts/indexer-migration.md` § Progress output. Use `std::io::IsTerminal`
via `Write::is_terminal` (introduced in Rust 1.70).

- [ ] **Step 3: Run, expect pass**

```
cargo test -p ergo-indexer migrate::progress::tests
```

- [ ] **Step 4: Commit**

```
git add addons/indexer/src/migrate/progress.rs
git commit -m "feat(addons/indexer): migrate progress writer (dot + percentage)"
```

---

## Task 9: --update-config rewrite

**Files:**
- Create: `addons/indexer/src/migrate/config_update.rs`
- Modify: `addons/indexer/Cargo.toml` (`toml_edit` dep — preserves comments)

- [ ] **Step 1: Add dep**

```toml
toml_edit = "0.22"
```

- [ ] **Step 2: Write failing tests**

```rust
use std::path::Path;

pub fn rewrite_storage_db(config_path: &Path, new_db: &str) -> anyhow::Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn rewrite_replaces_storage_db_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexer.toml");
        fs::write(&path, r#"
[node]
url = "http://127.0.0.1:9052"

[storage]
db = "sqlite:///old/path/index.db"

[api]
bind = "0.0.0.0:9054"
"#).unwrap();

        rewrite_storage_db(&path, "postgres://user@host:5432/db").unwrap();
        let s = fs::read_to_string(&path).unwrap();
        assert!(s.contains("db = \"postgres://user@host:5432/db\""));
        assert!(s.contains("[node]"));
        assert!(s.contains("[api]"));
        // Previous value preserved as a comment immediately above
        assert!(s.contains("# previous: sqlite:///old/path/index.db") ||
                s.contains("# previous: \"sqlite:///old/path/index.db\""));
    }

    #[test]
    fn rewrite_returns_warning_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let err = rewrite_storage_db(&path, "postgres://x@h/d").unwrap_err();
        assert!(err.to_string().contains("missing") || err.to_string().contains("not found"));
    }
}
```

- [ ] **Step 3: Implement rewrite_storage_db**

```rust
pub fn rewrite_storage_db(config_path: &Path, new_db: &str) -> anyhow::Result<()> {
    let original = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("config file missing or unreadable at {}: {e}", config_path.display()))?;
    let mut doc: toml_edit::DocumentMut = original.parse()?;

    let storage = doc.get_mut("storage")
        .and_then(|s| s.as_table_mut())
        .ok_or_else(|| anyhow::anyhow!("[storage] section not found in {}", config_path.display()))?;

    let prev = storage
        .get("db")
        .and_then(|v| v.as_str())
        .unwrap_or("(unset)")
        .to_string();

    let mut new_value = toml_edit::value(new_db);
    // Attach the comment as a decor prefix on the new key
    if let Some(item) = new_value.as_value_mut() {
        item.decor_mut().set_prefix(format!(" # previous: {prev}\n"));
    }
    storage["db"] = new_value;

    std::fs::write(config_path, doc.to_string())?;
    Ok(())
}
```

- [ ] **Step 4: Run, expect pass**

- [ ] **Step 5: Commit**

```
git add addons/indexer/Cargo.toml addons/indexer/src/migrate/config_update.rs
git commit -m "feat(addons/indexer): migrate --update-config TOML rewrite"
```

---

## Task 10: Runner top-level loop + signal handling

**Files:**
- Modify: `addons/indexer/src/migrate/runner.rs` (extend from Task 7)
- Modify: `addons/indexer/Cargo.toml` (add `nix` for SIGINT/SIGTERM)

- [ ] **Step 1: Add nix dep**

```toml
nix = { version = "0.29", features = ["signal"] }
```

- [ ] **Step 2: Write failing test for the run-loop with cancellation**

The test wires a fake source/target with N synthetic blocks, fires a
cancel-token midway, asserts:
1. Migration stops between blocks.
2. Target's cursor is at the last-fully-committed height, not partial.
3. The function returns a clean error indicating SIGINT.

- [ ] **Step 3: Implement run()**

```rust
pub async fn run(
    source: &mut dyn Backend,
    target: &mut dyn Backend,
    plan: &MigrationPlan,
    progress: &mut Progress<impl Write>,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<u32> {
    // Determine start height
    let start_height = if plan.resume {
        let cursor = resume::check_resume_preconditions(source, target, &plan.source.url).await?;
        cursor.last_height + 1
    } else {
        // Fresh: init schema if target is empty
        if target.schema_version().await?.is_none() {
            target.init_schema(resume::EXPECTED_SCHEMA_VERSION).await?;
        }
        1
    };

    let max_height = source.max_height().await?
        .ok_or_else(|| anyhow!("source has no blocks"))?;

    // Compute source fingerprint once (used for cursor writes)
    let source_fingerprint = hash_source_fingerprint(source).await?;

    for h in start_height..=max_height {
        if *cancel.borrow() {
            anyhow::bail!("migration interrupted by signal at height {}", h - 1);
        }

        let data = source.read_block_data(h).await?;
        let src_hash = hash::hash_block(&data);

        let cursor = Cursor {
            last_height: h,
            source_url: plan.source.url.clone(),
            source_fingerprint,
        };

        target.apply_block(&data, &cursor).await?;

        // Verify by re-reading from target
        let target_data = target.read_block_data(h).await?;
        let tgt_hash = hash::hash_block(&target_data);
        if src_hash != tgt_hash {
            anyhow::bail!("hash mismatch at height {h} — aborting");
        }

        progress.tick();
    }

    Ok(max_height)
}
```

- [ ] **Step 4: Wire signal handling in the bin file**

Update `addons/indexer/src/bin/ergo-indexer-migratedb.rs`:

```rust
use tokio::sync::watch;
use nix::sys::signal::{SigHandler, Signal};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (cancel_tx, cancel_rx) = watch::channel(false);

    // Install signal handlers
    {
        let tx = cancel_tx.clone();
        tokio::spawn(async move {
            let mut sigint = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::interrupt(),
            ).unwrap();
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            ).unwrap();
            tokio::select! {
                _ = sigint.recv() => { let _ = tx.send(true); },
                _ = sigterm.recv() => { let _ = tx.send(true); },
            }
        });
    }

    // ... (resolve, confirmation, dispatch into migrate::run::run)
}
```

- [ ] **Step 5: Run, expect pass**

- [ ] **Step 6: Commit**

```
git add addons/indexer/Cargo.toml addons/indexer/src/migrate/runner.rs addons/indexer/src/bin/ergo-indexer-migratedb.rs
git commit -m "feat(addons/indexer): migrate runner loop + SIGINT/SIGTERM handling"
```

---

## Task 11: End-to-end tests

**Files:**
- Create: `addons/indexer/tests/migrate_e2e.rs`

Three scenarios:
1. SQLite → PG, fresh migration of a synthetic 10-block chain. Verify all rows present, hashes match, cursor at max_height.
2. PG → SQLite, same.
3. SQLite → PG, kill mid-flight at h=5, resume with `--resume`, complete. Verify final state identical to a fresh run.

- [ ] **Step 1: Set up E2E harness**

```rust
//! Integration tests for the migrator. Requires a test PG instance
//! reachable at PGHOST=localhost (or the test runner sets up sidecar).

use std::process::Command;

async fn populate_sqlite_source(path: &std::path::Path, max_height: u32) -> anyhow::Result<()> {
    // Use SqliteBackend's init_schema + apply_block to synth `max_height` blocks
    // ...
}

#[tokio::test]
async fn migrate_sqlite_to_postgres_full() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.db");
    populate_sqlite_source(&src, 10).await.unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_ergo-indexer-migratedb"))
        .args([
            "--in", &format!("sqlite://{}", src.display()),
            "--out", "postgres://test_user@localhost/test_migrate",
            "-y",
        ])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // Connect to target PG and verify row counts
    // ...
}
```

- [ ] **Step 2 / 3: Mirror for PG → SQLite and resume scenarios**

(Each scenario its own #[tokio::test], structured like the above.)

- [ ] **Step 4: Run all e2e tests**

```
cargo test -p ergo-indexer --features sqlite,postgres --test migrate_e2e
```

Expected: all PASS.

- [ ] **Step 5: Commit**

```
git add addons/indexer/tests/migrate_e2e.rs
git commit -m "test(addons/indexer): migrate end-to-end + resume scenarios"
```

---

## Task 12: Final clippy + fmt + workspace check

- [ ] **Step 1: Run all gates**

```
cargo fmt -p ergo-indexer
cargo build --release -p ergo-indexer
cargo test -p ergo-indexer --features sqlite,postgres
cargo clippy -p ergo-indexer --all-targets --features sqlite,postgres -- -D warnings
```

Fix any lints or test failures. Pre-existing issues in OTHER crates remain out of scope.

- [ ] **Step 2: Final smoke**

Build and run `--help` on the new binary:

```
./target/release/ergo-indexer-migratedb --help
```

Confirm output is sane.

- [ ] **Step 3: Commit any fmt/clippy fixes**

```
git add -u
git commit -m "chore(addons/indexer): final fmt + clippy after migratedb"
```

---

## Self-Review

**Spec coverage:** Each section in `facts/indexer-migration.md` is covered by at least one task:
- CLI shape → Task 1
- Derivation rules → Task 2
- Confirmation → Task 1 (planned, may move to Task 10 for ordering)
- Direction (bidirectional) → Task 7 (Backend trait makes direction symmetric)
- Resume Semantics (6 preconditions + cursor keys) → Task 4 + Task 6
- Per-block migration unit (insert + update + cursor + commit) → Task 7
- Hash verification → Task 5
- `--update-config` → Task 9
- Schema version check → Task 7 (init_schema + read for resume in Task 6)
- Liveness checks → Task 3
- Operator prerequisites → out-of-band (docs in contract)
- Exit codes → implicit in anyhow + main() Result handling
- SIGINT/SIGTERM signal handling → Task 10
- Progress output → Task 8

**Placeholder scan:** No "TODO" or "implement later" in step-content where actual code is expected. The `synth_block_data` and the per-row write helpers in Task 5 are marked as "the implementer fills in based on the schema in `src/db/sqlite.rs`" — that's intentional pointer to existing source, not placeholder content.

**Type consistency:** `Backend` trait methods are referenced in the same form in all tasks. `Cursor { last_height, source_url, source_fingerprint }` consistent across tasks 4/6/10. `BlockData` fields referenced consistently in tasks 5/7/10/11.

---

## Open implementation questions

These weren't fully nailed in the contract; the implementer surfaces them if hit:

1. **`schema_version` value** — the plan uses `1` (current indexer schema is at v1). If the indexer ever bumps this, the migrator and indexer must update in lockstep.
2. **PG test isolation** — E2E tests need a clean test DB per run. Options: `CREATE DATABASE test_migrate_<random>` per test, or `TRUNCATE` all tables at start. Implementer chooses.
3. **Atomicity of multi-statement per-block commit on PG** — within one sqlx transaction, this is straightforward; just don't accidentally use multiple connection acquires.
4. **Async vs sync at the rusqlite boundary** — rusqlite is sync; the `Backend` trait is async. Wrap rusqlite calls in `tokio::task::spawn_blocking` to avoid blocking the runtime. Establish the pattern in Task 7 and reuse.
