//! Shared controller protocol values that do not depend on service internals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::HumanInputDeliveryDto;
use crate::api::{ExecutionStatusDto, Timeline};
use crate::display::TaskDisplay;
use crate::event::Event;
use crate::identifier::BackendId;
use crate::identifier::TaskId;
use serde_json::Value;

/// A controller's protocol-visible identity after enrollment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollResponse {
    pub controller_id: String,
    pub backend_id: BackendId,
    pub protocol_version: u32,
}

/// Proof-of-possession body for controller enrollment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollRequest {
    pub enrollment_code: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub proof: String,
}

/// Request to replace a controller's public key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateKeyRequest {
    pub public_key: String,
    pub proof: String,
}

/// Controller identity after key rotation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateKeyResponse {
    pub controller_id: String,
    pub rotated_at: DateTime<Utc>,
}

/// A controller's lease on one backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseView {
    pub backend_id: BackendId,
    pub controller_id: String,
    pub epoch: u64,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Controller heartbeat response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatResponse {
    pub controller_id: String,
    pub backend_id: BackendId,
    pub server_time: DateTime<Utc>,
    pub lease: Option<LeaseView>,
}

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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcquireLeaseRequest {
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseEpochRequest {
    pub epoch: u64,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCredentialRequest {
    pub lease_epoch: u64,
    pub generation: u64,
    pub incarnation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCredentialResponse {
    pub task_id: TaskId,
    pub generation: u64,
    pub incarnation: String,
    pub credential: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerRenewResponse {
    pub expires_at: DateTime<Utc>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputAckResponse {
    pub input: InputView,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCommandAckResponse {
    pub command: OperatorCommandView,
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
}

/// Controller command response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandsResponse {
    pub lease_epoch: u64,
    pub commands: Vec<DesiredCommand>,
}

/// The kinds of action the conductor can propose to an owner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    AnswerInput,
    MessageAgent,
    Retry,
    Transition,
    Abort,
}

/// A proposed action awaiting owner confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposedAction {
    pub kind: ProposalKind,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, deserialize_with = "nullable_map")]
    pub values: serde_json::Map<String, Value>,
    #[serde(default)]
    pub text: Option<String>,
}

impl ProposedAction {
    pub fn describe(&self) -> String {
        match self.kind {
            ProposalKind::Retry => self.text.as_ref().map_or_else(
                || "Restart the failed step".to_string(),
                |note| format!("Restart the failed step with the note: {note}"),
            ),
            ProposalKind::Transition => format!(
                "Take the transition of output '{}'",
                self.label.as_deref().unwrap_or_default()
            ),
            ProposalKind::Abort => "Cancel the execution".to_string(),
            ProposalKind::MessageAgent => format!(
                "Tell the agent: {}",
                self.text.as_deref().unwrap_or_default()
            ),
            ProposalKind::AnswerInput => {
                let values = Value::Object(self.values.clone());
                match &self.label {
                    Some(label) => format!("Answer the pending input with '{label}', {values}"),
                    None => format!("Answer the pending input with {values}"),
                }
            }
        }
    }
}

fn nullable_map<'de, D>(deserializer: D) -> Result<serde_json::Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<serde_json::Map<String, Value>>::deserialize(deserializer)?.unwrap_or_default())
}

