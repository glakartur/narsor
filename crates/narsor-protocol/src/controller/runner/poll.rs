//! Runner long-poll directives, deliveries and acknowledgements.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::HumanInputDeliveryDto;
use crate::controller::{
    ConductorReadRequest, ConversationPageRequest, ExecutionEventsRequest, InputView,
    OperatorCommand, OperatorCommandView, RunnerLaunchView, RunnerState,
};

/// Directive returned to a runner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerDirective {
    Continue,
    PauseAtSafePoint,
    Stop,
}

/// Query parameters for the runner long poll.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerPollQuery {
    #[serde(default)]
    pub pending_input_id: Option<String>,
    #[serde(default)]
    pub wait_secs: Option<u64>,
    #[serde(default)]
    pub defer_materials: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The key of the comment on the pause the runner has given to its
    /// agent: the control plane drops that comment from the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_message_taken: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputAckRequest {
    pub accepted: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorCommandAckRequest {
    pub applied: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputAckResponse {
    pub input: InputView,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCommandAckResponse {
    pub command: OperatorCommandView,
}

/// An owner answer handed to the runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredInput {
    pub input_id: String,
    pub pending_input_id: String,
    pub delivery: HumanInputDeliveryDto,
}

/// An operator command handed to the runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredOperatorCommand {
    pub command_id: String,
    pub requested_by: String,
    #[serde(flatten)]
    pub command: OperatorCommand,
}

/// Response to `GET /runner/v1/poll`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerPollResponse {
    pub directive: RunnerDirective,
    pub generation: u64,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub inputs: Vec<DeliveredInput>,
    #[serde(default)]
    pub commands: Vec<DeliveredOperatorCommand>,
    pub credential_expires_at: DateTime<Utc>,
    pub launch: RunnerLaunchView,
    #[serde(default)]
    pub conversation_pages: Vec<ConversationPageRequest>,
    #[serde(default)]
    pub execution_events: Vec<ExecutionEventsRequest>,
    #[serde(default)]
    pub conductor_reads: Vec<ConductorReadRequest>,
    #[serde(default)]
    pub materials: Vec<crate::api::MaterialAnnouncement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<TaskDefaultHarness>,
}

/// The installation default harness as it applies to a running task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskDefaultHarness {
    Available { harness: String },
    Unavailable { harness: String, reason: String },
}
