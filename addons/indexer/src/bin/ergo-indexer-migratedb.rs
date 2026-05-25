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