/// The durable proposal record as shown to an owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposalRecord {
    #[serde(default)]
    pub episode: Option<String>,
    #[serde(default)]
    pub state_id: Option<String>,
    pub proposal: ProposedAction,
    #[serde(default)]
    pub confidence: Option<crate::event::Confidence>,
    #[serde(default)]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Executed,
    Rejected,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposalView {
    pub proposal_id: String,
    pub text: String,
    pub at: DateTime<Utc>,
    pub record: ProposalRecord,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    pub state: ProposalState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionView {
    pub decision_id: String,
    pub kind: String,
    pub text: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub state_id: Option<String>,
    #[serde(default)]
    pub confidence: Option<crate::event::Confidence>,
    #[serde(default)]
    pub rationale: Option<String>,
    pub undone: bool,
    pub undoable: bool,
}

/// Pending or queued conductor work shown in the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConductorWorkView {
    pub purpose: crate::event::ConductorPurpose,
    pub trigger: crate::event::ConductorTrigger,
    #[serde(default)]
    pub episode: Option<String>,
    pub since: DateTime<Utc>,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureOutcome {
    Diagnosed {
        diagnosis: String,
        #[serde(default)]
        proposals: Vec<ProposedAction>,
    },
    Retried {
        note: String,
    },
    Unanswered {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailureReport {
    pub key: String,
    pub episode: String,
    pub state_id: String,
    pub reason: String,
    pub outcome: FailureOutcome,
    pub concluded_at: DateTime<Utc>,
}

impl FailureReport {
    pub fn is_problem(&self) -> bool {
        !matches!(self.outcome, FailureOutcome::Retried { .. })
    }
}

/// The conductor view the client receives over REST or runner relay.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConductorView {
    pub available: bool,
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub working: Vec<ConductorWorkView>,
    #[serde(default)]
    pub unanswered_chat: Vec<String>,
    #[serde(default)]
    pub failure: Option<FailureReport>,
    #[serde(default)]
    pub chat: Vec<crate::api::ChatEntryDto>,
    #[serde(default)]
    pub chat_has_more: bool,
    #[serde(default)]
    pub proposals: Vec<ProposalView>,
    #[serde(default)]
    pub decisions: Vec<DecisionView>,
}

impl ConductorView {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            unavailable_reason: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn available_on(mut self, harness: impl Into<String>) -> Self {
        self.available = true;
        self.unavailable_reason = None;
        self.harness = Some(harness.into());
        self
    }

    pub fn without_conductor(mut self, reason: impl Into<String>) -> Self {
        self.available = false;
        self.unavailable_reason = Some(reason.into());
        self
    }

    pub fn cleaned(self) -> Self {
        let Ok(mut value) = serde_json::to_value(&self) else {
            return self;
        };
        crate::display::without_unsafe_chars_json(&mut value);
        serde_json::from_value(value).unwrap_or(self)
    }

    pub fn has_more(&self) -> bool {
        self.chat_has_more
    }

    pub fn last_sequence(&self) -> u64 {
        self.chat.last().map_or(0, |entry| entry.sequence)
    }
}

/// Conductor state relayed from a runner or projected from local storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConductorReadResponse {
    Available {
        conductor: Box<ConductorView>,
        source: ExecutionEventsSource,
    },
    Unavailable {
        reason: ConversationUnavailableReason,
        message: String,
    },
}

/// State of a task's runner and its resources (slot, workload, storage) as
/// stored and exposed by the control plane. Older records and events spell
/// `Stopped` as `paused`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Queued,
    Provisioning,
    Running,
    PauseRequested,
    #[serde(alias = "paused")]
    Stopped,
    Interrupted,
    Unknown,
    Archiving,
    Archived,
}

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

impl RunnerState {
    /// True for `Archiving` and `Archived`.
    pub fn is_archival(self) -> bool {
        matches!(self, Self::Archiving | Self::Archived)
    }
}

/// Lifecycle-relevant fact a controller reported to the control plane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Observation {
    StartAccepted,
    RunnerReady,
    PausedAtSafePoint,
    WorkloadLost {
        state_known: bool,
    },
    LossConfirmed,
    ResourcesDeleted,
    Status,
    #[serde(other)]
    Unrecognized,
}

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

/// Identity of the backend resource an observation is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceIdentity {
    pub uid: String,
    pub resource_generation: u64,
}

/// Normalized summary of the last controller observation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservedSummary {
    pub workload_present: bool,
    pub storage_present: bool,
    pub runner_status: Option<ExecutionStatusDto>,
    pub detail: Option<String>,
}

/// Archive tombstone recording the accepted request and resource cleanup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveTombstone {
    pub idempotency_key: String,
    pub generation: u64,
    pub requested_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub resources_never_created: bool,
}

/// Resume-policy state for a task after workload loss.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResumeState {
    pub consecutive_losses: u32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_loss: Option<WorkloadLoss>,
    pub exhausted: Option<ResumeExhaustion>,
    pub uncounted_losses: u32,
    pub last_loss_uncounted: bool,
}

impl ResumeState {
    /// True when no workload loss is remembered.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Where and when a task lost its workload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadLoss {
    pub from: RunnerState,
    pub at: DateTime<Utc>,
    pub lease_epoch: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Why the resume policy gave up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeExhaustion {
    pub reason: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub announced: bool,
}

/// Engine cursor reported by a runner, without execution internals.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionCursor {
    pub execution_id: Option<String>,
    pub current_state: Option<String>,
    pub action_execution_id: Option<String>,
}

/// A default harness a task cannot follow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessUnavailable {
    pub harness: String,
    pub kept: String,
    pub reason: String,
}

/// A condition that keeps a runner from doing its work.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProblem {
    pub code: String,
    pub message: String,
}

/// Last controller observation exposed in an owner task summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationView {
    pub observation: Observation,
    pub summary: ObservedSummary,
    pub resource: Option<ResourceIdentity>,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub freshness: Freshness,
}

/// Scheduler verdict for a queued task, as exposed to its owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueView {
    pub reason: QueueReason,
    pub message: String,
    pub since: DateTime<Utc>,
}

/// Pending input metadata as reported by a runner. The form remains a JSON
/// value so the protocol does not depend on the engine's durable form model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingInputRefView {
    pub id: String,
    pub attempt_id: Option<String>,
    pub form: Value,
}

