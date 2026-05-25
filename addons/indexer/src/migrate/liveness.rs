use anyhow::Result;
use std::time::Duration;

const API_PING_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn api_responds(bind: &str) -> bool {
    let url = format!("http://{bind}/api/v1/info");
    let client = match reqwest::Client::builder().timeout(API_PING_TIMEOUT).build() {
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
    url.strip_prefix("sqlite://").unwrap_or(url).into()
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
        listener.set_nonblocking(true).unwrap();
        let app: Router = Router::new().route(
            "/api/v1/info",
            get(|| async { Json(json!({"indexedHeight": 1, "nodeHeight": 1})) }),
        );
        tokio::spawn(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app)
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
        conn.execute_batch("BEGIN EXCLUSIVE; CREATE TABLE t (x INT);")
            .unwrap();
        // While the transaction is open (holds the lock), our attempt must fail.
        let err = try_exclusive_sqlite_lock(&path).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("busy")
                || err.to_string().to_lowercase().contains("lock")
        );
    }

    #[tokio::test]
    async fn assert_safe_refuses_when_api_responds() {
        use super::super::{DbSpec, DbType, MigrationPlan};
        let bind = spawn_mock_indexer().await;
        let plan = MigrationPlan {
            source: DbSpec {
                url: "sqlite:///tmp/x.db".into(),
                kind: DbType::Sqlite,
            },
            target: DbSpec {
                url: "postgres://x@h/d".into(),
                kind: DbType::Postgres,
            },
            update_config: false,
            resume: false,
            yes: false,
        };
        let err = assert_safe_to_proceed(&plan, &bind).await.unwrap_err();
        assert!(err.to_string().contains("indexer appears to be running"));
    }
}
