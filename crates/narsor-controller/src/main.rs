use std::path::PathBuf;

use clap::{CommandFactory as _, Parser, Subcommand};
use narsor_controller::controller::{kubernetes_command, KubernetesArgs};
use narsor_controller::local::{
    check_command, local_command, local_helper_command, LocalArgs, LocalCommand,
};
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
    Local(LocalCli),
}

/// `narsor-controller local`: the controller, or one of its hidden helper
/// commands (`local secret set`, `local image refresh`), which take none of the controller's
/// arguments.
///
/// Written by hand because the derive cannot make the controller's
/// arguments optional beside a subcommand: they hold a nested flatten, so
/// an `Option` of them is always `None`.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // parsed once, at start
enum LocalCli {
    Controller(LocalArgs),
    Helper(LocalCommand),
}

impl clap::FromArgMatches for LocalCli {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        if matches.subcommand_name().is_some() {
            LocalCommand::from_arg_matches(matches).map(Self::Helper)
        } else {
            LocalArgs::from_arg_matches(matches).map(Self::Controller)
        }
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl clap::Args for LocalCli {
    fn augment_args(command: clap::Command) -> clap::Command {
        LocalCommand::augment_subcommands(LocalArgs::augment_args(command))
            .args_conflicts_with_subcommands(true)
            .subcommand_negates_reqs(true)
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
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
        Backend::Local(LocalCli::Controller(args)) => local_command(args).await,
        Backend::Local(LocalCli::Helper(command)) => local_helper_command(command),
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
