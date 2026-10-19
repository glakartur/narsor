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
//! That is what makes `narsor-cli list | grep paused` work. The TUI has its
//! own `narsor` entry point and log setup.
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
pub mod control_text;
pub mod daemon;
pub mod remote;
pub mod task_history;
pub mod task_registry;
pub mod task_text;

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
    name = "narsor-cli",
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

    /// The control plane to talk to, as an `https://` URL (U8). Without it,
    /// remote-task commands use the trusted local daemon of the state
    /// directory.
    #[arg(long, global = true, env = "NARSOR_SERVER", value_name = "URL")]
    pub server: Option<String>,

    /// A file holding the OIDC access token for `--server`, re-read after
    /// every `401`. `NARSOR_TOKEN` holds a fixed token instead.
    #[arg(long, global = true, env = "NARSOR_TOKEN_FILE", value_name = "FILE")]
    pub token_file: Option<PathBuf>,

    /// A command printing the OIDC access token for `--server`, run again
    /// after every `401`.
    #[arg(
        long,
        global = true,
        env = "NARSOR_TOKEN_COMMAND",
        value_name = "COMMAND"
    )]
    pub token_command: Option<String>,

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

        /// A remark kept in the event log with the answer; with
        /// `--permission`, a message the agent reads along with the decision.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,

        /// Resolve a Claude Code permission wait: deny, allow-exact,
        /// allow-group, or allow-pattern with `--pattern`.
        #[arg(long = "permission", value_name = "DECISION", value_parser = ["deny", "allow-exact", "allow-group", "allow-pattern"])]
        permission: Option<String>,

        /// Which denial in the pending request to resolve; every denial when
        /// omitted.
        #[arg(long = "denial-index")]
        denial_index: Option<usize>,

        /// With `--permission allow-pattern`: the Bash command or its
        /// beginning followed by `*` to allow for the rest of the task; `*`
        /// allows every Bash command.
        #[arg(long = "pattern", value_name = "TEXT")]
        pattern: Option<String>,

        /// Workflow file or name, for when it cannot be read back from the stored execution.
        #[arg(long = "file", value_name = "WORKFLOW")]
        file: Option<String>,
    },

    /// Starts a failed step of an execution afresh, with an optional note
    /// for its next attempt.
    ///
    /// Bound to the failure the execution is failed on (its episode key, as
    /// `show` prints it): refused if it was recovered or changed meanwhile.
    Recover {
        /// The execution identifier.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// A note handed to the step's next attempt.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
    },

    /// Talks to the conductor of an execution: its chat, its proposals, and
    /// the undo of its decisions.
    Conductor {
        #[command(subcommand)]
        command: ConductorCommand,
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

    /// Creates, inspects and steers remote tasks through the control plane.
    Task {
        /// What to do.
        #[command(subcommand)]
        command: remote::TaskCommand,
    },

    /// The backends of remote tasks: the default backend of new tasks.
    Backend {
        /// What to do.
        #[command(subcommand)]
        command: remote::BackendCommand,
    },

    /// The default agent harness of the daemon: which agent CLI runs new
    /// agent steps.
    Harness {
        /// What to do.
        #[command(subcommand)]
        command: remote::HarnessCommand,
    },

    /// Network-mode identity: claim the installation, see who you are.
    Auth {
        /// What to do.
        #[command(subcommand)]
        command: remote::AuthCommand,
    },

    /// Stops the local server through its discovery record.
    Daemon {
        /// The runner_state operation to perform.
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Manages controller identities through the control plane.
    Controller(ControllerCli),
}

/// Controller identity management in `narsor-cli`.
#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct ControllerCli {
    /// Controller identity management commands.
    #[command(subcommand)]
    pub admin: Option<remote::ControllerAdminCommand>,
}

