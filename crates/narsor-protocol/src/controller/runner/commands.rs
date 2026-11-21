//! Desired-state commands and reports exchanged with a backend controller.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::ExecutionStatusDto;
use crate::chat::is_false;
use crate::controller::{Observation, ObservedSummary, ResourceIdentity, RunnerState};
use crate::identifier::TaskId;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandsQuery {
    #[serde(default)]
    pub wait_secs: Option<u64>,
    #[serde(default)]
    pub all: bool,
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
}

/// Runner progress visible to an environment controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerProgress {
    pub incarnation: String,
    pub launch_generation: u64,
    pub execution_status: ExecutionStatusDto,
    pub event_cursor: u64,
    pub safe_point_acknowledged: bool,
    pub received_at: DateTime<Utc>,
}

/// Desired state sent to a backend controller. It carries no launch spec:
/// the controller fetches one only to create a workload or start a reader
/// ([`LaunchSpecQuery`]), and accepts it only when its recomputed content
/// digest equals [`Self::launch_digest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandsResponse {
    pub lease_epoch: u64,
    pub commands: Vec<DesiredCommand>,
}
