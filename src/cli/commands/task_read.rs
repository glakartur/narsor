//! `list`, `show`, `events`, `steps` and `conversation`: reading what an execution
//! did and where it stands.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::api::ConversationResponse;
use crate::config::Flags;
use crate::domain::{without_unsafe_chars, Event};
use crate::tui::conversation::{classify, unclassified_preview};
use narsor_protocol::api::{ConversationState, Position, PositionStatus};
use narsor_protocol::chat::ChatElement;

use super::daemon_access::{client_error, daemon_client, load_execution_summary};
use super::open_matters::{print_conductor_matters, print_pending_input};
use super::{CliError, Result};

/// How often `events --follow` looks for new entries in the log.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

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
///
/// [`find_execution`]: super::daemon_access::find_execution
/// [`ExecutionSummary`]: crate::api::ExecutionSummary
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
            for attempt_id in position.attempt_ids() {
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
