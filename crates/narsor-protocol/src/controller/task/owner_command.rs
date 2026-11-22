//! Owner commands on a task's desired runner state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controller::{EnvironmentDiff, RunnerState, TaskSummary};

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

/// `POST /control/v1/tasks/:task_id/refresh-environment`.
///
/// The body names no requester: the server records the owner it
/// authenticated, and ignores any other field a client sends.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshEnvironmentRequest {
    /// Refuse unless the task is still at this desired generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// Response of an environment refresh: what happened, and the task after it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RefreshEnvironmentResponse {
    pub outcome: EnvironmentRefreshResult,
    pub task: TaskSummary,
}

/// What an environment refresh did to the task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum EnvironmentRefreshResult {
    /// The launch spec now carries the environment the catalogue resolves
    /// to, under a new generation.
    Refreshed {
        /// The environment identity before the refresh.
        old_identity: String,
        /// The environment identity after the refresh.
        new_identity: String,
        /// The task's desired generation after the refresh.
        generation: u64,
        /// How many parts changed: [`EnvironmentDiff::change_count`], so a
        /// client need not count the diff itself.
        #[serde(default)]
        change_count: usize,
        /// What changed, part by part.
        #[serde(default)]
        diff: Box<EnvironmentDiff>,
    },
    /// The catalogue resolves to the environment the task already has:
    /// nothing was stored and no event was recorded.
    Unchanged {
        /// The environment identity the task keeps.
        #[serde(default)]
        identity: String,
    },
    /// An outcome a newer control plane sent that this client does not know.
    #[serde(other)]
    Unrecognized,
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
