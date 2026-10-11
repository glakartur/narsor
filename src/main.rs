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
//! # Where CLI logs go
//!
//! CLI logs go to stderr so stdout carries only command output. The separate
//! `narsor` TUI process writes logs to the resolved state directory.
//!
//! When nothing turns logging on — no `RUST_LOG`, no configured level — no
//! subscriber is installed at all and the records are dropped where they are
//! made. That is today's behaviour and it is kept deliberately: the built-in
//! level is `off`, so the CLI still creates no data directory unless logging
//! was explicitly requested (D-N).
//!
//! # The one command that survives a broken configuration
//!
//! A configuration file that exists but does not hold up stops the subcommand
//! with exit code 1, naming every fault at once — except `config show`, which
//! is the only instrument there is for diagnosing exactly that (D-D). It
//! prints what it has, marks the rows that did not hold up, and exits 1
//! itself.
//!
//! The exception does not extend to a `--config` path that is not there: that
//! is exit code 2 for every command including this one, because the operator
//! named a file that does not exist and there is nothing to diagnose.

use clap::Parser;
use narsor::cli::{self, Cli, EXIT_FAILURE, EXIT_USAGE};
use narsor::config::{self, Flags, Resolved};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

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

    init_tracing(&resolved)?;

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

    if cli.command.runs_on_broken_config() {
        return None;
    }

    report(resolved);
    Some(EXIT_FAILURE)
}

/// Prints every configuration failure on stderr, one per line.
fn report(resolved: &Resolved) {
    for error in resolved.errors() {
        eprintln!("{error}");
    }
}

/// Installs the CLI logger.
fn init_tracing(resolved: &Resolved) -> anyhow::Result<()> {
    // Nothing configured: leave the process without a subscriber, which is
    // what makes every subcommand silent by default (D-N).
    let Some(directive) = resolved.log_directive() else {
        return Ok(());
    };

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
/// `RUST_LOG` held, which may name targets.
///
/// Parsed *lossily*, which is what `EnvFilter::from_default_env` did before
/// the configuration layer existed: `RUST_LOG="info,narsor=nonsense"` keeps
/// the `info` that parsed and complains on stderr about the part that did not.
/// Refusing the whole directive would silence a run that asked to be noisy,
/// over a typo, with nothing said — and the logs are never worth more than
/// the run.
fn filter(directive: &str) -> EnvFilter {
    EnvFilter::builder().parse_lossy(directive)
}
