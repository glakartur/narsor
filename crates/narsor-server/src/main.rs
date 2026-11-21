use std::path::PathBuf;

use clap::{Parser, Subcommand};
use narsor_engine::config::{self, CheckReport, Flags};
use narsor_engine::persistence::DaemonInfo;
use narsor_server::runtime::run_daemon;
use narsor_server::settings::configure;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Debug, Parser)]
#[command(
    name = "narsor-server",
    version = narsor_protocol::VERSION,
    about = "Run the Narsor API server"
)]
struct Args {
    /// Narsor data directory (executions and workspaces).
    #[arg(long, value_name = "DIRECTORY", global = true)]
    root: Option<PathBuf>,

    /// Configuration file to read instead of searching for one.
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,

    /// Checks the configuration, prints the report as JSON and exits 0 (no
    /// problem) or 1, without starting anything (`narsor-cli config show`).
    /// The report includes the local controller's configuration a trusted
    /// server would refuse to start on.
    #[arg(long, hide = true)]
    check_config: bool,

    /// Optional server lifecycle operation; no operation starts the server.
    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    /// Ask the running server to shut down and remove stale discovery state.
    Stop,
}

fn main() {
    // Before the runtime starts a thread: capabilities belong to a thread,
    // and every later thread copies the main thread's.
    if let Err(error) = narsor_server::capabilities::confine_daemon() {
        eprintln!("cannot confine the server's capabilities: {error}");
        std::process::exit(1);
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    runtime.block_on(run());
}

async fn run() {
    let args = Args::parse();
    let flags = Flags {
        root: args.root,
        config: args.config,
    };
    let environment = config::resolve::environment_from_process();
    let resolved = config::resolve(&flags, &environment, config::Scope::SERVER);
    if args.check_config {
        let problems = configure(&resolved, &environment).err().unwrap_or_default();
        CheckReport::build("narsor-server", &resolved, &problems).print_and_exit();
    }
    if resolved.has_usage_error() {
        refuse(resolved.errors().iter().map(ToString::to_string), 2);
    }

    if matches!(args.command, Some(ServerCommand::Stop)) {
        if !resolved.is_ok() {
            refuse(resolved.errors().iter().map(ToString::to_string), 1);
        }
        if let Err(message) = stop_daemon(&resolved) {
            eprintln!("{message}");
            std::process::exit(1);
        }
        return;
    }

    let logging = resolved.log_directive();
    if let Some(directive) = &logging {
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

    // R26: an un-migrated configuration runs on built-in values; say so.
    if let Some(warning) = config::legacy::warning(&resolved, &environment) {
        if logging.is_some() {
            tracing::warn!("{warning}");
        } else {
            // With logging off nothing would carry a tracing event, and an
            // old configuration that set a level is itself ignored.
            eprintln!("narsor-server: warning: {warning}");
        }
    }

    // Every rule between the settings, before anything starts (R7, R8).
    let settings = match configure(&resolved, &environment) {
        Ok(settings) => settings,
        Err(problems) => refuse(problems.iter().map(ToString::to_string), 1),
    };

    if let Err(error) = run_daemon(&resolved, settings).await {
        eprintln!("{}", error.message);
        std::process::exit(error.exit_code);
    }
}

/// Prints every problem of the configuration and exits with `code`.
fn refuse(problems: impl Iterator<Item = String>, code: i32) -> ! {
    eprintln!("narsor-server: the configuration does not hold up:");
    for problem in problems {
        eprintln!("  {problem}");
    }
    std::process::exit(code);
}

/// Stop the daemon named by this installation's discovery record.
///
/// This lifecycle operation belongs to the server executable so the client
/// does not open server persistence records or signal process IDs itself.
fn stop_daemon(config: &narsor_engine::config::Resolved) -> Result<(), String> {
    let daemon_file = config.client_paths().daemon_file();
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
