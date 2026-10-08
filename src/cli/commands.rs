//! Implementation of every subcommand.
//!
//! # Where a `run` gets its workspace
//!
//! `run` owns the command line — parsing `--input name=value` pairs against the
//! declared parameters, and printing what happened — and nothing else: creating
//! the execution, provisioning its workspace and recording where its definition
//! lives is [`crate::tasks::start_task`], which the interface calls too. That
//! module documents the ordering those steps have to keep.
//!
//! # Where a definition comes from
//!
//! `validate`, `run` and `--file` all take a *workflow reference*, not a path:
//! an existing file wins, then `{root}/workflows/<name>.yaml`, then the user
//! directory, then the built-in of that name ([`crate::workflows`]). That is
//! what makes `narsor run compound-engineering` work from an installed
//! binary, which has no checkout to read from.
//!
//! # How `input` and `resume` find the definition
//!
//! The engine needs the [`WorkflowDefinition`] to continue an execution, but
//! `state.json` stores only the workflow *id*. So `run` records both the
//! absolute path of the file it read (when there was one) and the workflow's
//! name, and re-resolves from that — [`crate::tasks::resolve_definition`]. A
//! built-in has no path at all, so without the name an execution started
//! from one could never be resumed. This resolution happens daemon-side
//! (R4/D52): `input` and `resume` are pure `ApiClient` callers, so neither
//! reaches for `tasks::resolve_definition` itself any more, and neither
//! accepts `--file` — there is no way for a REST call to point the daemon at
//! a local override path it has no filesystem relationship to.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::{Map, Value};

use crate::api::{ApiClient, ClientError, ConversationResponse, ExecutionSummary};
use crate::conductor::{ChatAuthor, ChatEntry, ChatEntryKind};
use crate::domain::{
    Event, ExecutionStatus, InputParameter, ParamType, ProblemKind, WorkflowDefinition,
};
use crate::engine::{
    ConversationState, HumanInputDelivery, PendingHumanInput, Position, PositionStatus,
};
use crate::persistence::StatePaths;
use crate::registry::{infer_provider, RepositoryEntry, RepositoryRegistry};
use crate::tasks::{self, TaskError};
use crate::tui::conversation::classify;
use crate::workflows::{builtin, builtin_names, ResolvedWorkflow, WorkflowResolver};

use crate::config::{render_value, Flags, Resolved, SettingKey, Source};

use super::{ConfigFormat, EXIT_FAILURE, EXIT_USAGE};

/// How often `events --follow` looks for new entries in the log.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

/// A message for the operator plus the exit code it carries.
#[derive(Debug)]
pub struct CliError {
    /// What is printed to stderr.
    pub message: String,
    /// The process exit code.
    pub exit_code: i32,
}

impl CliError {
    /// A validation error, a failed execution, or an unreadable store.
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_FAILURE,
        }
    }

    /// A wrong invocation: nothing ran.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_USAGE,
        }
    }
}

/// Result of a subcommand.
pub type Result<T> = std::result::Result<T, CliError>;

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

/// `narsor validate <workflow>` — static validation, all violations at
/// once.
///
/// The argument is a workflow reference: a file, a name in the search path, or
/// a built-in.
pub fn validate(catalog: &Path, reference: &str) -> Result<()> {
    let resolved = resolve_workflow(catalog, reference)?;
    let definition = tasks::load_resolved(&resolved).map_err(|error| match error {
        TaskError::InvalidDefinition { violations } => {
            let mut message = format!(
                "Definition '{}' ({}) is invalid — {} {} found:",
                resolved.name,
                resolved.origin,
                violations.len(),
                violation_noun(violations.len())
            );
            for violation in &violations {
                message.push_str(&format!("\n  - {violation}"));
            }
            CliError::failure(message)
        }
        other => CliError::failure(other.to_string()),
    })?;

    println!(
        "The definition is valid: {} (version {}), states: {}, initial state: {}.",
        definition.name,
        definition.version,
        definition.states.len(),
        definition.initial_state
    );
    Ok(())
}

/// Singular or plural, for the violation count.
fn violation_noun(count: usize) -> &'static str {
    if count == 1 {
        "violation"
    } else {
        "violations"
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// `narsor run <workflow> [--input k=v]…` — create an execution and run
/// it.
///
/// No longer constructs `Engine`/`ExecutionStore` itself (R4/D52): it
/// discovers or transparently launches a daemon
/// ([`crate::api::ensure_daemon_running`], AE2) and drives the rest of this
/// command entirely as that daemon's REST client — `POST /tasks`, then
/// blocking until the execution stops needing a driver
/// ([`wait_until_stopped`]).
///
/// The workflow reference is still resolved and its definition still loaded
/// locally first, exactly as before: that is what lets a missing/invalid
/// definition or a missing required input be reported before anything talks
/// to the daemon at all (unchanged from the pre-daemon behaviour). The
/// *reference string itself* — not the locally resolved name — is what goes
/// over the wire, so the daemon repeats exactly the same resolution
/// (`WorkflowResolver::resolve`) this process just did, including the
/// "an existing file wins" case for a bespoke local path.
pub async fn run(
    config: &Resolved,
    flags: &Flags,
    reference: &str,
    inputs: &[String],
) -> Result<()> {
    let catalog = config.catalog_dir();
    let resolved = resolve_workflow(catalog, reference)?;
    eprintln!("Workflow '{}' — {}.", resolved.name, resolved.origin);
    let definition = tasks::load_resolved(&resolved).map_err(|error| match error {
        TaskError::InvalidDefinition { violations } => {
            let mut message = String::from("The definition is invalid — no execution was created:");
            for violation in &violations {
                message.push_str(&format!("\n  - {violation}"));
            }
            CliError::failure(message)
        }
        other => CliError::failure(other.to_string()),
    })?;

    // Inputs first: a missing required parameter must be reported before
    // anything is written to disk.
    let inputs = collect_inputs(&definition, inputs)?;

    let client = daemon_client(config.state_dir(), flags).await?;
    let submission_id = format!("task-{}", uuid::Uuid::new_v4().simple());

    let started = submit_task(&client, &submission_id, reference, inputs)
        .await
        .map_err(|error| CliError::failure(format!("Could not start the workflow: {error}")))?;
    println!("Queued task: {}", started.task.task_id);
    let task_id = started.task.task_id.to_string();
    let final_task = wait_until_task_terminal(&client, &task_id).await?;
    if let Some(runner) = &final_task.runner {
        if let Some(execution_id) = runner.execution_cursor.execution_id.as_deref() {
            // A task may have been queued and provisioned before this point,
            // but once its runner projects a real execution the established
            // attached-run output (including a human-input form) remains
            // useful and is read through the backend-aware execution route.
            let summary = load_execution_summary(&client, execution_id).await?;
            print_open_matters(&summary);
        }
        println!("Status: {}", runner.execution_status.as_str());
        if matches!(
            runner.execution_status,
            ExecutionStatus::Failed | ExecutionStatus::Cancelled
        ) {
            return Err(CliError::failure(format!("Task {task_id} failed.")));
        }
    }
    Ok(())
}

/// Parses `--input` pairs against the declared parameters, fills in defaults and
/// reports everything that is missing.
fn collect_inputs(definition: &WorkflowDefinition, pairs: &[String]) -> Result<Map<String, Value>> {
    let mut values = Map::new();

    for pair in pairs {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid input '{pair}' — expected the form name=value."
            ))
        })?;

        let parameter = definition
            .inputs
            .iter()
            .find(|parameter| parameter.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Workflow '{}' declares no input named '{name}'. Declared inputs: {}.",
                    definition.name,
                    declared_input_names(definition)
                ))
            })?;

        values.insert(name.to_string(), parse_input_value(parameter, raw)?);
    }

    let mut missing = Vec::new();
    for parameter in &definition.inputs {
        if values.contains_key(&parameter.name) {
            continue;
        }
        if let Some(default) = &parameter.default {
            values.insert(parameter.name.clone(), default.clone());
        } else if parameter.required {
            missing.push(parameter.name.clone());
        }
    }

    if !missing.is_empty() {
        return Err(CliError::usage(format!(
            "Missing required inputs: {}. Pass them with --input name=value.",
            missing.join(", ")
        )));
    }

    Ok(values)
}