/// Last runner snapshot exposed in an owner task summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerView {
    pub incarnation: String,
    pub execution_status: ExecutionStatusDto,
    pub execution_cursor: ExecutionCursor,
    pub event_cursor: u64,
    pub pending_input: Option<PendingInputRefView>,
    pub safe_point: bool,
    #[serde(default)]
    pub problem: Option<RunnerProblem>,
    #[serde(default)]
    pub conductor_available: bool,
    #[serde(default)]
    pub conductor_unavailable_reason: Option<String>,
    #[serde(default)]
    pub default_harness_unavailable: Option<HarnessUnavailable>,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub last_contact_at: Option<DateTime<Utc>>,
    pub freshness: Freshness,
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
    #[serde(default)]
    pub display: Option<TaskDisplay>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageRequest {
    pub request_id: String,
    pub attempt_id: String,
    pub from_entry: u64,
    pub byte_budget: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionEventsRequest {
    pub request_id: String,
    pub after: u64,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEventsPage {
    pub events: Vec<crate::event::Event>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayFailure {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConductorReadRequest {
    pub request_id: String,
    pub after: u64,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageAccepted {
    pub delivered: bool,
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
    #[serde(default)]
    pub display: crate::display::TaskDisplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_transitions: Option<TransitionOffer>,
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

/// Actor recorded for a runner_state command receipt.
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

/// Change recorded in one accepted task mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TaskEventKind {
    Created,
    OwnerCommandAccepted {
        command: OwnerCommand,
        idempotency_key: String,
    },
    ControllerReported {
        observation: Observation,
        lease_epoch: u64,
    },
    RunnerReported {
        incarnation: String,
        event_cursor: u64,
        execution_status: ExecutionStatusDto,
        pending_input_id: Option<String>,
        #[serde(default)]
        attention: Vec<AttentionItem>,
    },
    InputQueued {
        input_id: String,
        pending_input_id: String,
    },
    OperatorCommandQueued {
        command_id: String,
        command: OperatorCommand,
        requested_by: OwnerRef,
    },
    OperatorCommandResolved {
        command_id: String,
        status: OperatorCommandStatus,
        detail: Option<String>,
    },
    PriorityChanged {
        priority: i32,
    },
    Moved {
        rank: u32,
        order: Vec<crate::identifier::TaskId>,
    },
    QueueChanged {
        reason: QueueReason,
    },
    InputsUpdated {
        updates: Vec<InputUpdate>,
    },
    MaterialChanged {
        material_id: String,
        change: MaterialChange,
        files: u32,
        total_bytes: u64,
    },
    AutoResumed {
        attempt: u32,
        limit: u32,
        #[serde(default)]
        loss_reason: Option<String>,
    },
    AutoResumeExhausted {
        attempts: u32,
        reason: String,
    },
    /// The control plane planned (or moved) the time at which it frees the
    /// slot of a task that waits for its owner.
    SlotReleaseScheduled {
        due_at: DateTime<Utc>,
    },
    /// The control plane freed the task's slot: it asked the runner to stop
    /// at a safe point.
    SlotReleased {
        reason: SlotReleaseReason,
    },
    #[serde(other)]
    Unrecognized,
}

/// Why the control plane freed a task's slot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlotReleaseReason {
    /// The task's execution ended (completed or cancelled).
    Terminal,
    /// The task waited for its owner (input needed, failed) and the wait
    /// ran out.
    GraceElapsed,
    #[serde(other)]
    Unrecognized,
}

/// Material runner_state change recorded in a task event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialChange {
    Uploaded,
    Published,
    Delivered,
    Deleted,
    Expired,
}

/// One queued input status change in a task event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputUpdate {
    pub input_id: String,
    pub status: InputStatus,
}

/// Durable task event, also used by the owner event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskEvent {
    pub sequence: u64,
    pub task_id: crate::identifier::TaskId,
    pub at: DateTime<Utc>,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub generation: u64,
    pub kind: TaskEventKind,
}

/// One frame of the control-plane event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlEventEnvelope {
    pub cursor: String,
    pub event: TaskEvent,
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

/// One attempt's conversation, summarized without transcript content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationAttemptSummary {
    pub attempt_id: String,
    pub state_id: Option<String>,
    pub entries: u64,
    pub closed: bool,
    pub last_entry_at: Option<DateTime<Utc>>,
}

/// Whether a runner can serve its conversation and the summaries it holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationAvailability {
    Live {
        attempts: Vec<ConversationAttemptSummary>,
    },
    Unavailable {
        reason: String,
    },
}

impl Default for ConversationAvailability {
    fn default() -> Self {
        Self::Live {
            attempts: Vec::new(),
        }
    }
}

/// Owner response describing whether the runner can serve its conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationResponse {
    Available {
        attempts: Vec<ConversationAttemptSummary>,
        observed_at: DateTime<Utc>,
        #[serde(default)]
        source: ExecutionEventsSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recorded_at: Option<DateTime<Utc>>,
    },
    Unavailable {
        reason: ConversationUnavailableReason,
        last_attempts: Vec<ConversationAttemptSummary>,
        observed_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
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

/// `GET /control/v1/tasks/:task_id/attempts/:attempt_id/conversation` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageQuery {
    /// First entry number wanted (default 0: from the start).
    #[serde(default)]
    pub from: u64,
    /// Soft cap on the raw bytes; bounded by the server.
    #[serde(default)]
    pub byte_budget: Option<u64>,
}

