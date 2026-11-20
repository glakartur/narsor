use std::path::PathBuf;

use clap::{CommandFactory as _, Parser, Subcommand};
use narsor_controller::controller::{kubernetes_command, KubernetesArgs};
use narsor_controller::local::{check_command, local_command, LocalArgs};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// The filter in force when `RUST_LOG` is not set.
const DEFAULT_LOG_FILTER: &str = "warn";

#[derive(Debug, Parser)]
#[command(
    name = "narsor-controller",
    version = narsor_protocol::VERSION,
    about = "Reconcile one Narsor backend"
)]
struct Args {
    /// Checks the configuration of `local`, prints the report as JSON and
    /// exits 0 (no problem) or 1, without starting anything (`narsor-cli
    /// config show`). No backend is needed then.
    #[arg(long, hide = true)]
    check_config: bool,

    /// The configuration file `--check-config` reads, instead of searching
    /// for one.
    #[arg(long, hide = true, value_name = "FILE", requires = "check_config")]
    config: Option<PathBuf>,

    /// Required unless `--check-config` is given.
    #[command(subcommand)]
    backend: Option<Backend>,
}

/// The backend kinds `narsor-controller` can drive.
#[derive(Debug, Subcommand)]
enum Backend {
    /// One Pod and one PVC per task, in a Kubernetes namespace.
    Kubernetes(KubernetesArgs),
    /// One `narsor-runner` process per task, on this machine.
    Local(LocalArgs),
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if args.check_config {
        check_command(args.config.as_deref());
    }
    let Some(backend) = args.backend else {
        Args::command()
            .error(
                clap::error::ErrorKind::MissingSubcommand,
                "a backend is required: kubernetes or local",
            )
            .exit();
    };
    init_logging();
    let code = match backend {
        Backend::Kubernetes(args) => kubernetes_command(args).await,
        Backend::Local(args) => local_command(args).await,
    };
    std::process::exit(code);
}

/// Logs to stderr, filtered by `RUST_LOG` (R4). Under the daemon, stderr
/// goes to the daemon's own log.
fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}
