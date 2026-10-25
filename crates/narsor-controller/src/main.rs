use clap::{Parser, Subcommand};
use narsor_controller::controller::{kubernetes_command, KubernetesArgs};
use narsor_controller::local::{local_command, LocalArgs};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// The filter in force when `RUST_LOG` is not set.
const DEFAULT_LOG_FILTER: &str = "warn";

#[derive(Debug, Parser)]
#[command(
    name = "narsor-controller",
    about = "Reconcile one Narsor backend",
    subcommand_required = true
)]
struct Args {
    #[command(subcommand)]
    backend: Backend,
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
    init_logging();
    let code = match args.backend {
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
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}
