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
