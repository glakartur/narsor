//! Hand-built tasks, listings and stream events for the state and render tests
//! of the task application (U8). Nothing here needs a server: the application
//! takes a listing with `TaskApp::load` and an event with `TaskApp::on_stream`,
//! exactly as the loop would hand them over.

#![allow(dead_code)]

use chrono::{DateTime, TimeZone, Utc};
use narsor_engine::controller_protocol::{
    ControlEventEnvelope, Freshness, QueueView, RunnerView, TaskListResponse, TaskSummary,
    TimelineResponse,
};
use narsor_engine::domain::display::DisplayLink;
use narsor_engine::domain::{Event, EventData, ExecutionStatus, TaskId};
use narsor_engine::engine::{ConversationState, Position, PositionStatus, Timeline};
use narsor_engine::orchestration::{
    AttentionItem, AttentionKind, ExecutionCursor, QueueReason, ResumeExhaustion, ResumeState,
    TaskEvent, TaskEventKind, TaskLifecycle, WorkloadLoss,
};

pub fn at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}

/// A task of `backend` in `lifecycle` (a snake_case word) at queue position
/// `rank`, revision 1, generation 1.
pub fn task(id: &str, backend: &str, lifecycle: &str, rank: u32) -> TaskSummary {
    serde_json::from_value(serde_json::json!({
        "task_id": id,
        "backend_id": backend,
        "lifecycle": lifecycle,
        "generation": 1,
        "revision": 1,
        "rank": rank,
        "created_at": at(),
        "queue_sequence": rank,
        "workflow_id": "implement-feature",
        "workflow_version": 3,
        "launch_digest": "digest",
        "environment_identity": "env",
        "archive": null,
        "observation": null,
        "runner": null,
        "controller": null,
        "inputs": [],
        "as_of": at(),
    }))
    .expect("a task summary")
}

pub fn queued(id: &str, rank: u32) -> TaskSummary {
    task(id, "local", "queued", rank)
}

pub fn titled(mut task: TaskSummary, title: &str) -> TaskSummary {
    task.display.title = title.to_string();
    task
}

/// The runner reports `status`, in the workflow state `step`.
pub fn with_runner(mut task: TaskSummary, status: ExecutionStatus, step: &str) -> TaskSummary {
    task.runner = Some(runner(status, step));
    task
}

pub fn runner(status: ExecutionStatus, step: &str) -> RunnerView {
    serde_json::from_value(serde_json::json!({
        "incarnation": "pod-1",
        "execution_status": status,
        "execution_cursor": ExecutionCursor {
            execution_id: Some("exec-1".to_string()),
            current_state: Some(step.to_string()),
            action_execution_id: None,
        },
        "event_cursor": 1,
        "pending_input": null,
        "safe_point": false,
        "observed_at": at(),
        "received_at": at(),
        "last_contact_at": at(),
        "freshness": "live",
    }))
    .expect("a runner view")
}

/// A live controller holds the task's backend.
pub fn with_controller(mut task: TaskSummary) -> TaskSummary {
    task.controller = Some(
        serde_json::from_value(serde_json::json!({
            "controller_id": "controller-1",
            "lease_epoch": 1,
            "lease_expires_at": at(),
            "last_heartbeat_at": at(),
            "freshness": "live",
        }))
        .expect("a controller"),
    );
    task
}

pub fn waiting(mut task: TaskSummary, kind: AttentionKind) -> TaskSummary {
    task.attention.push(AttentionItem {
        kind,
        state_id: Some("review".to_string()),
        since: at(),
        waiting_id: format!("{kind:?}-{}", task.task_id),
    });
    task
}

pub fn queue_view(reason: QueueReason) -> QueueView {
    QueueView {
        message: reason.describe(),
        reason,
        since: at(),
    }
}

pub fn waiting_for_capacity(
    mut task: TaskSummary,
    position: u32,
    running: u32,
    limit: u32,
) -> TaskSummary {
    task.queue = Some(queue_view(QueueReason::WaitingForCapacity {
        position,
        running,
        limit,
    }));
    task
}

pub fn listing(tasks: Vec<TaskSummary>, cursor: &str) -> TaskListResponse {
    TaskListResponse {
        tasks,
        cursor: cursor.to_string(),
    }
}

