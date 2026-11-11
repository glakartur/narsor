//! Runner snapshot request and response DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{ExecutionStatusDto, Timeline};
use crate::controller::{
    AttentionItem, CommandActor, ConversationAvailability, ExecutionCursor, HarnessUnavailable,
    OwnerCommand, RunnerDirective, RunnerProblem, RunnerState, TaskDefaultHarness,
};
use crate::display::WorkspaceHeader;

/// Pending input metadata as reported by a runner. The form remains a JSON
/// value so the protocol does not depend on the engine's durable form model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingInputRefView {
    pub id: String,
    pub attempt_id: Option<String>,
    pub form: Value,
}

/// The engine-independent body of `POST /runner/v1/snapshots`.
///
/// The pending form is JSON at this boundary; the control plane validates it
/// against its internal form model after decoding the request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerSnapshotRequest {
    pub snapshot_sequence: u64,
    pub execution_status: ExecutionStatusDto,
    #[serde(default)]
    pub execution_cursor: ExecutionCursor,
    pub event_cursor: u64,
    #[serde(default)]
    pub pending_input: Option<PendingInputRefView>,
    #[serde(default)]
    pub conversation: ConversationAvailability,
    #[serde(default)]
    pub timeline: Option<Timeline>,
    #[serde(default)]
    pub safe_point: bool,
    #[serde(default)]
    pub problem: Option<RunnerProblem>,
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
    #[serde(default)]
    pub conductor_available: bool,
    #[serde(default)]
    pub conductor_unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness_unavailable: Option<HarnessUnavailable>,
    /// The header values of the task's workspaces, in the workflow's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceHeader>,
    /// Title the conductor generated for a task whose operator gave none;
    /// the last report that has one wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_title: Option<String>,
    pub observed_at: DateTime<Utc>,
}

/// What the control plane knows about the launch of the calling runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerLaunchView {
    pub incarnation: String,
    pub launch_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_by: Option<OwnerCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_by_actor: Option<CommandActor>,
    /// The owner's comment on the pause this launch ends: the runner gives
    /// it to the agent before the work goes on, once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_message: Option<ResumeMessage>,
}

/// What the owner told the agent while the task was paused, kept until the
/// runner of the next launch has taken it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeMessage {
    /// The key of the comment; the runner takes a key once.
    pub key: String,
    pub text: String,
}

/// Response to `POST /runner/v1/snapshots`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerSnapshotResponse {
    pub applied: bool,
    pub directive: RunnerDirective,
    pub generation: u64,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub launch: RunnerLaunchView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<TaskDefaultHarness>,
}
