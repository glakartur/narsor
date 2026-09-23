//! The `orchestrator` binary: logging plus the command-line interface.
//!
//! The logger writes to stderr so that stdout carries nothing but the output of
//! the subcommand — `orchestrator list | grep paused` has to keep working with
//! `RUST_LOG=debug` set.

use clap::Parser;
use orchestrator::cli::{self, Cli};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parsed before the logger is installed, so that `--help` and a usage error
    // (exit code 2, from clap itself) are not preceded by log lines.
    let cli = Cli::parse();

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    std::process::exit(cli::run(cli).await);
}
