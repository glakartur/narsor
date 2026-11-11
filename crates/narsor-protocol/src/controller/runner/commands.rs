//! Desired-state commands and reports exchanged with a backend controller.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Desired state sent to a backend controller. The sealed launch spec is
/// carried as JSON so the wire contract does not import the engine model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredCommand {
    pub task_id: TaskId,
    pub generation: u64,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub action: DesiredAction,
    pub resource_key: String,
    pub launch_spec: Value,
    pub runner: Option<RunnerProgress>,
    /// Whether the control plane wants a read-only runner for the task
    /// alongside (D63): an owner is reading a stopped or interrupted task and
    /// its reader lease has not run out. The controller starts one, or
    /// removes the one it started, to match. Absent from an older control
    /// plane, which never asks for one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reader_needed: bool,
}

/// Controller command response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandsResponse {
    pub lease_epoch: u64,
    pub commands: Vec<DesiredCommand>,
}
