//! Implementation of every subcommand.
//!
//! # Where a `run` gets its workspace
//!
//! `run` owns command-line parsing and presentation. It gets the workflow
//! schema and queues the task through the control plane API; the server owns
//! workflow resolution, task creation, and execution.
//!
//! # Where a definition comes from
//!
//! A catalogue reference is resolved by the server's workflow endpoint. A
//! file reference is validated by the local server and sent as an absolute
//! path, so the client does not parse a workflow definition itself.
//!
//! # How `input` and `resume` find the definition
//!
//! The server seals workflow definitions with accepted tasks. Follow-up
//! commands such as `input` and `resume` use API responses and never resolve
//! the definition or open execution storage themselves.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::{Map, Value};

use crate::api::{
    ApiClient, ChatAuthorDto, ChatEntryDto, ChatEntryKindDto, ClientError, ConversationResponse,
    ExecutionStatusDto, ExecutionSummary, HumanInputDeliveryDto, ProblemKindDto,
};
use crate::domain::{without_unsafe_chars, Event, ExecutionStatus};
use crate::tui::conversation::{classify, unclassified_preview};
use narsor_protocol::api::{ConversationState, Position, PositionStatus};
use narsor_protocol::chat::ChatElement;
use narsor_protocol::controller::{WorkflowInputType, WorkflowInputView, WorkflowSource};

use crate::config::{render_value, Flags, Resolved, SettingKey, Source};

use super::ConfigFormat;

/// How often `events --follow` looks for new entries in the log.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

pub use narsor_shared::cli_error::{CliError, Result};

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

/// `narsor validate <workflow>` — static validation, all violations at
/// once.
///
/// The argument is a workflow reference: a file, a name in the search path, or
/// a built-in.
pub async fn validate(config: &Resolved, flags: &Flags, reference: &str) -> Result<()> {
    let client = daemon_client(config.state_dir(), flags).await?;
    if Path::new(reference).is_file() {
        let path = absolute_workflow_path(reference)?;
        let verdict = client
            .control_validate_workflow(&path)
            .await
            .map_err(|error| {
                CliError::failure(format!("Could not validate '{reference}': {error}"))
            })?;
        return print_validation(
            verdict.name.as_deref().unwrap_or(reference),
            &path,
            verdict.valid,
            verdict.version,
            verdict.state_count,
            verdict.initial_state.as_deref(),
            &verdict.violations,
        );
    }

    let listing = client
        .control_workflows()
        .await
        .map_err(|error| CliError::failure(format!("Could not list workflows: {error}")))?;
    let Some(workflow) = listing
        .workflows
        .iter()
        .find(|workflow| workflow.name == reference)
    else {
        let builtins = listing
            .workflows
            .iter()
            .filter(|workflow| workflow.source == WorkflowSource::BuiltIn)
            .map(|workflow| workflow.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::usage(format!(
            "Workflow '{reference}' is not in the control plane catalogue. Looked for '{reference}' in the server workflow catalogue; available built-ins: {builtins}."
        )));
    };
    print_validation(
        &workflow.name,
        workflow_source_name(workflow.source),
        workflow.valid,
        workflow.version,
        workflow.state_count,
        workflow.initial_state.as_deref(),
        &workflow.violations,
    )
}

/// Singular or plural, for the violation count.
fn violation_noun(count: usize) -> &'static str {
    if count == 1 {
        "violation"
    } else {
        "violations"
    }
}

