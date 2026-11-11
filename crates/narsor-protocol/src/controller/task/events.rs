//! Durable task events and the control-plane event stream.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::ExecutionStatusDto;
use crate::controller::{
    AttentionItem, InputStatus, Observation, OperatorCommand, OperatorCommandStatus, OwnerCommand,
    OwnerRef, QueueReason, RunnerState,
};

/// Change recorded in one accepted task mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TaskEventKind {
    Created,
    OwnerCommandAccepted {
        command: OwnerCommand,
        idempotency_key: String,
    },
    ControllerReported {
        observation: Observation,
        lease_epoch: u64,
    },
    RunnerReported {
        incarnation: String,
        event_cursor: u64,
        execution_status: ExecutionStatusDto,
        pending_input_id: Option<String>,
        #[serde(default)]
        attention: Vec<AttentionItem>,
    },
    InputQueued {
        input_id: String,
        pending_input_id: String,
    },
    /// The owner commented on a paused task; the comment waits for the
    /// runner of the next launch.
    ResumeMessageStored {
        idempotency_key: String,
    },
    /// The runner of a launch gave the comment on the pause to its agent.
    ResumeMessageTaken {
        idempotency_key: String,
    },
    OperatorCommandQueued {
        command_id: String,
        command: OperatorCommand,
        requested_by: OwnerRef,
    },
    OperatorCommandResolved {
        command_id: String,
        status: OperatorCommandStatus,
        detail: Option<String>,
    },
    PriorityChanged {
        priority: i32,
    },
    Moved {
        rank: u32,
        order: Vec<crate::identifier::TaskId>,
    },
    QueueChanged {
        reason: QueueReason,
    },
    InputsUpdated {
        updates: Vec<InputUpdate>,
    },
    MaterialChanged {
        material_id: String,
        change: MaterialChange,
        files: u32,
        total_bytes: u64,
    },
    AutoResumed {
        attempt: u32,
        limit: u32,
        #[serde(default)]
        loss_reason: Option<String>,
    },
    AutoResumeExhausted {
        attempts: u32,
        reason: String,
    },
    /// The control plane planned (or moved) the time at which it frees the
    /// slot of a task that waits for its owner.
    SlotReleaseScheduled {
        due_at: DateTime<Utc>,
    },
    /// The control plane freed the task's slot: it asked the runner to stop
    /// at a safe point.
    SlotReleased {
        reason: SlotReleaseReason,
    },
    /// A lowered limit asked this task to stop, and the control plane
    /// took the request back at the safe point: the overshoot was gone.
    LimitStopWithdrawn,
    /// An owner is reading a stopped task: the control plane asked for a
    /// read-only runner until `until` (D63).
    ReaderLeased {
        until: DateTime<Utc>,
    },
    /// The read-only runner's lease ended, because it ran idle: the control
    /// plane no longer asks for it.
    ReaderLeaseEnded,
    #[serde(other)]
    Unrecognized,
}

/// Why the control plane freed a task's slot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlotReleaseReason {
    /// The task's execution ended (completed or cancelled).
    Terminal,
    /// The task waited for its owner (input needed, failed) and the wait
    /// ran out.
    GraceElapsed,
    /// The backend's limit of concurrent tasks went below the number of
    /// tasks that run, and this task was the first to reach a safe point.
    LimitLowered,
    #[serde(other)]
    Unrecognized,
}

/// Material runner state change recorded in a task event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialChange {
    Uploaded,
    Published,
    Delivered,
    Deleted,
    Expired,
}

/// One queued input status change in a task event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputUpdate {
    pub input_id: String,
    pub status: InputStatus,
}

/// Durable task event, also used by the owner event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskEvent {
    pub sequence: u64,
    pub task_id: crate::identifier::TaskId,
    pub at: DateTime<Utc>,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub generation: u64,
    pub kind: TaskEventKind,
}

/// One frame of the control-plane event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlEventEnvelope {
    pub cursor: String,
    pub event: TaskEvent,
}
