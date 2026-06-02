//! Config-file + CLI + env-var resolution per `facts/indexer.md` v1.2.1.
//!
//! Precedence per key (low → high): defaults < file < CLI. Libpq env vars
//! (`PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`, `PGSSLMODE`)
//! layer on top of the resolved Postgres URL at connection time — see
//! `db::postgres::apply_libpq_env`.
//!
//! `storage.db` is REQUIRED — there is no built-in default. Resolution
//! errors if neither the file nor the CLI supplies it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_NODE_URL: &str = "http://127.0.0.1:9053";
const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// Diagnostic emitted when neither config file nor CLI supplies `storage.db`.
/// Spelled here so tests can assert against the exact wording.
pub const MISSING_DB_DIAGNOSTIC: &str =
    "storage.db not configured — set [storage].db in the config file or pass --db <url-or-path>";

/// Default config-file path. Sibling to the node's `ergo.toml`.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/ergo-node/indexer.toml";

/// Schema of `/etc/ergo-node/indexer.toml`. All sections + keys are optional;
/// unknown sections/keys are rejected so typos surface immediately.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub node: NodeSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default)]
    pub api: ApiSection,
    #[serde(default)]
    pub sync: SyncSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    pub url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSection {
    pub db: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSection {
    pub bind: Option<SocketAddr>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSection {
    pub start_height: Option<u64>,
}

/// Resolution inputs from the CLI parser. `None` means "user did not set this
/// flag" (falls through to file → defaults).
#[derive(Debug, Default)]
pub struct CliInput {
    pub node_url: Option<String>,
    pub db: Option<String>,
    pub bind: Option<SocketAddr>,
    pub start_height: Option<u64>,
}

/// Effective config used by the rest of the process. All fields fully resolved.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub node_url: String,
    pub db: String,
    pub bind: SocketAddr,
    pub start_height: Option<u64>,
}

/// Resolve the path of the TOML file to load. CLI `--config` wins over
/// `INDEXER_CONFIG` env, which wins over the built-in default.
pub fn resolve_config_path(cli_config: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli_config {
        return p;
    }
    if let Ok(p) = std::env::var("INDEXER_CONFIG") {
        return PathBuf::from(p);
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}

/// Load a TOML file from `path`.
///
/// * `Ok(Some(_))` — file present and parsed.
/// * `Ok(None)` — file absent at the resolved path. Caller logs and falls back
///   to defaults + CLI per the contract's "no config exists" behavior.
/// * `Err(_)` — file present but malformed (unknown keys, syntax errors, wrong
///   types). The caller is expected to exit non-zero with the underlying
///   diagnostic; do NOT silently fall back, per the contract.
pub fn load_file(path: &Path) -> Result<Option<FileConfig>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let cfg: FileConfig = toml::from_str(&s)
                .with_context(|| format!("failed to parse config file {}", path.display()))?;
            Ok(Some(cfg))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e))
            .with_context(|| format!("failed to read config file {}", path.display())),
    }
}

