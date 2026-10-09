//! Hand-built tasks, listings and stream events for the state and render tests
//! of the task application (U8). Nothing here needs a server: the application
//! takes a listing with `TaskApp::load` and an event with `TaskApp::on_stream`,
//! exactly as the loop would hand them over.

#![allow(dead_code)]

use chrono::{DateTime, TimeZone, Utc};
use narsor::controller_protocol::{
    ControlEventEnvelope, Freshness, QueueView, RunnerView, TaskListResponse, TaskSummary,
    TimelineResponse,
};
use narsor::domain::display::DisplayLink;
use narsor::domain::{Event, EventData, ExecutionStatus, TaskId};
use narsor::engine::{ConversationState, Position, PositionStatus, Timeline};
use narsor::orchestration::{
    AttentionItem, AttentionKind, ExecutionCursor, QueueReason, TaskEvent, TaskEventKind,
    TaskLifecycle,
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

pub fn waiting_for_capacity(mut task: TaskSummary, position: u32, running: u32, limit: u32) -> TaskSummary {
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
        task_id: TaskId::parse(task).expect("a task id"),
        freshness: Freshness::Live,
        observed_at: Some(at()),
        timeline: Some(Timeline {
            default_position: positions.last().map(|position| position.label.clone()),
            positions,
            ..Timeline::default()
        }),
        execution_status: Some(ExecutionStatus::Running),
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
