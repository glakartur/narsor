use std::path::PathBuf;

use clap::Parser;
use narsor::config::{self, Flags};
use narsor::daemon::runtime::run_daemon;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Debug, Parser)]
#[command(name = "narsor-server", about = "Run the Narsor API server")]
struct Args {
    /// Narsor data directory (executions and workspaces).
    #[arg(long, value_name = "DIRECTORY")]
    root: Option<PathBuf>,

    /// Configuration file to read instead of searching for one.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let flags = Flags {
        root: args.root,
        config: args.config,
    };
    let environment = config::resolve::environment_from_process();
    let resolved = config::resolve(&flags, &environment);
    if !resolved.is_ok() {
        for error in resolved.errors() {
            eprintln!("{error}");
        }
        std::process::exit(if resolved.has_usage_error() { 2 } else { 1 });
    }

    if let Some(directive) = resolved.log_directive() {
        let subscriber = FmtSubscriber::builder()
            .with_env_filter(EnvFilter::builder().parse_lossy(directive))
            .with_writer(std::io::stderr)
            .with_target(false)
            .finish();
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            eprintln!("could not initialize tracing subscriber");
            std::process::exit(1);
        }
    }

    if let Err(error) = run_daemon(&resolved).await {
        eprintln!("{}", error.message);
        std::process::exit(error.exit_code);
    }
}