/// Apply the precedence ladder per-key. Errors with [`MISSING_DB_DIAGNOSTIC`]
/// when neither the file nor the CLI supplies `storage.db` — per contract
/// v1.2.1 the DB target is required and has no built-in default.
pub fn resolve(cli: CliInput, file: Option<FileConfig>) -> Result<EffectiveConfig> {
    let file = file.unwrap_or_default();
    let db = cli
        .db
        .or(file.storage.db)
        .ok_or_else(|| anyhow::anyhow!(MISSING_DB_DIAGNOSTIC))?;
    Ok(EffectiveConfig {
        node_url: cli
            .node_url
            .or(file.node.url)
            .unwrap_or_else(|| DEFAULT_NODE_URL.to_string()),
        db,
        bind: cli
            .bind
            .or(file.api.bind)
            .unwrap_or_else(|| DEFAULT_BIND.parse().expect("static parse")),
        start_height: cli.start_height.or(file.sync.start_height),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // File absent + db supplied via CLI → defaults applied to the other
    // keys, no error. (db has no built-in default per v1.2.1; see
    // `config_db_required_*` tests for the error path.)
    #[test]
    fn config_missing_uses_defaults() {
        let path = std::env::temp_dir().join("indexer-test-missing.toml");
        let _ = std::fs::remove_file(&path);
        let loaded = load_file(&path).expect("missing file is not an error");
        assert!(loaded.is_none(), "missing file should produce None");

        let cli = CliInput {
            db: Some("sqlite:///tmp/indexer.db".to_string()),
            ..Default::default()
        };
        let cfg = resolve(cli, loaded).expect("resolve succeeds when db is supplied");
        assert_eq!(cfg.node_url, DEFAULT_NODE_URL);
        assert_eq!(cfg.db, "sqlite:///tmp/indexer.db");
        assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
        assert_eq!(cfg.start_height, None);
    }

    /// Regression for contract v1.2.1: no file + no `--db` → hard error with
    /// the exact diagnostic operators will see.
    #[test]
    fn config_db_required_no_file_no_cli_errors() {
        let err = resolve(CliInput::default(), None)
            .expect_err("missing db must error when no file and no CLI");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(MISSING_DB_DIAGNOSTIC),
            "wrong diagnostic: {msg}"
        );
    }

    /// Regression for contract v1.2.1: file present but `[storage]` omitted /
    /// `db` unset, no `--db` → same hard error. Covers the realistic case
    /// where an operator's config file sets node/api/sync but forgets storage.
    #[test]
    fn config_db_required_file_without_storage_section_errors() {
        let file = FileConfig {
            node: NodeSection {
                url: Some("http://x:9052".to_string()),
            },
            // storage left default (None) — equivalent to no [storage] section
            // OR [storage] with no `db` key.
            ..Default::default()
        };
        let err = resolve(CliInput::default(), Some(file))
            .expect_err("file without storage.db must error when no CLI override");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(MISSING_DB_DIAGNOSTIC),
            "wrong diagnostic: {msg}"
        );
    }

    /// 2. Malformed TOML → hard error with diagnostic (line/column from toml crate).
    #[test]
    fn config_malformed_hard_error() {
        let dir = tempdir();
        let path = dir.join("bad.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        // Unclosed bracket on first key — toml errors with line/column.
        f.write_all(b"[node\nurl = \"http://x\"\n").unwrap();
        drop(f);

        let err = load_file(&path).expect_err("malformed TOML must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to parse config file"),
            "missing parse context: {msg}"
        );
    }

    /// 3a. Unknown top-level section → reject.
    #[test]
    fn config_unknown_section_rejected() {
        let dir = tempdir();
        let path = dir.join("unknown-section.toml");
        std::fs::write(
            &path,
            b"[node]\nurl = \"http://x\"\n[unknown_section]\nfoo = 1\n",
        )
        .unwrap();

        let err = load_file(&path).expect_err("unknown section must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown_section") || msg.contains("unknown field"),
            "unhelpful diagnostic: {msg}"
        );
    }

    /// 3b. Unknown key inside a known section → reject.
    #[test]
    fn config_unknown_keys_rejected() {
        let dir = tempdir();
        let path = dir.join("unknown-key.toml");
        std::fs::write(&path, b"[node]\nurl = \"http://x\"\nbogus_key = 42\n").unwrap();

        let err = load_file(&path).expect_err("unknown key must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus_key") || msg.contains("unknown field"),
            "unhelpful diagnostic: {msg}"
        );
    }

    /// 4. CLI flags override file values per key.
    #[test]
    fn cli_overrides_file() {
        let file = FileConfig {
            storage: StorageSection {
                db: Some("sqlite://path/a".to_string()),
            },
            ..Default::default()
        };
        let cli = CliInput {
            db: Some("sqlite://path/b".to_string()),
            ..Default::default()
        };
        let cfg = resolve(cli, Some(file)).expect("both file and cli supply db");
        assert_eq!(cfg.db, "sqlite://path/b");
    }

    /// File value applied when CLI does NOT set the flag.
    #[test]
    fn file_applied_when_cli_unset() {
        let file = FileConfig {
            node: NodeSection {
                url: Some("http://from-file:1234".to_string()),
            },
            storage: StorageSection {
                db: Some("sqlite:///var/lib/ergo-indexer/index.db".to_string()),
            },
            api: ApiSection {
                bind: Some("0.0.0.0:9054".parse().unwrap()),
            },
            sync: SyncSection {
                start_height: Some(1_782_320),
            },
        };
        let cfg = resolve(CliInput::default(), Some(file)).expect("file supplies storage.db");
        assert_eq!(cfg.node_url, "http://from-file:1234");
        assert_eq!(cfg.bind.to_string(), "0.0.0.0:9054");
        assert_eq!(cfg.start_height, Some(1_782_320));
        assert_eq!(cfg.db, "sqlite:///var/lib/ergo-indexer/index.db");
    }

    /// `--config` CLI arg wins over `INDEXER_CONFIG`. Cannot test the env-var
    /// fallback in parallel without contaminating other tests' env, but the
    /// `--config` path is the operator-facing one.
    #[test]
    fn resolve_path_prefers_cli_over_env() {
        let p = resolve_config_path(Some(PathBuf::from("/tmp/cli-supplied.toml")));
        assert_eq!(p, PathBuf::from("/tmp/cli-supplied.toml"));
    }

    /// Default applies when neither CLI nor env set the path. Done in a way
    /// that doesn't depend on the test env having INDEXER_CONFIG unset — we
    /// only assert the CLI-supplied case takes precedence (other case covered
    /// by manual smoke test).
    #[test]
    fn resolve_path_default_when_no_cli() {
        // Best-effort: if INDEXER_CONFIG is set in the test env, skip the
        // default-path assertion to avoid flakiness.
        if std::env::var("INDEXER_CONFIG").is_ok() {
            return;
        }
        let p = resolve_config_path(None);
        assert_eq!(p, PathBuf::from(DEFAULT_CONFIG_PATH));
    }

    // SIGHUP ignored: covered by the signal handler in `main.rs`.
    // TODO: integration-style test that spawns the binary, sends SIGHUP,
    // and asserts the process is still alive after a grace period. Skipped
    // here because the crate has no integration-test harness yet and a
    // single behavior test does not justify standing one up.
    #[test]
    fn sighup_ignored_documented() {
        // Marker test — keeps the contract requirement visible in the test
        // list. The handler itself lives in `main.rs::install_signal_handler`.
    }

    /// Helper: per-test temp directory. Avoids the `tempfile` dep.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("indexer-test-{pid}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