/// The declared input names, for an error message.
fn declared_input_names(definition: &WorkflowDefinition) -> String {
    if definition.inputs.is_empty() {
        return "(none)".to_string();
    }
    definition
        .inputs
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses one `--input` value according to the parameter's declared type.
///
/// This is why `--input is_experimental=true` reaches the context as a boolean
/// and not as the string `"true"`: the type comes from the definition, never
/// from guessing at the text.
fn parse_input_value(parameter: &InputParameter, raw: &str) -> Result<Value> {
    match parameter.param_type {
        ParamType::String | ParamType::Repository => Ok(Value::String(raw.to_string())),
        ParamType::Integer => raw.parse::<i64>().map(Value::from).map_err(|_| {
            CliError::usage(format!(
                "Input '{}' is of type integer, and '{raw}' is not a whole number.",
                parameter.name
            ))
        }),
        ParamType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(CliError::usage(format!(
                "Input '{}' is of type boolean — the allowed values are 'true' and 'false', got '{raw}'.",
                parameter.name
            ))),
        },
    }
}

// ---------------------------------------------------------------------------
// list / show / events
// ---------------------------------------------------------------------------

/// `narsor list` — every execution, newest first.
///
/// Goes through `ApiClient` (R4/D52): `GET /executions`, the same request
/// `ApiClient::list_executions` already makes for `find_execution`/the TUI's
/// own listing. Used to read `ExecutionStore` directly — the very
/// construction the daemon migration exists to keep out of this file (R1) —
/// left over from before this unit's own migration ever reached it.
pub async fn list(state: &Path, flags: &Flags) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let listing = client
        .list_executions()
        .await
        .map_err(|error| CliError::failure(format!("Could not read the executions: {error}")))?;

    if listing.executions.is_empty() {
        println!("No executions.");
        return Ok(());
    }

    // Newest first: the execution an operator just started is the one they
    // are looking for.
    let mut executions = listing.executions;
    executions.sort_by_key(|execution| std::cmp::Reverse(execution.started_at));

    println!("{:<38} {:<10} {:<22} WORKFLOW", "ID", "STATUS", "STATE");
    for execution in &executions {
        println!(
            "{:<38} {:<10} {:<22} {}",
            execution.id,
            execution.status.as_str(),
            execution.current_state.as_deref().unwrap_or("-"),
            execution.workflow_id
        );
    }
    Ok(())
}

/// `narsor show <id>` — the current state, the status and the context.
///
/// Goes through `ApiClient` (R4/D52), the same as [`list`] above: `GET
/// /executions` plus [`find_execution`] to pick out the one execution asked
/// for. There is no per-execution `GET`, only the listing — the same one the
/// TUI's own task view already relies on for exactly this (state, status,
/// context variables, pending question). One thing this can no longer show
/// that the old, direct-store version did: the workspace path, which
/// [`ExecutionSummary`] deliberately leaves out of the wire contract
/// (internal engine bookkeeping, not something a REST client needs).
pub async fn show(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let execution = load_execution_summary(&client, execution_id).await?;

    println!("Execution:     {}", execution.id);
    println!(
        "Workflow:      {} (version {})",
        execution.workflow_id, execution.workflow_version
    );
    println!("Status:        {}", execution.status.as_str());
    println!(
        "Current state: {}",
        execution.current_state.as_deref().unwrap_or("-")
    );
    println!("Started:       {}", execution.started_at.to_rfc3339());
    if let Some(completed_at) = execution.completed_at {
        println!("Finished:      {}", completed_at.to_rfc3339());
    }

    println!("Context variables:");
    if execution.variables.is_empty() {
        println!("  (none)");
    } else {
        let mut names: Vec<&String> = execution.variables.keys().collect();
        names.sort();
        for name in names {
            let value = &execution.variables[name];
            println!("  {name} = {value}");
        }
    }

    if let Some(pending) = &execution.pending_input {
        print_pending_input(pending);
    }
    print_conductor_matters(&execution);
    Ok(())
}

/// `narsor events <id> [--follow]` — the event log, in sequence order.
///
/// Reads through `ApiClient` (R4/D52) instead of `ExecutionStore` directly —
/// the same `GET /executions/:id/events` endpoint the TUI's task view uses —
/// so `after` is the highest sequence number already printed, not a count to
/// skip.
pub async fn events(state: &Path, flags: &Flags, execution_id: &str, follow: bool) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    // Fails early and readably when the id is unknown.
    let mut summary = load_execution_summary(&client, execution_id).await?;

    let mut after = 0u64;
    loop {
        let events = client
            .events(execution_id, after)
            .await
            .map_err(|error| client_error(execution_id, error))?;

        for event in &events {
            print_event(event);
        }
        if let Some(last) = events.last() {
            after = last.sequence;
        }

        if !follow || is_final(&summary.status) {
            break;
        }
        tokio::time::sleep(FOLLOW_INTERVAL).await;
        summary = load_execution_summary(&client, execution_id).await?;
    }
    Ok(())
}

/// One event line: sequence, timestamp, type, description.
fn print_event(event: &Event) {
    println!(
        "{:>5}  {}  {:<22} {}",
        event.sequence,
        event.timestamp.to_rfc3339(),
        event_type(event),
        event.description()
    );
}

/// The `type` tag serde writes for an event's data.
fn event_type(event: &Event) -> String {
    serde_json::to_value(&event.data)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// steps / conversation (U8)
// ---------------------------------------------------------------------------

/// `narsor steps <id>` — the step timeline, one line per attempt.
///
/// Reads through `ApiClient` (R4/D52) — `GET /executions/:id/timeline`,
/// which computes the same [`crate::engine::build_timeline`] fold server-side. The CLI and
/// the TUI's task view (U7) can therefore never compute a different answer
/// for the same execution: both are just callers of the one server-side
/// computation now.
pub async fn steps(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let timeline = client
        .timeline(execution_id)
        .await
        .map_err(|error| client_error(execution_id, error))?;

    if timeline.positions.is_empty() {
        println!("No steps.");
        return Ok(());
    }

    for position in &timeline.positions {
        print_position(position);
    }
    Ok(())
}

/// One position: label, status, action type, start/end, and — when they
/// apply — the error, the retry-pending and harness-interrupted flags, and
/// why the conversation cannot be read.
fn print_position(position: &Position) {
    println!(
        "{:<14} {:<18} {:<10} {}  ->  {}",
        position.label,
        position_status_name(position.status),
        position.action_type.as_deref().unwrap_or("-"),
        position
            .started_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".to_string()),
        position
            .ended_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".to_string()),
    );
    if let Some(error) = &position.error {
        println!("    error: {error}");
    }
    if position.retry_pending {
        println!("    waiting to retry");
    }
    if position.harness_session_interrupted {
        println!("    harness session interrupted");
    }
    if let Some(resumed) = &position.resumed_harness_session {
        println!("    resumed harness session: {resumed}");
    }
    match &position.conversation {
        ConversationState::Recorded => {
            if let Some(attempt_id) = &position.action_execution_id {
                println!("    attempt id: {attempt_id}");
            }
        }
        ConversationState::NotCreated { reason } => {
            println!("    conversation not created: {reason}")
        }
        ConversationState::NoRecord => println!("    no conversation record (old log)"),
    }
}

/// The schema spelling of a position's status.
fn position_status_name(status: PositionStatus) -> &'static str {
    match status {
        PositionStatus::Succeeded => "succeeded",
        PositionStatus::Failed => "failed",
        PositionStatus::Interrupted => "interrupted",
        PositionStatus::InProgress => "in_progress",
        PositionStatus::AwaitingDecision => "awaiting_decision",
    }
}

