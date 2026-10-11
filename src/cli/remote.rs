//! Commands on the remote-aware control plane (U8): `task`, `backend`,
//! `auth` and the operator half of `controller`.
//!
//! Every one of them talks to the control plane's owner API only
//! (`/control/v1`, `/auth/v1`): a task is named by its id, never by a
//! backend address, and the only credential sent is the caller's own token
//! (R1, R41). Where the control plane is comes from
//! [`crate::api::ServerTarget`]: `--server` / `NARSOR_SERVER` with an OIDC
//! token for a network deployment, otherwise the trusted local daemon of
//! the state directory, launched when none runs.
//!
//! The token is never a command-line value — argv is visible to every user
//! of the machine. It comes from `NARSOR_TOKEN`, a file (`--token-file`,
//! `NARSOR_TOKEN_FILE`) or a helper command (`--token-command`,
//! `NARSOR_TOKEN_COMMAND`); the latter two are re-read after a `401`.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use futures::StreamExt as _;
use serde_json::{Map, Value};

use super::task_text::{
    age, awaiting_input, command_outcome, conversation_lines, conversation_origin,
    default_backend_line, default_harness_line, detail_lines, event_line, lifecycle_name,
    list_line, next_waiting, record_unreadable_detail, safe_json, status_badge, waiting_summary,
    PAGE_BYTE_BUDGET,
};
use crate::api::{
    ApiClient, ClientError, ConversationResponse as PageResponse, ServerTarget, TokenSource,
};
use crate::conductor::{ChatAuthor, ChatEntry, ConductorView};
use crate::config::Flags;
use crate::controller_protocol::{
    ConductorReadQuery, ConductorReadResponse, ConversationPageQuery, CreateEnrollmentRequest,
    CreateTaskRequest, ExecutionEventsQuery, ExecutionEventsResponse, LaunchRequest,
    MoveTaskRequest, OwnerCommandRequest, SetDefaultBackendRequest, SubmitInputRequest,
    SubmitOperatorCommandRequest, TaskSummary, WorkspaceRequest,
};
use crate::domain::{without_unsafe_chars, TaskId};
use crate::engine::HumanInputDelivery;
use crate::materials::{snapshot_paths, write_files, MaterialLimits, MaterialOrigin, WriteMode};
use crate::orchestration::{
    MoveTarget, OperatorCommand, OperatorCommandStatus, OwnerCommand, TaskEvent,
};
use crate::persistence::StatePaths;
use crate::tui::conversation::classify;
use crate::workflows::WorkflowResolver;

use super::commands::{CliError, Result};
use super::task_history::{history, HistoryCommand};
use super::task_registry::{
    capabilities, create_from_file, repo, validate, workflows, RepoCommand,
};

/// The environment variable holding a fixed token.
pub const TOKEN_ENV: &str = "NARSOR_TOKEN";