/// One stream item: `kind` happened to `task` as its `sequence`-th event.
pub fn event(
    cursor: &str,
    task: &str,
    sequence: u64,
    lifecycle: TaskLifecycle,
    kind: TaskEventKind,
) -> Option<Result<ControlEventEnvelope, narsor::api::ClientError>> {
    Some(Ok(ControlEventEnvelope {
        cursor: cursor.to_string(),
        event: TaskEvent {
            sequence,
            task_id: TaskId::parse(task).expect("a task id"),
            at: at(),
            lifecycle,
            generation: 1,
            kind,
        },
    }))
}

pub fn ids(list: &[&str]) -> Vec<TaskId> {
    list.iter().map(|id| TaskId::parse(*id).unwrap()).collect()
}

/// The task shows `title` and these links: `(label, text, url)`.
pub fn displayed(
    mut task: TaskSummary,
    title: &str,
    links: &[(&str, &str, Option<&str>)],
) -> TaskSummary {
    task.display.title = title.to_string();
    task.display.links = links
        .iter()
        .map(|(label, text, url)| DisplayLink {
            label: (*label).to_string(),
            text: (*text).to_string(),
            url: url.map(str::to_string),
        })
        .collect();
    task
}

/// An attempt `"<state> #1"` of an agent action with `status`, and no
/// conversation record.
pub fn position(state: &str, status: PositionStatus) -> Position {
    Position {
        label: format!("{state} #1"),
        state_id: state.to_string(),
        visit: 1,
        action_execution_id: Some(format!("act-{state}")),
        action_type: Some("agent".to_string()),
        status,
        started_at: Some(at()),
        ended_at: None,
        error: None,
        resumed_harness_session: None,
        harness_session_interrupted: false,
        retry_pending: false,
        conversation: ConversationState::NoRecord,
    }
}

/// What the runner reported as the timeline of `task`: `positions`, and the
/// step it is at.
pub fn timeline(task: &str, positions: Vec<Position>, current: Option<&str>) -> TimelineResponse {
    TimelineResponse {
        task_id: task.to_owned(),
        freshness: Freshness::Live,
        observed_at: Some(at()),
        timeline: Some(Timeline {
            default_position: positions.last().map(|position| position.label.clone()),
            positions,
            ..Timeline::default()
        }),
        execution_status: Some(narsor_protocol::api::ExecutionStatusDto::Running),
        current_state: None,
        current_position: current.map(str::to_string),
    }
}

/// `n` events of an execution, sequences `1..=n`.
pub fn execution_events(n: u64) -> Vec<Event> {
    (1..=n)
        .map(|sequence| {
            Event::new(
                "exec-1".to_string(),
                EventData::StateEntered {
                    state_id: format!("state-{sequence}"),
                },
                sequence,
            )
        })
        .collect()
}

/// The form a runner reports as the pending input of a task, from the JSON of
/// an engine `PendingHumanInput`: the task's runner holds it and the row says
/// it waits (the kind follows the form's `continuation`).
pub fn with_pending(mut task: TaskSummary, form: serde_json::Value) -> TaskSummary {
    let wire_form = form.clone();
    let form: narsor_engine::engine::PendingHumanInput =
        serde_json::from_value(form).expect("a pending input");
    let kind = match form.continuation {
        narsor_engine::engine::InputContinuation::ResolveOutput => AttentionKind::Problem,
        narsor_engine::engine::InputContinuation::ApproveOutput => AttentionKind::Approval,
        _ => AttentionKind::Question,
    };
    let id = form.episode_key().to_string();
    let runner = task
        .runner
        .get_or_insert_with(|| runner(ExecutionStatus::InputNeeded, &form.state_id));
    runner.pending_input = Some(narsor_engine::controller_protocol::PendingInputRefView {
        id: id.clone(),
        attempt_id: None,
        form: wire_form,
    });
    task.attention.push(AttentionItem {
        kind,
        state_id: Some(form.state_id.to_string()),
        since: at(),
        waiting_id: id,
    });
    task
}

/// The task waits on `kind` with the identity `waiting_id`.
pub fn waiting_on(
    mut task: TaskSummary,
    kind: AttentionKind,
    waiting_id: &str,
    state: Option<&str>,
) -> TaskSummary {
    task.attention.push(AttentionItem {
        kind,
        state_id: state.map(str::to_string),
        since: at(),
        waiting_id: waiting_id.to_string(),
    });
    task
}

