//! Owner commands on a task's desired runner state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controller::{RunnerState, TaskSummary};

/// Actor recorded for a runner state command receipt.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CommandActor {
    #[default]
    Owner,
    Policy,
}

/// Opaque owner reference stored in task command records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct OwnerRef(pub String);

impl OwnerRef {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Receipt for an accepted owner command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandReceipt {
    pub idempotency_key: String,
    pub command: OwnerCommand,
    pub expected_generation: Option<u64>,
    pub accepted_at: DateTime<Utc>,
    pub generation_after: u64,
    #[serde(alias = "lifecycle_after")]
    pub runner_state_after: RunnerState,
    pub revision: u64,
    #[serde(default)]
    pub actor: CommandActor,
}

/// Response of an owner command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnerCommandResponse {
    pub receipt: CommandReceipt,
    pub replayed: bool,
    pub task: TaskSummary,
    /// The command was a resume that found the backend full: the task waits
    /// in the queue (at its front) for a slot instead of starting now.
    /// Derived from the receipt, so a replay says the same. Absent from an
    /// older control plane, which refused such a resume instead.
    #[serde(default)]
    pub queued_for_slot: bool,
}

/// Where an owner asks the control plane to place a queued task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "to", content = "task_id")]
pub enum MoveTaskTarget {
    /// Directly before the named task.
    Before(String),
    /// Directly after the named task.
    After(String),
    /// First of the backend's tasks.
    Top,
    /// Last of the backend's tasks.
    Bottom,
}

/// `POST /control/v1/tasks/:task_id/move`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MoveTaskRequest {
    /// Where the queued task goes. Repeating a move that already holds is
    /// a no-op, which makes a retry safe without a key.
    pub target: MoveTaskTarget,
    /// Refuse unless the task is still at this desired generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// An owner's requested change to a task's desired runner state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OwnerCommand {
    /// Retry an interrupted or unknown task.
    Start,
    /// Bring a running task to a safe point and release its compute.
    Pause,
    /// Free the slot of a running task that only waits for the owner: the
    /// pause of [`OwnerCommand::Pause`], sent before the wait policy would.
    /// Refused unless the task waits in a state that frees its slot.
    ReleaseSlot,
    /// Continue a paused, interrupted or unknown task on the same storage.
    Resume,
    /// Delete workload and storage; archive wins over every other state.
    Archive,
}

/// `POST /control/v1/tasks/:task_id/commands`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerCommandRequest {
    /// The command.
    pub command: OwnerCommand,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Refuse unless the task is still at this generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}
