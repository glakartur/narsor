//! Turning the Engine's files into a [`RunnerSnapshot`] (KTD3, KTD4).
//!
//! Positions and summaries only: the event cursor, the step timeline, the
//! request the Engine waits on and per-attempt conversation *counts*. Raw
//! conversation lines, workspace files and secret values never enter a
//! snapshot; the owner reads a transcript page by page through the relay
//! (the control loop answers `conversation_pages` poll requests), and nothing of
//! it is kept centrally.
//!
//! Everything is read back from task storage rather than from memory, so a
//! snapshot after a restart says exactly what one before it would have —
//! the event cursor in particular comes from the log, which outlives every
//! incarnation and therefore never goes backwards.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::domain::{ExecutionStatus, TaskDisplay, WorkflowDefinition, WorkflowExecution};
use crate::engine::{build_timeline, Engine, InputContinuation, PendingHumanInput, Timeline};
use crate::orchestration::{
    AttentionItem, AttentionKind, ConversationAttemptSummary, ConversationAvailability,
    ExecutionCursor, PendingInputRef, RunnerProblem, RunnerSnapshot,
};
use crate::persistence::ConversationStore;
use crate::workflow::TemplateScope;

use super::RunnerError;

/// Summaries of attempts whose conversation is closed: they never change
/// again, so each is counted once per process.
#[derive(Debug, Default)]
pub(crate) struct ConversationCache {
    closed: HashMap<String, ConversationAttemptSummary>,
}

/// What the runner knows beyond the files.
pub(crate) struct SnapshotContext<'a> {
    /// The workflow the task runs, for its `display`.
    pub definition: &'a WorkflowDefinition,
    /// The execution on this storage, once it exists.
    pub execution_id: Option<&'a str>,
    /// Whether an Engine pass is driving it right now.
    pub pass_active: bool,
    /// Why the runner cannot work, when it cannot.
    pub problem: Option<RunnerProblem>,
    /// Whether the runner is stopping at a safe point it has already
    /// recorded on storage. Only then does a snapshot claim `safe_point`:
    /// the control plane lets compute be removed the moment it accepts one
    /// during a pause (KTD2), so the claim must never run ahead of the
    /// runner's own durable record of stopping.
    pub stopping: bool,
}

/// Builds the snapshot of the storage's execution, without a sequence (the
/// caller stamps it).
pub(crate) fn build(
    engine: &Engine,
    cache: &mut ConversationCache,
    context: SnapshotContext<'_>,
) -> Result<RunnerSnapshot, RunnerError> {
    let Some(execution_id) = context.execution_id else {
        let observed_at = Utc::now();
        return Ok(RunnerSnapshot {
            snapshot_sequence: 0,
            // Nothing exists to have a status yet; `Unknown` is the honest
            // answer, and the problem (if any) says why.
            execution_status: ExecutionStatus::Unknown,
            execution_cursor: ExecutionCursor::default(),
            event_cursor: 0,
            pending_input: None,
            conversation: ConversationAvailability::default(),
            timeline: None,
            safe_point: context.stopping && !context.pass_active,
            attention: attention_items(None, context.problem.as_ref(), observed_at),
            problem: context.problem,
            // Nothing to render against yet; the control plane's own
            // rendering from the sealed inputs stays in force.
            display: None,
            observed_at,
        });
    };

    let store = engine.store();
    let execution = store.load_execution(execution_id)?;
    let events = store.event_log(execution_id).read_all()?;
    let event_cursor = events.last().map_or(0, |event| event.sequence);
    let conversations = store.conversation_store(execution_id);
    let recorded = conversations.recorded_attempt_ids()?;
    let timeline = build_timeline(&events, execution.status.clone(), &recorded);

    let observed_at = Utc::now();
    let pending = engine.pending_input(execution_id)?;
    let attention = attention_items(pending.as_ref(), context.problem.as_ref(), observed_at);
    let pending_input = pending.map(|pending| PendingInputRef {
        id: pending.episode_key().to_string(),
        attempt_id: (pending.continuation == InputContinuation::ResumeAgent)
            .then(|| pending.action_execution_id().to_string()),
        form: pending,
    });

    let conversation = if !context.stopping {
        ConversationAvailability::Live {
            attempts: summaries(&conversations, &recorded, &timeline, cache)?,
        }
    } else {
        ConversationAvailability::Unavailable {
            reason: "the runner is stopping".to_string(),
        }
    };

    Ok(RunnerSnapshot {
        snapshot_sequence: 0,
        execution_status: execution.status.clone(),
        execution_cursor: ExecutionCursor {
            execution_id: Some(execution.id.clone()),
            current_state: execution.current_state.clone(),
            action_execution_id: timeline
                .positions
                .iter()
                .rev()
                .find_map(|position| position.action_execution_id.clone()),
        },
        event_cursor,
        pending_input,
        conversation,
        timeline: Some(timeline),
        safe_point: context.stopping && !context.pass_active,
        attention,
        problem: context.problem,
        display: render_display(context.definition, &execution),
        observed_at,
    })
}

