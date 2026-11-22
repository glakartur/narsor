//! Desired-state commands and reports exchanged with a backend controller.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::chat::is_false;
use crate::controller::{Observation, ObservedSummary, ResourceIdentity, RunnerState};
use crate::identifier::TaskId;

/// `GET /controller/v1/commands`: a read of the backend's desired state.
///
/// Without `since` the read is full: every task, at once. With `since` —
/// the [`CommandsResponse::cursor`] of the last response the controller
/// applied — it is incremental: only the tasks whose desired state changed
/// since that response, waiting up to `wait_secs` for one to change. A
/// cursor the control plane no longer knows (it restarted, the lease
/// changed) is answered with a full read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandsQuery {
    #[serde(default)]
    pub wait_secs: Option<u64>,
    #[serde(default)]
    pub since: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandAck {
    pub task_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AckRequest {
    pub acks: Vec<CommandAck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AckResponse {
    pub acknowledged: Vec<CommandAck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportRequest {
    pub task_id: String,
    pub lease_epoch: u64,
    pub desired_generation: u64,
    pub observation: Observation,
    #[serde(default)]
    pub resource: Option<ResourceIdentity>,
    #[serde(default)]
    pub summary: ObservedSummary,
    pub observed_at: DateTime<Utc>,
}

/// `POST /controller/v1/reports` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportResponse {
    /// False for an absorbed duplicate.
    pub applied: bool,
    /// Lifecycle afterwards.
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    /// Desired generation afterwards.
    pub generation: u64,
}

/// Action a backend controller should take for a desired task generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesiredAction {
    Start,
    Provision,
    Run,
    PauseAtSafePoint,
    KeepPaused,
    Hold,
    Archive,
    /// The task left the backend's desired state (archived, or no longer
    /// admitted): the controller drops what it holds about it. Sent once,
    /// in an incremental read only; a full read just leaves the task out.
    Forget,
}

/// The runner progress an environment controller acts on. Nothing that
/// moves with every runner snapshot (event cursors, timestamps) belongs
/// here: a change of a command wakes the controller (KTD1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProgress {
    pub incarnation: String,
    pub launch_generation: u64,
    pub safe_point_acknowledged: bool,
}

/// Desired state sent to a backend controller. It carries no launch spec:
/// the controller fetches one only to create a workload or start a reader
/// ([`LaunchSpecQuery`]), and accepts it only when its recomputed content
/// digest equals [`Self::launch_digest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesiredCommand {
    pub task_id: TaskId,
    pub generation: u64,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub action: DesiredAction,
    pub resource_key: String,
    /// Digest of the task's sealed launch spec: what a fetched spec must
    /// hash to.
    pub launch_digest: String,
    pub runner: Option<RunnerProgress>,
    /// Whether the control plane wants a read-only runner for the task
    /// alongside (D63): an owner is reading a stopped or interrupted task and
    /// its reader lease has not run out. The controller starts one, or
    /// removes the one it started, to match. Absent from an older control
    /// plane, which never asks for one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reader_needed: bool,
}

/// `GET /controller/v1/tasks/:task_id/launch-spec`: the task's sealed
/// launch spec for this desired generation. Refused with `stale_generation`
/// when the task moved on, and to a controller not holding the lease.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchSpecQuery {
    pub generation: u64,
}

/// Controller command response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandsResponse {
    pub lease_epoch: u64,
    /// What to send as [`CommandsQuery::since`] once this response is
    /// applied.
    pub cursor: u64,
    /// True when `commands` is the whole desired state: a task the
    /// controller holds that is not in it is no longer wanted. False when it
    /// holds only the tasks that changed (and a `Forget` for each that left).
    pub full: bool,
    pub commands: Vec<DesiredCommand>,
}
