//! `narsor-migrate`: moves an installation of an older Narsor to the
//! per-binary layout, once, with a backup first. The installer runs it;
//! see `docs/operations/updates.md`.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use narsor_migrate::{
    runtime_catalog::{self, RuntimeCatalogMove},
    BackupChoice, ControllerMove, DockerProbe, Installation, Options, Target, EXIT_OK, EXIT_USAGE,
};
use narsor_shared::config::{self, Flags};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Debug, Parser)]
#[command(
    name = "narsor-migrate",
    version = narsor_protocol::VERSION,
    about = "Move an installation of an older Narsor to the per-binary layout, once, after a backup",
    after_help = "Exit status: 0 migrated, nothing to migrate or a clean dry run; 1 the \
                  migration stopped (run again to finish it); 2 usage or configuration error; \
                  3 refused before anything changed; 4 not confirmed."
)]
struct Args {
    /// Narsor data directory (`global.root`), as `narsor-server --root`.
    #[arg(long, value_name = "DIRECTORY")]
    root: Option<PathBuf>,

    /// Configuration file to read instead of searching for one, as
    /// `narsor-server --config`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Print the plan and run the checks; change nothing.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Directory for the backup archive (default: `<global.root>/backups`,
    /// or `<state root>/backups` for a controller).
    #[arg(long, value_name = "DIRECTORY", global = true)]
    backup_dir: Option<PathBuf>,

    /// Do not write a backup.
    #[arg(long, global = true, conflicts_with = "backup_dir")]
    no_backup: bool,

    /// Do not ask for confirmation (required without a terminal).
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    /// Do not ask Docker whether the old runner containers still run: you
    /// checked `docker ps` yourself.
    #[arg(long, global = true)]
    assume_containers_stopped: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Plan the move of an old runtime catalogue ({catalog}/runtime.yaml and
    /// {catalog}/components/) into {catalog}/runtime/. Changes nothing: it
    /// prints one `move` line per move (`dist/install.sh` runs them with
    /// `mv -n`) and a `warning` line per script outside components/. Exit
    /// status: 0 planned or nothing to move; 1 the check could not run; 2
    /// usage or configuration error; 3 the runtime directory already holds
    /// what would move; 5 a program of the installation runs.
    RuntimeCatalog,
    /// Move the files of a controller started by hand
    /// (`narsor-controller local`) on another machine.
    Controller {
        /// The old credential directory (for example
        /// `$XDG_STATE_HOME/narsor/workstation-controller`).
        #[arg(long, value_name = "DIRECTORY")]
        legacy_dir: PathBuf,

        /// The old storage directory (the old `--storage`), when it is not
        /// `--legacy-dir`.
        #[arg(long, value_name = "DIRECTORY")]
        legacy_work_dir: Option<PathBuf>,

        /// The new `--state-root` of the controller (default
        /// `$XDG_STATE_HOME/narsor`).
        #[arg(long, value_name = "DIRECTORY")]
        state_root: Option<PathBuf>,

        /// The new `--work-root` of the controller.
        #[arg(long, value_name = "DIRECTORY")]
        work_root: PathBuf,
    },
}

fn main() {
    let args = Args::parse();
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(
            EnvFilter::try_from_env("NARSOR_MIGRATE_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    if matches!(args.command, Some(Command::RuntimeCatalog)) {
        let target = runtime_catalog_target(&args).unwrap_or_else(|message| {
            eprintln!("narsor-migrate: {message}");
            std::process::exit(EXIT_USAGE);
        });
        match runtime_catalog::run(&target, &mut std::io::stdout()) {
            Ok(_) => std::process::exit(EXIT_OK),
            Err(failure) => {
                eprintln!("narsor-migrate: {failure}");
                std::process::exit(failure.exit_code());
            }
        }
    }

    let target = target(&args).unwrap_or_else(|message| {
        eprintln!("narsor-migrate: {message}");
        std::process::exit(EXIT_USAGE);
    });
    let options = Options {
        dry_run: args.dry_run,
        backup: match (&args.backup_dir, args.no_backup) {
            (_, true) => BackupChoice::Skip,
            (Some(dir), false) => BackupChoice::In(absolute(dir)),
            (None, false) => BackupChoice::Default,
        },
        assume_containers_stopped: args.assume_containers_stopped,
        ..Options::default()
    };
    let yes = args.yes;
    let mut confirm = move || {
        if yes {
            return true;
        }
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "narsor-migrate: no terminal to confirm on; pass --yes to migrate without a \
                 question"
            );
            return false;
        }
        print!("Back up and migrate now? [y/N]: ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
    };
    let mut out = std::io::stdout();
    match narsor_migrate::run(&target, &options, &DockerProbe, &mut confirm, &mut out) {
        Ok(_) => std::process::exit(EXIT_OK),
        Err(failure) => {
            eprintln!("narsor-migrate: {failure}");
            std::process::exit(failure.exit_code());
        }
    }
}

/// What to migrate, from the arguments: the installation the configuration
/// resolves (as `narsor-server` resolves it), or a controller.
fn target(args: &Args) -> Result<Target, String> {
    match &args.command {
        None => Ok(Target::Installation(Installation::of(
            &resolved_installation(args)?,
        ))),
        Some(Command::RuntimeCatalog) => unreachable!("handled before"),
        Some(Command::Controller {
            legacy_dir,
            legacy_work_dir,
            state_root,
            work_root,
        }) => {
            if args.root.is_some() || args.config.is_some() {
                return Err("--root and --config name an installation; the controller \
                            command takes --state-root and --work-root"
                    .into());
            }
            let state_root = state_root
                .clone()
                .or_else(default_state_root)
                .ok_or("--state-root was not given and neither XDG_STATE_HOME nor HOME is set")?;
            let legacy_work = legacy_work_dir
                .clone()
                .unwrap_or_else(|| legacy_dir.clone());
            Ok(Target::Controller(ControllerMove::by_hand(
                absolute(legacy_dir),
                absolute(&legacy_work),
                &absolute(&state_root),
                &absolute(work_root),
            )))
        }
    }
}

/// The configuration as `narsor-server` resolves it, from `--root` and
/// `--config`.
fn resolved_installation(args: &Args) -> Result<narsor_shared::config::Resolved, String> {
    let flags = Flags {
        root: args.root.clone(),
        config: args.config.clone(),
    };
    let environment = config::resolve::environment_from_process();
    let resolved = config::resolve(&flags, &environment, config::Scope::SERVER);
    if !resolved.is_ok() {
        let problems: Vec<String> = resolved.errors().iter().map(ToString::to_string).collect();
        return Err(format!(
            "the configuration does not hold up:\n  {}",
            problems.join("\n  ")
        ));
    }
    Ok(resolved)
}

/// The runtime catalogue move of the installation the configuration
/// resolves.
fn runtime_catalog_target(args: &Args) -> Result<RuntimeCatalogMove, String> {
    Ok(RuntimeCatalogMove::of(&resolved_installation(args)?))
}

/// The default state root of `narsor-controller local`.
fn default_state_root() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|dir| !dir.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("narsor"))
}

fn absolute(path: &PathBuf) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.clone())
}
