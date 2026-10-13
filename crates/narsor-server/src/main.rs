use std::path::PathBuf;

use clap::{Parser, Subcommand};
use narsor_engine::config::{self, Flags};
use narsor_engine::persistence::DaemonInfo;
use narsor_server::runtime::run_daemon;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Debug, Parser)]
#[command(name = "narsor-server", about = "Run the Narsor API server")]
struct Args {
    /// Narsor data directory (executions and workspaces).
    #[arg(long, value_name = "DIRECTORY", global = true)]
    root: Option<PathBuf>,

    /// Configuration file to read instead of searching for one.
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,

    /// Optional server lifecycle operation; no operation starts the server.
    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    /// Ask the running server to shut down and remove stale discovery state.
    Stop,
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

    if matches!(args.command, Some(ServerCommand::Stop)) {
        if let Err(message) = stop_daemon(&resolved) {
            eprintln!("{message}");
            std::process::exit(1);
        }
        return;
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

/// Stop the daemon named by this installation's discovery record.
///
/// This lifecycle operation belongs to the server executable so the client
/// does not open server persistence records or signal process IDs itself.
fn stop_daemon(config: &narsor_engine::config::Resolved) -> Result<(), String> {
    let daemon_file = narsor_engine::persistence::StatePaths::new(config.state_dir()).daemon_file();
    let info = match DaemonInfo::read(&daemon_file) {
        Ok(info) => info,
        Err(_) => {
            println!("No daemon is running.");
            return Err("no daemon is running".into());
        }
    };

    if !info.is_alive() {
        println!(
            "No daemon is running (stale discovery file for pid {}); cleaning up.",
            info.pid
        );
        let _ = std::fs::remove_file(&daemon_file);
        return Err("no daemon is running".into());
    }

    // SAFETY: this sends SIGTERM to the live PID read from this server's
    // discovery record; it does not affect Rust memory safety.
    let sent = unsafe { libc::kill(info.pid as libc::pid_t, libc::SIGTERM) };
    if sent != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "could not signal daemon (pid {}): {error}",
            info.pid
        ));
    }

    println!("Sent SIGTERM to daemon (pid {}).", info.pid);
    Ok(())
}