/// What waits on the owner, oldest first: the request the engine holds
/// (classified by what answering it does) and the runner's own problem.
///
/// Metadata only. A problem has no age of its own — it is reported again
/// by every snapshot of the same incarnation — so it counts from the
/// snapshot that says it.
pub(crate) fn attention_items(
    pending: Option<&PendingHumanInput>,
    problem: Option<&RunnerProblem>,
    observed_at: DateTime<Utc>,
) -> Vec<AttentionItem> {
    let mut items: Vec<AttentionItem> = pending
        .map(|pending| AttentionItem {
            kind: match pending.continuation {
                InputContinuation::CompleteStep | InputContinuation::ResumeAgent => {
                    AttentionKind::Question
                }
                InputContinuation::ResolveOutput => AttentionKind::Problem,
                InputContinuation::ApproveOutput => AttentionKind::Approval,
            },
            state_id: Some(pending.state_id.to_string()),
            since: pending.requested_at,
            waiting_id: pending.episode_key().to_string(),
        })
        .into_iter()
        .collect();
    items.extend(problem.map(|problem| AttentionItem {
        kind: AttentionKind::Problem,
        state_id: None,
        since: observed_at,
        waiting_id: problem.code.clone(),
    }));
    items.sort_by_key(|item| item.since);
    items
}

/// The workflow's `display` against what the execution knows now: its inputs
/// and the variables its steps have set so far.
fn render_display(
    definition: &WorkflowDefinition,
    execution: &WorkflowExecution,
) -> Option<TaskDisplay> {
    let spec = definition.display.as_ref()?;
    let scope = TemplateScope::from_execution(&execution.inputs, &execution.context).ok()?;
    Some(scope.render_display(spec))
}

/// One summary per attempt with a conversation record, in timeline order.
fn summaries(
    conversations: &ConversationStore,
    recorded: &HashSet<String>,
    timeline: &Timeline,
    cache: &mut ConversationCache,
) -> Result<Vec<ConversationAttemptSummary>, RunnerError> {
    let state_of: HashMap<&str, &str> = timeline
        .positions
        .iter()
        .filter_map(|position| {
            position
                .action_execution_id
                .as_deref()
                .map(|attempt| (attempt, position.state_id.as_str()))
        })
        .collect();
    let mut ordered: Vec<&String> = recorded.iter().collect();
    let order: HashMap<&str, usize> = timeline
        .positions
        .iter()
        .enumerate()
        .filter_map(|(index, position)| {
            position
                .action_execution_id
                .as_deref()
                .map(|attempt| (attempt, index))
        })
        .collect();
    ordered.sort_by_key(|attempt| (order.get(attempt.as_str()).copied(), (*attempt).clone()));

    let mut attempts = Vec::with_capacity(ordered.len());
    for attempt in ordered {
        if let Some(summary) = cache.closed.get(attempt) {
            attempts.push(summary.clone());
            continue;
        }
        let Some(read) = conversations.read(attempt, 0, usize::MAX)? else {
            continue;
        };
        let summary = ConversationAttemptSummary {
            attempt_id: attempt.clone(),
            state_id: state_of
                .get(attempt.as_str())
                .map(|state| state.to_string()),
            entries: read.entries.len() as u64,
            closed: read.closed,
            last_entry_at: read.entries.last().map(|entry| entry.timestamp),
        };
        if summary.closed {
            cache.closed.insert(attempt.clone(), summary.clone());
        }
        attempts.push(summary);
    }
    Ok(attempts)
}