/// The daemon half of the command line.
/// `narsor conductor ...` (F4, R28, R31).
#[derive(Debug, Subcommand)]
pub enum ConductorCommand {
    /// Writes a message to the conductor and prints its answer, with any
    /// action it proposes.
    Chat {
        /// The execution to talk about.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// The message.
        #[arg(value_name = "MESSAGE")]
        message: String,

        /// Returns once the message is written, without waiting for the
        /// answer.
        #[arg(long)]
        no_wait: bool,
    },

    /// Prints what the conductor is doing for an execution and what it
    /// kept: the invocation under way, the queue, the attempts counted, the
    /// latest failure it took up.
    Status {
        /// The execution whose conductor to show.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
    },

    /// Prints the chat history: messages, proposals and decisions.
    History {
        /// The execution whose chat to print.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
    },

    /// Carries out a proposal of the conductor's.
    Confirm {
        /// The execution the proposal is about.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// The proposal: its id, or its number in the chat history.
        #[arg(value_name = "PROPOSAL")]
        proposal: String,
    },

    /// Rejects a proposal of the conductor's.
    Reject {
        /// The execution the proposal is about.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// The proposal: its id, or its number in the chat history.
        #[arg(value_name = "PROPOSAL")]
        proposal: String,
    },

    /// Undoes the conductor's choice of a step's output: the step becomes a
    /// problem again at its state.
    Undo {
        /// The execution the decision is about.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,

        /// The decision: its id, or the number of its entry in the chat
        /// history.
        #[arg(value_name = "DECISION")]
        decision: String,
    },
}

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

        /// Branch used when a workflow declares no `ref` of its own; an explicit
        /// `ref` in the workflow takes precedence.
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
            | Command::Recover { .. }
            | Command::Conductor { .. }
            | Command::Workflow { .. }
            | Command::Repo { .. }
            | Command::Task { .. }
            | Command::Backend { .. }
            | Command::Harness { .. }
            | Command::Auth { .. }
            | Command::Daemon { .. } => false,
            // Configured by its own flags and environment only, like the
            // runner: it reads no Narsor configuration file. Its operator
            // subcommands talk to a control plane and need none either.
            Command::Controller(_) => true,
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