/// The state `task`'s runner is in offers `exits` — `(label, target,
/// description)` — and, when `override_targets` is given, an override to any
/// of them.
pub fn with_transitions(
    mut task: TaskSummary,
    from: &str,
    exits: &[(&str, &str, Option<&str>)],
    override_targets: Option<&[&str]>,
) -> TaskSummary {
    task.manual_transitions = Some(narsor_engine::controller_protocol::TransitionOffer {
        from_state: from.to_string(),
        exits: exits
            .iter()
            .map(|(label, target, description)| {
                narsor_engine::controller_protocol::TransitionExit {
                    label: (*label).to_string(),
                    target: (*target).to_string(),
                    description: description.map(str::to_string),
                }
            })
            .collect(),
        override_allowed: override_targets.is_some(),
        override_targets: override_targets
            .unwrap_or_default()
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
    });
    task
}

/// The runner of `task` reports a conductor that works.
pub fn with_conductor(mut task: TaskSummary) -> TaskSummary {
    if let Some(runner) = task.runner.as_mut() {
        runner.conductor_available = true;
        runner.conductor_unavailable_reason = None;
    }
    task
}

/// The runner of `task` reports that it has no conductor, and why.
pub fn without_conductor(mut task: TaskSummary, reason: &str) -> TaskSummary {
    if let Some(runner) = task.runner.as_mut() {
        runner.conductor_available = false;
        runner.conductor_unavailable_reason = Some(reason.to_string());
    }
    task
}

/// The runner of `task` reports that it cannot switch to the default
/// `harness`, keeps `kept`, and says why (U7, R16c).
pub fn with_harness_unavailable(
    mut task: TaskSummary,
    harness: &str,
    kept: &str,
    reason: &str,
) -> TaskSummary {
    if let Some(runner) = task.runner.as_mut() {
        runner.default_harness_unavailable =
            Some(narsor_engine::orchestration::HarnessUnavailable {
                harness: harness.to_string(),
                kept: kept.to_string(),
                reason: reason.to_string(),
            });
    }
    task
}

/// One chat entry of a conductor, as the runner would serve it.
pub fn chat_entry(
    id: &str,
    sequence: u64,
    author: &str,
    kind: &str,
    text: &str,
    payload: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "sequence": sequence,
        "at": at(),
        "author": author,
        "kind": kind,
        "text": text,
        "payload": payload,
    })
}

/// The view of a conductor that runs on `claude-code` and has said `chat`;
/// `extra` adds or replaces fields (`decisions`, `proposals`, …).
pub fn conductor_view(
    chat: Vec<serde_json::Value>,
    extra: serde_json::Value,
) -> narsor_protocol::controller::ConductorView {
    let mut view = serde_json::json!({
        "available": true,
        "harness": "claude-code",
        "chat": chat,
    });
    if let (Some(view), Some(extra)) = (view.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            view.insert(key.clone(), value.clone());
        }
    }
    serde_json::from_value(view).expect("a conductor view")
}

// ---------------------------------------------------------------------------
// The new-task form, the history and the repositories (U13)
// ---------------------------------------------------------------------------

use narsor::api::LegacyExecutionView;
use narsor_engine::domain::ParamType;
use narsor_engine::orchestration::ArchiveTombstone;
use narsor_protocol::controller::WorkflowInputType;
use narsor_protocol::controller::{
    CapabilitiesView, RepositoryListResponse, RepositoryView, WorkflowInputView,
    WorkflowListResponse, WorkflowSource, WorkflowView,
};

/// An input a workflow declares.
pub fn workflow_input(
    name: &str,
    kind: ParamType,
    required: bool,
    description: Option<&str>,
) -> WorkflowInputView {
    WorkflowInputView {
        name: name.to_string(),
        input_type: match kind {
            ParamType::String => WorkflowInputType::String,
            ParamType::Text => WorkflowInputType::Text,
            ParamType::Integer => WorkflowInputType::Integer,
            ParamType::Boolean => WorkflowInputType::Boolean,
            ParamType::Repository => WorkflowInputType::Repository,
        },
        required,
        description: description.map(str::to_string),
        default: None,
    }
}

/// A workflow of the catalogue that loads, at version 1.
pub fn catalogue_workflow(
    name: &str,
    description: &str,
    inputs: Vec<WorkflowInputView>,
) -> WorkflowView {
    WorkflowView {
        name: name.to_string(),
        source: WorkflowSource::BuiltIn,
        description: Some(description.to_string()),
        version: Some(1),
        state_count: 1,
        initial_state: Some("start".to_string()),
        inputs,
        valid: true,
        violations: Vec::new(),
        overrides: false,
        origin_label: "built-in".to_string(),
        shadow_labels: Vec::new(),
        has_builtin: Some(true),
    }
}

