use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

use clap::Parser;
use narsor::cli::{self, EXIT_FAILURE, EXIT_USAGE};
use narsor::config::{self, Flags};
use narsor::persistence::StatePaths;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Debug, Parser)]
#[command(name = "narsor", about = "Narsor terminal interface")]
struct Args {
    /// Narsor data directory (executions and workspaces).
    #[arg(long, global = true, value_name = "DIRECTORY")]
    root: Option<PathBuf>,

    /// Configuration file to read instead of searching for one.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The control plane to talk to.
    #[arg(long, global = true, env = "NARSOR_SERVER", value_name = "URL")]
    server: Option<String>,

    /// File holding the OIDC access token for `--server`.
    #[arg(long, global = true, env = "NARSOR_TOKEN_FILE", value_name = "FILE")]
    token_file: Option<PathBuf>,

    /// Command printing the OIDC access token for `--server`.
    #[arg(
        long,
        global = true,
        env = "NARSOR_TOKEN_COMMAND",
        value_name = "COMMAND"
    )]
    token_command: Option<String>,

    /// Kept for compatibility; the interface always uses the configured backend.
    #[arg(long)]
    remote: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Some(warning) = cli::tui_remote_warning(args.remote) {
        eprintln!("{warning}");
    }
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
        std::process::exit(if resolved.has_usage_error() {
            EXIT_USAGE
        } else {
            EXIT_FAILURE
        });
    }
    if let Some(directive) = resolved.log_directive() {
        if let Some(file) = open_log_file(&resolved.state_paths()) {
            let subscriber = FmtSubscriber::builder()
                .with_env_filter(EnvFilter::builder().parse_lossy(directive))
                .with_writer(Mutex::new(file))
                .with_ansi(false)
                .with_target(false)
                .finish();
            if tracing::subscriber::set_global_default(subscriber).is_err() {
                eprintln!("could not initialize tracing subscriber");
                std::process::exit(EXIT_FAILURE);
            }
        }
    }
    let target = cli::remote::server_target(args.server, args.token_file, args.token_command);
    if let Err(error) = cli::commands::tui(&resolved, &flags, &target) {
        eprintln!("{}", error.message);
        std::process::exit(error.exit_code);
    }
}

fn open_log_file(paths: &StatePaths) -> Option<File> {
    std::fs::create_dir_all(paths.root()).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log_file())
        .ok()
}
