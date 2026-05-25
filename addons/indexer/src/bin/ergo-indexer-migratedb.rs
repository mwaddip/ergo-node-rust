//! `ergo-indexer-migratedb` — one-shot migrator between the indexer's SQLite
//! and PostgreSQL backends.
//!
//! See `facts/indexer-migration.md` for the full contract. This file wires
//! together the pieces built in T1–T9 and is the integration surface for the
//! operator-facing command.

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Result};
use clap::Parser;
use tokio::sync::watch;

use ergo_indexer::migrate::{
    self, config_update, liveness, progress::Progress, runner, Backend, DbSpec, DbType,
    MigrationPlan, PgEnv, PlanInputs,
};

const INDEXER_CONFIG_PATH: &str = "/etc/ergo-node/indexer.toml";
const DEFAULT_INDEXER_BIND: &str = "127.0.0.1:8080";

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 1. Build PlanInputs from CLI + env + config file.
    let pg_env = PgEnv {
        host: std::env::var("PGHOST").ok(),
        port: std::env::var("PGPORT").ok(),
        user: std::env::var("PGUSER").ok(),
        database: std::env::var("PGDATABASE").ok(),
        sslmode: std::env::var("PGSSLMODE").ok(),
    };
    let config_db = load_config_db_from_indexer_toml();
    let plan_inputs = PlanInputs {
        cli_in: cli.src,
        cli_out: cli.tgt,
        update_config: cli.update_config,
        resume: cli.resume,
        yes: cli.yes,
        config_db,
        pg_env,
    };

    // 2. Resolve the migration plan.
    let plan = migrate::resolve(plan_inputs)?;

    // 3. Liveness gate: refuse to proceed if the indexer is running, or if
    //    the source SQLite file is locked by another process.
    let indexer_bind =
        std::env::var("INDEXER_BIND").unwrap_or_else(|_| DEFAULT_INDEXER_BIND.to_string());
    liveness::assert_safe_to_proceed(&plan, &indexer_bind).await?;

    // 4. Confirmation prompt — refuses in non-TTY contexts without -y.
    if !plan.yes {
        if !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "non-interactive context but -y not set; pass -y to proceed without confirmation"
            );
        }
        print_confirmation_prompt(&plan);
        if !user_confirms()? {
            anyhow::bail!("user aborted");
        }
    }

    // 5. Connect both backends.
    let mut source = open_backend(&plan.source).await?;
    let mut target = open_backend(&plan.target).await?;

    // 6. Build the progress reporter. The runner derives the real start
    //    height itself (re-checking the resume preconditions), but the
    //    progress display needs an estimate of "blocks already done" so
    //    the leading percentage marker is right on resume.
    let max = source
        .max_height()
        .await?
        .ok_or_else(|| anyhow!("source has no blocks (max_height returned None)"))?;
    let starting_from = if plan.resume {
        target
            .read_cursor()
            .await?
            .map(|c| c.last_height)
            .unwrap_or(0)
    } else {
        0
    };
    let total_blocks = max.saturating_sub(starting_from);
    let is_tty = std::io::stdout().is_terminal();
    let mut progress = Progress::new(std::io::stdout(), total_blocks, starting_from, is_tty);

    // 7. Signal handling — watch channel flipped by SIGINT or SIGTERM.
    let (cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(install_signal_handler(cancel_tx));

    // 8. Run the migration loop.
    let started_at = Instant::now();
    match runner::run(
        source.as_mut(),
        target.as_mut(),
        &plan,
        &mut progress,
        cancel_rx,
    )
    .await
    {
        Ok(_final_height) => {
            progress.finalize(started_at.elapsed());
        }
        Err(e) => {
            // Don't call progress.finalize on error — print the diagnostic
            // and exit non-zero. The contract specifies: progress output on
            // stdout, all other diagnostics on stderr.
            eprintln!("Migration failed: {e}");
            std::process::exit(1);
        }
    }

    // 9. Optional config rewrite. Best-effort: if it fails (file missing,
    //    permission denied), log a warning but exit zero — the migration
    //    itself succeeded.
    if plan.update_config {
        let config_path = Path::new(INDEXER_CONFIG_PATH);
        match config_update::rewrite_storage_db(config_path, &plan.target.url) {
            Ok(()) => {
                println!(
                    "Config rewritten: [storage].db = {} (previous value preserved as a comment)",
                    plan.target.url
                );
            }
            Err(e) => {
                eprintln!(
                    "WARNING: failed to rewrite {}: {e}",
                    config_path.display()
                );
            }
        }
    } else {
        // Hint per contract § --update-config behavior.
        println!(
            "Migration complete. To switch the indexer to the new database,\n\
             update [storage].db in {} or pass --db <new-target> to the indexer service.",
            INDEXER_CONFIG_PATH
        );
    }

    Ok(())
}

