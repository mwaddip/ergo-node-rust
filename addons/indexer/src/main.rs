#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod api;
mod db;
mod node_client;
mod parser;
mod sync;
pub mod types;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use tokio::sync::watch;

/// Bounded wait on sync's `JoinHandle` after the shutdown signal fires.
/// Caps how long the process blocks for sync to exit cleanly before forcing
/// termination. 30 s matches the node's `SHUTDOWN_GRACE` — chosen there as
/// "longer than any single flush could plausibly take" and the same reasoning
/// holds here. Single block-insert transactions commit in milliseconds; the
/// only path that approaches the limit is an in-flight reorg cascade.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

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
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ergo_indexer=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let start = Instant::now();

    let db = db::open_db(&cli.db).await?;

    // Shutdown signal — watch over oneshot because sync and api both need to
    // receive it independently in combined mode. SIGTERM (systemd stop) and
    // SIGINT (Ctrl-C) both flip the flag; consumers wait on `changed()`.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(install_signal_handler(shutdown_tx));

    match cli.mode {
        Some(Mode::Sync) => {
            tracing::info!("starting in sync-only mode");
            sync::run(db, &cli.node_url, cli.start_height, shutdown_rx).await
        }
        Some(Mode::Serve) => {
            tracing::info!(%cli.bind, "starting in serve-only mode");
            api::serve(db, cli.bind, start, cli.node_url.clone(), shutdown_rx).await
        }
        None => {
            tracing::info!(%cli.bind, "starting in combined mode (sync + serve)");
            let db2 = db::open_db(&cli.db).await?;
            let sync_shutdown_rx = shutdown_rx.clone();
            let api_shutdown_rx = shutdown_rx.clone();
            let mut sync_handle = tokio::spawn({
                let node_url = cli.node_url.clone();
                let start_height = cli.start_height;
                async move { sync::run(db, &node_url, start_height, sync_shutdown_rx).await }
            });
            let mut api_handle = tokio::spawn(api::serve(
                db2,
                cli.bind,
                start,
                cli.node_url.clone(),
                api_shutdown_rx,
            ));

            // Wait until either (a) a task fails on its own, or (b) the
            // shutdown signal fires. Case (b) is the normal SIGTERM path —
            // we then await both tasks within `SHUTDOWN_GRACE`.
            let mut shutdown_rx = shutdown_rx;
            let early_exit = tokio::select! {
                r = &mut sync_handle => Some(("sync", flatten_join(r))),
                r = &mut api_handle => Some(("api", flatten_join(r))),
                _ = shutdown_rx.changed() => None,
            };

            if let Some((task, result)) = early_exit {
                // One task exited before the shutdown signal — propagate
                // its error, drop the other handle (tokio aborts the task).
                tracing::info!(task, "task exited; stopping the other");
                return result;
            }

            // Shutdown signal fired. Wait for both handles to complete
            // within the grace period. Anything not done by then gets
            // aborted by the runtime when this function returns.
            let drain = async {
                let (sync_r, api_r) =
                    tokio::join!(&mut sync_handle, &mut api_handle);
                (flatten_join(sync_r), flatten_join(api_r))
            };
            match tokio::time::timeout(SHUTDOWN_GRACE, drain).await {
                Ok((sync_r, api_r)) => {
                    tracing::info!("indexer stopped");
                    sync_r.and(api_r)
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = SHUTDOWN_GRACE.as_secs(),
                        "tasks did not complete within shutdown timeout; forcing exit"
                    );
                    Ok(())
                }
            }
        }
    }
}

/// Collapse a `JoinHandle` result + inner `anyhow::Result` into one.
fn flatten_join(
    r: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match r {
        Ok(inner) => inner,
        Err(e) => Err(anyhow::anyhow!("task panicked: {e}")),
    }
}

/// Wait for SIGTERM or SIGINT, then flip the shutdown watch flag.
///
/// Both signals map to the same outcome — there's no "soft" vs "hard"
/// distinction here. The watch flag is one-shot in practice; once set to
/// `true` it stays true for the process lifetime.
async fn install_signal_handler(shutdown_tx: watch::Sender<bool>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to register SIGTERM handler");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to register SIGINT handler");
            return;
        }
    };

    let signal_name = tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
    };
    tracing::info!(signal = signal_name, "shutdown signal received");
    let _ = shutdown_tx.send(true);
}
