//! Owner-facing task summary, queue and attention DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{ExecutionStatusDto, Timeline};
use crate::controller::{
    ArchiveTombstone, ControllerPresence, GateAutomationPhase, InputView, ObservationView,
    OperatorCommandView, PanelOffer, ResumeState, RunnerState, RunnerView, StepView,
    TaskActionOffer, TransitionOffer,
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
    /// 1-based position among the backend's tasks that are not archived,
    /// computed by the server; 0 for an archived task.
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
    /// The observer queue entry of the wait episode the task waits in now
    /// (R21): a client shows one mark while it is there. `None` while the
    /// task waits at no gate with a `watch` block, when the control plane
    /// keeps no entry for its episode, and from a server before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_entry: Option<TaskQueueEntryView>,
    /// Where the entry (`in`) and exit (`out`) scripts of the watched gate
    /// the task waits at stand (R20): a control-plane overlay a client
    /// applies to the gate's position of the runner timeline. `None` while
    /// the gate has no such scripts, and from a server before the field: a
    /// client then shows the runner status unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_automation: Option<TaskGateAutomationView>,
    /// Whether the task's stored environment still matches the current
    /// catalogue (R10, R11). Worked out only for a single task fetched with
    /// [`TaskQuery::drift`], because it reads the catalogue: `None` in lists,
    /// in the summaries of action responses, and from a server before the
    /// field — a client then shows no marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_drift: Option<EnvironmentDrift>,
    pub as_of: DateTime<Utc>,
}

/// `GET /control/v1/tasks/:task_id` query.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskQuery {
    /// `true`: also compare the task's stored environment with the current
    /// catalogue ([`TaskSummary::environment_drift`]). It reads the
    /// catalogue, so pollers leave it out.
    #[serde(default)]
    pub drift: bool,
}

/// How a task's stored environment stands against the current catalogue.
///
/// A client that meets a status it does not know reads `Unrecognized` and
/// shows no marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum EnvironmentDrift {
    /// The catalogue resolves to the stored environment.
    Current,
    /// The catalogue resolves to another environment: the owner can take it
    /// with the environment refresh ([`super::TaskActionId::RefreshEnvironment`]).
    Differs {
        /// The identity the catalogue resolves to now.
        catalogue_identity: String,
    },
    /// The catalogue does not resolve for this task, so drift is not known;
    /// this is a catalogue problem, not drift.
    CatalogueProblem {
        /// Stable reason code, like the refusal of a refresh.
        code: String,
        /// Why, in words for the owner or operator.
        reason: String,
    },
    /// The task has ended (finished or archived): its environment is never
    /// used again.
    NotApplicable,
    #[serde(other)]
    Unrecognized,
}

/// The observer queue entry of a task's current wait episode, as the task
/// view shows it (R21). Observer runs report nothing to the task: this is
/// what the control plane knows from its own entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskQueueEntryView {
    /// The queue the gate's `watch` names.
    pub queue: String,
    /// The workflow state of the gate.
    pub step: String,
    /// The wait episode: the id of the gate's pending input request.
    pub episode: String,
    /// When the control plane added the entry.
    pub since: DateTime<Utc>,
}

/// The entry and exit scripts of a task's watched gate in its current wait
/// episode, as the task view shows them (R20).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskGateAutomationView {
    /// The workflow state of the gate: the step whose latest position the
    /// overlay applies to.
    pub step: String,
    /// The queue the gate's `watch` names.
    pub queue: String,
    /// The wait episode.
    pub episode: String,
    /// Where the episode stands; `entry_failed` shows the step as failed.
    pub phase: GateAutomationPhase,
    /// The scripts of the episode, `in` first, in the queue file's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<GateHookView>,
    /// The exit held while the exit scripts run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<GateExitView>,
}

/// One entry or exit script of one automation in a wait episode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateHookView {
    /// The automation.
    pub automation: String,
    /// The script phase: `in` or `out`.
    pub hook: String,
    /// `pending`, `succeeded`, `failed` or `skipped`.
    pub status: String,
    /// Attempts that ended.
    pub attempts: u32,
    /// Most attempts the script gets in the episode.
    pub max_attempts: u32,
    /// Why the latest attempt failed (bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Every ended attempt, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<GateHookAttemptView>,
    /// The catalogue file a pending script waits for: it cannot be used
    /// now, so the script neither runs nor is skipped until an operator
    /// fixes or removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

/// One ended attempt of an entry or exit script.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateHookAttemptView {
    /// The attempt, from 1.
    pub attempt: u32,
    /// When it ended.
    pub at: DateTime<Utc>,
    /// Why it failed; `None` when it succeeded or was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why no script ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// When the next automatic attempt runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
}

/// The exit a watched gate holds while its exit scripts run (R11).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateExitView {
    /// The choice; `None` when the task ended at the gate or the runner
    /// left it by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    /// Who chose it: `person`, `automation`, `operator`, `task_end`,
    /// `runner`.
    pub author: String,
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
