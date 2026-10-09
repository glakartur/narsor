//! Hand-built tasks, listings and stream events for the state and render tests
//! of the task application (U8). Nothing here needs a server: the application
//! takes a listing with `TaskApp::load` and an event with `TaskApp::on_stream`,
//! exactly as the loop would hand them over.

#![allow(dead_code)]

use chrono::{DateTime, TimeZone, Utc};
use narsor::controller_protocol::{
    ControlEventEnvelope, QueueView, RunnerView, TaskListResponse, TaskSummary,
};
use narsor::domain::{ExecutionStatus, TaskId};
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
