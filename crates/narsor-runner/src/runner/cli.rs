//! `narsor-runner`: the entry point a backend starts in a task workload.
//!
//! # Invocation contract (for controllers)
//!
//! ```text
//! narsor-runner --task-id <id> --task-storage <dir> --incarnation <id> \
//!               --control-plane <url> --credential-file <file> \
//!               [--launch-spec <file>] [--secrets-dir <dir>] \
//!               [--home private|host] [--codex-home <dir>] \
//!               [--claude-config-dir <dir>]
//! ```
//!
//! * `--task-storage` — the task's persistent volume (R9). Holds the Engine
//!   state, the workspace and the private HOME, nothing else.
//! * `--launch-spec` — the task's `TaskLaunchSpec` as JSON, mounted
//!   read-only. Required on the first start; afterwards the copy on storage
//!   is used and a mounted file must be identical to it.
//! * `--credential-file` — the incarnation's runner credential, mounted
//!   read-only *outside* the task storage and re-read after a refusal.
//! * `--secrets-dir` — read-only, outside the task storage, one file per
//!   secret reference name (R19).
//! * `--incarnation` — the identity the credential was issued for.
//! * `--home` — `private` (the default) points HOME at the task storage;
//!   `host` keeps the HOME the runner was started with, harness logins
//!   included. Read on every start: it is the backend's setting, not the
//!   task's.
//! * `--codex-home`, `--claude-config-dir` — the harness configuration
//!   directories, exported as `CODEX_HOME` and `CLAUDE_CONFIG_DIR`.
//!
//! Inherited `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `CODEX_API_KEY` are
//! dropped, so a harness keeps to its subscription login; a task that wants
//! a key gets it as an `env` secret.
//!
//! Exit codes, which a controller may read off the terminated workload:
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | paused at an acknowledged safe point, or stopped by the control plane |
//! | 1 | an error the runner could not report as a problem (storage, I/O) |
//! | 2 | invalid invocation (clap) |
//! | [`EXIT_PROBLEM`] | the runner could not work and reported why (base image, secret, initialization, launch spec) |
//! | [`EXIT_OWNER_DECISION`] | the storage was left by an unexpected stop and no newer launch generation (the owner's or the resume policy's) authorizes recovering it |
//!
//! The runner reads no configuration file and no catalogue: everything it
//! runs is in the launch spec.

use std::path::PathBuf;

use clap::Args;

use crate::domain::TaskId;

use super::{
    run, BaseImageContract, HarnessHome, HomeMode, ProcessEnvironment, RunnerConfig, RunnerExit,
    RunnerTiming, PROBLEM_OWNER_DECISION,
};

/// The runner could not work and said why (a [`crate::orchestration::RunnerProblem`]).
pub const EXIT_PROBLEM: i32 = 3;

/// The runner refused to touch storage left by an unexpected stop.
pub const EXIT_OWNER_DECISION: i32 = 4;

/// Runner exited successfully at an acknowledged safe point.
const EXIT_SUCCESS: i32 = 0;
/// Runner could not complete its work or report a problem.
const EXIT_FAILURE: i32 = 1;
/// Runner invocation did not contain a valid task identifier.
const EXIT_USAGE: i32 = 2;

/// Arguments of `narsor-runner`.
#[derive(Debug, Clone, Args)]
pub struct RunnerArgs {
    /// The task this runner works for.
    #[arg(long, value_name = "TASK_ID")]
    pub task_id: String,

    /// Where the task storage is mounted.
    #[arg(long, value_name = "DIRECTORY")]
    pub task_storage: PathBuf,

    /// The launch spec file (JSON); required on the first start.
    #[arg(long, value_name = "FILE")]
    pub launch_spec: Option<PathBuf>,

    /// The control plane's base URL, e.g. `https://narsor.example`.
    #[arg(long, value_name = "URL")]
    pub control_plane: String,

    /// The read-only file holding this incarnation's runner credential.
    #[arg(long, value_name = "FILE")]
    pub credential_file: PathBuf,

