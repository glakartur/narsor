//! The `narsor` binary: configuration, logging, then the command line.
//!
//! # The order of the first three steps
//!
//! `Cli::parse` first, so that `--help` and a usage error (exit code 2, from
//! clap itself) are not preceded by anything. Then the configuration, because
//! the logger's level and the file it writes into are both settings now. Only
//! then the subscriber, and only then the subcommand.
//!
//! Resolving the configuration therefore happens before any subscriber exists,
//! so its own failures go to stderr through `eprintln!` — the same way
//! [`cli::run`] already reports a [`CliError`](narsor::cli::commands::CliError).
//! Nothing in that path logs through `tracing`, so nothing is lost.
//!
//! # Where the logs go, and why it depends on the subcommand
//!
//! For every subcommand but `tui` the logger writes to stderr, so that stdout
//! carries nothing but the output of the command — `narsor list | grep paused`
//! has to keep working with `RUST_LOG=debug` set.
//!
//! `tui` is the exception, because it owns the whole terminal: it switches to
//! the alternate screen, and a log line written to stderr lands on top of the
//! panels, corrupting a display that nothing redraws from scratch. So the
//! interface's logs go to a file under the resolved data directory
//! (`{data_dir}/narsor.log`, opened for appending).
//!
//! When nothing turns logging on — no `RUST_LOG`, no configured level — no
//! subscriber is installed at all and the records are dropped where they are
//! made. That is today's behaviour and it is kept deliberately: the built-in
//! level is `off`, so a `tui` run still creates neither a data directory nor a
//! log file unless logging was actually asked for (D-N).
//!
//! # A broken configuration stops the run
//!
//! A configuration file that exists but does not hold up stops the subcommand
//! with exit code 1, naming every fault at once. A `--config` path that is not
//! there is exit code 2 instead: the operator named a file that does not
//! exist, so nothing was run and nothing could be.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use clap::Parser;
use narsor::cli::{self, Cli, Command, EXIT_FAILURE, EXIT_USAGE};
use narsor::config::{self, Flags, Resolved};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// Name of the log file the terminal interface writes into, under the data
/// directory.
const TUI_LOG_FILE: &str = "narsor.log";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let flags = Flags {
        root: cli.root.clone(),
        config: cli.config.clone(),
    };
    // Read once, here, and handed in: resolution never reaches for the process
    // environment itself, which is what keeps it testable without set_var.
    let environment = config::resolve::environment_from_process();
    let resolved = config::resolve(&flags, &environment);

    if let Some(code) = refuse_broken_configuration(&cli, &resolved) {
        std::process::exit(code);
    }

    init_tracing(&cli, &resolved)?;

    std::process::exit(cli::run(cli, resolved).await);
}

/// The exit code to stop on, when the configuration is not usable.
///
/// `None` lets the command run.
fn refuse_broken_configuration(cli: &Cli, resolved: &Resolved) -> Option<i32> {
    if resolved.is_ok() {
        return None;
    }

    // A named file that is not there stops even the command that exists to
    // diagnose configuration, because nothing about it is diagnosable.
    if resolved.has_usage_error() {
        report(resolved);
        return Some(EXIT_USAGE);
    }

    let _ = cli;
    report(resolved);
    Some(EXIT_FAILURE)
}

/// Prints every configuration failure on stderr, one per line.
fn report(resolved: &Resolved) {
    for error in resolved.errors() {
        eprintln!("{error}");
    }
}

/// Installs the logger the chosen subcommand can live with.
fn init_tracing(cli: &Cli, resolved: &Resolved) -> anyhow::Result<()> {
    // Nothing configured: leave the process without a subscriber, which is
    // what makes every subcommand silent by default (D-N).
    let Some(directive) = resolved.log_directive() else {
        return Ok(());
    };

    if matches!(cli.command, Command::Tui) {
        let Some(file) = open_log_file(resolved.data_dir()) else {
            // The directory is not writable. The interface still runs; the
            // logs are the thing given up, never the screen.
            return Ok(());
        };
        let subscriber = FmtSubscriber::builder()
            .with_env_filter(filter(&directive))
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
        .with_env_filter(filter(&directive))
        .with_writer(std::io::stderr)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

/// The filter for the resolved directive.
///
/// The directive is either a bare level from the settings or whatever
/// `RUST_LOG` held, which may name targets. A directive that does not parse
/// falls back to no filtering at all rather than stopping the command — the
/// logs are never worth more than the run.
fn filter(directive: &str) -> EnvFilter {
    EnvFilter::try_new(directive).unwrap_or_else(|_| EnvFilter::new("off"))
}

/// Opens `{data_dir}/narsor.log` for appending, creating the directory if
/// needed.
///
/// `None` when the file cannot be opened; the caller then runs without logs.
fn open_log_file(data_dir: &Path) -> Option<File> {
    std::fs::create_dir_all(data_dir).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join(TUI_LOG_FILE))
        .ok()
}