pub fn catalogue(workflows: Vec<WorkflowView>) -> WorkflowListResponse {
    WorkflowListResponse { workflows }
}

/// What a server offers: `paths` says whether it reads workflow files.
pub fn capabilities(paths: bool, reason: Option<&str>) -> CapabilitiesView {
    CapabilitiesView {
        connection_name: if paths { "local" } else { "staging" }.to_string(),
        workflow_paths: paths,
        workflow_paths_reason: reason.map(str::to_string),
        legacy_executions: paths,
        local_repository_paths: paths,
    }
}

pub fn repository(id: &str, remote: &str) -> RepositoryView {
    RepositoryView {
        id: id.to_string(),
        name: id.to_string(),
        provider: "github".to_string(),
        remote: remote.to_string(),
        default_branch: Some("main".to_string()),
        description: None,
    }
}

pub fn registry(repositories: Vec<RepositoryView>) -> RepositoryListResponse {
    RepositoryListResponse {
        repositories,
        unreadable: Vec::new(),
    }
}

/// An archived task, archived `hours` hours after the fixed moment of the
/// fixtures.
pub fn archived(id: &str, title: &str, hours: i64) -> TaskSummary {
    let mut task = titled(task(id, "local", "archived", 1), title);
    let requested_at = at() + chrono::Duration::hours(hours);
    task.archive = Some(ArchiveTombstone {
        idempotency_key: format!("archive-{id}"),
        generation: 2,
        requested_at,
        completed_at: Some(requested_at),
        resources_never_created: false,
    });
    task
}

/// An execution from before the task scheduler, started `hours` hours after
/// the fixed moment of the fixtures.
pub fn legacy(
    id: &str,
    workflow: &str,
    status: &str,
    hours: i64,
    recoverable: bool,
    reason: Option<&str>,
) -> LegacyExecutionView {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "workflow_id": workflow,
        "workflow_version": 1,
        "status": status,
        "current_state": "review",
        "started_at": at() + chrono::Duration::hours(hours),
        "completed_at": null,
        "problem": null,
        "recoverable": recoverable,
        "not_recoverable_reason": reason,
    }))
    .expect("a legacy execution")
}

// ---------------------------------------------------------------------------
// The installation's default harness (U5)
// ---------------------------------------------------------------------------

/// The daemon's default harness: `configured` from the configuration,
/// `override_harness` chosen over it, `supported` in the daemon's order.
pub fn harness_selection(
    configured: &str,
    override_harness: Option<&str>,
    supported: &[&str],
) -> narsor_engine::api::HarnessSelectionSnapshot {
    narsor_engine::api::HarnessSelectionSnapshot {
        supported: supported.iter().map(|name| (*name).to_string()).collect(),
        configured: configured.to_string(),
        override_harness: override_harness.map(str::to_string),
        effective: override_harness.unwrap_or(configured).to_string(),
        revision: 1,
    }
}

/// The harnesses a daemon of the tests supports, in its order.
pub const HARNESSES: [&str; 3] = ["claude-code", "codex", "opencode"];

/// An `Interrupted` task whose loss the control plane counted as `losses`
/// in a row, lost from `from`; `planned` says an automatic resume is due.
pub fn interrupted(id: &str, from: TaskLifecycle, losses: u32, planned: bool) -> TaskSummary {
    let mut task = task(id, "local", "interrupted", 1);
    task.resume = ResumeState {
        consecutive_losses: losses,
        next_attempt_at: planned.then(at),
        last_loss: Some(WorkloadLoss {
            from,
            at: at(),
            lease_epoch: 1,
            reason: None,
        }),
        ..ResumeState::default()
    };
    task
}

/// The resume policy gave up on `task` with `reason` (the server's words).
pub fn exhausted(mut task: TaskSummary, reason: &str) -> TaskSummary {
    task.resume.next_attempt_at = None;
    task.resume.exhausted = Some(ResumeExhaustion {
        reason: reason.to_string(),
        at: at(),
        announced: true,
    });
    task
}

/// The stream's `AutoResumed` of `task` at `sequence`: attempt `attempt` of
/// five, the task back to provisioning.
pub fn auto_resumed(
    cursor: &str,
    task: &str,
    sequence: u64,
    attempt: u32,
) -> Option<Result<ControlEventEnvelope, narsor::api::ClientError>> {
    event(
        cursor,
        task,
        sequence,
        TaskLifecycle::Provisioning,
        TaskEventKind::AutoResumed {
            attempt,
            limit: 5,
            loss_reason: None,
        },
    )
}
