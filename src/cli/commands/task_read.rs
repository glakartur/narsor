//! `list`, `show`, `events`, `steps` and `conversation`: reading what an execution
//! did and where it stands.

//!
//! An execution is named by its id; the CLI finds the task that runs it in
//! the task records and reads it through that task's control-plane reads,
//! the same for every backend (U8). Its events and conversation come from
//! the task's runner — for a stopped task, a read-only runner started on
//! demand (D63) — and its steps from the timeline the runner reported.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::api::{ApiClient, ClientError, ConversationResponse};
use crate::config::Flags;
use crate::controller_protocol::{
    ConversationPageQuery, ConversationUnavailableReason, ExecutionEventsQuery,
    ExecutionEventsResponse, CONVERSATION_READER_STARTING,
};
use crate::domain::{without_unsafe_chars, Event};
use crate::tui::conversation::{classify, unclassified_preview};
use narsor_protocol::api::{ConversationState, Position};
use narsor_protocol::chat::ChatElement;

use super::daemon_access::{client_error, daemon_client, load_execution, load_execution_summary};
use super::open_matters::{conductor_matter_lines, pending_input_lines};
use super::{CliError, Result};
use crate::cli::task_text::position_status_name;

/// How often `events --follow` looks for new entries in the log.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

/// How long a read of a stopped task waits for its read-only runner to
/// connect (D63) before it gives up with the reason.
pub const READER_WAIT: Duration = Duration::from_secs(30);

/// How often a read asks again while the read-only runner is starting.
const READER_RETRY: Duration = Duration::from_millis(250);

/// `narsor list` — every execution, newest first, from the task records
/// (`GET /control/v1/executions`): nothing is read from a task's storage.
pub async fn list(state: &Path, flags: &Flags) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    print_lines(execution_list_lines(&client).await?);
    Ok(())
}

/// The lines `narsor list` prints: a header and one row per execution,
/// newest first, or `No executions.`.
pub async fn execution_list_lines(client: &ApiClient) -> Result<Vec<String>> {
    let listing = client
        .control_executions()
        .await
        .map_err(|error| CliError::failure(format!("Could not read the executions: {error}")))?;

    if listing.executions.is_empty() {
        return Ok(vec!["No executions.".to_string()]);
    }

    // Newest first: the execution an operator just started is the one they
    // are looking for.
    let mut executions = listing.executions;
    executions.sort_by_key(|execution| std::cmp::Reverse(execution.started_at));

    let mut lines = vec![format!(
        "{:<38} {:<10} {:<22} WORKFLOW",
        "ID", "STATUS", "STATE"
    )];
    for execution in &executions {
        lines.push(format!(
            "{:<38} {:<10} {:<22} {}",
            execution.id,
            execution.status.as_str(),
            execution.current_state.as_deref().unwrap_or("-"),
            execution.workflow_id
        ));
    }
    Ok(lines)
}

