mod cli;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing();

    let parsed = Cli::parse();
    let config_path = parsed.config.as_deref();

    match parsed.command {
        Command::Catalog { command } => cli::catalog::run(config_path, command).await,
        Command::Serve { bind } => cli::serve::run(cli::node::open(config_path).await?, bind).await,
        Command::Scan { source, hash } => {
            let context = cli::node::open(config_path).await?;
            let policy = cli::hash_policy(hash, context.config.index.hash_policy);
            cli::scan::run(context, source, policy).await
        }
        Command::Ls {
            query,
            json,
            limit,
            sort,
            asc,
        } => {
            cli::ls::run(
                cli::node::open(config_path).await?,
                query,
                json,
                limit,
                sort,
                asc,
            )
            .await
        }
        Command::Path { target } => {
            cli::path::run(cli::node::open(config_path).await?, target).await
        }
        Command::Loadout { command } => {
            cli::loadout::run(cli::node::open(config_path).await?, command).await
        }
        Command::Apply {
            loadout,
            node,
            profile,
        } => cli::apply::run(cli::node::open(config_path).await?, loadout, node, profile).await,
        Command::Link {
            dir,
            profile,
            query,
            shelf,
        } => {
            cli::link::run(
                cli::node::open(config_path).await?,
                dir,
                profile,
                query,
                shelf,
            )
            .await
        }
        Command::Nodes => cli::node::list(cli::node::open(config_path).await?).await,
        Command::Doctor => cli::doctor::run(cli::node::open(config_path).await?).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("hoard=info,hoard_server=info,hoard_index=info,hoard_mirror=info")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();
}
