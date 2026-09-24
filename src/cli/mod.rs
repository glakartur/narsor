//! Command-line interface.
//!
//! The operator-facing half of narsor: validate a definition, start a
//! workflow, look at what is on disk, and answer a question a workflow asked.
//! Everything here is a thin shell around the library — argument parsing,
//! resolving the workflow definition, and formatting — so that every operation
//! reachable from the terminal is also reachable from the engine API (and, from
//! U10 on, from the TUI).
//!
//! # Two streams
//!
//! Command output goes to stdout, diagnostics and `tracing` logs to stderr.
//! That is what makes `narsor list | grep paused` work. The one
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

use crate::config::Resolved;

/// The command did what it was asked.
pub const EXIT_SUCCESS: i32 = 0;

/// A validation error, a failed execution, or an unreadable store.
pub const EXIT_FAILURE: i32 = 1;

/// The invocation itself was wrong.
pub const EXIT_USAGE: i32 = 2;

/// The `narsor` command line.
#[derive(Debug, Parser)]
#[command(
    name = "narsor",
    version,
    about = "Narsor — an AI workflow orchestrator: validate definitions, run them, and inspect executions",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Narsor data directory (executions and workspaces).
    ///
    /// Deliberately without a clap `default_value`: with one, an unset flag and
    /// a flag set to the default value are indistinguishable, and answering
    /// "where did this value come from" (R8) needs them told apart. The
    /// built-in value lives in the settings registry instead
    /// ([`crate::config::SettingKey::built_in`]).
    #[arg(long, global = true, value_name = "DIRECTORY")]
    pub root: Option<PathBuf>,

    /// Configuration file to read instead of searching for one.
    ///
    /// Global, like `--root`, so that it also works after the subcommand. A
    /// path that is not there is a usage error rather than a fall-through to
    /// the next step of the cascade: the operator named this file.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Everything the operator can ask for.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Checks a workflow definition and reports every violation at once.
    Validate {
        /// A YAML file, or the name of a workflow on the search path.
        #[arg(value_name = "WORKFLOW")]
        workflow: String,
    },

    /// Runs a workflow with the given inputs.
    Run {
        /// A YAML file, or the name of a workflow on the search path.
        #[arg(value_name = "WORKFLOW")]
        workflow: String,

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

        /// Workflow file or name, for when it cannot be read back from the stored execution.
        #[arg(long = "file", value_name = "WORKFLOW")]
        file: Option<String>,
    },

    /// Resumes a paused execution that is not waiting for human input.
    Resume {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// Workflow file or name, for when it cannot be read back from the stored execution.
        #[arg(long = "file", value_name = "WORKFLOW")]
        file: Option<String>,
    },

    /// Lists, shows and ejects the workflow definitions that can be run by name.
    Workflow {
        /// What to do with the workflows.
        #[command(subcommand)]
        command: WorkflowCommand,
    },

    /// Manages the registry of repositories workflows name by id.
    Repo {
        /// What to do with the registry.
        #[command(subcommand)]
        command: RepoCommand,
    },

    /// Shows the configuration in force.
    Config {
        /// What to do with the configuration.
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Opens the full-screen terminal interface.
    Tui,
}

/// The workflow half of the command line.
///
/// Every one of these honours the global `--root`, because `{root}/workflows`
/// is one of the directories the resolver searches.
#[derive(Debug, Subcommand)]
pub enum WorkflowCommand {
    /// Lists every workflow reachable right now, overrides marked.
    List,

    /// Prints the YAML that would run, and says on stderr where it came from.
    Show {
        /// A YAML file, or the name of a workflow on the search path.
        #[arg(value_name = "WORKFLOW")]
        workflow: String,
    },

    /// Writes a built-in workflow out as a file that then takes precedence.
    Eject {
        /// The name of the built-in workflow.
        #[arg(value_name = "NAME")]
        name: String,

        /// Where to write it. Defaults to the user workflow directory.
        #[arg(long = "to", value_name = "DIRECTORY")]
        to: Option<PathBuf>,

        /// Overwrites a file that is already there.
        #[arg(long)]
        force: bool,
    },
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

/// What `config` can be asked for.
///
/// A group with a single verb, like [`Command::Workflow`] and
/// [`Command::Repo`], so that a later `config path` has somewhere to go.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Prints every setting, its effective value and where that value came
    /// from.
    Show {
        /// How to print it.
        #[arg(long, value_name = "FORMAT", default_value = "table")]
        format: ConfigFormat,
    },
}

/// The shapes `config show` prints in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConfigFormat {
    /// Fixed-width columns, for pasting into a report.
    Table,
    /// A JSON document on stdout, for a script to read.
    Json,
}

/// Runs the parsed command line and returns the process exit code.
///
/// Nothing here panics on an operator mistake: every failure becomes a message
/// on stderr plus one of the three documented codes.
pub async fn run(cli: Cli, config: Resolved) -> i32 {
    // The resolved value, never the raw flag: the data directory is a setting
    // now, and a file or an environment variable may be the thing that set it.
    let root = config.data_dir().to_path_buf();
    let outcome = match cli.command {
        Command::Validate { workflow } => commands::validate(&root, &workflow),
        Command::Run { workflow, inputs } => commands::run(&config, &workflow, &inputs).await,
        Command::List => commands::list(&root),
        Command::Show { execution_id } => commands::show(&root, &execution_id),
        Command::Events {
            execution_id,
            follow,
        } => commands::events(&root, &execution_id, follow).await,
        Command::Input {
            execution_id,
            fields,
            label,
            file,
        } => commands::input(&config, &execution_id, &fields, label, file.as_deref()).await,
        Command::Resume { execution_id, file } => {
            commands::resume(&config, &execution_id, file.as_deref()).await
        }
        Command::Workflow { command } => match command {
            WorkflowCommand::List => commands::workflow_list(&root),
            WorkflowCommand::Show { workflow } => commands::workflow_show(&root, &workflow),
            WorkflowCommand::Eject { name, to, force } => {
                commands::workflow_eject(&root, &name, to.as_deref(), force)
            }
        },
        Command::Repo { command } => match command {
            RepoCommand::Add {
                id,
                remote,
                name,
                provider,
                default_branch,
                description,
            } => commands::repo_add(
                &root,
                &id,
                &remote,
                name,
                provider,
                default_branch,
                description,
            ),
            RepoCommand::List => commands::repo_list(&root),
            RepoCommand::Show { id } => commands::repo_show(&root, &id),
            RepoCommand::Remove { id } => commands::repo_remove(&root, &id),
        },
        Command::Config { command } => match command {
            ConfigCommand::Show { format } => commands::config_show(&config, format),
        },
        Command::Tui => commands::tui(&config),
    };

    match outcome {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            error.exit_code
        }
    }
}