    /// The read-only directory of mounted secrets.
    #[arg(long, value_name = "DIRECTORY")]
    pub secrets_dir: Option<PathBuf>,

    /// The incarnation the credential was issued for.
    #[arg(long, value_name = "ID")]
    pub incarnation: String,

    /// Where HOME points for every action: the task storage, or the host's.
    #[arg(long, value_enum, value_name = "MODE", default_value_t = HomeMode::Private)]
    pub home: HomeMode,

    /// `codex`'s configuration directory (`CODEX_HOME`).
    #[arg(long, value_name = "DIRECTORY")]
    pub codex_home: Option<PathBuf>,

    /// Claude Code's configuration directory (`CLAUDE_CONFIG_DIR`).
    #[arg(long, value_name = "DIRECTORY")]
    pub claude_config_dir: Option<PathBuf>,

    /// Long-poll wait in seconds (the control plane caps it).
    #[arg(long, value_name = "SECONDS", default_value_t = 25, hide = true)]
    pub poll_wait_secs: u64,

    /// Shortest retry delay in milliseconds after a failed request.
    #[arg(long, value_name = "MILLISECONDS", default_value_t = 500, hide = true)]
    pub retry_min_ms: u64,

    /// Longest retry delay in milliseconds.
    #[arg(
        long,
        value_name = "MILLISECONDS",
        default_value_t = 30_000,
        hide = true
    )]
    pub retry_max_ms: u64,

    /// Least time between change-driven snapshots, in milliseconds.
    #[arg(
        long,
        value_name = "MILLISECONDS",
        default_value_t = 1_000,
        hide = true
    )]
    pub report_interval_ms: u64,

    /// How long a runner that cannot work tries to report why, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 120, hide = true)]
    pub problem_report_secs: u64,
}

/// Runs the runner and maps its end onto the exit-code contract.
pub async fn run_command(args: RunnerArgs) -> i32 {
    let task_id = match TaskId::parse(&args.task_id) {
        Ok(task_id) => task_id,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_USAGE;
        }
    };
    let defaults = RunnerTiming::default();
    let config = RunnerConfig {
        task_id,
        storage: args.task_storage,
        launch_spec: args.launch_spec,
        control_plane: args.control_plane,
        credential_file: args.credential_file,
        secrets_dir: args.secrets_dir,
        harness_home: HarnessHome {
            mode: args.home,
            codex_home: args.codex_home,
            claude_config_dir: args.claude_config_dir,
        },
        incarnation: args.incarnation,
        timing: RunnerTiming {
            poll_wait: std::time::Duration::from_secs(args.poll_wait_secs),
            retry_min: std::time::Duration::from_millis(args.retry_min_ms),
            retry_max: std::time::Duration::from_millis(args.retry_max_ms),
            report_interval: std::time::Duration::from_millis(args.report_interval_ms),
            problem_report_deadline: std::time::Duration::from_secs(args.problem_report_secs),
            ..defaults
        },
        contract: BaseImageContract::default(),
    };

    match run(config, apply_environment).await {
        Ok(RunnerExit::PausedAtSafePoint) | Ok(RunnerExit::Stopped { .. }) => EXIT_SUCCESS,
        Ok(RunnerExit::Problem(problem)) => {
            eprintln!("runner problem ({}): {}", problem.code, problem.message);
            if problem.code == PROBLEM_OWNER_DECISION {
                EXIT_OWNER_DECISION
            } else {
                EXIT_PROBLEM
            }
        }
        Err(error) => {
            eprintln!("runner failed: {error}");
            EXIT_FAILURE
        }
    }
}

/// Sets the variables every action's child process inherits.
///
/// The one place the runner changes its own process environment, done
/// before the Engine starts any child: nothing else in the process reads
/// the environment concurrently at that point (the long-lived tasks are not
/// spawned yet), which is what makes `set_var` safe enough here.
fn apply_environment(environment: &ProcessEnvironment) {
    for name in environment.removed() {
        std::env::remove_var(name);
    }
    for (name, value) in environment.variables() {
        std::env::set_var(name, value);
    }
}
