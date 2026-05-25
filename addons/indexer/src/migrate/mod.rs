pub mod cursor;
pub mod liveness;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

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

// ---------------------------------------------------------------------------
// Backend trait + Cursor + row types
// ---------------------------------------------------------------------------

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

/// One row from the `blocks` table.
///
/// Column source (`addons/indexer/src/db/sqlite.rs` lines 66–74):
///   height INTEGER PRIMARY KEY, header_id BLOB NOT NULL UNIQUE,
///   timestamp INTEGER NOT NULL, difficulty INTEGER NOT NULL,
///   miner_pk BLOB NOT NULL, block_size INTEGER NOT NULL,
///   tx_count INTEGER NOT NULL
///
/// Note: `height` is the real PK in the schema (not `header_id` as the
/// contract stub skeleton suggested). This matches both SQLite and PG
/// schemas verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockRow {
    pub height: u32,
    pub header_id: [u8; 32],
    pub timestamp: i64,
    pub difficulty: i64,
    pub miner_pk: Vec<u8>,
    pub block_size: u32,
    pub tx_count: u32,
}

/// One row from the `transactions` table.
///
/// Column source (`addons/indexer/src/db/sqlite.rs` lines 76–81):
///   tx_id BLOB PRIMARY KEY, header_id BLOB NOT NULL, height INTEGER NOT NULL,
///   tx_index INTEGER NOT NULL, size INTEGER NOT NULL
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionRow {
    pub tx_id: [u8; 32],
    pub header_id: [u8; 32],
    pub height: u32,
    pub tx_index: u32,
    pub size: u32,
}

/// One row from the `boxes` table (created side — `spent_tx_id` / `spent_height`
/// are NULL at creation time and are excluded here; spend state is carried via
/// `BoxSpentUpdate`).
///
/// Column source (`addons/indexer/src/db/sqlite.rs` lines 83–96):
///   box_id BLOB PRIMARY KEY, tx_id BLOB NOT NULL, header_id BLOB NOT NULL,
///   height INTEGER NOT NULL, output_index INTEGER NOT NULL,
///   ergo_tree BLOB NOT NULL, ergo_tree_hash BLOB NOT NULL,
///   address TEXT NOT NULL, value INTEGER NOT NULL,
///   spent_tx_id BLOB (nullable), spent_height INTEGER (nullable)
///
/// `spent_tx_id` and `spent_height` are included because `read_block_data`
/// needs to round-trip the full row for hash verification.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxRow {
    pub box_id: [u8; 32],
    pub tx_id: [u8; 32],
    pub header_id: [u8; 32],
    pub height: u32,
    pub output_index: u32,
    pub ergo_tree: Vec<u8>,
    pub ergo_tree_hash: [u8; 32],
    pub address: String,
    pub value: i64,
    pub spent_tx_id: Option<[u8; 32]>,
    pub spent_height: Option<u32>,
}

/// One row from the `box_registers` table.
///
/// Column source (`addons/indexer/src/db/sqlite.rs` lines 105–110):
///   box_id BLOB NOT NULL, register_id INTEGER NOT NULL, serialized BLOB NOT NULL
#[derive(Debug, Clone, PartialEq)]
pub struct BoxRegisterRow {
    pub box_id: [u8; 32],
    pub register_id: u8,
    pub serialized: Vec<u8>,
}

/// One row from the `box_tokens` table.
///
/// Column source (`addons/indexer/src/db/sqlite.rs` lines 98–103):
///   box_id BLOB NOT NULL, token_id BLOB NOT NULL, amount INTEGER NOT NULL
#[derive(Debug, Clone, PartialEq)]
pub struct BoxTokenRow {
    pub box_id: [u8; 32],
    pub token_id: [u8; 32],
    pub amount: i64,
}

/// One row from the `tokens` table.
///
/// Column source (`addons/indexer/src/db/sqlite.rs` lines 112–119):
///   token_id BLOB PRIMARY KEY, minting_tx_id BLOB NOT NULL,
///   minting_height INTEGER NOT NULL, name TEXT (nullable),
///   description TEXT (nullable), decimals INTEGER (nullable)
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    pub token_id: [u8; 32],
    pub minting_tx_id: [u8; 32],
    pub minting_height: u32,
    pub name: Option<String>,
    pub description: Option<String>,
    pub decimals: Option<i32>,
}

/// The UPDATE-side counterpart to `BoxRow`. Carries the box_id and the columns
/// that change on spend, derived from the UPDATE statement in
/// `addons/indexer/src/db/sqlite.rs` line 328:
///   UPDATE boxes SET spent_tx_id = ?1, spent_height = ?2 WHERE box_id = ?3
#[derive(Debug, Clone, PartialEq)]
pub struct BoxSpentUpdate {
    pub box_id: [u8; 32],
    pub spent_tx_id: [u8; 32],
    pub spent_height: u32,
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
