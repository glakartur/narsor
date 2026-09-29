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
pub mod daemon;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{Flags, Resolved};

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

    /// Prints the step timeline of an execution: one line per attempt.
    ///
    /// The CLI counterpart of the TUI's task view axis (U7) — same
    /// [`crate::engine::build_timeline`] fold over the same event log, read
    /// directly from the store rather than through the embedded API, exactly
    /// like `events` already does (parity principle, not a shortcut).
    Steps {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
    },

    /// Prints one attempt's conversation record, in order, in full.
    ///
    /// The CLI counterpart of the TUI's task view content pane when it shows
    /// a conversation (U7). `ATTEMPT_ID` is the attempt's
    /// `action_execution_id`, as printed by `steps` or `events`.
    Conversation {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// The attempt identifier (`action_execution_id`).
        #[arg(value_name = "ATTEMPT_ID")]
        attempt_id: String,
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

        /// A key of the step's `data`, written `name=value`, when resolving a
        /// problem (may be given more than once). A value that reads as JSON
        /// (`3`, `true`, `["a"]`) is taken as that; anything else is text.
        #[arg(long = "data", value_name = "NAME=VALUE")]
        data: Vec<String>,

        /// A context variable of the step's result, written `name=value`, when
        /// resolving a problem (may be given more than once). Values read as
        /// for `--data`.
        #[arg(long = "variable", value_name = "NAME=VALUE")]
        variables: Vec<String>,

        /// A remark kept in the event log with the answer.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,

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

    /// Runs the daemon that owns `Engine`/`ExecutionStore` for as long as the
    /// machine stays up (U2), or sends the running one a graceful stop
    /// signal.
    ///
    /// Bare `narsor daemon` runs in the foreground until it receives
    /// `SIGTERM` — it does not detach itself; that is the job of the client
    /// that discovers no daemon is running and spawns one
    /// (`src/daemon/launch.rs`, wired in by U5). `narsor daemon stop` is the
    /// other half: it never runs a daemon, only signals one.
    Daemon {
        /// What to do about the daemon: nothing (run it) or `stop` it.
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
}

/// The daemon half of the command line.
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Sends a graceful stop signal (`SIGTERM`) to the running daemon.
    Stop,
}

/// The workflow half of the command line.
///
/// Every one of these honours `catalog_dir`, because `{catalog}/workflows`
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
/// The registry lives in `{catalog}/repositories/`, so every one of these
/// honours `catalog_dir` — and the global `--root`, which sets it.
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

impl Command {
    /// Whether this command runs on a configuration that did not hold up.
    ///
    /// Only `config show`, and only because it is the instrument for
    /// diagnosing exactly that: a diagnostic tool that stops working in the
    /// situation it exists for is no tool at all (D-D). It prints what it has,
    /// marks the settings that did not hold up, and still exits 1.
    ///
    /// Spelled out per variant rather than caught by `_`, so that a new
    /// subcommand has to decide rather than inherit — and so that the policy
    /// lives beside the commands it is about, where it can be tested without
    /// spawning the binary.
    ///
    /// It does not extend to a `--config` path that is not there: that is a
    /// usage error for every command including this one, because there is
    /// nothing to diagnose.
    pub fn runs_on_broken_config(&self) -> bool {
        match self {
            Command::Config { command } => match command {
                ConfigCommand::Show { .. } => true,
            },
            Command::Validate { .. }
            | Command::Run { .. }
            | Command::List
            | Command::Show { .. }
            | Command::Events { .. }
            | Command::Steps { .. }
            | Command::Conversation { .. }
            | Command::Input { .. }
            | Command::Resume { .. }
            | Command::Workflow { .. }
            | Command::Repo { .. }
            | Command::Tui
            | Command::Daemon { .. } => false,
        }
    }
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
    // The resolved values, never the raw flag: each root is a setting now, and
    // a file or an environment variable may be the thing that set it.
    //
    // Handed out one at a time rather than as one root, because that is the
    // whole point: a subcommand that is given a root it does not use is a
    // subcommand that can write somewhere it has no business writing.
    // Built once, from the same raw flags `main` resolved `config` from
    // (`--root`/`--config`), for every command below that has to hand a
    // daemon it discovers-or-launches the same roots this process itself
    // resolved (`ensure_daemon_running`'s contract) — see `cli.root`/
    // `cli.config`'s own doc comments on why the raw flag, not the resolved
    // path, is what a spawned daemon's argv needs.
    let flags = Flags {
        root: cli.root.clone(),
        config: cli.config.clone(),
    };
    let catalog = config.catalog_dir().to_path_buf();
    let state = config.state_dir().to_path_buf();
    let outcome = match cli.command {
        Command::Validate { workflow } => commands::validate(&catalog, &workflow),
        Command::Run { workflow, inputs } => {
            commands::run(&config, &flags, &workflow, &inputs).await
        }
        Command::List => commands::list(&state, &flags).await,
        Command::Show { execution_id } => commands::show(&state, &flags, &execution_id).await,
        Command::Events {
            execution_id,
            follow,
        } => commands::events(&state, &flags, &execution_id, follow).await,
        Command::Steps { execution_id } => commands::steps(&state, &flags, &execution_id).await,
        Command::Conversation {
            execution_id,
            attempt_id,
        } => commands::conversation(&state, &flags, &execution_id, &attempt_id).await,
        Command::Input {
            execution_id,
            fields,
            label,
            data,
            variables,
            comment,
            file,
        } => {
            let answer = commands::InputAnswer {
                fields,
                label,
                data,
                variables,
                comment,
            };
            commands::input(&config, &flags, &execution_id, answer, file.as_deref()).await
        }
        Command::Resume { execution_id, file } => {
            commands::resume(&config, &flags, &execution_id, file.as_deref()).await
        }
        Command::Workflow { command } => match command {
            WorkflowCommand::List => commands::workflow_list(&catalog),
            WorkflowCommand::Show { workflow } => commands::workflow_show(&catalog, &workflow),
            WorkflowCommand::Eject { name, to, force } => {
                commands::workflow_eject(&catalog, &name, to.as_deref(), force)
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
                &catalog,
                &id,
                &remote,
                name,
                provider,
                default_branch,
                description,
            ),
            RepoCommand::List => commands::repo_list(&catalog),
            RepoCommand::Show { id } => commands::repo_show(&catalog, &id),
            RepoCommand::Remove { id } => commands::repo_remove(&catalog, &id),
        },
        Command::Config { command } => match command {
            ConfigCommand::Show { format } => commands::config_show(&config, format),
        },
        Command::Tui => commands::tui(&config, &flags),
        Command::Daemon { command } => match command {
            None => daemon::run_daemon(&config).await,
            Some(DaemonCommand::Stop) => daemon::stop_daemon(&config),
        },
    };

    match outcome {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            error.exit_code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_show_is_the_only_command_that_runs_on_a_broken_configuration() {
        // D-D. Asserted here rather than by spawning the binary, which is the
        // point of the policy living beside the commands.
        assert!(Command::Config {
            command: ConfigCommand::Show {
                format: ConfigFormat::Table
            }
        }
        .runs_on_broken_config());

        for command in [
            Command::List,
            Command::Tui,
            Command::Validate {
                workflow: "any".to_string(),
            },
            Command::Run {
                workflow: "any".to_string(),
                inputs: Vec::new(),
            },
        ] {
            assert!(
                !command.runs_on_broken_config(),
                "{command:?} must refuse to start on a configuration it cannot use"
            );
        }
    }
}