/// Print the confirmation block per contract § Confirmation, then flush so the
/// prompt is visible before `user_confirms()` reads stdin.
fn print_confirmation_prompt(plan: &MigrationPlan) {
    println!("Migration plan:");
    println!("  Source:        {} ({})", plan.source.url, dbtype_label(plan.source.kind));
    println!("  Target:        {} ({})", plan.target.url, dbtype_label(plan.target.kind));
    println!(
        "  Update config: {}",
        if plan.update_config { "yes" } else { "no" }
    );
    println!(
        "  Resume:        {}",
        if plan.resume {
            "yes (resuming interrupted migration)"
        } else {
            "no (fresh migration)"
        }
    );
    print!("Proceed? [y/N]: ");
    let _ = std::io::stdout().flush();
}

fn dbtype_label(k: DbType) -> &'static str {
    match k {
        DbType::Sqlite => "sqlite",
        DbType::Postgres => "postgres",
    }
}

/// Read one line of stdin and return true iff it parses to a case-insensitive
/// `y` or `yes`. Anything else (including EOF) is treated as "no".
fn user_confirms() -> Result<bool> {
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

/// Best-effort read of `[storage].db` from `/etc/ergo-node/indexer.toml`.
///
/// Returns `None` for any error (file missing, parse error, missing keys) —
/// the caller (resolve) then surfaces the same "storage.db not configured"
/// diagnostic that the indexer itself uses.
fn load_config_db_from_indexer_toml() -> Option<String> {
    let path = Path::new(INDEXER_CONFIG_PATH);
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Value = toml::from_str(&content).ok()?;
    parsed
        .get("storage")?
        .get("db")?
        .as_str()
        .map(|s| s.to_string())
}

/// Open the appropriate backend for the given `DbSpec`. SQLite paths are
/// canonicalized by stripping `sqlite://` if present, falling through to a
/// bare filesystem path. PG URLs go straight to the sqlx pool builder.
async fn open_backend(spec: &DbSpec) -> Result<Box<dyn Backend>> {
    match spec.kind {
        DbType::Sqlite => {
            #[cfg(feature = "sqlite")]
            {
                let path: PathBuf = spec
                    .url
                    .strip_prefix("sqlite://")
                    .unwrap_or(&spec.url)
                    .into();
                let b = ergo_indexer::db::sqlite::SqliteBackend::new_for_migration(&path).await?;
                Ok(Box::new(b))
            }
            #[cfg(not(feature = "sqlite"))]
            anyhow::bail!("SQLite backend requires the `sqlite` feature at build time");
        }
        DbType::Postgres => {
            #[cfg(feature = "postgres")]
            {
                let b = ergo_indexer::db::postgres::PostgresBackend::new_for_migration(&spec.url)
                    .await?;
                Ok(Box::new(b))
            }
            #[cfg(not(feature = "postgres"))]
            anyhow::bail!("PostgreSQL backend requires the `postgres` feature at build time");
        }
    }
}

/// Wait for SIGTERM or SIGINT, then flip the watch channel to `true`.
///
/// The runner loop polls `*cancel.borrow()` between blocks; once this fires,
/// the next iteration bails cleanly. In-flight per-block transactions either
/// commit (if they reached COMMIT) or roll back via Drop. Worst-case waste:
/// one block of work, re-done by `--resume`.
async fn install_signal_handler(cancel_tx: watch::Sender<bool>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("WARNING: failed to register SIGINT handler: {e}");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("WARNING: failed to register SIGTERM handler: {e}");
            return;
        }
    };

    let signal_name = tokio::select! {
        _ = sigint.recv() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    };
    eprintln!("\nReceived {signal_name} — finishing current block then exiting");
    // Per contract: the signal handler does NOT mutate the active
    // transaction. The runner sees the cancel flag between blocks.
    let _ = cancel_tx.send(true);
}
