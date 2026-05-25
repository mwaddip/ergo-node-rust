#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod api;
mod config;
mod db;
pub mod migrate;
mod node_client;
mod parser;
mod sync;
pub mod types;

use std::net::SocketAddr;
use std::path::PathBuf;
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
#[command(
    name = "ergo-indexer",
    version,
    about = "Blockchain indexer for ergo-node-rust"
)]
struct Cli {
    /// Path to TOML config file. Falls back to $INDEXER_CONFIG, then
    /// `/etc/ergo-node/indexer.toml`. A missing file is not an error.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Node REST API URL. Overrides `[node].url` from the config file.
    #[arg(long)]
    node_url: Option<String>,

    /// Database path (SQLite) or URL (postgres://...). Overrides
    /// `[storage].db` from the config file.
    #[arg(long)]
    db: Option<String>,

    /// Query API bind address. Overrides `[api].bind` from the config file.
    #[arg(long)]
    bind: Option<SocketAddr>,

    /// Resume from specific height (default: auto-detect from DB).
    /// Overrides `[sync].start_height` from the config file.
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

    let config_path = config::resolve_config_path(cli.config.clone());
    let file = config::load_file(&config_path)?;
    if file.is_none() {
        tracing::info!(
            path = %config_path.display(),
            "config file not found, using defaults"
        );
    }
    let cfg = config::resolve(
        config::CliInput {
            node_url: cli.node_url,
            db: cli.db,
            bind: cli.bind,
            start_height: cli.start_height,
        },
        file,
    )?;

    let db = db::open_db(&cfg.db).await?;

    // Shutdown signal — watch over oneshot because sync and api both need to
    // receive it independently in combined mode. SIGTERM (systemd stop) and
    // SIGINT (Ctrl-C) both flip the flag; consumers wait on `changed()`.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(install_signal_handler(shutdown_tx));
    // SIGHUP is explicitly ignored — without an explicit handler the default
    // action terminates the process, which would surprise operators expecting
    // libpq-style "reload on HUP." The contract specifies no live reload.
    tokio::spawn(ignore_sighup());

    match cli.mode {
        Some(Mode::Sync) => {
            tracing::info!("starting in sync-only mode");
            sync::run(db, &cfg.node_url, cfg.start_height, shutdown_rx).await
        }
        Some(Mode::Serve) => {
            tracing::info!(bind = %cfg.bind, "starting in serve-only mode");
            api::serve(db, cfg.bind, start, cfg.node_url.clone(), shutdown_rx).await
        }
        None => {
            tracing::info!(bind = %cfg.bind, "starting in combined mode (sync + serve)");
            let db2 = db::open_db(&cfg.db).await?;
            let sync_shutdown_rx = shutdown_rx.clone();
            let api_shutdown_rx = shutdown_rx.clone();
            let mut sync_handle = tokio::spawn({
                let node_url = cfg.node_url.clone();
                let start_height = cfg.start_height;
                async move { sync::run(db, &node_url, start_height, sync_shutdown_rx).await }
            });
            let mut api_handle = tokio::spawn(api::serve(
                db2,
                cfg.bind,
                start,
                cfg.node_url.clone(),
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
                let (sync_r, api_r) = tokio::join!(&mut sync_handle, &mut api_handle);
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
fn flatten_join(r: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
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

/// SIGHUP handler that drains and discards the signal.
///
/// Without an explicit handler the default action terminates the process —
/// surprising for operators who expect HUP to mean "reload config." The
/// indexer's contract specifies that config is read once at startup and
/// `systemctl restart` is the supported reload path; HUP is therefore a no-op.
async fn ignore_sighup() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to register SIGHUP handler — default action (terminate) will apply");
            return;
        }
    };
    while sighup.recv().await.is_some() {
        tracing::info!("received SIGHUP — ignored (restart required to reload config)");
    }
}