fn print_lines(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

/// `narsor show <id>` — the current state, the status and what waits on
/// the owner, as the execution's task record holds them.
pub async fn show(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    print_lines(execution_show_lines(&client, execution_id).await?);
    Ok(())
}

/// The lines `narsor show` prints for one execution.
pub async fn execution_show_lines(client: &ApiClient, execution_id: &str) -> Result<Vec<String>> {
    let execution = load_execution_summary(client, execution_id).await?;

    let mut lines = vec![
        format!("Execution:     {}", execution.id),
        format!(
            "Workflow:      {} (version {})",
            execution.workflow_id, execution.workflow_version
        ),
        format!("Status:        {}", execution.status.as_str()),
        format!(
            "Current state: {}",
            execution.current_state.as_deref().unwrap_or("-")
        ),
        format!("Started:       {}", execution.started_at.to_rfc3339()),
    ];
    if let Some(completed_at) = execution.completed_at {
        lines.push(format!("Finished:      {}", completed_at.to_rfc3339()));
    }

    if let Some(pending) = &execution.pending_input {
        lines.extend(pending_input_lines(pending));
    }
    lines.extend(conductor_matter_lines(&execution));
    Ok(lines)
}

/// `narsor events <id> [--follow]` — the event log, in sequence order.
pub async fn events(state: &Path, flags: &Flags, execution_id: &str, follow: bool) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    // Fails early and readably when the id is unknown.
    let found = load_execution(&client, execution_id).await?;
    let mut summary = found.execution;

    let mut after = 0u64;
    loop {
        let events = execution_events_after(&client, &found.task_id, after, READER_WAIT).await?;
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

/// The lines `narsor events` (without `--follow`) prints; a stopped task's
/// read-only runner is waited for up to `reader_wait`.
pub async fn execution_event_lines(
    client: &ApiClient,
    execution_id: &str,
    reader_wait: Duration,
) -> Result<Vec<String>> {
    let found = load_execution(client, execution_id).await?;
    Ok(
        execution_events_after(client, &found.task_id, 0, reader_wait)
            .await?
            .iter()
            .map(event_line)
            .collect(),
    )
}

/// Every event of the execution of `task_id` with a sequence above `after`,
/// page by page, from the task's runner.
async fn execution_events_after(
    client: &ApiClient,
    task_id: &str,
    after: u64,
    reader_wait: Duration,
) -> Result<Vec<Event>> {
    let deadline = Instant::now() + reader_wait;
    let mut query = ExecutionEventsQuery { after, limit: None };
    let mut events = Vec::new();
    loop {
        let response = client
            .control_execution_events(task_id, &query)
            .await
            .map_err(|error| read_error("events", error))?;
        match response {
            ExecutionEventsResponse::Available {
                events: page,
                has_more,
                ..
            } => {
                let last = page.last().map(|event| event.sequence);
                events.extend(page);
                match last {
                    Some(last) if has_more => query.after = last,
                    _ => return Ok(events),
                }
            }
            ExecutionEventsResponse::Unavailable {
                reason: ConversationUnavailableReason::ReaderStarting,
                ..
            } if Instant::now() < deadline => tokio::time::sleep(READER_RETRY).await,
            ExecutionEventsResponse::Unavailable { message, .. } => {
                return Err(CliError::failure(format!(
                    "The execution's events cannot be read: {}",
                    without_unsafe_chars(&message)
                )));
            }
        }
    }
}

/// What the operator sees when a relayed read of `what` fails.
fn read_error(what: &str, error: ClientError) -> CliError {
    match error {
        ClientError::Refused { message, .. }
        | ClientError::Degraded { message, .. }
        | ClientError::Request { message, .. } => CliError::failure(format!(
            "The execution's {what} cannot be read: {}",
            without_unsafe_chars(&message)
        )),
        other => CliError::failure(format!("The execution's {what} cannot be read: {other}")),
    }
}

/// One event line: sequence, timestamp, type, description.
pub(crate) fn print_event(event: &Event) {
    println!("{}", event_line(event));
}

/// The text of [`print_event`].
///
/// The description may quote an agent or a remote runner, so it is cleaned
/// of anything a terminal would act on.
fn event_line(event: &Event) -> String {
    format!(
        "{:>5}  {}  {:<22} {}",
        event.sequence,
        event.timestamp.to_rfc3339(),
        event_type(event),
        without_unsafe_chars(&event.description())
    )
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

/// `narsor steps <id>` — the step timeline, one line per attempt.
pub async fn steps(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    print_lines(execution_steps_lines(&client, execution_id).await?);
    Ok(())
}

/// The lines `narsor steps` prints: the timeline the execution's runner
/// reported last, kept in its task record.
pub async fn execution_steps_lines(client: &ApiClient, execution_id: &str) -> Result<Vec<String>> {
    let found = load_execution(client, execution_id).await?;
    let response = client
        .control_timeline(&found.task_id)
        .await
        .map_err(|error| client_error(execution_id, error))?;

    match response
        .timeline
        .filter(|timeline| !timeline.positions.is_empty())
    {
        Some(timeline) => Ok(timeline.positions.iter().flat_map(position_lines).collect()),
        None => Ok(vec!["No steps.".to_string()]),
    }
}

/// Prints [`position_lines`].
pub(crate) fn print_position(position: &Position) {
    print_lines(position_lines(position));
}

/// One position: label, status, action type, start/end, and — when they
/// apply — the error, the retry-pending and harness-interrupted flags, and
/// why the conversation cannot be read.
fn position_lines(position: &Position) -> Vec<String> {
    let mut lines = vec![format!(
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
    )];
    if let Some(error) = &position.error {
        lines.push(format!("    error: {}", without_unsafe_chars(error)));
    }
    if position.retry_pending {
        lines.push("    waiting to retry".to_string());
    }
    if position.harness_session_interrupted {
        lines.push("    harness session interrupted".to_string());
    }
    if let Some(resumed) = &position.resumed_harness_session {
        lines.push(format!(
            "    resumed harness session: {}",
            without_unsafe_chars(resumed)
        ));
    }
    match &position.conversation {
        ConversationState::Recorded => {
            for attempt_id in position.attempt_ids() {
                lines.push(format!(
                    "    attempt id: {}",
                    without_unsafe_chars(attempt_id)
                ));
            }
        }
        ConversationState::NotCreated { reason } => lines.push(format!(
            "    conversation not created: {}",
            without_unsafe_chars(reason)
        )),
        ConversationState::NoRecord => {
            lines.push("    no conversation record (old log)".to_string())
        }
    }
    lines
}

/// `narsor conversation <id> <attempt>` — one attempt's conversation, in
/// order, in full (R9: nothing is truncated).
///
/// Every entry is printed through [`conversation_entry_lines`]: one line per
/// chat element, with the same category label and the same summary the TUI
/// shows (D60). The raw line stays the lossless record and is what the API
/// returns next to the elements.
pub async fn conversation(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    attempt_id: &str,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    print_lines(
        execution_conversation_lines(&client, execution_id, attempt_id, READER_WAIT).await?,
    );
    Ok(())
}

/// The lines `narsor conversation` prints: every page of the attempt, from
/// the task's runner; a stopped task's read-only runner is waited for up to
/// `reader_wait`.
pub async fn execution_conversation_lines(
    client: &ApiClient,
    execution_id: &str,
    attempt_id: &str,
    reader_wait: Duration,
) -> Result<Vec<String>> {
    let found = load_execution(client, execution_id).await?;
    let deadline = Instant::now() + reader_wait;
    let mut lines = Vec::new();
    let mut from = 0;
    loop {
        let page = client
            .control_conversation_page(
                &found.task_id,
                attempt_id,
                &ConversationPageQuery {
                    from,
                    byte_budget: None,
                },
            )
            .await;
        match page {
            Ok(ConversationResponse::Recorded {
                entries,
                next_entry,
                truncated_tail,
                ..
            }) => {
                let done = entries.is_empty() || next_entry <= from;
                lines.extend(entries.iter().flat_map(conversation_entry_lines));
                if done {
                    if truncated_tail {
                        lines.push(
                            "(the last line on disk was incomplete — the attempt was interrupted \
                             mid-write)"
                                .to_string(),
                        );
                    }
                    return Ok(lines);
                }
                from = next_entry;
            }
            Ok(ConversationResponse::NoRecord) => {
                return Err(CliError::failure(format!(
                    "No conversation record for attempt '{attempt_id}' in execution \
                     '{execution_id}'."
                )))
            }
            Err(ClientError::Refused { code, .. })
                if code == CONVERSATION_READER_STARTING && Instant::now() < deadline =>
            {
                tokio::time::sleep(READER_RETRY).await
            }
            Err(error) => return Err(read_error("conversation", error)),
        }
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
/// harness printed. A line a subagent wrote is indented by two spaces and its
/// text starts with `↳ [name]`.
pub fn conversation_entry_lines(entry: &crate::api::ConversationEntryDto) -> Vec<String> {
    let narsor = matches!(entry.source, crate::api::ConversationEntrySource::Narsor);
    classify(entry)
        .into_iter()
        .filter(|classified| !narsor || classified.element_index == 0)
        .map(|classified| {
            let body = if narsor {
                let text = crate::tui::scroll::sanitize(&String::from_utf8_lossy(&classified.raw));
                match &classified.element {
                    ChatElement::Prompt {
                        author: Some(author),
                        ..
                    } => format!("[{}] {text}", author.label()),
                    _ => text,
                }
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
            let line = format!(
                "{:>5}  {}  {:<10} {}{body}",
                classified.entry_number,
                classified.timestamp.to_rfc3339(),
                classified.kind.label(),
                subagent_prefix(classified.subagent.as_ref()),
            );
            if classified.subagent.is_some() {
                format!("  {line}")
            } else {
                line
            }
        })
        .collect()
}

/// `↳ [name] ` before the text of a line a subagent wrote; nothing for the
/// main agent. The name is one safe line, as in the TUI.
fn subagent_prefix(subagent: Option<&narsor_protocol::chat::SubagentRef>) -> String {
    subagent.map_or_else(String::new, |subagent| {
        format!(
            "↳ [{}] ",
            crate::tui::scroll::one_line(&subagent.name).trim()
        )
    })
}
