//! Command-line interface.
//!
//! The operator-facing half of the orchestrator: validate a definition, start a
//! workflow, look at what is on disk, and answer a question a workflow asked.
//! Everything here is a thin shell around the library — argument parsing,
//! resolving the workflow definition, and formatting — so that every operation
//! reachable from the terminal is also reachable from the engine API (and, from
//! U10 on, from the TUI).
//!
//! # Two streams
//!
//! Command output goes to stdout, diagnostics and `tracing` logs to stderr.
//! That is what makes `orchestrator list | grep paused` work. The one
//! exception is [`Command::Tui`], which owns the whole terminal: its logs are
//! diverted to a file under `--root` (see `src/main.rs`) so that nothing is
//! written over the alternate screen.
//!
//! # Exit codes are a contract
//!
//! CI is expected to branch on them, so they are fixed:
//!
//! | code | meaning |
//! |------|---------|
//! | [`EXIT_SUCCESS`] | the command did what it was asked |
//! | [`EXIT_FAILURE`] | the definition is invalid, the execution failed, or the store could not answer |
//! | [`EXIT_USAGE`]   | the invocation itself is wrong: bad flags, an unknown input, a missing required input |
//!
//! A *failing workflow* is [`EXIT_FAILURE`], not a usage error: the engine ran
//! exactly as asked and the workflow ended in a failure terminal state.
//! Conversely a missing `--input` is [`EXIT_USAGE`], because nothing was run —
//! which is also the code clap itself returns for a malformed command line.

pub mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::persistence::DEFAULT_ROOT;

/// The command did what it was asked.
pub const EXIT_SUCCESS: i32 = 0;

/// A validation error, a failed execution, or an unreadable store.
pub const EXIT_FAILURE: i32 = 1;

/// The invocation itself was wrong.
pub const EXIT_USAGE: i32 = 2;

/// The `orchestrator` command line.
#[derive(Debug, Parser)]
#[command(
    name = "orchestrator",
    version,
    about = "AI workflow orchestrator — validate definitions, run them, and inspect executions",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Orchestrator data directory (executions and workspaces).
    #[arg(
        long,
        global = true,
        value_name = "DIRECTORY",
        default_value = DEFAULT_ROOT
    )]
    pub root: PathBuf,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Everything the operator can ask for.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Checks a workflow definition and reports every violation at once.
    Validate {
        /// YAML file holding the workflow definition.
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// Runs a workflow with the given inputs.
    Run {
        /// YAML file holding the workflow definition.
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Workflow input, written `name=value` (may be given more than once).
        #[arg(long = "input", value_name = "NAME=VALUE")]
        inputs: Vec<String>,
    },

    /// Lists the executions stored in the data directory.
    List,

    /// Shows the current state, the status and the context variables of one execution.
    Show {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
    },

    /// Prints the event history of an execution.
    Events {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// Follows new events until the execution stops.
        #[arg(long)]
        follow: bool,
    },

    /// Delivers human input to a paused execution and resumes it.
    Input {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// Form field value, written `name=value` (may be given more than once).
        #[arg(long = "field", value_name = "NAME=VALUE")]
        fields: Vec<String>,

        /// Output label chosen explicitly (when the action has no fields, or several).
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,

        /// Workflow definition file, for when it cannot be read back from the stored execution.
        #[arg(long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
    },

    /// Resumes a paused execution that is not waiting for human input.
    Resume {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// Workflow definition file, for when it cannot be read back from the stored execution.
        #[arg(long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
    },

    /// Manages the registry of repositories workflows name by id.
    Repo {
        /// What to do with the registry.
        #[command(subcommand)]
        command: RepoCommand,
    },

    /// Opens the full-screen terminal interface.
    Tui,
}

/// The registry half of the command line.
///
/// The registry lives in `{root}/repositories.json`, so every one of these
/// honours the global `--root` exactly like the execution subcommands do.
#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Registers a repository under an id workflows can name.
    Add {
        /// The identifier a workflow writes in `globals.workspace.repository`.
        #[arg(value_name = "ID")]
        id: String,

        /// Anything `git clone` accepts: a URL or an absolute local path.
        #[arg(long, value_name = "URL|PATH")]
        remote: String,

        /// Human-readable name. Defaults to the id.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Where the repository is hosted. Inferred from the remote when omitted.
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,

        /// Branch used when a workflow declares no `ref` of its own.
        #[arg(long = "default-branch", value_name = "BRANCH")]
        default_branch: Option<String>,

        /// Free-form note about what the repository is.
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
    },

    /// Lists the registered repositories, ordered by id.
    List,

    /// Shows every field of one registered repository.
    Show {
        /// The repository identifier.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Removes a repository from the registry.
    Remove {
        /// The repository identifier.
        #[arg(value_name = "ID")]
        id: String,
    },
}

/// Runs the parsed command line and returns the process exit code.
///
/// Nothing here panics on an operator mistake: every failure becomes a message
/// on stderr plus one of the three documented codes.
pub async fn run(cli: Cli) -> i32 {
    let outcome = match cli.command {
        Command::Validate { file } => commands::validate(&file),
        Command::Run { file, inputs } => commands::run(&cli.root, &file, &inputs).await,
        Command::List => commands::list(&cli.root),
        Command::Show { execution_id } => commands::show(&cli.root, &execution_id),
        Command::Events {
            execution_id,
            follow,
        } => commands::events(&cli.root, &execution_id, follow).await,
        Command::Input {
            execution_id,
            fields,
            label,
            file,
        } => commands::input(&cli.root, &execution_id, &fields, label, file.as_deref()).await,
        Command::Resume { execution_id, file } => {
            commands::resume(&cli.root, &execution_id, file.as_deref()).await
        }
        Command::Repo { command } => match command {
            RepoCommand::Add {
                id,
                remote,
                name,
                provider,
                default_branch,
                description,
            } => commands::repo_add(
                &cli.root,
                &id,
                &remote,
                name,
                provider,
                default_branch,
                description,
            ),
            RepoCommand::List => commands::repo_list(&cli.root),
            RepoCommand::Show { id } => commands::repo_show(&cli.root, &id),
            RepoCommand::Remove { id } => commands::repo_remove(&cli.root, &id),
        },
        Command::Tui => commands::tui(&cli.root),
    };

    match outcome {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            error.exit_code
        }
    }
}