/// `GET /control/v1/tasks/:task_id/execution-events` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionEventsQuery {
    /// Events with a greater `sequence` (default 0: from the start).
    #[serde(default)]
    pub after: u64,
    /// Most events to return; bounded by the server.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Result of the owner task's execution-events request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ExecutionEventsResponse {
    /// The events after the requested sequence.
    Available {
        /// The events, oldest first.
        events: Vec<Event>,
        /// True when more follow; ask again `after` the last one.
        has_more: bool,
        /// Where they were read from.
        source: ExecutionEventsSource,
    },
    /// Not readable right now.
    Unavailable {
        /// Why the event stream cannot be read.
        reason: ConversationUnavailableReason,
        /// Human-readable explanation.
        message: String,
    },
}

/// `GET /control/v1/tasks/:task_id/conductor` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConductorReadQuery {
    /// Chat entries with a greater `sequence` (default 0: from the start).
    #[serde(default)]
    pub after: u64,
    /// Most chat entries to return; bounded by the server.
    #[serde(default)]
    pub limit: Option<u32>,
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

/// An owner's requested change to a task's desired runner_state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OwnerCommand {
    /// Retry an interrupted or unknown task.
    Start,
    /// Bring a running task to a safe point and release its compute.
    Pause,
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

/// A requested CPU and memory profile, expressed independently of the
/// server's environment policy types.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceProfileRequest {
    /// CPU in millicores (`1000` = one core).
    pub cpu_millicores: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
}

/// The workspace portion of a task creation request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRequest {
    /// Registered repository id; the workflow's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Full commit id to clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// What a new task runs, before the control plane resolves it against its
/// current configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    /// Workflow to run.
    pub workflow_id: String,
    /// Version the caller expects.
    pub workflow_version: u32,
    /// Workflow inputs.
    #[serde(default)]
    pub inputs: Value,
    /// Repository and pinned commit to clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRequest>,
    /// Environment profile; owner default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Harness for agent steps that name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// CPU and memory; backend default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceProfileRequest>,
}

/// `POST /control/v1/tasks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateTaskRequest {
    /// Client-chosen id; repeating the identical request is safe.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Backend to pin the task to; the owner's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<String>,
    /// What to run.
    pub launch: LaunchRequest,
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

/// Why a task conversation cannot be served at the moment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationUnavailableReason {
    NotStarted,
    Paused,
    Interrupted,
    Archived,
    RunnerUnreachable,
    RunnerReported,
    StorageUnreadable,
    #[serde(other)]
    Unrecognized,
}

/// Where task events or a conversation page was read from.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEventsSource {
    #[default]
    Runner,
    Storage,
}

/// The installation default harness as it applies to a running task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskDefaultHarness {
    Available { harness: String },
    Unavailable { harness: String, reason: String },
}

/// Current lease holder of a task's backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerPresence {
    pub controller_id: String,
    pub lease_epoch: u64,
    pub lease_expires_at: DateTime<Utc>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub freshness: Freshness,
}

/// Capabilities a control plane exposes to its owner clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitiesView {
    pub connection_name: String,
    pub workflow_paths: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_paths_reason: Option<String>,
    pub legacy_executions: bool,
    pub local_repository_paths: bool,
}

/// Default backend selection as presented to an owner client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultBackendView {
    pub configured: String,
    #[serde(rename = "override")]
    pub override_backend: Option<String>,
    pub effective: String,
    pub revision: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Request to choose or clear the default backend for newly created tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetDefaultBackendRequest {
    #[serde(default)]
    pub backend_id: Option<String>,
}

/// A workflow input's declared value type in the owner API.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInputType {
    #[default]
    String,
    Text,
    Integer,
    Boolean,
    Repository,
}

/// One declared workflow input, reduced to fields an owner form needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowInputView {
    pub name: String,
    #[serde(rename = "type")]
    pub input_type: WorkflowInputType,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// Where the selected copy of a workflow comes from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSource {
    BuiltIn,
    Catalog,
    User,
}

/// One workflow a task may be created from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowView {
    pub name: String,
    pub source: WorkflowSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default)]
    pub state_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_state: Option<String>,
    pub inputs: Vec<WorkflowInputView>,
    pub valid: bool,
    #[serde(default)]
    pub violations: Vec<String>,
    pub overrides: bool,
    /// The resolved origin name shown by the CLI (for example `root`).
    #[serde(default)]
    pub origin_label: String,
    /// Lower-priority copies hidden by this selected workflow.
    #[serde(default)]
    pub shadow_labels: Vec<String>,
    /// Whether a built-in definition exists under this name. `None` means
    /// the server predates this field, so clients must not infer absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_builtin: Option<bool>,
}

/// Request to retrieve exactly one selected or built-in workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSourceRequest {
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
}

/// One workflow definition returned on demand by the owner API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSourceResponse {
    pub name: String,
    pub origin_label: String,
    pub content: String,
}

/// Workflows available to the control-plane owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowListResponse {
    pub workflows: Vec<WorkflowView>,
}

/// Request to validate a workflow path on the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidateWorkflowRequest {
    pub path: String,
}

