//! Owner-facing task summary, queue and attention DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{ExecutionStatusDto, Timeline};
use crate::controller::{
    ArchiveTombstone, ControllerPresence, InputView, ObservationView, OperatorCommandView,
    PanelOffer, ResumeState, RunnerState, RunnerView, StepView, TaskActionOffer, TransitionOffer,
};
use crate::display::WorkspaceHeader;

/// The stage of a task's work as the owner sees it, worked out by the control
/// plane from the runner state, the runner's last execution status and the
/// resume policy. Clients show it and never work it out themselves.
///
/// A client that meets a state it does not know reads `Unrecognized`, which
/// is also what a summary from before the field existed reads.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    HumanNeeded,
    Paused,
    Finished,
    Error,
    Archived,
    #[default]
    #[serde(other)]
    Unrecognized,
}

/// Scheduler verdict for a queued task, as exposed to its owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueView {
    pub reason: QueueReason,
    pub message: String,
    pub since: DateTime<Utc>,
}

/// Normalized, backend-neutral summary of an owned task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    pub task_id: crate::identifier::TaskId,
    pub backend_id: crate::identifier::BackendId,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    /// What the owner sees; see [`TaskState`].
    #[serde(default)]
    pub state: TaskState,
    /// Whether the task occupies one of its backend's capacity slots.
    #[serde(default)]
    pub holds_slot: bool,
    pub generation: u64,
    pub revision: u64,
    pub rank: u32,
    pub created_at: DateTime<Utc>,
    /// When the task entered its current owner-visible state, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<DateTime<Utc>>,
    pub queue_sequence: u64,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub launch_digest: String,
    pub environment_identity: String,
    pub archive: Option<ArchiveTombstone>,
    pub observation: Option<ObservationView>,
    pub runner: Option<RunnerView>,
    pub controller: Option<ControllerPresence>,
    pub inputs: Vec<InputView>,
    #[serde(default)]
    pub operator_commands: Vec<OperatorCommandView>,
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
    #[serde(default)]
    pub queue: Option<QueueView>,
    /// The header values of the task's workspaces, in the workflow's order.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceHeader>,
    /// Ticket the operator gave the task, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Title of the task: the operator's, else the one the conductor
    /// generated, if either exists yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_transitions: Option<TransitionOffer>,
    /// Whether the task waits for the owner in a state that frees its slot
    /// and the owner may free it now. `false` from a server before the field.
    #[serde(default)]
    pub slot_release_pending: bool,
    /// Every step of the workflow, for the states in which the owner may
    /// choose one. `None` where the owner cannot, or from a server before the
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<StepView>>,
    /// What the owner can do to the task now, and why not where it cannot.
    /// `None` when the server did not say (a server from before the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<TaskActionOffer>>,
    /// The forms the owner's panel offers now, and which one opens first.
    /// `None` when the server did not say (a server from before the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<PanelOffer>,
    #[serde(default)]
    pub resume: ResumeState,
    pub as_of: DateTime<Utc>,
}

/// `GET /control/v1/tasks` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskListResponse {
    pub tasks: Vec<TaskSummary>,
    pub cursor: String,
}

/// Scheduling verdict for a queued task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum QueueReason {
    Admitted,
    WaitingForCapacity {
        position: u32,
        running: u32,
        limit: u32,
    },
    ControllerOffline,
    BackendUnknown {
        detail: String,
    },
}

impl QueueReason {
    /// A one-line explanation for a person reading a task summary.
    pub fn describe(&self) -> String {
        match self {
            Self::Admitted => "admitted; waiting for the controller to start it".into(),
            Self::WaitingForCapacity {
                position,
                running,
                limit,
            } => format!(
                "waiting for capacity: position {position} in the queue, \
                 {running} of {limit} slots in use"
            ),
            Self::ControllerOffline => "waiting for the backend's controller to come online".into(),
            Self::BackendUnknown { detail } => format!("cannot be scheduled: {detail}"),
        }
    }
}

/// Category of a task's current wait on its owner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Question,
    Approval,
    Problem,
    EntryLimit,
    FailedStep,
    ConductorProposal,
    ProvisioningFailed,
    #[serde(other)]
    Unrecognized,
}

/// Metadata about one thing a task waits on its owner to resolve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionItem {
    pub kind: AttentionKind,
    #[serde(default)]
    pub state_id: Option<String>,
    pub since: DateTime<Utc>,
    pub waiting_id: String,
}

/// Freshness of a value the control plane only observed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Live,
    Stale,
}

/// Latest runner-reported timeline for an owned control-plane task.
///
/// Identifiers and execution status are represented by protocol values so
/// clients do not need the server's validated domain types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimelineResponse {
    pub task_id: String,
    pub freshness: Freshness,
    pub observed_at: Option<DateTime<Utc>>,
    pub timeline: Option<Timeline>,
    #[serde(default)]
    pub execution_status: Option<ExecutionStatusDto>,
    #[serde(default)]
    pub current_state: Option<String>,
    #[serde(default)]
    pub current_position: Option<String>,
}