/// `narsor conversation <id> <attempt>` — one attempt's conversation, in
/// order, in full (R9: nothing is truncated).
///
/// Reads through `ApiClient` (R4/D52) — `GET
/// /executions/:id/attempts/:attempt/conversation` — with `byte_budget:
/// usize::MAX`, which the server-side read never actually reaches (an
/// attempt's conversation is nowhere near [`usize::MAX`] bytes), so this is
/// one request, not a paginated loop, exactly as the direct store read used
/// to be one call. Every entry is printed classified ([`classify`], reused
/// as-is from the TUI's task view, U7) for readability, but the classified
/// text is always the entry's full decoded content, never a summary — the
/// raw-and-complete requirement holds regardless of the kind sniffed from it.
pub async fn conversation(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    attempt_id: &str,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    load_execution_summary(&client, execution_id).await?;

    let response = client
        .conversation(execution_id, attempt_id, 1, usize::MAX)
        .await
        .map_err(|error| client_error(execution_id, error))?;

    match response {
        ConversationResponse::Recorded {
            entries,
            truncated_tail,
            ..
        } => {
            for entry in &entries {
                print_conversation_entry(entry);
            }
            if truncated_tail {
                println!(
                    "(the last line on disk was incomplete — the attempt was interrupted mid-write)"
                );
            }
            Ok(())
        }
        ConversationResponse::NoRecord => Err(CliError::failure(format!(
            "No conversation record for attempt '{attempt_id}' in execution '{execution_id}'."
        ))),
    }
}

/// One conversation entry: number, timestamp, source/kind, decoded content.
fn print_conversation_entry(entry: &crate::api::ConversationEntryDto) {
    let classified = classify(entry);
    let body = if matches!(
        entry.source,
        crate::persistence::conversation::ConversationSource::Narsor
    ) {
        String::from_utf8_lossy(&classified.raw).into_owned()
    } else {
        classified.summary
    };
    println!(
        "{:>5}  {}  {:<10} {}",
        classified.entry_number,
        classified.timestamp.to_rfc3339(),
        classified.kind.label(),
        body
    );
}

// ---------------------------------------------------------------------------
// input / resume
// ---------------------------------------------------------------------------

/// What `narsor input` was told on its command line.
#[derive(Debug, Clone, Default)]
pub struct InputAnswer {
    /// `--field name=value` pairs.
    pub fields: Vec<String>,
    /// `--label`.
    pub label: Option<String>,
    /// `--data name=value` pairs.
    pub data: Vec<String>,
    /// `--variable name=value` pairs.
    pub variables: Vec<String>,
    /// `--comment`.
    pub comment: Option<String>,
}

/// `narsor input <id> [--field k=v]… [--label l] [--data k=v]… [--variable k=v]…
/// [--comment text]` — answer a question, or resolve a problem.
///
/// Goes through `ApiClient` (R4/D52), exactly like [`resume`]: `POST
/// /executions/:id/input`, then [`wait_until_stopped`]. Used to construct its
/// own `Engine`/`ExecutionStore` and call `Engine::deliver_human_input`
/// directly in this process — a leftover from before the daemon migration
/// that raced the daemon's own writes to the same store files exactly the
/// way every other command here was rewritten specifically to avoid (R1).
///
/// `--file` has no REST equivalent, for the identical reason [`resume`]'s own
/// doc comment gives: the daemon always resolves the workflow from the
/// execution's own recorded metadata, with no way for a REST call to point
/// it at a local override path.
///
/// The answer is bound to the request this command read (KTD9): if another
/// client answered or closed it in between, the daemon refuses this one
/// rather than letting it land on whatever the execution waits on next.
pub async fn input(
    config: &Resolved,
    flags: &Flags,
    execution_id: &str,
    answer: InputAnswer,
    definition_file: Option<&str>,
) -> Result<()> {
    if let Some(file) = definition_file {
        return Err(CliError::usage(format!(
            "'--file {file}' is not supported for 'input': the daemon always resolves the \
             workflow from the execution's own record, with no way for a REST call to point it \
             at a local override path."
        )));
    }

    let mut values = Map::new();
    for pair in &answer.fields {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid field '{pair}' — expected the form name=value."
            ))
        })?;
        values.insert(name.to_string(), Value::String(raw.to_string()));
    }

    let mut delivery = HumanInputDelivery::with_values(values)
        .with_data(step_values("data", &answer.data)?)
        .with_variables(step_values("variable", &answer.variables)?);
    if let Some(label) = answer.label {
        delivery = delivery.choosing(label);
    }
    if let Some(comment) = answer.comment {
        delivery = delivery.with_comment(comment);
    }

    let client = daemon_client(config.state_dir(), flags).await?;

    // Checked here, not left to the server: an execution that is not
    // awaiting human input is a caller mistake, not an engine failure, and
    // deserves the same readable "not waiting (status: X)" message this
    // command always gave — `EngineError`'s mapping to `ApiError` (U3) does
    // not single this case out from every other "engine can't proceed"
    // reason, so relying on the REST error alone would only report a bare
    // `500`.
    let current = load_execution_summary(&client, execution_id).await?;
    let Some(pending) = &current.pending_input else {
        return Err(CliError::failure(format!(
            "Execution '{execution_id}' is not waiting for human input (status: {}).",
            current.status.as_str()
        )));
    };
    let delivery = delivery.for_episode(pending.episode_key());

    client
        .deliver_input(execution_id, delivery)
        .await
        .map_err(|error| match error {
            ClientError::Conflict => CliError::failure(format!(
                "Execution '{execution_id}' is no longer waiting for the input this answer was \
                 for: it was answered or closed meanwhile, or another process is driving it \
                 right now. Check it with 'narsor show {execution_id}'."
            )),
            other => client_error(execution_id, other),
        })?;

    let summary = wait_until_stopped(&client, execution_id, current.status.clone()).await?;

    println!("Input delivered to execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            summary.id
        )));
    }
    Ok(())
}

/// Parses `name=value` pairs of `--data`/`--variable` into a map. A value
/// that reads as JSON is taken as that, so `units_completed=3` is a number;
/// anything else is the text as written.
fn step_values(flag: &str, pairs: &[String]) -> Result<Map<String, Value>> {
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

/// `narsor resume <id>` — continue a paused execution.
///
/// Goes through `ApiClient` (R4/D52): `POST /executions/:id/resume`, then
/// [`wait_until_stopped`], exactly like [`run`]'s own tail end. A `409` from
/// the daemon — another driver (the TUI, another `resume`/`run`) already
/// holds this execution (R11/AE4) — is reported as a readable sentence via
/// [`client_error`], never a raw HTTP status.
///
/// `--file` has no REST equivalent: the daemon always resolves the workflow
/// from the execution's own recorded metadata
/// (`tasks::resolve_definition`), the same way `resume` without `--file`
/// already did — there is no way for a REST caller to point the daemon at a
/// local override path it has no filesystem relationship to (see
/// `src/api/routes.rs`'s `load_definition_for`). Passing `--file` is
/// refused with a usage error rather than silently ignored, so an operator
/// relying on it to work around a moved definition is told plainly, not left
/// wondering why it had no effect.
pub async fn resume(
    config: &Resolved,
    flags: &Flags,
    execution_id: &str,
    definition_file: Option<&str>,
) -> Result<()> {
    if let Some(file) = definition_file {
        return Err(CliError::usage(format!(
            "'--file {file}' is not supported for 'resume': the daemon always resolves the \
             workflow from the execution's own record, with no way for a REST call to point it \
             at a local override path."
        )));
    }

    let client = daemon_client(config.state_dir(), flags).await?;

    client
        .resume(execution_id)
        .await
        .map_err(|error| client_error(execution_id, error))?;

    let summary = wait_until_stopped(&client, execution_id, ExecutionStatus::Running).await?;

    println!("Resumed execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            summary.id
        )));
    }
    Ok(())
}

