//! The `narsor` binary: logging plus the command-line interface.
//!
//! # Where the logs go, and why it depends on the subcommand
//!
//! For every subcommand but `tui` the logger writes to stderr, so that stdout
//! carries nothing but the output of the command — `narsor list | grep
//! paused` has to keep working with `RUST_LOG=debug` set.
//!
//! `tui` is the exception, because it owns the whole terminal: it switches to
//! the alternate screen, and a log line written to stderr lands on top of the
//! panels, corrupting a display that nothing redraws from scratch. So the
//! interface's logs go to a file under the data directory
//! (`{root}/narsor.log`, opened for appending), and if no filter is
//! configured — `RUST_LOG` unset or empty — no subscriber is installed at all
//! and the records are dropped where they are made. Either way nothing reaches
//! the terminal while the alternate screen is up.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use clap::Parser;
use narsor::cli::{self, Cli, Command};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// Name of the log file the terminal interface writes into, under `--root`.
const TUI_LOG_FILE: &str = "narsor.log";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parsed before the logger is installed, so that `--help` and a usage error
    // (exit code 2, from clap itself) are not preceded by log lines — and
    // because which logger to install depends on the subcommand.
    let cli = Cli::parse();

    init_tracing(&cli)?;

    std::process::exit(cli::run(cli).await);
}

/// Installs the logger the chosen subcommand can live with.
fn init_tracing(cli: &Cli) -> anyhow::Result<()> {
    if matches!(cli.command, Command::Tui) {
        // Nothing to write: leave the process without a subscriber rather than
        // create a log file no one asked for.
        if !filter_is_configured() {
            return Ok(());
        }
        let Some(file) = open_log_file(&cli.root) else {
            // The directory is not writable. The interface still runs; the
            // logs are the thing given up, never the screen.
            return Ok(());
        };
        let subscriber = FmtSubscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(Mutex::new(file))
            // A file is not a terminal: escape sequences would only make it
            // harder to read.
            .with_ansi(false)
            .with_target(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
        return Ok(());
    }

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

/// Whether the operator asked for any logging at all.
///
/// [`EnvFilter::from_default_env`] reads `RUST_LOG`; unset or empty it enables
/// nothing, so there is no reason to open a file for it.
fn filter_is_configured() -> bool {
    std::env::var(EnvFilter::DEFAULT_ENV)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Opens `{root}/narsor.log` for appending, creating the root if needed.
///
/// `None` when the file cannot be opened; the caller then runs without logs.
fn open_log_file(root: &Path) -> Option<File> {
    std::fs::create_dir_all(root).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(TUI_LOG_FILE))
        .ok()
}
