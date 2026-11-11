//! Owner inputs, operator commands and comments submitted to a task.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::HumanInputDeliveryDto;
use crate::controller::TaskSummary;

/// Result state of a queued owner input.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    Queued,
    Delivered,
    Consumed,
    Rejected,
    Obsolete,
}

impl InputStatus {
    /// True once no further delivery or acknowledgement can occur.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Consumed | Self::Rejected | Self::Obsolete)
    }
}

/// Result state of a queued operator command.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OperatorCommandStatus {
    Queued,
    Applied,
    Rejected,
}

/// An owner input in a task summary, without its payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputView {
    pub input_id: String,
    pub pending_input_id: String,
    pub status: InputStatus,
    pub submitted_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub detail: Option<String>,
}

/// An operator command in a task summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCommandView {
    pub command_id: String,
    pub command: OperatorCommand,
    pub status: OperatorCommandStatus,
    pub submitted_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub detail: Option<String>,
}

/// A queued operator command response; completion arrives in the task event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitOperatorCommandResponse {
    pub command: OperatorCommandView,
    pub replayed: bool,
}

/// A queued input response; delivery and consumption arrive in task updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitInputResponse {
    pub input: InputView,
    pub replayed: bool,
}

impl OperatorCommandStatus {
    /// True once no further command outcome can occur.
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Queued)
    }
}

/// `POST /control/v1/tasks/:task_id/input`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitInputRequest {
    /// The request it answers.
    pub pending_input_id: String,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// The answer.
    pub delivery: HumanInputDeliveryDto,
}

/// A command an owner asks the runner of a running task to perform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperatorCommand {
    /// Cut the step in flight short; the execution is left recoverable.
    Interrupt,
    /// Cut the step in flight short for a stop: the execution is left
    /// paused, and the task ends in `Stopped` (never `Interrupted`). Nothing
    /// to cut is not an error.
    StopStep,
    /// Make a declared or forced transition from the current workflow state.
    Transition {
        /// State to enter.
        target: String,
        /// Whether this overrides declared transitions.
        #[serde(default, rename = "override")]
        forced: bool,
        /// Why the owner overrides.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Go on from any step of the workflow, a finished workflow included:
    /// no declared transition and no `allow_manual_override` is needed.
    SelectStep {
        /// The step to go on from.
        target: String,
        /// What the owner tells the agent when the step is an agent step.
        #[serde(default)]
        note: Option<String>,
    },
    /// Start the failed or interrupted step again.
    RestartStep {
        /// Instruction for the agent's next attempt.
        #[serde(default)]
        note: Option<String>,
        /// Failure key this command answers.
        #[serde(default)]
        failure_key: Option<String>,
    },
    /// Write to the conductor.
    ConductorChat { text: String },
    /// Carry out a conductor proposal.
    ConfirmProposal { proposal_id: String },
    /// Turn down a conductor proposal.
    RejectProposal { proposal_id: String },
    /// Take back a chosen conductor output.
    UndoDecision { decision_id: String },
    /// Historical command retained so older queued records decode.
    SetConductorHarness { harness: String },
}

impl OperatorCommand {
    /// A short name for people.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::StopStep => "stop step",
            Self::SelectStep { .. } => "select step",
            Self::Transition { forced: false, .. } => "transition",
            Self::Transition { forced: true, .. } => "override transition",
            Self::RestartStep { .. } => "restart step",
            Self::ConductorChat { .. } => "conductor chat",
            Self::ConfirmProposal { .. } => "confirm proposal",
            Self::RejectProposal { .. } => "reject proposal",
            Self::UndoDecision { .. } => "undo decision",
            Self::SetConductorHarness { .. } => "set conductor harness",
        }
    }
}

/// `POST /control/v1/tasks/:task_id/operator-commands`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitOperatorCommandRequest {
    /// What to ask the runner to do.
    #[serde(flatten)]
    pub command: OperatorCommand,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Refuse unless the task is still at this desired generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// `POST /control/v1/tasks/:task_id/comment`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitCommentRequest {
    /// What the owner tells the agent.
    pub text: String,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Refuse unless the task is still at this generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// Answer to a comment the server took.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitCommentResponse {
    pub task: TaskSummary,
    /// The key was used before; nothing was done again.
    pub replayed: bool,
}