/// Validation result with every reported violation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidateWorkflowResponse {
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default)]
    pub state_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_state: Option<String>,
    #[serde(default)]
    pub inputs: Vec<WorkflowInputView>,
    #[serde(default)]
    pub violations: Vec<String>,
}

/// A repository registered for workflow task creation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryView {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Repositories available to the control-plane owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryListResponse {
    pub repositories: Vec<RepositoryView>,
    #[serde(default)]
    pub unreadable: Vec<String>,
}

/// Request to register a repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateRepositoryRequest {
    pub id: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Request to create a one-time controller enrollment code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateEnrollmentRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// One-time controller enrollment code and its backend scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollmentResponse {
    pub enrollment_code: String,
    pub backend_id: String,
    pub expires_at: DateTime<Utc>,
}

/// Controller identity details without its credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerView {
    pub controller_id: String,
    pub backend_id: String,
    pub name: Option<String>,
    pub enrolled_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub holds_lease: bool,
}

/// All controller identities visible to an owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerListResponse {
    pub controllers: Vec<ControllerView>,
}

/// A manual transition a workflow declares from its current state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionOffer {
    pub from_state: String,
    pub exits: Vec<TransitionExit>,
    pub override_allowed: bool,
    #[serde(default)]
    pub override_targets: Vec<String>,
}

/// One declared transition out of a workflow state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionExit {
    pub label: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub const CONVERSATION_UNAVAILABLE: &str = "conversation_unavailable";
pub const CONVERSATION_RECORD_UNREADABLE: &str = "conversation_record_unreadable";
pub const CONVERSATION_RECORD_UNREADABLE_PREFIX: &str =
    "the conversation record could not be read: ";

#[cfg(test)]
mod tests {
    use crate::api::HumanInputDeliveryDto;
    use chrono::Utc;

    use super::{
        AttentionItem, AttentionKind, CapabilitiesView, ConductorReadQuery, ControllerListResponse,
        ControllerView, ConversationPageQuery, ConversationUnavailableReason,
        CreateEnrollmentRequest, CreateRepositoryRequest, CreateTaskRequest, DefaultBackendView,
        EnrollmentResponse, ExecutionEventsQuery, ExecutionEventsResponse, ExecutionEventsSource,
        Freshness, InputStatus, InputView, LaunchRequest, MoveTaskRequest, MoveTaskTarget,
        Observation, ObservationView, ObservedSummary, OperatorCommand, OperatorCommandStatus,
        OperatorCommandView, OwnerCommand, OwnerCommandRequest, QueueReason, QueueView,
        RepositoryListResponse, RepositoryView, ResourceIdentity, ResourceProfileRequest,
        RunnerState, SetDefaultBackendRequest, SubmitInputRequest, SubmitInputResponse,
        SubmitOperatorCommandRequest, SubmitOperatorCommandResponse, TaskDefaultHarness,
        TimelineResponse, ValidateWorkflowRequest, ValidateWorkflowResponse, WorkflowInputType,
        WorkflowInputView, WorkflowListResponse, WorkflowSource, WorkflowView, WorkspaceRequest,
    };

    #[test]
    fn entry_limit_attention_kind_round_trips_and_unknown_kinds_fall_back() {
        assert_eq!(
            serde_json::from_value::<AttentionKind>(serde_json::json!("entry_limit")).unwrap(),
            AttentionKind::EntryLimit
        );
        assert_eq!(
            serde_json::from_value::<AttentionKind>(serde_json::json!("future_kind")).unwrap(),
            AttentionKind::Unrecognized
        );
    }

    #[test]
    fn task_event_keeps_the_durable_tagged_wire_shape() {
        let kind = super::TaskEventKind::RunnerReported {
            incarnation: "pod-1".into(),
            event_cursor: 7,
            execution_status: crate::api::ExecutionStatusDto::Running,
            pending_input_id: None,
            attention: Vec::new(),
        };
        let value = serde_json::to_value(&kind).unwrap();
        assert_eq!(value["type"], "runner_reported");
        assert_eq!(value["execution_status"], "running");
        assert_eq!(
            serde_json::from_value::<super::TaskEventKind>(value).unwrap(),
            kind
        );

        let unknown = serde_json::json!({ "type": "future_event" });
        assert_eq!(
            serde_json::from_value::<super::TaskEventKind>(unknown).unwrap(),
            super::TaskEventKind::Unrecognized
        );
    }