fn print_validation(
    name: &str,
    source: &str,
    valid: bool,
    version: Option<u32>,
    state_count: usize,
    initial_state: Option<&str>,
    violations: &[String],
) -> Result<()> {
    if !valid {
        let mut message = format!(
            "Definition '{name}' ({source}) is invalid — {} {} found:",
            violations.len(),
            violation_noun(violations.len())
        );
        for violation in violations {
            message.push_str(&format!("\n  - {}", without_unsafe_chars(violation)));
        }
        return Err(CliError::failure(message));
    }

    println!(
        "The definition is valid: {name} (version {}), states: {state_count}, initial state: {}.",
        version.unwrap_or_default(),
        initial_state.unwrap_or("-")
    );
    Ok(())
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
/// Workflow discovery and validation come from the control plane. A file
/// reference is validated by the local server, and a catalogue reference is
/// looked up through its workflow endpoint; this client never parses a
/// workflow definition before creating the task.
pub async fn run(
    config: &Resolved,
    flags: &Flags,
    reference: &str,
    inputs: &[String],
) -> Result<()> {
    let client = daemon_client(config.state_dir(), flags).await?;
    let (workflow, name, source, schema) = workflow_for_run(&client, reference).await?;
    eprintln!("Workflow '{name}' — {source}.");

    // Inputs first: a missing required parameter must be reported before
    // anything is written to disk.
    let inputs = collect_inputs(&name, &schema, inputs)?;

    let submission_id = format!("task-{}", uuid::Uuid::new_v4().simple());

    let started = submit_task(&client, &submission_id, &workflow, inputs)
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
fn collect_inputs(
    workflow_name: &str,
    schema: &[WorkflowInputView],
    pairs: &[String],
) -> Result<Map<String, Value>> {
    let mut values = Map::new();

    for pair in pairs {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid input '{pair}' — expected the form name=value."
            ))
        })?;

        let parameter = schema
            .iter()
            .find(|parameter| parameter.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Workflow '{}' declares no input named '{name}'. Declared inputs: {}.",
                    workflow_name,
                    declared_input_names(schema)
                ))
            })?;

        values.insert(name.to_string(), parse_input_value(parameter, raw)?);
    }

    let mut missing = Vec::new();
    for parameter in schema {
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
fn declared_input_names(schema: &[WorkflowInputView]) -> String {
    if schema.is_empty() {
        return "(none)".to_string();
    }
    schema
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
fn parse_input_value(parameter: &WorkflowInputView, raw: &str) -> Result<Value> {
    match parameter.input_type {
        WorkflowInputType::String | WorkflowInputType::Text | WorkflowInputType::Repository => {
            Ok(Value::String(raw.to_string()))
        }
        WorkflowInputType::Integer => raw.parse::<i64>().map(Value::from).map_err(|_| {
            CliError::usage(format!(
                "Input '{}' is of type integer, and '{raw}' is not a whole number.",
                parameter.name
            ))
        }),
        WorkflowInputType::Boolean => match raw.to_ascii_lowercase().as_str() {
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

        if !follow || summary.status.is_terminal() {
            break;
        }
        tokio::time::sleep(FOLLOW_INTERVAL).await;
        summary = load_execution_summary(&client, execution_id).await?;
    }
    Ok(())
}

/// One event line: sequence, timestamp, type, description.
///
/// The description may quote an agent or a remote runner, so it is cleaned
/// of anything a terminal would act on before it is printed.
pub(crate) fn print_event(event: &Event) {
    println!(
        "{:>5}  {}  {:<22} {}",
        event.sequence,
        event.timestamp.to_rfc3339(),
        event_type(event),
        without_unsafe_chars(&event.description())
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
pub(crate) fn print_position(position: &Position) {
    println!(
        "{:<14} {:<18} {:<10} {}  ->  {}",
        without_unsafe_chars(&position.label),
        position_status_name(position.status),
        without_unsafe_chars(position.action_type.as_deref().unwrap_or("-")),
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
        println!("    error: {}", without_unsafe_chars(error));
    }
    if position.retry_pending {
        println!("    waiting to retry");
    }
    if position.harness_session_interrupted {
        println!("    harness session interrupted");
    }
    if let Some(resumed) = &position.resumed_harness_session {
        println!(
            "    resumed harness session: {}",
            without_unsafe_chars(resumed)
        );
    }
    match &position.conversation {
        ConversationState::Recorded => {
            if let Some(attempt_id) = &position.action_execution_id {
                println!("    attempt id: {}", without_unsafe_chars(attempt_id));
            }
        }
        ConversationState::NotCreated { reason } => {
            println!(
                "    conversation not created: {}",
                without_unsafe_chars(reason)
            )
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
/// to be one call. Every entry is printed through [`conversation_entry_lines`]:
/// one line per chat element, with the same category label and the same
/// summary the TUI shows (D60). The raw line stays the lossless record and is
/// what the API returns next to the elements.
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
    for line in conversation_entry_lines(entry) {
        println!("{line}");
    }
}

/// The printed lines of one conversation entry, shared by every CLI command
/// that prints a conversation.
///
/// One line per chat element — entry number, timestamp, category label,
/// element summary — so the CLI and the TUI ([`classify`]) agree on both the
/// label and the text. An entry narsor wrote itself (the prompt, the process
/// outcome) is printed once, as its decoded text, as it always was: that text
/// is narsor's own and needs no summary. An unclassified element (an
/// opencode or codex line, a legacy record) says why, followed by the start
/// of its raw line on the same line — the summary alone would hide what the
/// harness printed.
pub fn conversation_entry_lines(entry: &crate::api::ConversationEntryDto) -> Vec<String> {
    let narsor = matches!(entry.source, crate::api::ConversationEntrySource::Narsor);
    classify(entry)
        .into_iter()
        .filter(|classified| !narsor || classified.element_index == 0)
        .map(|classified| {
            let body = if narsor {
                crate::tui::scroll::sanitize(&String::from_utf8_lossy(&classified.raw))
            } else if matches!(
                classified.element,
                ChatElement::Unclassified { .. } | ChatElement::Unrecognized
            ) {
                let (preview, cut) = unclassified_preview(&classified.raw);
                let ellipsis = if cut { "…" } else { "" };
                crate::tui::scroll::one_line(&format!(
                    "{}: {preview}{ellipsis}",
                    classified.summary
                ))
            } else {
                crate::tui::scroll::sanitize(&classified.summary)
            };
            format!(
                "{:>5}  {}  {:<10} {body}",
                classified.entry_number,
                classified.timestamp.to_rfc3339(),
                classified.kind.label(),
            )
        })
        .collect()
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
    /// `--permission` choice for a Claude Code permission wait.
    pub permission: Option<String>,
    /// `--denial-index` within the pending permission request; every denial
    /// when `None`.
    pub denial_index: Option<usize>,
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

    let mut delivery = HumanInputDeliveryDto {
        values,
        data: step_values("data", &answer.data)?,
        variables: step_values("variable", &answer.variables)?,
        chosen_label: answer.label,
        comment: answer.comment,
        ..HumanInputDeliveryDto::default()
    };

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
    delivery.episode = Some(pending.episode_key().to_string());

    let permission_wait =
        pending.continuation == narsor_protocol::api::InputContinuationView::ResolvePermission;
    if permission_wait {
        let choice = answer.permission.as_deref().ok_or_else(|| CliError::usage(
            "This execution needs a permission decision. Use --permission deny, allow-exact, or allow-group.",
        ))?;
        let (decision, scope) = match choice {
            "deny" => (narsor_protocol::api::PermissionDecisionKindDto::Deny, None),
            "allow-exact" => (
                narsor_protocol::api::PermissionDecisionKindDto::Allow,
                Some(narsor_protocol::api::PermissionGrantScopeDto::ExactOperation),
            ),
            _ => (
                narsor_protocol::api::PermissionDecisionKindDto::Allow,
                Some(narsor_protocol::api::PermissionGrantScopeDto::CommandGroup),
            ),
        };
        client
            .resolve_permission(
                execution_id,
                narsor_protocol::api::PermissionDecisionRequestDto {
                    episode: pending.episode_key().to_string(),
                    permission_decision: narsor_protocol::api::PermissionDecisionDto {
                        denial_index: answer.denial_index,
                        decision,
                        scope,
                    },
                },
            )
            .await
            .map_err(|error| client_error(execution_id, error))?;
        wait_until_stopped(&client, execution_id, current.status).await?;
        return Ok(());
    }
    if answer.permission.is_some() {
        return Err(CliError::usage(
            "--permission can only answer a permission request.",
        ));
    }

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

    let summary = wait_until_stopped(&client, execution_id, current.status).await?;

    println!("Input delivered to execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatusDto::Failed {
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

    let summary = wait_until_stopped(&client, execution_id, ExecutionStatusDto::Running).await?;

    println!("Resumed execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatusDto::Failed {
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
        ExecutionStatusDto::Failed | ExecutionStatusDto::Unknown
    ) {
        return Err(CliError::failure(format!(
            "Execution '{execution_id}' has no failed step to recover (status: {}).",
            current.status.as_str()
        )));
    }
    let failure = current
        .problem
        .as_ref()
        .filter(|problem| problem.kind == ProblemKindDto::Failure)
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

    let summary = wait_until_stopped(&client, execution_id, ExecutionStatusDto::Running).await?;

    println!("Recovered execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatusDto::Failed {
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
    let proposal_id = chat_entry_id(&client, execution_id, proposal, ChatEntryKindDto::Proposal)
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
                .find(|entry| {
                    entry.sequence == sequence && entry.kind == ChatEntryKindDto::Decision
                })
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
    kind: ChatEntryKindDto,
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
fn print_chat_entry(entry: &ChatEntryDto) {
    let who = match entry.author {
        ChatAuthorDto::Operator => "operator",
        ChatAuthorDto::Conductor => "conductor",
    };
    let what = match entry.kind {
        ChatEntryKindDto::Message => "",
        ChatEntryKindDto::Error => " (error)",
        ChatEntryKindDto::Decision => " (decision)",
        ChatEntryKindDto::Proposal => " (proposal)",
    };
    println!("#{:<4} {who}{what}: {}", entry.sequence, entry.text);
    match entry.kind {
        ChatEntryKindDto::Proposal => println!("      proposal id: {}", entry.id),
        ChatEntryKindDto::Decision => {
            if let Some(id) = entry
                .payload
                .as_ref()
                .and_then(|payload| payload.get("decision_id"))
                .and_then(Value::as_str)
            {
                println!("      decision id: {id}");
            }
        }
        ChatEntryKindDto::Message | ChatEntryKindDto::Error => {}
    }
}

// ---------------------------------------------------------------------------
// tui
// ---------------------------------------------------------------------------

/// `narsor tui` — the full-screen interface over the control plane `target`
/// names: a remote server, or the local daemon of the state directory
/// (launched when none runs, as for every other command).
///
/// The loop is async but this entry point is not, because the subcommand
/// dispatch calls it from inside the runtime `main` already started. So the
/// current worker thread is handed over with `block_in_place` and the loop is
/// driven on it: spawning a second runtime inside the first would panic, and
/// the interface has to own a thread anyway — it blocks on keys.
pub fn tui(config: &Resolved, flags: &Flags, target: &crate::api::ServerTarget) -> Result<()> {
    let state = config.state_dir();
    let handle = tokio::runtime::Handle::current();
    let refresh_interval = config
        .tui_refresh_interval()
        .map(std::time::Duration::from_secs);
    let connection_name = target.server.clone().unwrap_or_else(|| "local".to_string());
    tokio::task::block_in_place(move || {
        handle.block_on(async {
            let client = crate::api::connect(target, state, flags)
                .await
                .map_err(CliError::failure)?;
            crate::tui::run(client, connection_name, refresh_interval)
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

/// Resolves the input schema through the server, returning the original
/// reference to submit so the server applies the same catalogue resolution.
async fn workflow_for_run(
    client: &ApiClient,
    reference: &str,
) -> Result<(String, String, String, Vec<WorkflowInputView>)> {
    if Path::new(reference).is_file() {
        let path = absolute_workflow_path(reference)?;
        let verdict = client
            .control_validate_workflow(&path)
            .await
            .map_err(|error| {
                CliError::failure(format!("The file could not be validated: {error}"))
            })?;
        if !verdict.valid {
            return Err(invalid_workflow_error(
                verdict.name.as_deref().unwrap_or(reference),
                &verdict.violations,
            ));
        }
        let name = verdict
            .name
            .unwrap_or_else(|| Path::new(reference).display().to_string());
        return Ok((path, name, "file".to_string(), verdict.inputs));
    }

    let listing = client
        .control_workflows()
        .await
        .map_err(|error| CliError::failure(format!("Could not list workflows: {error}")))?;
    let Some(workflow) = listing
        .workflows
        .iter()
        .find(|workflow| workflow.name == reference)
    else {
        let builtins = listing
            .workflows
            .iter()
            .filter(|workflow| workflow.source == WorkflowSource::BuiltIn)
            .map(|workflow| workflow.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::usage(format!(
            "Workflow '{reference}' is not in the control plane catalogue. Looked for '{reference}' in the server workflow catalogue; available built-ins: {builtins}."
        )));
    };
    if !workflow.valid {
        return Err(invalid_workflow_error(&workflow.name, &workflow.violations));
    }
    Ok((
        reference.to_string(),
        workflow.name.clone(),
        workflow_source_name(workflow.source).to_string(),
        workflow.inputs.clone(),
    ))
}

fn absolute_workflow_path(reference: &str) -> Result<String> {
    std::path::absolute(reference)
        .map(|path| path.display().to_string())
        .map_err(|error| CliError::usage(format!("Cannot make '{reference}' absolute: {error}")))
}

fn invalid_workflow_error(name: &str, violations: &[String]) -> CliError {
    let mut message = format!("The definition '{name}' is invalid — no task was created:");
    for violation in violations {
        message.push_str(&format!("\n  - {}", without_unsafe_chars(violation)));
    }
    CliError::failure(message)
}

fn workflow_source_name(source: WorkflowSource) -> &'static str {
    match source {
        WorkflowSource::BuiltIn => "built-in",
        WorkflowSource::Catalog => "root/catalog",
        WorkflowSource::User => "user",
    }
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
fn print_status_line(status: &ExecutionStatusDto, current_state: Option<&str>) {
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
    crate::api::ensure_daemon_running(state, flags)
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
        ClientError::Request { message, .. }
        | ClientError::Refused { message, .. }
        | ClientError::Degraded { message, .. } => CliError::failure(message),
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
    before: ExecutionStatusDto,
) -> Result<ExecutionSummary> {
    let mut stream = Some(Box::pin(client.stream_execution(execution_id)));
    loop {
        if let Some(summary) = find_execution(client, execution_id).await? {
            if summary.status != ExecutionStatusDto::Running && summary.status != before {
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
    if summary.status == ExecutionStatusDto::InputNeeded {
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
/// A problem input ([`ProblemKindDto::Output`]) is not repeated here: the
/// pending input's form already says it.
fn print_conductor_matters(summary: &ExecutionSummary) {
    if let Some(problem) = summary
        .problem
        .as_ref()
        .filter(|problem| problem.kind == ProblemKindDto::Failure)
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
fn print_pending_input(pending: &impl serde::Serialize) {
    let Ok(pending) = serde_json::to_value(pending) else {
        println!("Human input expected: (unavailable)");
        return;
    };
    println!(
        "Human input expected: {}",
        pending["instruction"].as_str().unwrap_or("(unavailable)")
    );
    for field in pending["fields"].as_array().into_iter().flatten() {
        let requirement = if field["required"].as_bool().unwrap_or(false) {
            "required"
        } else {
            "optional"
        };
        let options_list = field["options"].as_array();
        let options = if options_list.is_none_or(Vec::is_empty) {
            String::new()
        } else {
            format!(
                ", allowed values: {}",
                options_list
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let field_type = match field["type"].as_str().unwrap_or_default() {
            "text" => "Text",
            "select" => "Select",
            other => other,
        };
        println!(
            "  --field {}=…  ({field_type}, {requirement}{options})",
            field["name"].as_str().unwrap_or("?")
        );
    }
    if let Some(denials) = pending["permission"]["denials"].as_array() {
        for (index, denial) in denials.iter().enumerate() {
            let operation = serde_json::to_string(&denial["input"])
                .unwrap_or_else(|_| "(unavailable)".to_string());
            println!(
                "  Permission denial {index}: {} {}",
                without_unsafe_chars(denial["tool_name"].as_str().unwrap_or("tool")),
                without_unsafe_chars(&operation),
            );
            println!(
                "    cwd: {}",
                without_unsafe_chars(denial["cwd"].as_str().unwrap_or("(unknown)")),
            );
            if let Some(reason) = denial["reason"].as_str() {
                println!("    reason: {}", without_unsafe_chars(reason));
            }
        }
        println!("  Decide with --permission deny, allow-exact, or allow-group.");
    }
    let outputs = pending["outputs"].as_array().cloned().unwrap_or_default();
    let problem = &pending["problem"];
    if problem.is_null() {
        if !outputs.is_empty() {
            let labels = outputs
                .iter()
                .filter_map(|output| output.as_str().or_else(|| output["label"].as_str()))
                .collect::<Vec<_>>();
            println!("  Output labels: {}", labels.join(", "));
        }
        return;
    }

    // A problem: the step's output is what is asked for (KTD3).
    println!(
        "  Problem: {}",
        problem["reason"].as_str().unwrap_or("(unknown)")
    );
    for answer in problem["answers"].as_array().into_iter().flatten() {
        if let Some(tail) = answer["answer_tail"].as_str() {
            println!(
                "  Agent answered ({}):",
                answer["action_execution_id"].as_str().unwrap_or("unknown")
            );
            for line in tail.lines() {
                println!("    | {line}");
            }
        }
    }
    println!("  Resolve with --label and the keys that label needs:");
    for output in outputs {
        let label = output
            .as_str()
            .or_else(|| output["label"].as_str())
            .unwrap_or("?");
        let description = output["description"]
            .as_str()
            .map(|description| format!("  — {description}"))
            .unwrap_or_default();
        println!("    --label {label}{description}");
        if let Some(requirement) = problem["requirements"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|requirement| requirement["label"].as_str() == Some(label))
        {
            for key in requirement["data"].as_array().into_iter().flatten() {
                if let Some(key) = key.as_str() {
                    println!("        requires --data {key}=…");
                }
            }
            for key in requirement["variables"].as_array().into_iter().flatten() {
                if let Some(key) = key.as_str() {
                    println!("        requires --variable {key}=…");
                }
            }
        }
    }
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