/// `narsor recover <id> [--comment TEXT]` — starts the failed step of an
/// execution afresh (`POST /executions/:id/recover`), the CLI counterpart of
/// the TUI's failure form.
///
/// Bound to the failure the execution is failed on, like `input` is bound
/// to the input it answers: the key is taken from the summary's problem, so
/// a recovery that comes after somebody else acted on the failure (another
/// window, the conductor's own retry) is refused with a `409` — said here as
/// a sentence — instead of starting the step a second time. An execution
/// failed without such a problem (its failure still being diagnosed, or
/// left `unknown` by a restart) is recovered without a key.
pub async fn recover(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    comment: Option<String>,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let current = load_execution_summary(&client, execution_id).await?;
    if !matches!(
        current.status,
        ExecutionStatus::Failed | ExecutionStatus::Unknown
    ) {
        return Err(CliError::failure(format!(
            "Execution '{execution_id}' has no failed step to recover (status: {}).",
            current.status.as_str()
        )));
    }
    let failure = current
        .problem
        .as_ref()
        .filter(|problem| problem.kind == ProblemKind::Failure)
        .and_then(|problem| problem.episode.clone());

    let accepted = match &failure {
        Some(episode) => client.recover_failure(execution_id, episode, comment).await,
        None => client.recover(execution_id, comment).await,
    };
    accepted.map_err(|error| match error {
        ClientError::Conflict => CliError::failure(format!(
            "Execution '{execution_id}' is no longer failed on the failure this recovery was \
             for: it was recovered or changed meanwhile, or another process is driving it right \
             now. Check it with 'narsor show {execution_id}'."
        )),
        other => client_error(execution_id, other),
    })?;

    let summary = wait_until_stopped(&client, execution_id, ExecutionStatus::Running).await?;

    println!("Recovered execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            summary.id
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// repo
// ---------------------------------------------------------------------------

/// How long `conductor chat` waits for the conductor's answer.
const CHAT_REPLY_TIMEOUT: Duration = Duration::from_secs(600);

/// How often it looks for the answer meanwhile.
const CHAT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// `narsor conductor chat <id> "<message>"` — writes to the conductor of an
/// execution (F4) and, unless told not to wait, prints its answer and the
/// actions it proposes. The message goes into the same history the TUI and
/// the API read.
pub async fn conductor_chat(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    message: &str,
    wait: bool,
) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        return Err(CliError::usage("A chat message cannot be empty."));
    }
    let client = daemon_client(state, flags).await?;
    let posted = client
        .post_chat(execution_id, message)
        .await
        .map_err(|error| client_error(execution_id, error))?;
    print_chat_entry(&posted);
    if !wait {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + CHAT_REPLY_TIMEOUT;
    let mut after = posted.sequence;
    let mut answered = false;
    loop {
        let entries = client
            .conductor_chat(execution_id, after)
            .await
            .map_err(|error| client_error(execution_id, error))?
            .entries;
        for entry in &entries {
            print_chat_entry(entry);
            after = entry.sequence;
            answered |= entry.in_reply_to.as_deref() == Some(posted.id.as_str());
        }
        // The answer and its proposals are written together, so once the
        // answer is there, so is everything that goes with it.
        if answered {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::failure(format!(
                "The conductor has not answered yet; see its answer later with \
                 'narsor conductor history {execution_id}'."
            )));
        }
        tokio::time::sleep(CHAT_POLL_INTERVAL).await;
    }
}

/// `narsor conductor history <id>` — the whole chat of an execution.
pub async fn conductor_history(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let entries = client
        .conductor_chat(execution_id, 0)
        .await
        .map_err(|error| client_error(execution_id, error))?
        .entries;
    if entries.is_empty() {
        println!("The conductor's chat of execution {execution_id} is empty.");
    }
    for entry in &entries {
        print_chat_entry(entry);
    }
    Ok(())
}

/// `narsor conductor confirm|reject <id> <proposal>` — settles a proposal of
/// the conductor's (R28). `<proposal>` is its id or its number in the chat.
pub async fn conductor_settle(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    proposal: &str,
    confirm: bool,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let proposal_id = chat_entry_id(&client, execution_id, proposal, ChatEntryKind::Proposal)
        .await?
        .unwrap_or_else(|| proposal.to_string());
    let outcome = if confirm {
        client.confirm_proposal(execution_id, &proposal_id).await
    } else {
        client.reject_proposal(execution_id, &proposal_id).await
    }
    .map_err(|error| match error {
        ClientError::NotFound => CliError::failure(format!(
            "Execution '{execution_id}' has no proposal '{proposal}'."
        )),
        other => client_error(execution_id, other),
    })?;

    println!("{}", outcome.message);
    if outcome.repeated {
        println!("(It had already been settled; nothing was done now.)");
    }
    Ok(())
}

/// `narsor conductor undo <id> <decision>` — undoes the conductor's choice
/// of a step's output (KTD13). `<decision>` is its id or the number of its
/// entry in the chat.
pub async fn conductor_undo(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    decision: &str,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let decision_id = match decision.parse::<u64>() {
        Ok(sequence) => {
            let entries = client
                .conductor_chat(execution_id, sequence.saturating_sub(1))
                .await
                .map_err(|error| client_error(execution_id, error))?
                .entries;
            entries
                .iter()
                .find(|entry| entry.sequence == sequence && entry.kind == ChatEntryKind::Decision)
                .and_then(|entry| entry.payload.as_ref()?.get("decision_id")?.as_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| decision.to_string())
        }
        Err(_) => decision.to_string(),
    };
    let outcome = client
        .undo_decision(execution_id, &decision_id)
        .await
        .map_err(|error| match error {
            ClientError::NotFound => CliError::failure(format!(
                "Execution '{execution_id}' has no decision '{decision}' of the conductor's."
            )),
            other => client_error(execution_id, other),
        })?;
    println!("{}", outcome.message);
    if let Some(warning) = &outcome.warning {
        eprintln!("Warning: {warning}");
    }
    Ok(())
}

/// `narsor conductor status <id>` — what the conductor of an execution is
/// doing and what it kept (`GET /executions/:id/conductor`), as the TUI's
/// conductor pane reads it.
pub async fn conductor_status(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let conductor = client
        .conductor_state(execution_id)
        .await
        .map_err(|error| client_error(execution_id, error))?;

    match &conductor.pending {
        Some(pending) => println!(
            "Working on:    {} ({}, since {}){}",
            pending.request.purpose.name(),
            pending.request.trigger.name(),
            pending.started_at.to_rfc3339(),
            episode_suffix(pending.request.episode.as_deref())
        ),
        None => println!("Working on:    nothing"),
    }
    println!("Queued:        {}", conductor.queue.len());
    for queued in &conductor.queue {
        println!(
            "  {} ({}){}",
            queued.purpose.name(),
            queued.trigger.name(),
            episode_suffix(queued.episode.as_deref())
        );
    }
    println!("Unanswered:    {}", conductor.unanswered_chat.len());
    for (episode, attempts) in &conductor.invocation_attempts {
        println!("Attempts:      {attempts} for {episode}");
    }
    for episode in &conductor.used_retries {
        println!("Retry used:    {episode}");
    }
    if !conductor.unapplied_decisions.is_empty() {
        println!(
            "Unapplied:     {} decision(s)",
            conductor.unapplied_decisions.len()
        );
    }
    if let Some(failure) = &conductor.failure {
        println!(
            "Last failure:  {} at '{}': {}",
            failure.key, failure.state_id, failure.reason
        );
    }
    Ok(())
}

/// ` for <episode>`, or nothing.
fn episode_suffix(episode: Option<&str>) -> String {
    episode
        .map(|episode| format!(" for {episode}"))
        .unwrap_or_default()
}

