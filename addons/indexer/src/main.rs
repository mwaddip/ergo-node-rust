mod api;
mod db;
mod node_client;
mod parser;
mod sync;
pub mod types;

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
            api::serve(db, cli.bind, start, cli.node_url.clone()).await
        }
        None => {
            tracing::info!(%cli.bind, "starting in combined mode (sync + serve)");
            let db2 = db::open_db(&cli.db).await?;
            let sync_handle = tokio::spawn({
                let node_url = cli.node_url.clone();
                let start_height = cli.start_height;
                async move { sync::run(db, &node_url, start_height).await }
            });
            let api_handle = tokio::spawn(api::serve(db2, cli.bind, start, cli.node_url.clone()));
            tokio::select! {
                r = sync_handle => r?,
                r = api_handle => r?,
            }
        }
    }
}
