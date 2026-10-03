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

use crate::api::{
    ApiClient, ClientError, ConversationResponse as PageResponse, ServerTarget, TokenSource,
};
use crate::config::Flags;
use crate::controller_protocol::{
    ConversationPageQuery, CreateEnrollmentRequest, CreateTaskRequest, LaunchRequest,
    OwnerCommandRequest, SetDefaultBackendRequest, SetPriorityRequest, SubmitInputRequest,
    TaskSummary, WorkspaceRequest,
};
use crate::engine::HumanInputDelivery;
use crate::materials::{snapshot_paths, write_files, MaterialLimits, MaterialOrigin, WriteMode};
use crate::orchestration::{OwnerCommand, TaskEvent, TaskEventKind};
use crate::persistence::StatePaths;
use crate::tui::conversation::classify;
use crate::tui::remote::{
    age, awaiting_input, command_outcome, conversation_lines, default_backend_line, detail_lines,
    lifecycle_name, status_badge, PAGE_BYTE_BUDGET,
};
use crate::workflows::WorkflowResolver;

use super::commands::{CliError, Result};

/// The environment variable holding a fixed token.
pub const TOKEN_ENV: &str = "NARSOR_TOKEN";

/// `narsor task …` — remote tasks on any backend.
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Creates a task and queues it on a backend.
    Create {
        /// Workflow to run (its id in the control plane's catalogue).
        #[arg(value_name = "WORKFLOW")]
        workflow: String,
        /// Workflow version; read from the local catalogue when omitted.
        #[arg(long = "workflow-version", value_name = "N")]
        workflow_version: Option<u32>,
        /// Workflow input, `name=value` (repeatable). A value that reads as
        /// JSON is taken as that.
        #[arg(long = "input", value_name = "NAME=VALUE")]
        inputs: Vec<String>,
        /// Backend to pin the task to; the default backend when omitted.
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// Queue priority, higher first.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        priority: i32,
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
    /// Lists your tasks with backend, lifecycle and queue state.
    List,
    /// Shows one task: observations with their age, inputs, timeline and
    /// conversation availability.
    Show {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
    },
    /// Brings a running task to a safe point and releases its compute.
    Pause(CommandArgs),
    /// Continues a paused, interrupted or unknown task on its storage.
    Resume(CommandArgs),
    /// Starts an interrupted or unknown task afresh.
    Start(CommandArgs),
    /// Deletes the task's workload and storage. Cannot be undone.
    Archive(CommandArgs),
    /// Changes the priority of a queued task.
    Priority {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// The new priority, higher first.
        #[arg(value_name = "PRIORITY", allow_hyphen_values = true)]
        priority: i32,
    },
    /// Answers the input request the task's runner waits on.
    Input {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Form field value, `name=value` (repeatable).
        #[arg(long = "field", value_name = "NAME=VALUE")]
        fields: Vec<String>,
        /// Output label chosen explicitly.
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
        /// A remark kept with the answer.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
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
    /// Prints the task's control-plane events.
    Events {
        /// The task id.
        #[arg(value_name = "TASK_ID")]
        task_id: String,
        /// Keeps following new events (reconnecting as needed).
        #[arg(long)]
        follow: bool,
    },
    /// Follows the events of every task, from now or from `--cursor`.
    Watch {
        /// Resume after this stream cursor.
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
    },
    /// Bounded one-off files for a task: upload, list, download, delete.
    Materials {
        /// What to do.
        #[command(subcommand)]
        action: MaterialsCommand,
    },
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

/// `narsor controller …` for the installation operator.
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
fn failure(what: &str, error: ClientError) -> CliError {
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
            workflow,
            workflow_version,
            inputs,
            backend,
            priority,
            profile,
            harness,
            repository,
            commit,
            task_id,
        } => Some(CreateTaskRequest {
            task_id: task_id.clone(),
            backend_id: backend.clone(),
            priority: *priority,
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
    let client = client(target, state, flags).await?;

    match command {
        TaskCommand::Create { .. } => {
            let request = create.expect("built above");
            let task = client
                .create_control_task(&request)
                .await
                .map_err(|error| failure("Could not create the task", error))?;
            println!("Created task: {}", task.task_id);
            println!("Backend: {}", task.backend_id);
            print_status(&task);
            Ok(())
        }
        TaskCommand::List => {
            let listing = client
                .control_tasks()
                .await
                .map_err(|error| failure("Could not list the tasks", error))?;
            if listing.tasks.is_empty() {
                println!("No remote tasks.");
            }
            for task in &listing.tasks {
                println!(
                    "{:<28} {:<14} {:<10} {}",
                    task.task_id,
                    task.backend_id,
                    format!("p{}", task.priority),
                    status_badge(task).text
                );
            }
            Ok(())
        }
        TaskCommand::Show { task_id } => {
            let task = client
                .control_task(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            let timeline = client.control_timeline(&task_id).await.ok();
            let conversation = client.control_conversation(&task_id).await.ok();
            for line in detail_lines(&task, timeline.as_ref(), conversation.as_ref()) {
                println!("{}", line.text);
            }
            Ok(())
        }
        TaskCommand::Pause(args) => owner_command(&client, OwnerCommand::Pause, args).await,
        TaskCommand::Resume(args) => owner_command(&client, OwnerCommand::Resume, args).await,
        TaskCommand::Start(args) => owner_command(&client, OwnerCommand::Start, args).await,
        TaskCommand::Archive(args) => owner_command(&client, OwnerCommand::Archive, args).await,
        TaskCommand::Priority { task_id, priority } => {
            let current = client
                .control_task(&task_id)
                .await
                .map_err(|error| failure(&format!("Could not load task '{task_id}'"), error))?;
            let task = client
                .control_set_priority(
                    &task_id,
                    &SetPriorityRequest {
                        priority,
                        expected_generation: Some(current.generation),
                    },
                )
                .await
                .map_err(|error| failure("Priority not changed", error))?;
            println!("Priority of {} is now {}.", task.task_id, task.priority);
            Ok(())
        }
        TaskCommand::Input {
            task_id,
            fields,
            label,
            comment,
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
            let values = fields
                .iter()
                .map(|pair| {
                    pair.split_once('=')
                        .map(|(name, value)| (name.to_string(), Value::String(value.to_string())))
                        .ok_or_else(|| {
                            CliError::usage(format!(
                                "Invalid field '{pair}' — expected the form name=value."
                            ))
                        })
                })
                .collect::<Result<Map<String, Value>>>()?;
            let mut delivery =
                HumanInputDelivery::with_values(values).for_episode(pending.id.clone());
            if let Some(label) = label {
                delivery = delivery.choosing(label);
            }
            if let Some(comment) = comment {
                delivery = delivery.with_comment(comment);
            }
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
                .map_err(|error| failure("The conversation cannot be read", error))?;
            match page {
                PageResponse::Recorded {
                    entries,
                    next_entry,
                    closed,
                    ..
                } => {
                    for entry in &entries {
                        let classified = classify(entry);
                        println!(
                            "{:>5}  {}  {:<10} {}",
                            classified.entry_number,
                            classified.timestamp.to_rfc3339(),
                            classified.kind.label(),
                            classified.text
                        );
                    }
                    if !closed {
                        println!("(continue with --from {next_entry})");
                    }
                    Ok(())
                }
                PageResponse::NoRecord => Err(CliError::failure(format!(
                    "No conversation record for attempt '{attempt_id}' of task '{task_id}'."
                ))),
            }
        }
        TaskCommand::Events { task_id, follow } => {
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
        TaskCommand::Materials { action } => materials(&client, action).await,
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

fn print_status(task: &TaskSummary) {
    println!("Lifecycle: {}", status_badge(task).text);
    if let Some(queue) = &task.queue {
        println!("Queue: {}", queue.message);
    }
}

fn print_event(event: &TaskEvent) {
    let kind = match &event.kind {
        TaskEventKind::Created => "created".to_string(),
        TaskEventKind::OwnerCommandAccepted { command, .. } => {
            format!("owner command {command:?}")
        }
        TaskEventKind::ControllerReported { observation, .. } => {
            format!("controller reported {observation:?}")
        }
        TaskEventKind::RunnerReported {
            execution_status,
            pending_input_id,
            ..
        } => match pending_input_id {
            Some(pending) => format!(
                "runner reported {} (awaiting input {pending})",
                execution_status.as_str()
            ),
            None => format!("runner reported {}", execution_status.as_str()),
        },
        TaskEventKind::InputQueued { input_id, .. } => format!("input {input_id} queued"),
        TaskEventKind::PriorityChanged { priority } => format!("priority {priority}"),
        TaskEventKind::QueueChanged { reason } => format!("queue: {}", reason.describe()),
        TaskEventKind::InputsUpdated { updates } => format!("{} input(s) updated", updates.len()),
        TaskEventKind::MaterialChanged {
            material_id,
            change,
            files,
            total_bytes,
        } => format!("material {material_id} {change:?} ({files} file(s), {total_bytes} bytes)"),
    };
    println!(
        "{:>4}  {}  {:<24} {:<12} {kind}",
        event.sequence,
        event.at.to_rfc3339(),
        event.task_id,
        lifecycle_name(event.lifecycle)
    );
}

/// `name=value` pairs into a JSON object; a value that reads as JSON is
/// taken as that.
fn parse_pairs(flag: &str, pairs: &[String]) -> Result<Map<String, Value>> {
    let mut values = Map::new();
    for pair in pairs {
        let (name, raw) = pair
            .split_once('=')
            .filter(|(name, _)| !name.trim().is_empty())
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Invalid --{flag} '{pair}' — expected the form name=value."
                ))
            })?;
        let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        values.insert(name.trim().to_string(), value);
    }
    Ok(values)
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

/// Runs one operator `narsor controller` command.
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