/// `narsor task …` — remote tasks on any backend.
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Creates a task and queues it on a backend: from a workflow of the
    /// catalogue, or — on a local daemon — from a workflow file (`--file`).
    Create {
        /// Workflow to run (its id in the control plane's catalogue).
        #[arg(
            value_name = "WORKFLOW",
            required_unless_present = "file",
            conflicts_with = "file"
        )]
        workflow: Option<String>,
        /// A workflow definition on the local daemon's disk, instead of a
        /// catalogue workflow: validated first (every violation is listed),
        /// then queued on the local backend. Not accepted by any other
        /// server (see `capabilities`).
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with_all = [
                "workflow_version", "backend", "profile", "harness", "repository", "commit"
            ]
        )]
        file: Option<PathBuf>,
        /// Workflow version; read from the local catalogue when omitted.
        #[arg(long = "workflow-version", value_name = "N")]
        workflow_version: Option<u32>,
        /// Workflow input, `name=value` (repeatable). A value that reads as
        /// JSON is taken as that; with `--file` the declared type decides.
        #[arg(long = "input", value_name = "NAME=VALUE")]
        inputs: Vec<String>,
        /// Backend to pin the task to; the default backend when omitted.
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// Environment profile; the owner default when omitted.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Harness for agent steps that name none.
        #[arg(long, value_name = "HARNESS")]
        harness: Option<String>,
        /// Registered repository to clone; the workflow's when omitted.
        #[arg(long, value_name = "ID")]
        repository: Option<String>,
        /// Full commit id to clone.
        #[arg(long, value_name = "SHA")]
        commit: Option<String>,
        /// Client-chosen task id: repeating the same request with it is
        /// safe and returns the same task.
        #[arg(long = "task-id", value_name = "ID")]
        task_id: Option<String>,
    },
    /// Lists your tasks with backend, lifecycle and queue state, the
    /// attention mark (`?` asks, `!` is wrong) and the title.
    List {
        /// Only the archived tasks, newest archive first (the TUI's history).
        #[arg(long)]
        archived: bool,
        /// Print the tasks as JSON (an array of task summaries).
        #[arg(long)]
        json: bool,
    },
    /// Shows one task: title and links, what it waits on, observations with
    /// their age, inputs, timeline and conversation availability.
    Show {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Print the task summary as JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Brings a running task to a safe point and releases its compute.
    Pause(CommandArgs),
    /// Continues a paused, interrupted or unknown task on its storage.
    Resume(CommandArgs),
    /// Starts an interrupted or unknown task afresh.
    Start(CommandArgs),
    /// Deletes the task's workload and storage. Cannot be undone.
    Archive(CommandArgs),
    /// Moves a queued task within its backend's queue.
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(false)))]
    Move {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Put it directly before this task.
        #[arg(long, value_name = "TASK_ID", group = "target")]
        before: Option<String>,
        /// Put it directly after this task.
        #[arg(long, value_name = "TASK_ID", group = "target")]
        after: Option<String>,
        /// Put it first.
        #[arg(long, group = "target")]
        top: bool,
        /// Put it last.
        #[arg(long, group = "target")]
        bottom: bool,
    },
    /// Cuts short the step the task's runner is working on; the execution
    /// is left recoverable (see `restart-step`).
    Interrupt(OperatorCommandArgs),
    /// Takes a manual transition out of the task's current state, to a
    /// state one of its declared transitions leads to — or, with
    /// `--override`, to any state its workflow has.
    Transition {
        #[command(flatten)]
        args: OperatorCommandArgs,
        /// The state to go to.
        #[arg(value_name = "TARGET")]
        target: String,
        /// Override the declared transitions. Needs a state that allows it
        /// (`allow_manual_override`) and a `--reason`, which is recorded in
        /// the task's audit trail.
        #[arg(long = "override")]
        forced: bool,
        /// Why the transition is overridden.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Starts the task's failed or interrupted step again, with a note for
    /// the agent's next attempt.
    RestartStep {
        #[command(flatten)]
        args: OperatorCommandArgs,
        /// An instruction for the agent's next attempt.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Only restart the step failure with this key; refused when the
        /// task has moved on from it.
        #[arg(long = "failure-key", value_name = "KEY")]
        failure_key: Option<String>,
    },
    /// Answers the input request the task's runner waits on.
    Input {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Form field value, `name=value` (repeatable). A value that reads
        /// as a JSON object or array is taken as that; anything else is text.
        #[arg(long = "field", value_name = "NAME=VALUE")]
        fields: Vec<String>,
        /// A key of the step's `data`, `name=value`, when resolving a
        /// problem (repeatable). A value that reads as JSON (`3`, `true`,
        /// `["a"]`) is taken as that; anything else is text.
        #[arg(long = "data", value_name = "NAME=VALUE")]
        data: Vec<String>,
        /// A context variable of the step's result, `name=value`, when
        /// resolving a problem (repeatable). Values read as for `--data`.
        #[arg(long = "variable", value_name = "NAME=VALUE")]
        variables: Vec<String>,
        /// Output label chosen explicitly.
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
        /// A remark kept with the answer.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
        /// The wait to answer (its id as `show` lists it): refused when the
        /// task waits on something else.
        #[arg(long = "waiting-id", value_name = "ID")]
        waiting_id: Option<String>,
    },
    /// Prints the conversation summary, or one attempt's entries read from
    /// the live runner through the control plane.
    Conversation {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// The attempt to read; only the summary when omitted.
        #[arg(value_name = "ATTEMPT_ID")]
        attempt_id: Option<String>,
        /// First entry to read.
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Prints the task's step timeline, one entry per attempt, marking the
    /// one under way.
    Steps {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
    },
    /// Prints the task's control-plane events, or with `--execution` the
    /// raw events of its workflow execution (read from the live runner, or
    /// from the task's storage when this host holds it).
    Events {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Keeps following new events (reconnecting as needed).
        #[arg(long, conflicts_with = "execution")]
        follow: bool,
        /// The execution's own events instead of the control plane's.
        #[arg(long)]
        execution: bool,
    },
    /// Follows the events of every task, from now or from `--cursor`.
    Watch {
        /// Resume after this stream cursor.
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
    },
    /// The task's conductor: chat with it, read its history, confirm or
    /// reject its proposals, undo a decision, choose its harness.
    Conductor {
        /// What to do.
        #[command(subcommand)]
        action: ConductorCommand,
    },
    /// Bounded one-off files for a task: upload, list, download, delete.
    Materials {
        /// What to do.
        #[command(subcommand)]
        action: MaterialsCommand,
    },
    /// Executions from before the task scheduler (the TUI's history): list,
    /// read and recover. A local daemon only.
    History {
        /// What to do.
        #[command(subcommand)]
        action: HistoryCommand,
    },
    /// The control plane's registry of repositories: list, add, remove.
    Repo {
        /// What to do.
        #[command(subcommand)]
        action: RepoCommand,
    },
    /// Lists the workflows a task can be created from, with their source,
    /// description and inputs.
    Workflows,
    /// Checks a workflow file on the local daemon's disk and lists every
    /// violation. Fails when there is one.
    Validate {
        /// The workflow definition.
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Shows what the control plane offers: workflow paths, old executions,
    /// local repository paths.
    Capabilities,
}

/// `narsor task materials …` (U9).
#[derive(Debug, Subcommand)]
pub enum MaterialsCommand {
    /// Uploads a one-off snapshot of files and folders to the task. Each
    /// file goes in under its own name, each folder's content under the
    /// folder's name. Links are never followed; links and special files
    /// inside a folder are skipped and listed.
    Upload {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Files and folders to send.
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<PathBuf>,
        /// Material id: repeating the same upload with it is safe.
        #[arg(long = "id", value_name = "ID")]
        material_id: Option<String>,
        /// A label for people; the single path's name when omitted.
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Time to live in seconds; bounded by the control plane.
        #[arg(long = "ttl-secs", value_name = "SECONDS")]
        ttl_secs: Option<u64>,
    },
    /// Lists the task's materials, with delivery state and expiry.
    List {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
    },
    /// Shows one material's files.
    Show {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// The material id.
        #[arg(value_name = "MATERIAL_ID")]
        material_id: String,
    },
    /// Downloads a material into a folder. Writes only inside it, never
    /// through a link, and refuses to overwrite an existing file unless
    /// `--force`.
    Download {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// The material id.
        #[arg(value_name = "MATERIAL_ID")]
        material_id: String,
        /// The folder to write into; created when missing.
        #[arg(value_name = "DEST_DIR")]
        dest: PathBuf,
        /// Replace existing regular files.
        #[arg(long)]
        force: bool,
    },
    /// Deletes a material at the control plane. A copy already delivered
    /// to the task's storage stays there until the task is archived.
    Delete {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// The material id.
        #[arg(value_name = "MATERIAL_ID")]
        material_id: String,
    },
}

/// Arguments of a lifecycle command.
#[derive(Debug, clap::Args)]
pub struct CommandArgs {
    /// The task id.
    #[arg(value_name = "TASK_ID")]
    pub task_id: String,
    /// Refuse unless the task is still at this generation.
    #[arg(long = "expected-generation", value_name = "N")]
    pub expected_generation: Option<u64>,
    /// Idempotency key; reuse it to retry safely. Generated when omitted.
    #[arg(long = "idempotency-key", value_name = "KEY")]
    pub idempotency_key: Option<String>,
}

/// `narsor task conductor …`.
#[derive(Debug, Subcommand)]
pub enum ConductorCommand {
    /// Writes a message to the conductor. Its answer arrives in the chat
    /// (see `history`).
    Chat {
        #[command(flatten)]
        args: OperatorCommandArgs,
        /// What to say.
        #[arg(value_name = "TEXT")]
        text: String,
    },
    /// Prints the conductor's state: what it works on, its diagnosis of a
    /// failed step, its proposals and decisions, and the chat.
    History {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Only the chat entries after this sequence number.
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Carries out a proposal of the conductor's.
    Confirm {
        #[command(flatten)]
        args: OperatorCommandArgs,
        /// The proposal's id (see `history`).
        #[arg(value_name = "PROPOSAL_ID")]
        proposal_id: String,
    },
    /// Turns a proposal of the conductor's down.
    Reject {
        #[command(flatten)]
        args: OperatorCommandArgs,
        /// The proposal's id (see `history`).
        #[arg(value_name = "PROPOSAL_ID")]
        proposal_id: String,
    },
    /// Takes back a chosen output of the conductor's (only the latest one).
    Undo {
        #[command(flatten)]
        args: OperatorCommandArgs,
        /// The decision's id (see `history`).
        #[arg(value_name = "DECISION_ID")]
        decision_id: String,
    },
}

/// Arguments shared by the commands that act on a task's runner.
#[derive(Debug, clap::Args)]
pub struct OperatorCommandArgs {
    /// The task id.
    #[arg(value_name = "TASK_ID")]
    pub task_id: String,
    /// Refuse unless the task is still at this generation.
    #[arg(long = "expected-generation", value_name = "N")]
    pub expected_generation: Option<u64>,
    /// Idempotency key; reuse it to retry safely. Generated when omitted.
    #[arg(long = "idempotency-key", value_name = "KEY")]
    pub idempotency_key: Option<String>,
    /// How long to wait for the runner to carry the command out before
    /// leaving it queued.
    #[arg(long = "wait-secs", value_name = "SECONDS", default_value_t = 30)]
    pub wait_secs: u64,
}

/// `narsor backend …`.
#[derive(Debug, Subcommand)]
pub enum BackendCommand {
    /// Shows or changes the default backend of new tasks.
    Default {
        /// What to do; `get` when omitted.
        #[command(subcommand)]
        action: Option<DefaultBackendAction>,
    },
}

/// `narsor backend default …`.
#[derive(Debug, Subcommand)]
pub enum DefaultBackendAction {
    /// Prints the effective default, the configured fallback and the
    /// override.
    Get,
    /// Makes `BACKEND` the default of tasks created from now on.
    Set {
        /// A registered backend id.
        #[arg(value_name = "BACKEND")]
        backend: String,
    },
    /// Removes the override: the configured default applies again.
    Reset,
}

/// `narsor harness …`.
#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// Shows or changes the default agent harness of the daemon.
    Default {
        /// What to do; `get` when omitted.
        #[command(subcommand)]
        action: Option<DefaultHarnessAction>,
        /// Print the selection as JSON instead (`effective`, `configured`,
        /// `override`, `supported`, `revision`).
        #[arg(long, global = true)]
        json: bool,
    },
}

/// `narsor harness default …`.
#[derive(Debug, Subcommand)]
pub enum DefaultHarnessAction {
    /// Prints the effective default, the configured fallback, the override
    /// and the harnesses this daemon supports.
    Get,
    /// Makes `HARNESS` the default of agent steps from now on.
    Set {
        /// A harness the daemon supports (see `get`).
        #[arg(value_name = "HARNESS")]
        harness: String,
    },
    /// Removes the override: the configured harness applies again.
    Reset,
}

/// `narsor auth …`.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Claims an unowned network-mode installation with its one-time code.
    Claim {
        /// The claim code the daemon printed at startup.
        #[arg(value_name = "CODE")]
        code: String,
    },
    /// Shows who the control plane takes you for.
    Whoami,
}

/// `narsor-cli controller …` for the installation operator.
#[derive(Debug, Subcommand)]
pub enum ControllerAdminCommand {
    /// Creates a one-time code that enrolls one controller for `BACKEND`.
    EnrollCode {
        /// The backend the controller will be scoped to.
        #[arg(value_name = "BACKEND")]
        backend: String,
        /// Label for the controller.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// How long the code may be redeemed, in seconds.
        #[arg(long = "ttl-secs", value_name = "SECONDS")]
        ttl_secs: Option<u64>,
    },
    /// Lists every controller identity, revoked ones included.
    List,
    /// Revokes a controller identity and fences its lease.
    Revoke {
        /// The controller id.
        #[arg(value_name = "CONTROLLER_ID")]
        controller_id: String,
    },
}

/// Builds the target from the command line and the environment.
///
/// `NARSOR_TOKEN` is read here only, never accepted as a flag.
pub fn server_target(
    server: Option<String>,
    token_file: Option<PathBuf>,
    token_command: Option<String>,
) -> ServerTarget {
    let token = match (token_file, token_command) {
        (Some(file), _) => Some(TokenSource::File(file)),
        (None, Some(command)) => Some(TokenSource::Command(command)),
        (None, None) => std::env::var(TOKEN_ENV)
            .ok()
            .filter(|token| !token.trim().is_empty())
            .map(|token| TokenSource::Static(token.trim().to_string())),
    };
    ServerTarget {
        server: server.filter(|server| !server.trim().is_empty()),
        token,
    }
}

async fn client(target: &ServerTarget, state: &Path, flags: &Flags) -> Result<ApiClient> {
    crate::api::connect(target, &StatePaths::new(state), flags)
        .await
        .map_err(CliError::failure)
}

/// A [`ClientError`] as the operator reads it.
pub(super) fn failure(what: &str, error: ClientError) -> CliError {
    let detail = match error {
        ClientError::NotFound => "not found".to_string(),
        ClientError::Unauthorized => {
            "the control plane rejected the token (expired or wrong audience?)".to_string()
        }
        ClientError::Request { code, message } | ClientError::Refused { code, message } => {
            format!("{message} ({code})")
        }
        other => other.to_string(),
    };
    CliError::failure(format!("{what}: {detail}"))
}

/// What `task create` prints about the task it queued.
pub(super) fn print_created(task: &TaskSummary) {
    println!("Created task: {}", task.task_id);
    println!("Backend: {}", task.backend_id);
    print_status(task);
}

/// The capabilities of the control plane, once it is known to be a trusted
/// local daemon: the only one that reads files and old executions of its own
/// disk for its owner. Any other server is refused with the reason it gives.
pub(super) async fn require_local_daemon(client: &ApiClient, what: &str) -> Result<()> {
    let capabilities = client
        .control_capabilities()
        .await
        .map_err(|error| failure("Could not read the server's capabilities", error))?;
    if capabilities.workflow_paths {
        return Ok(());
    }
    let reason = capabilities
        .workflow_paths_reason
        .as_deref()
        .map(|reason| format!(": {reason}"))
        .unwrap_or_default();
    Err(CliError::failure(format!(
        "{what} needs the local daemon; '{}' is not one{}",
        without_unsafe_chars(&capabilities.connection_name),
        without_unsafe_chars(&reason)
    )))
}

/// The answer of `task input` from its command-line pieces: the form's
/// fields, the step's `data` and `variables`, the chosen label and a remark.
fn input_delivery(
    fields: &[String],
    data: &[String],
    variables: &[String],
    label: &Option<String>,
    comment: &Option<String>,
) -> Result<HumanInputDelivery> {
    let mut delivery = HumanInputDelivery::with_values(field_values(fields)?)
        .with_data(parse_pairs("data", data)?)
        .with_variables(parse_pairs("variable", variables)?);
    if let Some(label) = label {
        delivery = delivery.choosing(label.clone());
    }
    if let Some(comment) = comment {
        delivery = delivery.with_comment(comment.clone());
    }
    Ok(delivery)
}

/// The `--field name=value` pairs of a form. A form takes text (that is what
/// the TUI sends too); a value that reads as a JSON object or array is the
/// exception and goes in as that.
fn field_values(fields: &[String]) -> Result<Map<String, Value>> {
    let mut values = Map::new();
    for pair in fields {
        let (name, raw) = split_pair("field", pair)?;
        let value = match serde_json::from_str::<Value>(raw) {
            Ok(structured @ (Value::Object(_) | Value::Array(_))) => structured,
            _ => Value::String(raw.to_string()),
        };
        values.insert(name.to_string(), value);
    }
    Ok(values)
}

/// `value` as pretty JSON, cleaned of what a terminal would act on.
fn json_output<T: serde::Serialize>(value: &T) -> Result<String> {
    safe_json(value).map_err(|error| CliError::failure(format!("Could not write JSON: {error}")))
}

/// Prints a page of a conversation record: number, time, kind and text, with
/// a hint for how to continue when the record is still open.
pub(super) fn print_conversation_entries(
    entries: &[crate::api::ConversationEntryDto],
    next_entry: u64,
    closed: bool,
) {
    for entry in entries {
        let classified = classify(entry);
        let body = if matches!(entry.source, crate::api::ConversationEntrySource::Narsor) {
            String::from_utf8_lossy(&classified.raw).into_owned()
        } else {
            classified.summary
        };
        let body = crate::tui::scroll::sanitize(&body);
        println!(
            "{:>5}  {}  {:<10} {}",
            classified.entry_number,
            classified.timestamp.to_rfc3339(),
            classified.kind.label(),
            body
        );
    }
    if !closed {
        println!("(continue with --from {next_entry})");
    }
}

// ---------------------------------------------------------------------------
// task
// ---------------------------------------------------------------------------

/// Runs one `narsor task` command.
pub async fn task(
    command: TaskCommand,
    target: &ServerTarget,
    state: &Path,
    catalog: &Path,
    flags: &Flags,
) -> Result<()> {
    // Parse everything local before connecting: a usage error never needs
    // the control plane.
    let create = match &command {
        TaskCommand::Create {
            workflow: Some(workflow),
            workflow_version,
            inputs,
            backend,
            profile,
            harness,
            repository,
            commit,
            task_id,
            ..
        } => Some(CreateTaskRequest {
            task_id: task_id.clone(),
            backend_id: backend.clone(),
            launch: LaunchRequest {
                workflow_id: workflow.clone(),
                workflow_version: match workflow_version {
                    Some(version) => *version,
                    None => local_workflow_version(catalog, workflow)?,
                },
                inputs: Value::Object(parse_pairs("input", inputs)?),
                workspace: (repository.is_some() || commit.is_some()).then(|| WorkspaceRequest {
                    repository: repository.clone(),
                    commit: commit.clone(),
                }),
                profile: profile.clone(),
                harness: harness.clone(),
                resources: None,
            },
        }),
        _ => None,
    };
    let delivery = match &command {
        TaskCommand::Input {
            fields,
            data,
            variables,
            label,
            comment,
            ..
        } => Some(input_delivery(fields, data, variables, label, comment)?),
        _ => None,
    };
    let client = client(target, state, flags).await?;

    match command {
        TaskCommand::Create {
            file: Some(file),
            inputs,
            task_id,
            ..
        } => create_from_file(&client, &file, &inputs, task_id).await,
        TaskCommand::Create { .. } => {
            let request = create.expect("built above");
            let task = client
                .create_control_task(&request)
                .await
                .map_err(|error| failure("Could not create the task", error))?;
            print_created(&task);
            Ok(())
        }
        TaskCommand::List { archived, json } => {
            let listing = if archived {
                client.control_tasks_archived(true).await
            } else {
                client.control_tasks().await
            }
            .map_err(|error| failure("Could not list the tasks", error))?;
            if json {
                println!("{}", json_output(&listing.tasks)?);
                return Ok(());
            }
            if listing.tasks.is_empty() {
                println!(
                    "{}",
                    if archived {
                        "No archived tasks."
                    } else {
                        "No remote tasks."
                    }
                );
            }
            for task in &listing.tasks {
                println!("{}", list_line(task));
            }
            Ok(())
        }
        TaskCommand::Show { task_id, json } => {
            let task = client
                .control_task(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            if json {
                println!("{}", json_output(&task)?);
                return Ok(());
            }
            let timeline = client.control_timeline(&task_id).await.ok();
            let conversation = client.control_conversation(&task_id).await.ok();
            for line in detail_lines(&task, timeline.as_ref(), conversation.as_ref()) {
                println!("{}", line.text);
            }
            if !task.attention.is_empty() {
                // The next task that waits on you, like the TUI's `Space`:
                // best effort, the details above stand without it.
                if let Ok(listing) = client.control_tasks().await {
                    if let Some(next) = next_waiting(&listing.tasks, &task_id) {
                        println!("Next       waiting: {}", next.task_id);
                    }
                }
            }
            Ok(())
        }
        TaskCommand::Pause(args) => owner_command(&client, OwnerCommand::Pause, args).await,
        TaskCommand::Resume(args) => owner_command(&client, OwnerCommand::Resume, args).await,
        TaskCommand::Start(args) => owner_command(&client, OwnerCommand::Start, args).await,
        TaskCommand::Archive(args) => owner_command(&client, OwnerCommand::Archive, args).await,
        TaskCommand::Move {
            task_id,
            before,
            after,
            top,
            bottom,
        } => {
            let target = move_target(before, after, top, bottom)?;
            let current = client
                .control_task(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            let task = client
                .control_move_task(
                    &task_id,
                    &MoveTaskRequest {
                        target,
                        expected_generation: Some(current.generation),
                    },
                )
                .await
                .map_err(|error| failure("Task not moved", error))?;
            println!(
                "{} is now #{} in its backend's queue.",
                task.task_id, task.rank
            );
            Ok(())
        }
        TaskCommand::Interrupt(args) => {
            operator_command(&client, OperatorCommand::Interrupt, args).await
        }
        TaskCommand::Transition {
            args,
            target,
            forced,
            reason,
        } => {
            let command = OperatorCommand::Transition {
                target,
                forced,
                reason,
            };
            operator_command(&client, command, args).await
        }
        TaskCommand::RestartStep {
            args,
            note,
            failure_key,
        } => {
            let command = OperatorCommand::RestartStep { note, failure_key };
            operator_command(&client, command, args).await
        }
        TaskCommand::Input {
            task_id,
            waiting_id,
            ..
        } => {
            let task = client
                .control_task(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            let Some(pending) = awaiting_input(&task) else {
                return Err(CliError::failure(format!(
                    "Task '{task_id}' is not waiting for input (lifecycle: {}).",
                    lifecycle_name(task.lifecycle)
                )));
            };
            if let Some(wanted) = waiting_id.filter(|wanted| *wanted != pending.id) {
                return Err(CliError::failure(format!(
                    "Task '{task_id}' is not waiting for input '{}'; it waits on: {}.",
                    without_unsafe_chars(&wanted),
                    without_unsafe_chars(&waiting_summary(&task.attention))
                )));
            }
            let delivery = delivery
                .expect("built above")
                .for_episode(pending.id.clone());
            let response = client
                .control_input(
                    &task_id,
                    &SubmitInputRequest {
                        pending_input_id: pending.id.clone(),
                        idempotency_key: uuid::Uuid::new_v4().to_string(),
                        delivery,
                    },
                )
                .await
                .map_err(|error| failure("The answer was not accepted", error))?;
            println!(
                "Answer {} queued for request {} ({:?}); the runner picks it up on its next poll.",
                response.input.input_id, response.input.pending_input_id, response.input.status
            );
            Ok(())
        }
        TaskCommand::Conversation {
            task_id,
            attempt_id,
            from,
        } => {
            let summary = client
                .control_conversation(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            let Some(attempt_id) = attempt_id else {
                let task = client
                    .control_task(&task_id)
                    .await
                    .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
                for line in conversation_lines(task.as_of, &summary) {
                    println!("{}", line.text);
                }
                return Ok(());
            };
            let page = client
                .control_conversation_page(
                    &task_id,
                    &attempt_id,
                    &ConversationPageQuery {
                        from,
                        byte_budget: Some(PAGE_BYTE_BUDGET),
                    },
                )
                .await
                .map_err(|error| match record_unreadable_detail(&error) {
                    Some(detail) => CliError::failure(format!(
                        "The conversation record of task '{task_id}' could not be read: {detail}"
                    )),
                    None => failure("The conversation cannot be read", error),
                })?;
            match page {
                PageResponse::Recorded {
                    entries,
                    next_entry,
                    closed,
                    source,
                    recorded_at,
                    ..
                } => {
                    println!(
                        "{}",
                        conversation_origin(
                            chrono::Utc::now(),
                            source.unwrap_or_default(),
                            recorded_at
                        )
                    );
                    if entries.is_empty() {
                        println!("(no entries from here)");
                    }
                    print_conversation_entries(&entries, next_entry, closed);
                    Ok(())
                }
                PageResponse::NoRecord => Err(CliError::failure(format!(
                    "No conversation record for attempt '{attempt_id}' of task '{task_id}'."
                ))),
            }
        }
        TaskCommand::Steps { task_id } => {
            let response = client
                .control_timeline(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            let Some(timeline) = response.timeline.filter(|t| !t.positions.is_empty()) else {
                println!("No steps.");
                return Ok(());
            };
            if let Some(current) = &response.current_position {
                println!("Under way: {}", without_unsafe_chars(current));
            }
            for position in &timeline.positions {
                crate::cli::commands::print_position(position);
            }
            Ok(())
        }
        TaskCommand::Events {
            task_id,
            execution: true,
            ..
        } => execution_events(&client, &task_id).await,
        TaskCommand::Events {
            task_id, follow, ..
        } => {
            let events = client
                .control_events(&task_id, 0)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            for event in &events {
                print_event(event);
            }
            if follow {
                // The listing's cursor covers every task; the stream is
                // restricted to this one, from now on.
                let listing = client
                    .control_tasks()
                    .await
                    .map_err(|error| failure("Could not read the stream cursor", error))?;
                follow_events(&client, Some(listing.cursor), Some(task_id)).await?;
            }
            Ok(())
        }
        TaskCommand::Conductor { action } => conductor(&client, action).await,
        TaskCommand::Materials { action } => materials(&client, action).await,
        TaskCommand::History { action } => history(&client, action).await,
        TaskCommand::Repo { action } => repo(&client, action).await,
        TaskCommand::Workflows => workflows(&client).await,
        TaskCommand::Validate { path } => validate(&client, &path).await,
        TaskCommand::Capabilities => capabilities(&client).await,
        TaskCommand::Watch { cursor } => {
            let cursor = match cursor {
                Some(cursor) => cursor,
                None => {
                    client
                        .control_tasks()
                        .await
                        .map_err(|error| failure("Could not read the stream cursor", error))?
                        .cursor
                }
            };
            follow_events(&client, Some(cursor), None).await
        }
    }
}

/// Runs one `narsor task conductor` command.
async fn conductor(client: &ApiClient, command: ConductorCommand) -> Result<()> {
    let (args, command) = match command {
        ConductorCommand::History { task_id, after } => {
            return conductor_history(client, &task_id, after).await
        }
        ConductorCommand::Chat { args, text } => (args, OperatorCommand::ConductorChat { text }),
        ConductorCommand::Confirm { args, proposal_id } => {
            (args, OperatorCommand::ConfirmProposal { proposal_id })
        }
        ConductorCommand::Reject { args, proposal_id } => {
            (args, OperatorCommand::RejectProposal { proposal_id })
        }
        ConductorCommand::Undo { args, decision_id } => {
            (args, OperatorCommand::UndoDecision { decision_id })
        }
    };
    operator_command(client, command, args).await
}

/// Prints the conductor's state, then its chat page by page. A task whose
/// runner is not running is answered with what its storage holds when this
/// host has it, and with the reason when it has not.
async fn conductor_history(client: &ApiClient, task_id: &str, after: u64) -> Result<()> {
    let mut query = ConductorReadQuery { after, limit: None };
    let mut first = true;
    loop {
        let response = client
            .control_conductor(task_id, &query)
            .await
            .map_err(|error| failure("The conductor's state cannot be read", error))?;
        let ConductorReadResponse::Available { conductor, .. } = response else {
            let ConductorReadResponse::Unavailable { message, .. } = response else {
                unreachable!("the other variant was matched above");
            };
            return Err(CliError::failure(format!(
                "The conductor's state is not available: {}",
                without_unsafe_chars(&message)
            )));
        };
        // What the runner or the conductor wrote is not trusted with the
        // terminal, whoever served it.
        let view = conductor.cleaned();
        if first {
            print_conductor_overview(&view);
            first = false;
        }
        for entry in &view.chat {
            print_chat_entry(entry);
        }
        match view.chat.last() {
            Some(last) if view.chat_has_more => query.after = last.sequence,
            _ => return Ok(()),
        }
    }
}

fn print_conductor_overview(view: &ConductorView) {
    match (&view.harness, &view.unavailable_reason) {
        (Some(harness), _) if view.available => println!("Conductor: running on {harness}"),
        (_, Some(reason)) => println!("Conductor: not running ({reason})"),
        _ => println!("Conductor: not running"),
    }
    for work in &view.working {
        println!(
            "Working: {:?} ({:?}){}",
            work.purpose,
            work.trigger,
            if work.running { "" } else { ", queued" }
        );
    }
    if let Some(failure) = &view.failure {
        println!(
            "Failed step {}: {}",
            failure.state_id,
            failure.reason.replace('\n', " ")
        );
    }
    for proposal in &view.proposals {
        println!(
            "Proposal {}  [{:?}]  {}",
            proposal.proposal_id, proposal.state, proposal.text
        );
    }
    for decision in &view.decisions {
        let note = match (decision.undone, decision.undoable) {
            (true, _) => " (undone)",
            (false, true) => " (can be undone)",
            (false, false) => "",
        };
        println!(
            "Decision {}  {}  {}{note}",
            decision.decision_id, decision.kind, decision.text
        );
    }
}

fn print_chat_entry(entry: &ChatEntry) {
    let author = match entry.author {
        ChatAuthor::Operator => "you",
        ChatAuthor::Conductor => "conductor",
    };
    println!(
        "#{:<4} {}  {author}: {}",
        entry.sequence,
        entry.at.to_rfc3339(),
        entry.text
    );
}

/// Prints every event of the task's execution, page by page. A task whose
/// runner is not running (and whose storage this host does not hold) is
/// answered with the reason, which is a failure of the command, not of the
/// server.
async fn execution_events(client: &ApiClient, task_id: &str) -> Result<()> {
    let mut query = ExecutionEventsQuery::default();
    loop {
        let response = client
            .control_execution_events(task_id, &query)
            .await
            .map_err(|error| failure("The execution's events cannot be read", error))?;
        match response {
            ExecutionEventsResponse::Available {
                events, has_more, ..
            } => {
                for event in &events {
                    crate::cli::commands::print_event(event);
                }
                match events.last() {
                    Some(last) if has_more => query.after = last.sequence,
                    _ => return Ok(()),
                }
            }
            ExecutionEventsResponse::Unavailable { message, .. } => {
                return Err(CliError::failure(format!(
                    "The execution's events are not available: {}",
                    without_unsafe_chars(&message)
                )));
            }
        }
    }
}

/// Runs one `narsor task materials` command.
async fn materials(client: &ApiClient, command: MaterialsCommand) -> Result<()> {
    match command {
        MaterialsCommand::Upload {
            task_id,
            paths,
            material_id,
            label,
            ttl_secs,
        } => {
            let limits = MaterialLimits::default();
            let snapshot = snapshot_paths(&paths, &limits, limits.max_total_bytes)
                .map_err(|error| CliError::failure(format!("Nothing was uploaded: {error}")))?;
            for skipped in &snapshot.skipped {
                eprintln!("skipped {}: {}", skipped.path.display(), skipped.reason);
            }
            let mut bundle = snapshot.bundle;
            bundle.manifest.material_id = material_id;
            if label.is_some() {
                bundle.manifest.label = label;
            }
            bundle.manifest.ttl_secs = ttl_secs;
            let body = bundle
                .encode()
                .map_err(|error| CliError::failure(format!("Nothing was uploaded: {error}")))?;
            let response = client
                .upload_material(&task_id, body.into())
                .await
                .map_err(|error| failure("The material was not uploaded", error))?;
            let material = &response.material;
            println!(
                "{} material {}: {} file(s), {} bytes, expires {}.",
                if response.replayed {
                    "Already uploaded:"
                } else {
                    "Uploaded"
                },
                material.material_id,
                material.file_count,
                material.total_bytes,
                material.expires_at.to_rfc3339()
            );
            println!("The task's runner receives it on its next poll; nothing is committed.");
            Ok(())
        }
        MaterialsCommand::List { task_id } => {
            let listing = client
                .materials(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            if listing.materials.is_empty() {
                println!("No materials.");
            }
            for material in &listing.materials {
                let state = match (material.origin, material.delivered_at) {
                    (MaterialOrigin::Runner, _) => "from the task".to_string(),
                    (MaterialOrigin::Owner, Some(at)) => format!("delivered {}", at.to_rfc3339()),
                    (MaterialOrigin::Owner, None) => "waiting for the runner".to_string(),
                };
                println!(
                    "{:<40} {:>5} file(s) {:>10} bytes  {:<34} expires {}  {}",
                    material.material_id,
                    material.file_count,
                    material.total_bytes,
                    state,
                    material.expires_at.to_rfc3339(),
                    material.label.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        MaterialsCommand::Show {
            task_id,
            material_id,
        } => {
            let view = client
                .material(&task_id, &material_id)
                .await
                .map_err(|error| {
                    failure(&format!("Could not load material '{material_id}'"), error)
                })?;
            for file in &view.files {
                println!("{:>10}  {}  {}", file.size, file.sha256, file.path);
            }
            Ok(())
        }
        MaterialsCommand::Download {
            task_id,
            material_id,
            dest,
            force,
        } => {
            let (view, blobs) = client
                .fetch_material(&task_id, &material_id)
                .await
                .map_err(|error| failure(&format!("Could not download '{material_id}'"), error))?;
            std::fs::create_dir_all(&dest).map_err(|error| {
                CliError::failure(format!("Cannot create '{}': {error}", dest.display()))
            })?;
            let mode = if force {
                WriteMode::Overwrite
            } else {
                WriteMode::Refuse
            };
            let written = write_files(&dest, &view.files, &blobs, mode)
                .map_err(|error| CliError::failure(format!("Nothing was written: {error}")))?;
            println!(
                "Wrote {} file(s) of material {} into {}.",
                written.len(),
                material_id,
                dest.display()
            );
            Ok(())
        }
        MaterialsCommand::Delete {
            task_id,
            material_id,
        } => {
            let removed = client
                .delete_material(&task_id, &material_id)
                .await
                .map_err(|error| failure(&format!("Could not delete '{material_id}'"), error))?;
            println!(
                "Deleted material {} at the control plane ({} file(s)).",
                removed.material_id, removed.file_count
            );
            Ok(())
        }
    }
}

async fn follow_events(
    client: &ApiClient,
    cursor: Option<String>,
    task_id: Option<String>,
) -> Result<()> {
    let mut stream = Box::pin(client.follow_control_events(cursor, task_id));
    while let Some(item) = stream.next().await {
        match item {
            Ok(envelope) => {
                print_event(&envelope.event);
                eprintln!("cursor {}", envelope.cursor);
            }
            Err(error @ (ClientError::Unavailable(_) | ClientError::Server(_))) => {
                eprintln!("(reconnecting: {error})");
            }
            Err(error) => return Err(failure("The event stream ended", error)),
        }
    }
    Ok(())
}

async fn owner_command(client: &ApiClient, command: OwnerCommand, args: CommandArgs) -> Result<()> {
    let request = OwnerCommandRequest {
        command,
        idempotency_key: args
            .idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        expected_generation: args.expected_generation,
    };
    let result = client.control_command(&args.task_id, &request).await;
    let outcome = command_outcome(command, &result);
    match result {
        Ok(response) => {
            println!("{}", outcome.text);
            print_status(&response.task);
            Ok(())
        }
        Err(ClientError::NotFound) => Err(CliError::failure(format!(
            "Task '{}' not found.",
            args.task_id
        ))),
        // A full backend (the outcome's warning) is reported like any other
        // refusal: nothing changed, the exit code says so.
        Err(_) => Err(CliError::failure(outcome.text)),
    }
}

/// How often a command waiting for its runner looks at the task again.
const COMMAND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Sends an operator command to the task's runner and waits up to
/// `wait_secs` for its verdict. A command the runner has not settled by
/// then stays queued — it is delivered on the runner's next poll — and the
/// answer says so; a command the runner rejected is a failure.
async fn operator_command(
    client: &ApiClient,
    command: OperatorCommand,
    args: OperatorCommandArgs,
) -> Result<()> {
    let request = SubmitOperatorCommandRequest {
        command,
        idempotency_key: args
            .idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        expected_generation: args.expected_generation,
    };
    let task_id = args.task_id;
    let accepted = client
        .control_operator_command(&task_id, &request)
        .await
        .map_err(|error| failure("The command was not accepted", error))?;
    let mut status = accepted.command;
    let command_id = status.command_id.clone();
    let name = status.command.describe();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.wait_secs);
    loop {
        match status.status {
            OperatorCommandStatus::Applied => {
                println!("The runner carried out the {name} ({command_id}).");
                return Ok(());
            }
            OperatorCommandStatus::Rejected => {
                return Err(CliError::failure(format!(
                    "The runner rejected the {name} ({command_id}): {}",
                    without_unsafe_chars(status.detail.as_deref().unwrap_or("no reason given"))
                )));
            }
            OperatorCommandStatus::Queued => {}
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "The {name} ({command_id}) is queued: the runner has not confirmed it yet. \
                 It is delivered on the runner's next poll; follow it with `narsor task events \
                 {task_id}`."
            );
            return Ok(());
        }
        tokio::time::sleep(COMMAND_POLL_INTERVAL).await;
        let task = client
            .control_task(&task_id)
            .await
            .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
        if let Some(current) = task
            .operator_commands
            .into_iter()
            .find(|view| view.command_id == command_id)
        {
            status = current;
        }
    }
}

/// The move the flags of `task move` ask for; clap guarantees exactly one.
fn move_target(
    before: Option<String>,
    after: Option<String>,
    top: bool,
    bottom: bool,
) -> Result<MoveTarget> {
    let anchor = |id: String| {
        TaskId::parse(id).map_err(|error| CliError::failure(format!("Invalid task id: {error}")))
    };
    match (before, after, top, bottom) {
        (Some(id), None, false, false) => Ok(MoveTarget::Before(anchor(id)?)),
        (None, Some(id), false, false) => Ok(MoveTarget::After(anchor(id)?)),
        (None, None, true, false) => Ok(MoveTarget::Top),
        (None, None, false, true) => Ok(MoveTarget::Bottom),
        _ => Err(CliError::usage(
            "Give exactly one of --before, --after, --top and --bottom.",
        )),
    }
}

fn print_status(task: &TaskSummary) {
    println!("Lifecycle: {}", status_badge(task).text);
    if let Some(queue) = &task.queue {
        println!("Queue: {}", queue.message);
    }
}

fn print_event(event: &TaskEvent) {
    println!("{}", event_line(event));
}

/// `name=value` pairs into a JSON object; a value that reads as JSON is
/// taken as that.
fn parse_pairs(flag: &str, pairs: &[String]) -> Result<Map<String, Value>> {
    let mut values = Map::new();
    for pair in pairs {
        let (name, raw) = split_pair(flag, pair)?;
        let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        values.insert(name.to_string(), value);
    }
    Ok(values)
}

/// `name=value` of `--{flag}`, the name trimmed; a usage error otherwise.
fn split_pair<'a>(flag: &str, pair: &'a str) -> Result<(&'a str, &'a str)> {
    pair.split_once('=')
        .map(|(name, raw)| (name.trim(), raw))
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| {
            CliError::usage(format!(
                "Invalid --{flag} '{pair}' — expected the form name=value."
            ))
        })
}

/// The version of `workflow` in the local catalogue, for a create without
/// `--workflow-version`. The control plane refuses a version its own
/// catalogue does not hold, so a stale local copy fails loudly, never runs
/// something unexpected.
fn local_workflow_version(catalog: &Path, workflow: &str) -> Result<u32> {
    let resolved = WorkflowResolver::new(catalog)
        .resolve(workflow)
        .map_err(|_| {
            CliError::usage(format!(
                "Workflow '{workflow}' is not in the local catalogue; pass --workflow-version."
            ))
        })?;
    crate::tasks::load_resolved(&resolved)
        .map(|definition| definition.version)
        .map_err(|error| CliError::usage(format!("{error}; pass --workflow-version.")))
}

// ---------------------------------------------------------------------------
// backend, auth, controller
// ---------------------------------------------------------------------------

/// Runs one `narsor backend` command.
pub async fn backend(
    command: BackendCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    let client = client(target, state, flags).await?;
    let BackendCommand::Default { action } = command;
    let view = match action.unwrap_or(DefaultBackendAction::Get) {
        DefaultBackendAction::Get => client.control_default_backend().await,
        DefaultBackendAction::Set { backend } => {
            client
                .control_set_default_backend(&SetDefaultBackendRequest {
                    backend_id: Some(backend),
                })
                .await
        }
        DefaultBackendAction::Reset => {
            client
                .control_set_default_backend(&SetDefaultBackendRequest { backend_id: None })
                .await
        }
    }
    .map_err(|error| failure("Default backend", error))?;
    println!("{}", default_backend_line(&view));
    println!("Effective: {}", view.effective);
    println!("Configured: {}", view.configured);
    println!(
        "Override: {}",
        view.override_backend
            .as_ref()
            .map_or("-".to_string(), ToString::to_string)
    );
    println!("Applies to tasks created from now on; existing tasks never move.");
    Ok(())
}

/// Runs one `narsor harness` command.
pub async fn harness(
    command: HarnessCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    let client = client(target, state, flags).await?;
    let HarnessCommand::Default { action, json } = command;
    let selection = match action.unwrap_or(DefaultHarnessAction::Get) {
        DefaultHarnessAction::Get => client.harness_selection().await,
        DefaultHarnessAction::Set { harness } => client.set_harness_selection(&harness).await,
        DefaultHarnessAction::Reset => client.reset_harness_selection().await,
    }
    .map_err(|error| failure("Default harness", error))?;
    if json {
        println!("{}", json_output(&selection)?);
        return Ok(());
    }
    println!("{}", default_harness_line(&selection));
    println!("Effective: {}", without_unsafe_chars(&selection.effective));
    println!(
        "Configured: {}",
        without_unsafe_chars(&selection.configured)
    );
    println!(
        "Override: {}",
        selection
            .override_harness
            .as_deref()
            .map_or("-".to_string(), without_unsafe_chars)
    );
    println!(
        "Supported: {}",
        without_unsafe_chars(&selection.supported.join(", "))
    );
    Ok(())
}

/// Runs one `narsor auth` command.
pub async fn auth(
    command: AuthCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    let client = client(target, state, flags).await?;
    match command {
        AuthCommand::Claim { code } => match client.claim(code.trim()).await {
            Ok(owner) => {
                println!(
                    "This installation is owned by {} at {} (bound by {:?}).",
                    owner.subject, owner.issuer, owner.bound_by
                );
                Ok(())
            }
            Err(ClientError::NotFound) => Err(CliError::failure(
                "This control plane runs in trusted mode: there is no owner to claim.",
            )),
            Err(ClientError::Forbidden) => Err(CliError::failure(
                "Claim refused: the code is wrong or spent, or the installation already has an \
                 owner.",
            )),
            Err(error) => Err(failure("Claim failed", error)),
        },
        AuthCommand::Whoami => {
            let who = client
                .whoami()
                .await
                .map_err(|error| failure("Who am I", error))?;
            println!("Mode: {}", who.mode);
            if let (Some(issuer), Some(subject)) = (&who.issuer, &who.subject) {
                println!("Identity: {subject} at {issuer}");
            }
            println!("Owner: {}", if who.owner { "yes" } else { "no" });
            println!("Claimed: {}", if who.claimed { "yes" } else { "no" });
            if let Some(expires_at) = who.expires_at {
                println!("Token expires: {}", expires_at.to_rfc3339());
            }
            Ok(())
        }
    }
}

/// Runs one operator `narsor-cli controller` command.
pub async fn controller_admin(
    command: ControllerAdminCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    let client = client(target, state, flags).await?;
    match command {
        ControllerAdminCommand::EnrollCode {
            backend,
            name,
            ttl_secs,
        } => {
            let code = client
                .create_enrollment(&backend, &CreateEnrollmentRequest { name, ttl_secs })
                .await
                .map_err(|error| failure("Could not create an enrollment code", error))?;
            println!("Enrollment code: {}", code.enrollment_code);
            println!("Backend: {}", code.backend_id);
            println!("Expires: {}", code.expires_at.to_rfc3339());
            eprintln!(
                "Shown once. Hand it to the controller as NARSOR_ENROLLMENT_CODE; it is spent on \
                 first use."
            );
            Ok(())
        }
        ControllerAdminCommand::List => {
            let listing = client
                .controllers()
                .await
                .map_err(|error| failure("Could not list the controllers", error))?;
            let now = chrono::Utc::now();
            if listing.controllers.is_empty() {
                println!("No controllers.");
            }
            for controller in &listing.controllers {
                let state = match (controller.revoked_at, controller.holds_lease) {
                    (Some(at), _) => format!("revoked {} ago", age(now, at)),
                    (None, true) => "holds the lease".to_string(),
                    (None, false) => "no lease".to_string(),
                };
                println!(
                    "{:<40} {:<14} {:<20} heartbeat {}",
                    controller.controller_id,
                    controller.backend_id,
                    state,
                    controller
                        .last_heartbeat_at
                        .map_or("never".to_string(), |at| format!("{} ago", age(now, at)))
                );
            }
            Ok(())
        }
        ControllerAdminCommand::Revoke { controller_id } => {
            let view = client
                .revoke_controller(&controller_id)
                .await
                .map_err(|error| failure("Could not revoke the controller", error))?;
            println!(
                "Revoked controller {} of backend {}; its lease is fenced.",
                view.controller_id, view.backend_id
            );
            Ok(())
        }
    }
}