/// The warning `narsor --remote` prints: the flag no longer selects a
/// mode, the one interface runs either way.
pub fn tui_remote_warning(remote: bool) -> Option<&'static str> {
    remote.then_some(
        "warning: `narsor --remote` is ignored; there is one interface, over the control \
         plane of the configured backend (use --server for a remote one)",
    )
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
    let target = remote::server_target(cli.server, cli.token_file, cli.token_command);
    let state = config.state_dir().to_path_buf();
    let outcome = match cli.command {
        Command::Validate { workflow } => commands::validate(&config, &flags, &workflow).await,
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
            permission,
            denial_index,
            pattern,
            file,
        } => {
            let answer = commands::InputAnswer {
                fields,
                label,
                data,
                variables,
                comment,
                permission,
                denial_index,
                pattern,
            };
            commands::input(&config, &flags, &execution_id, answer, file.as_deref()).await
        }
        Command::Resume { execution_id, file } => {
            commands::resume(&config, &flags, &execution_id, file.as_deref()).await
        }
        Command::Recover {
            execution_id,
            comment,
        } => commands::recover(&state, &flags, &execution_id, comment).await,
        Command::Conductor { command } => match command {
            ConductorCommand::Status { execution_id } => {
                commands::conductor_status(&state, &flags, &execution_id).await
            }
            ConductorCommand::Chat {
                execution_id,
                message,
                no_wait,
            } => commands::conductor_chat(&state, &flags, &execution_id, &message, !no_wait).await,
            ConductorCommand::History { execution_id } => {
                commands::conductor_history(&state, &flags, &execution_id).await
            }
            ConductorCommand::Confirm {
                execution_id,
                proposal,
            } => commands::conductor_settle(&state, &flags, &execution_id, &proposal, true).await,
            ConductorCommand::Reject {
                execution_id,
                proposal,
            } => commands::conductor_settle(&state, &flags, &execution_id, &proposal, false).await,
            ConductorCommand::Undo {
                execution_id,
                decision,
            } => commands::conductor_undo(&state, &flags, &execution_id, &decision).await,
        },
        Command::Workflow { command } => remote::workflow(command, &target, &state, &flags).await,
        Command::Repo { command } => remote::repo_admin(command, &target, &state, &flags).await,
        Command::Config { command } => match command {
            ConfigCommand::Show { format } => commands::config_show(&config, format),
        },
        Command::Task { command } => remote::task(command, &target, &state, &flags).await,
        Command::Backend { command } => remote::backend(command, &target, &state, &flags).await,
        Command::Harness { command } => remote::harness(command, &target, &state, &flags).await,
        Command::Auth { command } => remote::auth(command, &target, &state, &flags).await,
        Command::Controller(ControllerCli { admin, .. }) => match admin {
            Some(command) => remote::controller_admin(command, &target, &state, &flags).await,
            None => Err(commands::CliError::usage(
                "narsor-cli controller needs an operator subcommand (enroll-code, list, revoke)",
            )),
        },
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => daemon::stop_daemon(&flags),
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
    fn an_entry_limit_says_how_to_decide() {
        let pending = serde_json::json!({
            "state_id": "fix",
            "instruction": "Krok 'fix' osiągnął limit wejść: 2/2.",
            "fields": [{
                "name": "entry_limit_decision",
                "type": "select",
                "options": ["continue", "reset_counter"],
                "required": false,
            }],
            "outputs": [{"label": "fixed"}, {"label": "manual_complete"}],
            "continuation": "entry_limit",
            "entry_limit": {"entries": 2, "max_entries": 2},
            "problem": {
                "reason": "entry limit reached (2/2)",
                "answers": [],
                "requirements": [{"label": "manual_complete", "data": [], "variables": ["summary"]}],
            },
        });
        let text = commands::pending_input_lines(&pending).join("\n");
        assert!(
            text.contains("Entry limit: step 'fix' entered 2/2 times."),
            "{text}"
        );
        assert!(
            text.contains("--field entry_limit_decision=continue"),
            "{text}"
        );
        assert!(
            text.contains("--field entry_limit_decision=reset_counter"),
            "{text}"
        );
        assert!(text.contains("Or choose an output with --label"), "{text}");
        assert!(text.contains("    --label fixed"), "{text}");
        assert!(
            text.contains("    --label manual_complete\n        requires --variable summary=…"),
            "{text}"
        );
        assert!(!text.contains("Resolve with --label"), "{text}");
    }

    #[test]
    fn a_pattern_goes_only_with_allow_pattern_and_the_comment_reaches_the_agent() {
        use narsor_protocol::api::PermissionGrantScopeDto;
        let decision =
            commands::permission_decision(Some("allow-pattern"), None, Some("npm run *"), None)
                .unwrap()
                .unwrap();
        assert_eq!(decision.scope, Some(PermissionGrantScopeDto::Pattern));
        assert_eq!(decision.pattern.as_deref(), Some("npm run *"));

        let denied =
            commands::permission_decision(Some("deny"), Some(1), None, Some("Use cargo build."))
                .unwrap()
                .unwrap();
        assert_eq!(denied.scope, None);
        assert_eq!(denied.denial_index, Some(1));
        assert_eq!(denied.message.as_deref(), Some("Use cargo build."));

        assert!(commands::permission_decision(None, None, None, None)
            .unwrap()
            .is_none());
        for (permission, pattern) in [
            (Some("allow-pattern"), None),
            (Some("allow-exact"), Some("*")),
            (None, Some("*")),
        ] {
            assert!(
                commands::permission_decision(permission, None, pattern, None).is_err(),
                "{permission:?} {pattern:?}"
            );
        }
    }

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
