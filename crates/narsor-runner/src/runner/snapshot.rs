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

use chrono::{DateTime, Utc};

use crate::conductor::pending_chat_proposals;
use crate::domain::{ExecutionStatus, TaskDisplay, WorkflowDefinition, WorkflowExecution};
use crate::engine::conversation_summary::summaries;
pub(crate) use crate::engine::conversation_summary::ConversationCache;
use crate::engine::{build_timeline, step_failure, Engine, InputContinuation, PendingHumanInput};
use crate::orchestration::{
    AttentionItem, AttentionKind, ConversationAvailability, ExecutionCursor, HarnessUnavailable,
    PendingInputRef, RunnerProblem, RunnerSnapshot,
};
use crate::persistence::ExecutionStore;
use crate::workflow::TemplateScope;

use super::RunnerError;

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
    /// Why the runner hosts no conductor; `None` while it hosts one.
    pub conductor_unavailable: Option<&'a str>,
    /// The default harness the task cannot switch to, while it cannot.
    pub default_harness_unavailable: Option<&'a HarnessUnavailable>,
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
            conductor_available: context.conductor_unavailable.is_none(),
            conductor_unavailable_reason: context.conductor_unavailable.map(str::to_string),
            default_harness_unavailable: context.default_harness_unavailable.cloned(),
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
    let timeline = build_timeline(&events, execution.status, &recorded);

    let observed_at = Utc::now();
    let pending = engine.pending_input(execution_id)?;
    let mut attention = attention_items(pending.as_ref(), context.problem.as_ref(), observed_at);
    attention.extend(conductor_attention(
        store,
        &execution,
        context.conductor_unavailable.is_none(),
    )?);
    attention.sort_by_key(|item| item.since);
    let pending_input = pending.map(|pending| PendingInputRef {
        id: pending.episode_key().to_string(),
        attempt_id: (pending.continuation == InputContinuation::ResumeAgent)
            .then(|| pending.action_execution_id().to_string()),
        form: pending,
    });

    let conversation = if !context.stopping {
        ConversationAvailability::Live {
            attempts: summaries(&conversations, &recorded, &timeline, cache)?.attempts,
        }
    } else {
        ConversationAvailability::Unavailable {
            reason: "the runner is stopping".to_string(),
        }
    };

    Ok(RunnerSnapshot {
        snapshot_sequence: 0,
        execution_status: execution.status,
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
        conductor_available: context.conductor_unavailable.is_none(),
        conductor_unavailable_reason: context.conductor_unavailable.map(str::to_string),
        default_harness_unavailable: context.default_harness_unavailable.cloned(),
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

/// What waits on the owner because of a step that failed or a proposal of
/// the conductor's (U5).
///
/// A failed step is waiting whether or not a conductor looks at it: with
/// one the owner finds its diagnosis and proposals in the conductor's
/// state, without one the bare failure. Its identity is the failure's key,
/// the one a restart of the step names. A proposal from the chat counts
/// only while a conductor runs to carry it out.
fn conductor_attention(
    store: &ExecutionStore,
    execution: &WorkflowExecution,
    conductor_available: bool,
) -> Result<Vec<AttentionItem>, RunnerError> {
    let mut items = Vec::new();
    if execution.status == ExecutionStatus::Failed {
        if let Ok(Some(failure)) = step_failure(execution) {
            items.push(AttentionItem {
                kind: AttentionKind::FailedStep,
                state_id: Some(failure.state_id.to_string()),
                since: execution.completed_at.unwrap_or(execution.started_at),
                waiting_id: failure.key().to_string(),
            });
        }
    }
    if conductor_available {
        for proposal in pending_chat_proposals(store, &execution.id)? {
            items.push(AttentionItem {
                kind: AttentionKind::ConductorProposal,
                state_id: proposal.record.state_id.clone(),
                since: proposal.at,
                waiting_id: proposal.proposal_id,
            });
        }
    }
    Ok(items)
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::{Context, EventData, Workspace};
    use crate::engine::conversation_summary::read_summaries;

    const WORKFLOW: &str = r#"
name: summaries
version: 1
states:
  plan:
    action:
      type: script
      command: "true"
    outputs: [done]
    transitions:
      done: completed
"#;

    struct TempRoot(std::path::PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn execution(id: &str) -> WorkflowExecution {
        WorkflowExecution {
            id: id.to_string(),
            workflow_id: "summaries".to_string(),
            workflow_version: 1,
            status: ExecutionStatus::Running,
            current_state: Some("plan".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            inputs: serde_json::json!({}),
            context: Context::new(),
            workspace: Workspace::local_folder("/tmp/none".to_string()),
            current_action_execution: None,
            metadata: serde_json::json!({}),
        }
    }

    fn started(attempt: &str) -> EventData {
        EventData::ActionStarted {
            action_execution_id: attempt.to_string(),
            state_id: "plan".to_string(),
            action_type: "agent".to_string(),
        }
    }

    /// The daemon reads a stopped local task's summaries with the very
    /// function the runner's snapshot uses: both say the same of the same
    /// storage.
    #[test]
    fn the_snapshot_summarises_attempts_like_a_read_of_the_storage() {
        let root = TempRoot(
            std::env::temp_dir().join(format!("narsor-snapshot-{}", uuid::Uuid::new_v4())),
        );
        let store = ExecutionStore::new(&root.0);
        let execution = execution("exec-1");
        store.save_execution(&execution).expect("execution");
        let log = store.event_log("exec-1");
        log.append_next(&execution.id, started("act-2"))
            .expect("event");
        log.append_next(
            &execution.id,
            EventData::ActionCompleted {
                action_execution_id: "act-2".to_string(),
                output_label: "done".to_string(),
            },
        )
        .expect("event");
        log.append_next(&execution.id, started("act-1"))
            .expect("event");
        let conversations = store.conversation_store("exec-1");
        for (attempt, lines) in [("act-2", 2), ("act-1", 3)] {
            let mut writer = conversations.writer(attempt).expect("writer");
            for line in 0..lines {
                writer.append_narsor(&format!("line {line}")).expect("line");
            }
            writer.finish().expect("finish");
        }
        conversations.close_attempt("act-2").expect("close");

        let engine = Engine::new(store.clone());
        let definition = crate::workflow::load_from_str(WORKFLOW).expect("workflow");
        let snapshot = build(
            &engine,
            &mut ConversationCache::default(),
            SnapshotContext {
                definition: &definition,
                execution_id: Some("exec-1"),
                pass_active: false,
                problem: None,
                stopping: false,
                conductor_unavailable: Some("no conductor in this test"),
                default_harness_unavailable: None,
            },
        )
        .expect("snapshot");
        let ConversationAvailability::Live { attempts } = snapshot.conversation else {
            panic!("a live conversation: {:?}", snapshot.conversation);
        };
        let stored =
            read_summaries(&store, "exec-1", &mut ConversationCache::default()).expect("stored");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts, stored.attempts);
    }
}