    #[test]
    fn owner_conversation_response_keeps_summary_only_wire_fields() {
        let now = Utc::now();
        let response = super::ConversationResponse::Available {
            attempts: vec![super::ConversationAttemptSummary {
                attempt_id: "attempt-1".into(),
                state_id: Some("review".into()),
                entries: 3,
                closed: true,
                last_entry_at: Some(now),
            }],
            observed_at: now,
            source: super::ExecutionEventsSource::Storage,
            recorded_at: Some(now),
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["status"], "available");
        assert_eq!(value["source"], "storage");
        assert_eq!(value["attempts"][0]["entries"], 3);
        assert!(value["attempts"][0].get("content").is_none());
        assert_eq!(
            serde_json::from_value::<super::ConversationResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn runner_conversation_availability_keeps_its_tagged_shape() {
        let availability = super::ConversationAvailability::Unavailable {
            reason: "runner_unreachable".into(),
        };
        let value = serde_json::to_value(&availability).unwrap();
        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["reason"], "runner_unreachable");
        assert_eq!(
            serde_json::from_value::<super::ConversationAvailability>(value).unwrap(),
            availability
        );
    }

    #[test]
    fn unknown_conversation_reasons_keep_the_forward_compatible_fallback() {
        let reason: ConversationUnavailableReason =
            serde_json::from_str("\"new_server_reason\"").unwrap();
        assert_eq!(reason, ConversationUnavailableReason::Unrecognized);
    }

    #[test]
    fn freshness_keeps_snake_case_wire_values() {
        assert_eq!(serde_json::to_string(&Freshness::Live).unwrap(), "\"live\"");
        assert_eq!(
            serde_json::to_string(&Freshness::Stale).unwrap(),
            "\"stale\""
        );
    }

    #[test]
    fn timeline_response_uses_only_protocol_values() {
        let response = TimelineResponse {
            task_id: "task-123".into(),
            freshness: Freshness::Live,
            observed_at: None,
            timeline: None,
            execution_status: Some(crate::api::ExecutionStatusDto::Running),
            current_state: None,
            current_position: None,
        };
        let value = serde_json::to_value(&response).unwrap();

        assert_eq!(value["task_id"], "task-123");
        assert_eq!(value["freshness"], "live");
        assert_eq!(value["execution_status"], "running");
        assert_eq!(
            serde_json::from_value::<TimelineResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn queued_owner_responses_keep_their_wire_fields() {
        let input = SubmitInputResponse {
            input: InputView {
                input_id: "in-1".into(),
                pending_input_id: "wait-1".into(),
                status: InputStatus::Queued,
                submitted_at: Utc::now(),
                updated_at: Utc::now(),
                detail: None,
            },
            replayed: false,
        };
        let input_json = serde_json::to_value(input).unwrap();
        assert_eq!(input_json["input"]["input_id"], "in-1");
        assert_eq!(input_json["input"]["status"], "queued");
        assert_eq!(input_json["replayed"], false);

        let command = SubmitOperatorCommandResponse {
            command: OperatorCommandView {
                command_id: "cmd-1".into(),
                command: OperatorCommand::Interrupt,
                status: OperatorCommandStatus::Queued,
                submitted_at: Utc::now(),
                updated_at: Utc::now(),
                detail: None,
            },
            replayed: true,
        };
        let command_json = serde_json::to_value(command).unwrap();
        assert_eq!(command_json["command"]["command_id"], "cmd-1");
        assert_eq!(command_json["command"]["status"], "queued");
        assert_eq!(command_json["replayed"], true);
    }

    #[test]
    fn owner_observation_and_queue_views_keep_their_shapes() {
        let observation = ObservationView {
            observation: Observation::RunnerReady,
            summary: ObservedSummary {
                workload_present: true,
                storage_present: true,
                runner_status: Some(crate::api::ExecutionStatusDto::Running),
                detail: None,
            },
            resource: Some(ResourceIdentity {
                uid: "pod-uid".into(),
                resource_generation: 4,
            }),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            freshness: Freshness::Live,
        };
        let value = serde_json::to_value(observation).unwrap();
        assert_eq!(value["observation"]["kind"], "runner_ready");
        assert_eq!(value["summary"]["runner_status"], "running");
        assert_eq!(value["resource"]["uid"], "pod-uid");

        let queue = QueueView {
            reason: QueueReason::Admitted,
            message: "admitted; waiting for the controller to start it".into(),
            since: Utc::now(),
        };
        let value = serde_json::to_value(queue).unwrap();
        assert_eq!(value["reason"]["reason"], "admitted");
        assert_eq!(
            value["message"],
            "admitted; waiting for the controller to start it"
        );
    }

    #[test]
    fn task_harness_keeps_its_tagged_wire_shape() {
        let value = serde_json::to_value(TaskDefaultHarness::Unavailable {
            harness: "codex".into(),
            reason: "not installed".into(),
        })
        .unwrap();

        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["harness"], "codex");
        assert_eq!(value["reason"], "not installed");
    }

    #[test]
    fn capabilities_keep_optional_reason_omitted_when_absent() {
        let value = serde_json::to_value(CapabilitiesView {
            connection_name: "local".into(),
            workflow_paths: true,
            workflow_paths_reason: None,
            legacy_executions: true,
            local_repository_paths: true,
        })
        .unwrap();

        assert_eq!(value["connection_name"], "local");
        assert_eq!(value["workflow_paths"], true);
        assert!(value.get("workflow_paths_reason").is_none());
    }

    #[test]
    fn default_backend_keeps_the_override_wire_name_and_value() {
        let value = serde_json::to_value(DefaultBackendView {
            configured: "local".into(),
            override_backend: Some("k8s".into()),
            effective: "k8s".into(),
            revision: 3,
            updated_at: None,
        })
        .unwrap();

        assert_eq!(value["override"], "k8s");
        assert!(value.get("override_backend").is_none());
        assert_eq!(
            serde_json::to_value(SetDefaultBackendRequest { backend_id: None }).unwrap(),
            serde_json::json!({ "backend_id": null })
        );
    }

    #[test]
    fn workflow_input_keeps_the_wire_type_field_and_snake_case_value() {
        let value = serde_json::to_value(WorkflowInputView {
            name: "count".into(),
            input_type: WorkflowInputType::Integer,
            required: true,
            description: None,
            default: None,
        })
        .unwrap();

        assert_eq!(value["type"], "integer");
        assert_eq!(value["required"], true);
        assert!(value.get("input_type").is_none());
    }

    #[test]
    fn repository_request_refuses_unknown_fields() {
        let value =
            serde_json::json!({"id":"repo","remote":"https://example.test/r.git","secret":"no"});
        assert!(serde_json::from_value::<CreateRepositoryRequest>(value).is_err());
    }

    #[test]
    fn workflow_and_repository_views_keep_their_wire_names() {
        let workflow = WorkflowListResponse {
            workflows: vec![WorkflowView {
                name: "hello".into(),
                source: WorkflowSource::BuiltIn,
                description: None,
                version: Some(1),
                state_count: 1,
                initial_state: Some("start".into()),
                inputs: vec![WorkflowInputView {
                    name: "count".into(),
                    input_type: WorkflowInputType::Integer,
                    required: true,
                    description: None,
                    default: None,
                }],
                valid: true,
                violations: vec![],
                overrides: false,
                origin_label: "built-in".into(),
                shadow_labels: vec![],
                has_builtin: Some(true),
            }],
        };
        let repository = RepositoryListResponse {
            repositories: vec![RepositoryView {
                id: "repo".into(),
                name: "Repo".into(),
                provider: "github".into(),
                remote: "https://example.test/repo.git".into(),
                default_branch: None,
                description: None,
            }],
            unreadable: vec![],
        };

        let workflow_json = serde_json::to_value(workflow).unwrap();
        let repository_json = serde_json::to_value(repository).unwrap();
        assert_eq!(workflow_json["workflows"][0]["source"], "built_in");
        assert_eq!(workflow_json["workflows"][0]["state_count"], 1);
        assert_eq!(workflow_json["workflows"][0]["initial_state"], "start");
        assert_eq!(
            workflow_json["workflows"][0]["inputs"][0]["type"],
            "integer"
        );
        assert_eq!(repository_json["repositories"][0]["id"], "repo");
        assert_eq!(repository_json["unreadable"], serde_json::json!([]));
    }

    #[test]
    fn a_legacy_workflow_view_marks_builtin_support_as_unknown() {
        let workflow: WorkflowView = serde_json::from_value(serde_json::json!({
            "name": "example",
            "source": "built_in",
            "inputs": [],
            "valid": true,
            "overrides": false
        }))
        .expect("legacy workflow view remains readable");

        assert_eq!(workflow.has_builtin, None);
    }

    #[test]
    fn workflow_validation_keeps_the_path_request_and_result_fields() {
        let request = ValidateWorkflowRequest {
            path: "/workflows/task.yaml".into(),
        };
        let response = ValidateWorkflowResponse {
            valid: false,
            name: Some("task".into()),
            version: Some(2),
            state_count: 0,
            initial_state: None,
            inputs: vec![],
            violations: vec!["missing state".into()],
        };

        let request = serde_json::to_value(request).unwrap();
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(request["path"], "/workflows/task.yaml");
        assert_eq!(response["violations"][0], "missing state");
        assert_eq!(response["state_count"], 0);
    }

    #[test]
    fn controller_enrollment_and_listing_use_string_backend_ids() {
        let enrollment = EnrollmentResponse {
            enrollment_code: "once".into(),
            backend_id: "k8s".into(),
            expires_at: Utc::now(),
        };
        let view = ControllerView {
            controller_id: "ctrl-1".into(),
            backend_id: "k8s".into(),
            name: None,
            enrolled_at: Utc::now(),
            revoked_at: None,
            last_heartbeat_at: None,
            holds_lease: false,
        };
        let value = serde_json::to_value(enrollment).unwrap();
        let list = serde_json::to_value(ControllerListResponse {
            controllers: vec![view],
        })
        .unwrap();

        assert_eq!(value["backend_id"], "k8s");
        assert_eq!(list["controllers"][0]["backend_id"], "k8s");
        assert_eq!(
            serde_json::to_value(CreateEnrollmentRequest::default()).unwrap(),
            serde_json::json!({ "name": null, "ttl_secs": null })
        );
    }

    #[test]
    fn control_plane_queries_keep_their_default_wire_values() {
        assert_eq!(
            serde_json::to_value(ConversationPageQuery::default()).unwrap(),
            serde_json::json!({"from": 0, "byte_budget": null})
        );
        assert_eq!(
            serde_json::to_value(ExecutionEventsQuery::default()).unwrap(),
            serde_json::json!({"after": 0, "limit": null})
        );
        assert_eq!(
            serde_json::to_value(ConductorReadQuery::default()).unwrap(),
            serde_json::json!({"after": 0, "limit": null})
        );
        let events = ExecutionEventsResponse::Unavailable {
            reason: ConversationUnavailableReason::NotStarted,
            message: "runner has not started".into(),
        };
        assert_eq!(
            serde_json::to_value(events).unwrap(),
            serde_json::json!({
                "status": "unavailable",
                "reason": "not_started",
                "message": "runner has not started"
            })
        );
        let events = ExecutionEventsResponse::Available {
            events: vec![],
            has_more: false,
            source: ExecutionEventsSource::Storage,
        };
        assert_eq!(
            serde_json::to_value(events).unwrap(),
            serde_json::json!({
                "status": "available",
                "events": [],
                "has_more": false,
                "source": "storage"
            })
        );
    }

    #[test]
    fn move_request_keeps_its_tagged_wire_shape_with_opaque_task_ids() {
        let request = MoveTaskRequest {
            target: MoveTaskTarget::Before("task-2".into()),
            expected_generation: Some(4),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "target": {"to": "before", "task_id": "task-2"},
                "expected_generation": 4
            })
        );
        assert_eq!(
            serde_json::from_value::<MoveTaskRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn owner_request_values_keep_their_wire_shape() {
        let owner = OwnerCommandRequest {
            command: OwnerCommand::Pause,
            idempotency_key: "cmd-1".into(),
            expected_generation: Some(7),
        };
        assert_eq!(
            serde_json::to_value(owner).unwrap(),
            serde_json::json!({
                "command": "pause",
                "idempotency_key": "cmd-1",
                "expected_generation": 7
            })
        );

        let input = SubmitInputRequest {
            pending_input_id: "input-1".into(),
            idempotency_key: "answer-1".into(),
            delivery: HumanInputDeliveryDto {
                chosen_label: Some("yes".into()),
                ..HumanInputDeliveryDto::default()
            },
        };
        let value = serde_json::to_value(input).unwrap();
        assert_eq!(value["delivery"]["chosen_label"], "yes");
        assert_eq!(value["idempotency_key"], "answer-1");

        let command = SubmitOperatorCommandRequest {
            command: OperatorCommand::Transition {
                target: "done".into(),
                forced: true,
                reason: Some("operator decision".into()),
            },
            idempotency_key: "operator-1".into(),
            expected_generation: Some(8),
        };
        let value = serde_json::to_value(command).unwrap();
        assert_eq!(value["kind"], "transition");
        assert_eq!(value["override"], true);
        assert_eq!(value["reason"], "operator decision");
        assert_eq!(value["idempotency_key"], "operator-1");
    }

    #[test]
    fn task_creation_contract_uses_only_protocol_values() {
        let request = CreateTaskRequest {
            task_id: Some("task-1".into()),
            backend_id: Some("local".into()),
            launch: LaunchRequest {
                workflow_id: "hello".into(),
                workflow_version: 2,
                inputs: serde_json::json!({"count": 3}),
                workspace: Some(WorkspaceRequest {
                    repository: Some("repo".into()),
                    commit: Some("abc123".into()),
                }),
                profile: None,
                harness: None,
                resources: Some(ResourceProfileRequest {
                    cpu_millicores: 1500,
                    memory_mib: 2048,
                }),
            },
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["launch"]["workflow_id"], "hello");
        assert_eq!(value["launch"]["workspace"]["commit"], "abc123");
        assert_eq!(value["launch"]["resources"]["cpu_millicores"], 1500);
        assert_eq!(value["launch"]["resources"]["memory_mib"], 2048);
    }

    #[test]
    fn task_lifecycle_keeps_snake_case_states_and_archival_classification() {
        assert_eq!(
            serde_json::to_string(&RunnerState::PauseRequested).unwrap(),
            "\"pause_requested\""
        );
        assert!(!RunnerState::Stopped.is_archival());
        assert!(RunnerState::Archiving.is_archival());
        assert!(RunnerState::Archived.is_archival());
    }

    #[test]
    fn orchestration_summary_values_keep_their_wire_shapes() {
        assert_eq!(
            serde_json::to_value(Observation::WorkloadLost { state_known: false }).unwrap(),
            serde_json::json!({"kind": "workload_lost", "state_known": false})
        );
        assert_eq!(
            serde_json::to_value(InputStatus::Obsolete).unwrap(),
            serde_json::json!("obsolete")
        );
        assert_eq!(
            serde_json::to_value(OperatorCommandStatus::Rejected).unwrap(),
            serde_json::json!("rejected")
        );
        assert_eq!(
            serde_json::to_value(QueueReason::WaitingForCapacity {
                position: 2,
                running: 3,
                limit: 4,
            })
            .unwrap(),
            serde_json::json!({
                "reason": "waiting_for_capacity",
                "position": 2,
                "running": 3,
                "limit": 4
            })
        );
        let attention = AttentionItem {
            kind: AttentionKind::Question,
            state_id: None,
            since: Utc::now(),
            waiting_id: "input-1".into(),
        };
        assert_eq!(
            serde_json::to_value(attention).unwrap()["waiting_id"],
            "input-1"
        );
    }
}