/// The id of the `kind` entry numbered `reference` in the chat, when
/// `reference` is a number; `None` when it is not (it is then an id itself).
async fn chat_entry_id(
    client: &ApiClient,
    execution_id: &str,
    reference: &str,
    kind: ChatEntryKind,
) -> Result<Option<String>> {
    let Ok(sequence) = reference.parse::<u64>() else {
        return Ok(None);
    };
    let entries = client
        .conductor_chat(execution_id, sequence.saturating_sub(1))
        .await
        .map_err(|error| client_error(execution_id, error))?
        .entries;
    Ok(entries
        .into_iter()
        .find(|entry| entry.sequence == sequence && entry.kind == kind)
        .map(|entry| entry.id))
}

/// One chat entry: its number, who wrote it and what it is, its text, and
/// — for a proposal or a decision — the id to act on it by.
fn print_chat_entry(entry: &ChatEntry) {
    let who = match entry.author {
        ChatAuthor::Operator => "operator",
        ChatAuthor::Conductor => "conductor",
    };
    let what = match entry.kind {
        ChatEntryKind::Message => "",
        ChatEntryKind::Error => " (error)",
        ChatEntryKind::Decision => " (decision)",
        ChatEntryKind::Proposal => " (proposal)",
    };
    println!("#{:<4} {who}{what}: {}", entry.sequence, entry.text);
    match entry.kind {
        ChatEntryKind::Proposal => println!("      proposal id: {}", entry.id),
        ChatEntryKind::Decision => {
            if let Some(id) = entry
                .payload
                .as_ref()
                .and_then(|payload| payload.get("decision_id"))
                .and_then(Value::as_str)
            {
                println!("      decision id: {id}");
            }
        }
        ChatEntryKind::Message | ChatEntryKind::Error => {}
    }
}

/// `narsor repo add <id> --remote <url|path> …` — register a repository.
///
/// The remote is checked with the workspace provisioner's own rules, so a
/// remote the registry accepts is one a workflow will be able to clone: a value
/// starting with `-` or using the `ext::` transport is refused here rather than
/// halfway through a run.
pub fn repo_add(
    catalog: &Path,
    id: &str,
    remote: &str,
    name: Option<String>,
    provider: Option<String>,
    default_branch: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let mut entry = RepositoryEntry::new(id, remote)
        .with_name(name.unwrap_or_else(|| id.to_string()))
        .with_provider(provider.unwrap_or_else(|| infer_provider(remote).to_string()));
    entry.default_branch = default_branch;
    entry.description = description;

    let registry = RepositoryRegistry::new(catalog);
    registry.add(entry).map_err(|error| {
        CliError::failure(format!("Could not register the repository: {error}"))
    })?;

    println!("Registered repository '{id}' -> {remote}");
    Ok(())
}

/// `narsor repo list` — every registered repository, ordered by id.
///
/// A file that cannot be read as an entry does not hide the ones that can: the
/// listing prints what loaded, names what did not, and *then* fails. The
/// pattern is `config show`'s — a diagnostic that goes silent in the situation
/// it exists for is no diagnostic — and the exit code is what keeps a script
/// branching on it from being misled.
pub fn repo_list(catalog: &Path) -> Result<()> {
    let listing = RepositoryRegistry::new(catalog)
        .list()
        .map_err(|error| CliError::failure(format!("Could not read the registry: {error}")))?;

    if listing.is_empty() {
        println!("No repositories registered.");
        return Ok(());
    }

    if !listing.entries.is_empty() {
        println!(
            "{:<24} {:<10} {:<16} REMOTE",
            "ID", "PROVIDER", "DEFAULT BRANCH"
        );
        for entry in &listing.entries {
            println!(
                "{:<24} {:<10} {:<16} {}",
                entry.id,
                entry.provider,
                entry.default_branch.as_deref().unwrap_or("-"),
                entry.remote
            );
        }
    }

    if listing.is_ok() {
        return Ok(());
    }

    for failure in &listing.failures {
        eprintln!("{}: {}", failure.path.display(), failure.reason);
    }
    Err(CliError::failure(format!(
        "{} registry entr(y/ies) could not be read; they are named above.",
        listing.failures.len()
    )))
}

/// `narsor repo show <id>` — every field of one registered repository.
pub fn repo_show(catalog: &Path, id: &str) -> Result<()> {
    let registry = RepositoryRegistry::new(catalog);
    let entry = registry
        .get(id)
        .map_err(|error| CliError::failure(format!("Could not read the registry: {error}")))?
        .ok_or_else(|| {
            CliError::failure(format!(
                "Repository '{id}' is not registered in '{}'.",
                registry.dir().display()
            ))
        })?;

    println!("Repository:     {}", entry.id);
    println!("Name:           {}", entry.name);
    println!("Provider:       {}", entry.provider);
    println!("Remote:         {}", entry.remote);
    println!(
        "Default branch: {}",
        entry.default_branch.as_deref().unwrap_or("-")
    );
    println!(
        "Description:    {}",
        entry.description.as_deref().unwrap_or("-")
    );
    Ok(())
}

/// `narsor repo remove <id>` — drop a repository from the registry.
pub fn repo_remove(catalog: &Path, id: &str) -> Result<()> {
    RepositoryRegistry::new(catalog)
        .remove(id)
        .map_err(|error| CliError::failure(format!("Could not remove the repository: {error}")))?;

    println!("Removed repository '{id}'.");
    Ok(())
}

// ---------------------------------------------------------------------------
// workflow
// ---------------------------------------------------------------------------

/// `narsor workflow list` — every workflow reachable right now.
///
/// One line per *name*: where the copy that wins comes from, what it overrides
/// when it overrides something, and its `description`. A user copy shadowing a
/// built-in is one row marked as an override, never two rows that leave the
/// operator guessing which one would run.
pub fn workflow_list(catalog: &Path) -> Result<()> {
    let resolver = WorkflowResolver::new(catalog);
    let entries = resolver.list();

    if entries.is_empty() {
        println!("No workflows are reachable.");
        return Ok(());
    }

    println!("{:<28} {:<10} DESCRIPTION", "NAME", "SOURCE");
    for entry in &entries {
        let description = entry.description.as_deref().unwrap_or("-");
        println!(
            "{:<28} {:<10} {}",
            entry.name,
            entry.origin.label(),
            truncate(description, 80)
        );
        if entry.is_override() {
            println!("{:<28} {:<10} overrides: {}", "", "", entry.shadow_labels());
        }
        if let Some(path) = entry.origin.path() {
            println!("{:<28} {:<10} path: {}", "", "", path.display());
        }
    }
    Ok(())
}

