use clap::Parser;
use narsor_observer_worker::cli::{run, WorkerArgs};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[tokio::main]
async fn main() {
    let args = WorkerArgs::parse();
    init_tracing();
    std::process::exit(run(args).await);
}

/// Logs to stderr at the level `RUST_LOG` names, which `narsor-server` sets
/// from its own; without it the worker is silent.
fn init_tracing() {
    let Ok(directive) = std::env::var("RUST_LOG") else {
        return;
    };
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::builder().parse_lossy(directive))
        .with_writer(std::io::stderr)
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}