/// `narsor workflow show <name>` — the YAML that would run.
///
/// The definition goes to stdout and nothing else does, so
/// `narsor workflow show compound-engineering > mine.yaml` is a copy;
/// where it came from is said on stderr.
pub fn workflow_show(catalog: &Path, reference: &str) -> Result<()> {
    let resolved = resolve_workflow(catalog, reference)?;
    eprintln!("Workflow '{}' — {}.", resolved.name, resolved.origin);
    print!("{}", resolved.source);
    if !resolved.source.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `narsor workflow eject <name> [--to <dir>] [--force]` — write a
/// built-in out as a file.
///
/// The default target is the user directory, which the resolver searches ahead
/// of the built-ins: the ejected copy takes precedence the moment it is
/// written, which is the whole point of ejecting one.
pub fn workflow_eject(catalog: &Path, name: &str, to: Option<&Path>, force: bool) -> Result<()> {
    let builtin = builtin(name).ok_or_else(|| {
        CliError::usage(format!(
            "There is no built-in workflow named '{}'. Available built-in workflows: {}.",
            crate::workflows::workflow_name(name),
            builtin_names().join(", ")
        ))
    })?;

    let resolver = WorkflowResolver::new(catalog);
    let directory = match to {
        Some(directory) => directory.to_path_buf(),
        None => resolver.user_dir().map(Path::to_path_buf).ok_or_else(|| {
            CliError::usage(
                "There is no user configuration directory (neither XDG_CONFIG_HOME nor HOME is \
                 set) — name a directory with --to."
                    .to_string(),
            )
        })?,
    };

    std::fs::create_dir_all(&directory).map_err(|error| {
        CliError::failure(format!(
            "Could not create '{}': {error}",
            directory.display()
        ))
    })?;

    let target = directory.join(format!("{}.yaml", builtin.name));
    if target.exists() && !force {
        return Err(CliError::failure(format!(
            "'{}' already exists — pass --force to overwrite it.",
            target.display()
        )));
    }

    std::fs::write(&target, builtin.source).map_err(|error| {
        CliError::failure(format!("Could not write '{}': {error}", target.display()))
    })?;

    println!("Wrote {}", target.display());
    match resolver.resolve_name(builtin.name) {
        Ok(resolved) if resolved.path() == Some(target.as_path()) => println!(
            "'{}' now resolves to this copy instead of the built-in.",
            builtin.name
        ),
        Ok(resolved) => println!(
            "Note: '{}' still resolves to {} — that copy has precedence over this one.",
            builtin.name, resolved.origin
        ),
        Err(_) => println!(
            "Note: '{}' is not on the search path from this data directory; \
             run it by path, or point --root at the directory holding it.",
            builtin.name
        ),
    }
    Ok(())
}

/// Shortens a description to one line of a listing.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

// ---------------------------------------------------------------------------
// tui (U10)
// ---------------------------------------------------------------------------

/// `narsor tui` — the full-screen interface, added in U10.
///
/// The loop is async but this entry point is not, because the subcommand
/// dispatch calls it from inside the runtime `main` already started. So the
/// current worker thread is handed over with `block_in_place` and the loop is
/// driven on it: spawning a second runtime inside the first would panic, and
/// the interface has to own a thread anyway — it blocks on keys.
///
/// No longer constructs `Engine`/`ExecutionStore` itself (R4/U7): like every
/// other command in this module, it discovers or transparently launches a
/// daemon (`crate::tui::run`'s own `ensure_daemon_running` call, AE2) and
/// drives the whole interface as that daemon's REST(+SSE) client. The three
/// roots are still opened and handed to the interface separately — the state
/// root (now just `StatePaths`, path arithmetic only — no store is opened
/// here) decides which daemon this discovers/launches, the catalogue is
/// where workflows and the registry are read from, and the work tree is
/// where the interface's own reads (`resolve_definition`) look for a pinned
/// definition copy.
pub fn tui(config: &Resolved, flags: &Flags) -> Result<()> {
    let state = StatePaths::new(config.state_dir());
    let handle = tokio::runtime::Handle::current();
    let catalog = config.catalog_paths();
    let work = config.work_paths();
    let refresh_interval = config
        .tui_refresh_interval()
        .map(std::time::Duration::from_secs);
    tokio::task::block_in_place(move || {
        handle.block_on(crate::tui::run(
            state,
            flags.clone(),
            catalog,
            work,
            refresh_interval,
        ))
    })
    .map_err(|error| {
        CliError::failure(format!(
            "The terminal interface stopped with an error: {error}"
        ))
    })
}

/// `narsor tui --remote` (or `narsor --server <url> tui`) — the remote
/// tasks mode ([`crate::tui::remote`]) over the control plane `target`
/// names, or over the local daemon's control plane without one.
pub fn tui_remote(
    config: &Resolved,
    flags: &Flags,
    target: &crate::api::ServerTarget,
) -> Result<()> {
    let state = StatePaths::new(config.state_dir());
    let handle = tokio::runtime::Handle::current();
    let refresh_interval = config
        .tui_refresh_interval()
        .map(std::time::Duration::from_secs);
    tokio::task::block_in_place(move || {
        handle.block_on(async {
            let client = crate::api::connect(target, &state, flags)
                .await
                .map_err(CliError::failure)?;
            crate::tui::run_remote(client, refresh_interval)
                .await
                .map_err(|error| {
                    CliError::failure(format!(
                        "The terminal interface stopped with an error: {error}"
                    ))
                })
        })
    })
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Resolves a workflow reference, turning a miss into the message that names
/// every place that was tried.
fn resolve_workflow(catalog: &Path, reference: &str) -> Result<ResolvedWorkflow> {
    WorkflowResolver::new(catalog)
        .resolve(reference)
        .map_err(|error| CliError::usage(error.to_string()))
}

/// The REST counterpart of the old, in-process
/// `print_execution_summary(&WorkflowExecution)`: same two lines, from
/// an [`ExecutionSummary`] the daemon answered with instead of a
/// [`WorkflowExecution`] read straight off the store.
fn print_summary(summary: &ExecutionSummary) {
    print_status_line(&summary.status, summary.current_state.as_deref());
}

/// The two lines both of the above print, factored out so `run`/`resume`
/// print byte-for-byte what they always did regardless of which of the two
/// shapes above they are holding.
fn print_status_line(status: &ExecutionStatus, current_state: Option<&str>) {
    println!("Status: {}", status.as_str());
    println!("Current state: {}", current_state.unwrap_or("-"));
}

// ---------------------------------------------------------------------------
// daemon client (U6)
// ---------------------------------------------------------------------------

/// Discovers a running daemon for `state`'s state root, or transparently
/// launches one (AE2) — the one entry point every command above goes
/// through instead of constructing `Engine`/`ExecutionStore` itself (R4).
async fn daemon_client(state: &Path, flags: &Flags) -> Result<ApiClient> {
    let paths = StatePaths::new(state);
    crate::api::ensure_daemon_running(&paths, flags)
        .await
        .map_err(|error| CliError::failure(format!("Could not reach the narsor daemon: {error}")))
}

/// Turns a [`ClientError`] into the message an operator sees, naming the
/// execution the call was about.
///
/// [`ClientError::Conflict`] (a `409`) is the one case this unit's test
/// scenarios call out by name: another driver — another `resume`/`run`, or
/// the TUI once U7 migrates it — already holds this execution (R11/AE4).
/// Reported as a plain sentence, never the raw `409` or a JSON body, exactly
/// as every other variant below already is.
fn client_error(execution_id: &str, error: ClientError) -> CliError {
    match error {
        ClientError::Conflict => CliError::failure(format!(
            "Execution '{execution_id}' is already being driven by another process right now \
             (another 'run'/'resume', or the TUI) — try again once it settles."
        )),
        ClientError::NotFound => CliError::failure(format!(
            "Could not load execution '{execution_id}': not found"
        )),
        ClientError::Unauthorized => {
            CliError::failure("The narsor daemon rejected the request: bad token".to_string())
        }
        error @ (ClientError::Forbidden | ClientError::Credential(_)) => {
            CliError::failure(error.to_string())
        }
        ClientError::Unavailable(detail) => {
            CliError::failure(format!("Could not reach the narsor daemon: {detail}"))
        }
        ClientError::Request { message, .. } | ClientError::Refused { message, .. } => {
            CliError::failure(message)
        }
        ClientError::Server(detail) => {
            CliError::failure(format!("The narsor daemon reported an error: {detail}"))
        }
    }
}

/// `execution_id`'s summary, or `None` if the daemon does not know it —
/// `GET /executions`, filtered client-side (there is no single-execution GET
/// endpoint; every existing caller of that list is small enough that this is
/// not worth adding one for).
async fn find_execution(
    client: &ApiClient,
    execution_id: &str,
) -> Result<Option<ExecutionSummary>> {
    let listing = client
        .list_executions()
        .await
        .map_err(|error| client_error(execution_id, error))?;
    Ok(listing
        .executions
        .into_iter()
        .find(|execution| execution.id == execution_id))
}

/// [`find_execution`], turning "not found" into a readable message.
async fn load_execution_summary(
    client: &ApiClient,
    execution_id: &str,
) -> Result<ExecutionSummary> {
    find_execution(client, execution_id).await?.ok_or_else(|| {
        CliError::failure(format!(
            "Could not load execution '{execution_id}': not found"
        ))
    })
}

/// How often [`wait_until_stopped`] asks the daemon directly whether
/// `execution_id` is still `Running`, as a bound on how long it can ever be
/// waiting on an SSE event that will never come — see that function's doc
/// comment for why that bound is needed at all, not just an optimisation.
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Blocks until `execution_id` is no longer `Running`, returning its final
/// summary. This is the shared tail of [`run`] and [`resume`]: both start a
/// pass driving an execution over REST and then need to wait, exactly as
/// they always blocked on `Engine::run`/`Engine::resume` directly before
/// this unit.
///
/// # Why this polls `GET /executions` at all, rather than only watching SSE
///
/// The obvious shape — drain `client.stream_execution(execution_id)` until
/// it ends itself on a terminal event — has a real race this command cannot
/// let an operator hit: `POST /tasks`/`POST /executions/:id/resume` both
/// answer `202` the instant the execution is created/claimed and hand the
/// actual run to a background `tokio::spawn`, which can start — and, for a
/// trivial workflow, finish — before this call's *next* HTTP request (the
/// SSE connection) ever reaches the daemon. `Engine::subscribe()`'s
/// broadcast channel has no replay: a subscriber only ever sees events
/// published after it connects, so a stream that connects after the
/// execution already settled would wait forever for an event that was
/// already sent. Also, the two ways a workflow parks (an operator pause —
/// `WorkflowPaused` — or a question raised — `HumanInputRequested`, from
/// `human::suspend`) are not among the three events
/// [`ApiClient::stream_execution`] treats as terminal, since that contract
/// was written for the workflow-*finishes* case (AE5) alone.
///
/// So the SSE stream here is only ever a low-latency wake-up: every loop
/// iteration checks the authoritative status directly first and returns the
/// moment it is not `Running`, and only *then* waits (bounded by
/// [`STATUS_POLL_INTERVAL`]) for the stream to say something, so as not to
/// busy-poll. Once the stream ends — on its own terminal event, or because
/// the connection dropped — this falls back to a plain, bounded sleep for
/// the rest of the wait. Either way, the wait can never exceed
/// [`STATUS_POLL_INTERVAL`] past the moment the execution actually stopped.
async fn wait_until_stopped(
    client: &ApiClient,
    execution_id: &str,
    before: ExecutionStatus,
) -> Result<ExecutionSummary> {
    let mut stream = Some(Box::pin(client.stream_execution(execution_id)));
    loop {
        if let Some(summary) = find_execution(client, execution_id).await? {
            if summary.status != ExecutionStatus::Running && summary.status != before {
                return Ok(summary);
            }
        }

        match &mut stream {
            // Wait for the stream to say *something* — any event at all, not
            // only a stopping one — or for the bound to elapse; either way,
            // loop back around to the authoritative status check above.
            // `Ok(None)` means the stream ended on its own (its terminal
            // event, or the connection dropping): stop polling it, a
            // finished stream would otherwise just return `None` forever.
            Some(active) => {
                if let Ok(None) = tokio::time::timeout(STATUS_POLL_INTERVAL, active.next()).await {
                    stream = None;
                }
            }
            None => tokio::time::sleep(STATUS_POLL_INTERVAL).await,
        }
    }
}

/// How many times an uncertain submission is repeated before giving up.
const SUBMIT_ATTEMPTS: u32 = 3;

/// Submits a task, repeating the identical request when the outcome is
/// uncertain.
///
/// A transport failure says nothing about whether the daemon recorded the
/// task. The id and body are kept byte-equivalent across attempts, so a
/// repeat returns the already-queued task instead of creating another.
async fn submit_task(
    client: &ApiClient,
    task_id: &str,
    workflow: &str,
    inputs: Map<String, Value>,
) -> std::result::Result<crate::api::StartTaskResponse, ClientError> {
    let mut attempt = 1;
    loop {
        match client.start_task(task_id, workflow, inputs.clone()).await {
            Err(ClientError::Unavailable(_)) if attempt < SUBMIT_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(STATUS_POLL_INTERVAL).await;
            }
            outcome => return outcome,
        }
    }
}

/// Waits on the task identity returned by local creation.  A task can be
/// queued or provisioning before a runner has an execution at all, so the
/// legacy execution stream is not a valid acknowledgement boundary here.
async fn wait_until_task_terminal(
    client: &ApiClient,
    task_id: &str,
) -> Result<crate::controller_protocol::TaskSummary> {
    let mut announced_execution = false;
    loop {
        let task = client.control_task(task_id).await.map_err(|error| {
            CliError::failure(format!("Could not load task '{task_id}': {error}"))
        })?;
        if let Some(runner) = &task.runner {
            if !announced_execution {
                if let Some(execution_id) = runner.execution_cursor.execution_id.as_deref() {
                    // Retain the long-standing machine-readable hand-off
                    // line, but only once the task projection names a real
                    // execution. Creation itself remains task-first.
                    println!("Created execution: {execution_id}");
                    announced_execution = true;
                }
            }
            if let Some(problem) = &runner.problem {
                return Err(CliError::failure(format!(
                    "Task {task_id} failed: {}",
                    problem.message
                )));
            }
            // `run` has always returned when a real execution reaches an
            // operator-visible stopping point (not only when it is terminal).
            // Before task-first admission that fell out of the in-process
            // engine pass; preserve it once the runner projection exists.
            // A pre-execution provisioning failure is handled above: its
            // honest `Unknown` snapshot must be a nonzero outcome, not an
            // apparently successful stop.
            if !matches!(runner.execution_status, ExecutionStatus::Running) {
                return Ok(task);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Prints the pending question's full form ([`print_pending_input`]) —
/// fields, types, allowed values, output labels, not just its instruction
/// text — that [`run`]/[`resume`]/[`input`]/[`recover`] always printed when
/// a pass stops on a question, if `summary` says the execution is
/// `InputNeeded` right now; then what the conductor adds to it
/// ([`print_conductor_matters`]).
///
/// `ExecutionSummary::pending_input` (added to `GET /executions` while
/// migrating the TUI off `Arc<Engine>`, U7) already carries this whole form
/// — an earlier version of this function predated that field and had to
/// settle for re-reading the event log just to recover the bare instruction
/// text from `HumanInputRequested`, the one piece it could get any other
/// way. Callers already fetch `summary` from `GET /executions` before
/// calling this, so reusing it here costs nothing further. An
/// operator-requested pause (`WorkflowPaused`, `Engine::pause_now`) has no
/// pending question and `pending_input` is `None` for it, matching
/// `engine.pending_input()` returning `None` for that case before this
/// unit.
fn print_open_matters(summary: &ExecutionSummary) {
    if summary.status == ExecutionStatus::InputNeeded {
        if let Some(pending) = &summary.pending_input {
            print_pending_input(pending);
        }
    }
    print_conductor_matters(summary);
}

/// What the conductor adds to `summary`, as the TUI shows it: the failure
/// a `Failed` execution is a problem on (F3), the summary of the gate it
/// waits on (R30), and what the conductor is working on right now (R11).
///
/// A problem input ([`ProblemKind::Output`]) is not repeated here: the
/// pending input's form already says it.
fn print_conductor_matters(summary: &ExecutionSummary) {
    if let Some(problem) = summary
        .problem
        .as_ref()
        .filter(|problem| problem.kind == ProblemKind::Failure)
    {
        println!("Problem ({}): {}", problem.kind.name(), problem.reason);
        if let Some(episode) = &problem.episode {
            println!("  Failure: {episode}");
        }
        println!(
            "  Recover with: narsor recover {} [--comment TEXT]",
            summary.id
        );
    }
    if let Some(annotation) = &summary.annotation {
        println!("Conductor's summary: {}", annotation.summary);
        if let Some(recommendation) = &annotation.recommendation {
            println!("  Recommends: {recommendation}");
        }
    }
    if let Some(activity) = &summary.conductor {
        println!(
            "Conductor working: {} ({}, since {})",
            activity.purpose.name(),
            activity.trigger.name(),
            activity.started_at.to_rfc3339()
        );
    }
}

/// The question an execution is waiting on, as a form the operator can fill in.
fn print_pending_input(pending: &PendingHumanInput) {
    println!("Human input expected: {}", pending.instruction());
    for field in pending.fields() {
        let requirement = if field.required {
            "required"
        } else {
            "optional"
        };
        let options = if field.options.is_empty() {
            String::new()
        } else {
            format!(", allowed values: {}", field.options.join(", "))
        };
        println!(
            "  --field {}=…  ({:?}, {requirement}{options})",
            field.name, field.field_type
        );
    }
    let Some(problem) = &pending.problem else {
        if !pending.outputs.is_empty() {
            println!("  Output labels: {}", pending.output_labels().join(", "));
        }
        return;
    };

    // A problem: the step's output is what is asked for (KTD3).
    println!("  Problem: {}", problem.reason);
    for answer in &problem.answers {
        if let Some(tail) = &answer.answer_tail {
            println!("  Agent answered ({}):", answer.action_execution_id);
            for line in tail.lines() {
                println!("    | {line}");
            }
        }
    }
    println!("  Resolve with --label and the keys that label needs:");
    for output in &pending.outputs {
        let description = output
            .description
            .as_deref()
            .map(|description| format!("  — {description}"))
            .unwrap_or_default();
        println!("    --label {}{description}", output.label);
        if let Some(requirement) = problem.requirement_for(&output.label) {
            for key in &requirement.data {
                println!("        requires --data {key}=…");
            }
            for key in &requirement.variables {
                println!("        requires --variable {key}=…");
            }
        }
    }
}

/// Whether an execution can still move.
fn is_final(status: &ExecutionStatus) -> bool {
    matches!(
        status,
        ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
    )
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// `narsor config show [--format json]` — every setting, its value and its
/// source.
///
/// The complete set, including the settings nobody touched: a listing that
/// hides the built-ins answers "what did I change" when the question being
/// asked is "what is in force" (R7).
///
/// # It still works when the configuration does not
///
/// This is the only command that runs on a file it could not use (D-D). An
/// operator reaching for it is, by definition, reaching for it because
/// something is wrong; a diagnostic tool that stops working in the situation
/// it exists for is no tool at all. Broken keys are printed as rows carrying
/// the complaint instead of a value, and the command still exits 1 so that a
/// script branching on the code is not misled.
///
/// The one failure it does not survive is a `--config` path that is not there
/// — that is a usage error, handled before this is ever called.
pub fn config_show(config: &Resolved, format: ConfigFormat) -> Result<()> {
    match format {
        ConfigFormat::Table => print_config_table(config),
        ConfigFormat::Json => print_config_json(config),
    }

    if config.is_ok() {
        return Ok(());
    }

    // The payload has already gone to stdout; this is the diagnostic half, and
    // it carries the exit code.
    Err(CliError::failure(format!(
        "The configuration has {} problem(s); they are reported above.",
        config.errors().len()
    )))
}

/// What stands in the value column of a setting whose value did not hold up.
const CONFIG_ERROR_MARKER: &str = "error";

/// Width of the setting-name column.
const SETTING_WIDTH: usize = 16;

/// Width the value column is padded to.
///
/// A minimum, not a maximum: a value longer than this pushes the source column
/// right rather than being cut. `data_dir` is a path and routinely runs past
/// it, and a truncated path is a *wrong* answer to the one question this
/// command exists to answer (R8).
const VALUE_WIDTH: usize = 28;

/// The table `config show` prints by default.
///
/// The same fixed-width shape as `workflow list`, continuation lines and all,
/// because it is the same idea: one row per thing, with what is behind it on
/// the lines below.
fn print_config_table(config: &Resolved) {
    println!(
        "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} SOURCE",
        "SETTING", "VALUE"
    );

    for setting in SettingKey::ALL {
        match masking_error(config, setting) {
            Some(message) => {
                println!(
                    "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} -",
                    setting.key(),
                    format!("({CONFIG_ERROR_MARKER})")
                );
                println!("{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} {}", "", "", message);
            }
            None => {
                let value = render_value(config.value(setting), setting.is_sensitive());
                println!(
                    "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} {}",
                    setting.key(),
                    value,
                    config.source(setting)
                );
            }
        }
    }

    if let Some(path) = config.file().path() {
        println!(
            "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} file: {}",
            "",
            "",
            path.display()
        );
    }
    for shadowed in &config.file().shadowed {
        println!(
            "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} shadowed: {}",
            "",
            "",
            shadowed.path.display()
        );
    }

    // Everything that is wrong but belongs to no row of the table: an unknown
    // key, a syntax error, a candidate that is not a readable file.
    for error in config.errors() {
        // Everything that marked no row of the table: an unknown key, a syntax
        // error, a candidate that is not a readable file — and a keyed fault
        // that a higher-precedence layer overruled.
        let marked_a_row = error
            .key()
            .and_then(SettingKey::from_key)
            .is_some_and(|setting| masking_error(config, setting).is_some());
        if !marked_a_row {
            println!(
                "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} {CONFIG_ERROR_MARKER}: {error}",
                "", ""
            );
        }
    }
}

/// The machine-readable shape, alone on stdout.
///
/// The version number is the point of the envelope: this is the contract a
/// `jq` invocation in someone's runbook will be written against, and a later
/// change of shape should be a recognisable change rather than a breakage
/// (D-O). The source is an object rather than the string `"file (/etc/…)"` so
/// that nothing downstream has to parse a sentence.
fn print_config_json(config: &Resolved) {
    let mut settings = serde_json::Map::new();
    for setting in SettingKey::ALL {
        let mut entry = serde_json::Map::new();
        match masking_error(config, setting) {
            Some(message) => {
                entry.insert("error".to_string(), serde_json::Value::String(message));
                entry.insert("value".to_string(), serde_json::Value::Null);
                entry.insert("source".to_string(), serde_json::Value::Null);
            }
            None => {
                entry.insert("error".to_string(), serde_json::Value::Null);
                entry.insert(
                    "value".to_string(),
                    serde_json::Value::String(render_value(
                        config.value(setting),
                        setting.is_sensitive(),
                    )),
                );
                let source = config.source(setting);
                entry.insert(
                    "source".to_string(),
                    serde_json::json!({
                        "kind": source.label(),
                        "detail": source.detail(),
                    }),
                );
            }
        }
        entry.insert(
            "sensitive".to_string(),
            serde_json::Value::Bool(setting.is_sensitive()),
        );
        settings.insert(setting.key().to_string(), serde_json::Value::Object(entry));
    }

    let document = serde_json::json!({
        "version": CONFIG_JSON_VERSION,
        "file": config.file().path().map(|path| path.display().to_string()),
        "shadowed": config
            .file()
            .shadowed
            .iter()
            .map(|candidate| candidate.path.display().to_string())
            .collect::<Vec<_>>(),
        "settings": serde_json::Value::Object(settings),
        "errors": config
            .errors()
            .iter()
            .map(|error| serde_json::json!({
                "key": error.key(),
                "message": error.to_string(),
            }))
            .collect::<Vec<_>>(),
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&document).expect("the document is plain data")
    );
}

/// The version of the JSON shape.
///
/// Bumped when the shape changes in a way a reader would notice.
const CONFIG_JSON_VERSION: u32 = 1;

/// The complaint that actually cost this setting its value, if any.
///
/// A fault in a layer that lost decided nothing: with a bad `agent_harness` in
/// the file *and* a good one in the environment, the environment still wins,
/// and the row has to say so. Marking it `(error)` would hide the value that
/// is genuinely in force and contradict the per-setting layering this whole
/// module is about (R2).
///
/// A fault masks the value exactly when nothing outranked it — which is
/// exactly when the setting fell back to the value built into the binary.
fn masking_error(config: &Resolved, setting: SettingKey) -> Option<String> {
    if config.source(setting) != &Source::BuiltIn {
        return None;
    }
    config
        .errors()
        .iter()
        .find(|error| error.key() == Some(setting.key()))
        .map(ToString::to_string)
}
