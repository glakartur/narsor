//! Records and messages of the control plane's task orchestration.
//!
//! Everything here is plain data: the transition rules live in
//! [`super::lifecycle`] and the durable mutation protocol in
//! [`super::control_plane`].

use crate::domain::{BackendId, ExecutionStatus, TaskId, TaskLaunchSpec};
use crate::engine::{HumanInputDelivery, PendingHumanInput, Timeline};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version of [`TaskRecord`].
pub const TASK_RECORD_VERSION: u32 = 1;

/// Current schema version of [`BackendLease`].
pub const BACKEND_LEASE_VERSION: u32 = 1;

/// Orchestration state of a task (the plan's state diagram).
///
/// Deliberately separate from [`ExecutionStatus`]: this is what the control
/// plane wants and last saw of the *infrastructure*, while the runner's
/// execution status stays the work-state detail on task storage.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycle {
    /// Waiting for a controller to accept it (admission is U10's concern).
    Queued,
    /// A controller accepted the start or a resume; the runner is not ready.
    Provisioning,
    /// The runner reported ready.
    Running,
    /// The owner asked to pause; waiting for the runner's safe point.
    PauseRequested,
    /// Safe point acknowledged and compute removed; storage retained (R12).
    Paused,
    /// The workload was observed to disappear unexpectedly (R13).
    Interrupted,
    /// The controller cannot tell what happened to the workload (R13, R14).
    Unknown,
    /// The owner archived it; waiting for workload and storage deletion.
    Archiving,
    /// Tombstone: infrastructure is gone, the record stays as the audit trail.
    Archived,
}

impl TaskLifecycle {
    /// True for [`Self::Archiving`] and [`Self::Archived`]: archive has won
    /// and no other desired state can be reached any more.
    pub fn is_archival(self) -> bool {
        matches!(self, Self::Archiving | Self::Archived)
    }
}

/// A lifecycle command issued by the task's owner through the control plane.
///
/// Owner commands need no controller lease: they change *desired* state,
/// which the control plane alone owns (KTD1). Fencing applies to what
/// controllers report back.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OwnerCommand {
    /// Retry an interrupted or unknown task (explicit owner decision, R13).
    Start,
    /// Bring a running task to a safe point and release its compute.
    Pause,
    /// Continue a paused, interrupted or unknown task on the same storage.
    Resume,
    /// Delete workload and storage (R34). Wins over every other state.
    Archive,
}

/// Opaque reference to the task's owner.
///
/// Stored, but deliberately not part of the task identity (R32, R33): a later
/// hand-over changes this field without changing [`TaskId`] or any backend
/// resource name. What the string means (an OIDC `(issuer, subject)` pair, a
/// local user) is U7's business.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct OwnerRef(pub String);

impl OwnerRef {
    /// Wraps an opaque owner reference.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Where a task runs, resolved once at creation (R10: never migrates).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAssignment {
    /// The backend the task is pinned to.
    pub backend_id: BackendId,
    /// Deterministic key the adapter derives its resource names from (R7),
    /// so a retried create reconciles an existing resource instead of
    /// duplicating it.
    pub resource_key: String,
}

/// Identity of the backend resource an observation is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceIdentity {
    /// Backend-assigned unique id of this incarnation (a Pod UID, a container
    /// id). A new incarnation after resume has a new one.
    pub uid: String,
    /// Monotonic version of that one resource as the backend reports it. A
    /// report older than the last accepted one for the same `uid` arrived out
    /// of order and is rejected.
    pub resource_generation: u64,
}

/// What a controller observed, as a lifecycle-relevant fact.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Observation {
    /// The controller took the queued start over (Queued → Provisioning).
    StartAccepted,
    /// The runner is up and reported ready (Provisioning → Running).
    RunnerReady,
    /// The runner acknowledged a safe point and the compute was removed
    /// (PauseRequested → Paused).
    PausedAtSafePoint,
    /// The workload disappeared without being asked to. `state_known: false`
    /// means the controller cannot even tell that much (→ Unknown).
    WorkloadLost {
        /// Whether the loss itself is certain (Interrupted) or not (Unknown).
        state_known: bool,
    },
    /// Workload and storage deletion acknowledged, or both found absent
    /// (Archiving → Archived).
    ResourcesDeleted,
    /// A status refresh with no lifecycle meaning of its own.
    Status,
}

/// Normalized summary of the last observation (KTD4): what clients may show,
/// nothing backend-specific and nothing secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservedSummary {
    /// Whether compute for the task exists.
    pub workload_present: bool,
    /// Whether task storage exists.
    pub storage_present: bool,
    /// The runner's own execution status, when it reported one.
    pub runner_status: Option<ExecutionStatus>,
    /// Short human-readable detail (a reason, a phase).
    pub detail: Option<String>,
}

/// The last controller observation accepted for a task.
///
/// `observed_at` is when the backend saw it, `received_at` when the control
/// plane accepted it; a client compares either with "now" to show a stale
/// observation explicitly instead of inventing a state (R14).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservedState {
    /// The observation itself.
    pub observation: Observation,
    /// What it said about the infrastructure and the runner.
    pub summary: ObservedSummary,
    /// The resource it was about, if any existed.
    pub resource: Option<ResourceIdentity>,
    /// Lease epoch of the controller that reported it.
    pub lease_epoch: u64,
    /// Desired generation the controller was acting on.
    pub desired_generation: u64,
    /// When the backend observed it.
    pub observed_at: DateTime<Utc>,
    /// When the control plane accepted it.
    pub received_at: DateTime<Utc>,
}

/// Proof that the controller reporting holds its backend's lease.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseToken {
    /// Backend the lease is for.
    pub backend_id: BackendId,
    /// Controller holding it.
    pub controller_id: String,
    /// Fencing epoch handed out at acquisition.
    pub epoch: u64,
}

/// The one fenced lease of a backend (KTD3).
///
/// Held per backend, not per task: one controller reconciles all of a
/// backend's tasks, and a takeover by a new controller incarnation must fence
/// every task at once. `epoch` only ever grows — each acquisition increments
/// it, a renewal keeps it — so a report carrying an older epoch is stale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendLease {
    /// Schema version of this record.
    pub record_version: u32,
    /// Backend the lease is for.
    pub backend_id: BackendId,
    /// Controller holding it.
    pub controller_id: String,
    /// Fencing epoch.
    pub epoch: u64,
    /// When this epoch was acquired.
    pub acquired_at: DateTime<Utc>,
    /// Until when it is valid unless renewed.
    pub expires_at: DateTime<Utc>,
}

impl BackendLease {
    /// The token a controller presents with every report.
    pub fn token(&self) -> LeaseToken {
        LeaseToken {
            backend_id: self.backend_id.clone(),
            controller_id: self.controller_id.clone(),
            epoch: self.epoch,
        }
    }
}

/// The durable acknowledgement of one owner command.
///
/// Kept forever in the task record: it is both the idempotency table and the
/// audit trail, so archive never erases it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandReceipt {
    /// Key the owner sent; unique per logical command.
    pub idempotency_key: String,
    /// The command.
    pub command: OwnerCommand,
    /// The generation the owner expected, if it sent one.
    pub expected_generation: Option<u64>,
    /// When it was accepted.
    pub accepted_at: DateTime<Utc>,
    /// Desired generation right after it was applied.
    pub generation_after: u64,
    /// Lifecycle right after it was applied.
    pub lifecycle_after: TaskLifecycle,
    /// Record revision (= event sequence) it produced.
    pub revision: u64,
}

/// Archive tombstone: archive won and the task never runs again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveTombstone {
    /// Idempotency key of the archive command that set it.
    pub idempotency_key: String,
    /// Desired generation the archive created; every older start or pause is
    /// fenced by it.
    pub generation: u64,
    /// When the owner archived.
    pub requested_at: DateTime<Utc>,
    /// When deletion was confirmed (or found unnecessary).
    pub completed_at: Option<DateTime<Utc>>,
    /// True when no controller ever accepted the task, so there was nothing
    /// to delete and archive completed immediately.
    pub resources_never_created: bool,
}

/// What changed in one accepted mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TaskEventKind {
    /// The task was created and queued.
    Created,
    /// An owner command was accepted.
    OwnerCommandAccepted {
        /// The command.
        command: OwnerCommand,
        /// Its idempotency key.
        idempotency_key: String,
    },
    /// A controller report was accepted.
    ControllerReported {
        /// What was observed.
        observation: Observation,
        /// Epoch of the reporting controller's lease.
        lease_epoch: u64,
    },
    /// A runner snapshot was accepted (KTD3). Carries only the cursors a
    /// client needs to decide whether to re-read the task, never content.
    RunnerReported {
        /// Pod (or container) incarnation that reported.
        incarnation: String,
        /// The runner's own engine event cursor.
        event_cursor: u64,
        /// The runner's execution status.
        execution_status: ExecutionStatus,
        /// The input request the runner waits on, if any.
        pending_input_id: Option<String>,
    },
    /// The owner queued an answer for a pending input request.
    InputQueued {
        /// Control-plane id of the queued input.
        input_id: String,
        /// The runner's pending-input id it answers.
        pending_input_id: String,
    },
    /// Queued inputs changed status (delivered, consumed, rejected or made
    /// obsolete by the runner moving on).
    InputsUpdated {
        /// Every input whose status changed, with its new status.
        updates: Vec<InputUpdate>,
    },
}

/// One status change of a queued input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputUpdate {
    /// The input.
    pub input_id: String,
    /// Its new status.
    pub status: InputStatus,
}

/// One entry of a task's durable control-plane event log.
///
/// `sequence` equals the record revision it produced, so the log and the
/// record share one counter and a client cursor is simply "last sequence
/// seen" (U2's SSE replay).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskEvent {
    /// Position in the task's log, starting at 1, without gaps.
    pub sequence: u64,
    /// Task it belongs to.
    pub task_id: TaskId,
    /// When the control plane accepted the mutation.
    pub at: DateTime<Utc>,
    /// Lifecycle after the mutation.
    pub lifecycle: TaskLifecycle,
    /// Desired generation after the mutation.
    pub generation: u64,
    /// What changed.
    pub kind: TaskEventKind,
}

/// The authoritative control-plane record of one task (KTD1).
///
/// Holds assignment, desired state, the last observation and the audit trail;
/// never the runner's work state, which stays on task storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    /// Schema version of this record.
    pub record_version: u32,
    /// Identity of the task.
    pub task_id: TaskId,
    /// Current owner; mutable in principle (R32).
    pub owner: OwnerRef,
    /// Resolved backend assignment.
    pub assignment: TaskAssignment,
    /// The immutable launch input (R18).
    pub launch_spec: TaskLaunchSpec,
    /// Owner priority for queue ordering (R11); higher first. Default 0.
    pub priority: i32,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// Creation order across all tasks, the FIFO tie-breaker among equal
    /// priorities (U10). Strictly increasing, never reused.
    pub queue_sequence: u64,
    /// Current orchestration state.
    pub lifecycle: TaskLifecycle,
    /// Desired generation: starts at 1, grows with every owner command that
    /// changes desired state. Controllers must report against the current one.
    pub generation: u64,
    /// Record revision: grows with every accepted mutation, equal to the
    /// sequence of the last event.
    pub revision: u64,
    /// Whether a controller ever accepted the task, so backend resources may
    /// exist. False means archive needs no deletion acknowledgement.
    pub resources_may_exist: bool,
    /// Highest lease epoch whose report was accepted.
    pub last_report_epoch: Option<u64>,
    /// The last accepted controller observation.
    pub observed: Option<ObservedState>,
    /// Set once archived; never cleared.
    pub archive: Option<ArchiveTombstone>,
    /// Every accepted owner command, in order.
    pub commands: Vec<CommandReceipt>,
    /// The last accepted runner snapshot (KTD4's "last safe summary"). Kept
    /// after the runner goes away, so a paused or interrupted task still
    /// shows what it last did — with its observation time, never upgraded
    /// to a fabricated terminal state.
    #[serde(default)]
    pub runner: Option<RunnerObservation>,
    /// Owner inputs queued for the runner, oldest first; the audit trail of
    /// every answer, whose payload is dropped once it is settled.
    #[serde(default)]
    pub inputs: Vec<QueuedInput>,
    /// The event of revision [`Self::revision`]: an outbox of one, so a crash
    /// between writing the record and appending the event is repaired from
    /// the record itself (see `OrchestrationStore::publish_last_event`).
    pub last_event: TaskEvent,
}

impl TaskRecord {
    /// Finds the receipt of an idempotency key.
    pub fn receipt(&self, idempotency_key: &str) -> Option<&CommandReceipt> {
        self.commands
            .iter()
            .find(|receipt| receipt.idempotency_key == idempotency_key)
    }

    /// Whether a runner launched at `launch_generation` was superseded by a
    /// later owner start or resume (KTD3).
    ///
    /// A start or resume launches a new incarnation on the same storage, so
    /// every credential and report of an older launch is fenced from that
    /// moment on — even before the controller issued the new incarnation's
    /// credential. Pause and archive do not supersede: the running runner
    /// must still see them and acknowledge its safe point.
    pub fn supersedes_launch(&self, launch_generation: u64) -> bool {
        self.commands.iter().any(|receipt| {
            matches!(receipt.command, OwnerCommand::Start | OwnerCommand::Resume)
                && receipt.generation_after > launch_generation
        })
    }

    /// The pending-input id of the last accepted runner snapshot.
    pub fn pending_input_id(&self) -> Option<&str> {
        self.runner
            .as_ref()
            .and_then(|runner| runner.snapshot.pending_input.as_ref())
            .map(|pending| pending.id.as_str())
    }

    /// Finds a queued input by id.
    pub fn input(&self, input_id: &str) -> Option<&QueuedInput> {
        self.inputs.iter().find(|input| input.input_id == input_id)
    }
}

/// Where the runner's engine stands, as positions rather than content.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionCursor {
    /// The execution on task storage, once the runner created it.
    pub execution_id: Option<String>,
    /// The workflow state it is in.
    pub current_state: Option<String>,
    /// The action execution (attempt) under way or last run.
    pub action_execution_id: Option<String>,
}

/// The input request a runner is waiting on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingInputRef {
    /// Identity of the request: [`PendingHumanInput::episode_key`]. An owner
    /// answer names it, and is refused for any other.
    pub id: String,
    /// The agent attempt that asked, when an agent asked.
    pub attempt_id: Option<String>,
    /// The form, exactly as the runner's engine recorded it, so a client
    /// renders it without the workflow definition.
    pub form: PendingHumanInput,
}

/// One attempt's conversation, summarized: never its content (KTD4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationAttemptSummary {
    /// Attempt (action execution) id.
    pub attempt_id: String,
    /// State the attempt ran in.
    pub state_id: Option<String>,
    /// Number of recorded entries.
    pub entries: u64,
    /// Whether the attempt's record is closed.
    pub closed: bool,
    /// When its last entry was written.
    pub last_entry_at: Option<DateTime<Utc>>,
}

/// Whether the runner can serve its conversation, and what it holds.
///
/// The raw transcript stays on task storage; the control plane only ever
/// stores this summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationAvailability {
    /// The runner is live and holds these attempts' records.
    Live {
        /// Summary of each attempt with a conversation record.
        attempts: Vec<ConversationAttemptSummary>,
    },
    /// The runner cannot serve it (for instance while shutting down).
    Unavailable {
        /// Why, for humans.
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

/// Everything a runner reports about its work (KTD3, KTD4).
///
/// Cursors and summaries only: workspace files, secret values and raw
/// conversation lines never leave task storage through this type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerSnapshot {
    /// Runner-assigned, strictly increasing within one incarnation. Orders
    /// reports that arrive out of order; restarts at any value with a new
    /// incarnation.
    pub snapshot_sequence: u64,
    /// The runner's execution status — the work-state detail.
    pub execution_status: ExecutionStatus,
    /// Where its engine stands.
    #[serde(default)]
    pub execution_cursor: ExecutionCursor,
    /// Sequence of the last event in the runner's own event log. Never goes
    /// backwards, not even across incarnations: the log lives on task
    /// storage, which outlives every Pod.
    pub event_cursor: u64,
    /// The input request it waits on.
    #[serde(default)]
    pub pending_input: Option<PendingInputRef>,
    /// Its conversation's availability and summary.
    #[serde(default)]
    pub conversation: ConversationAvailability,
    /// The step timeline (positions and links, no content).
    #[serde(default)]
    pub timeline: Option<Timeline>,
    /// True when the runner stands at a durable safe point and may lose its
    /// compute without losing work (the acknowledgement a pause waits for).
    #[serde(default)]
    pub safe_point: bool,
    /// When the runner took this snapshot.
    pub observed_at: DateTime<Utc>,
}

/// The last accepted runner snapshot with its provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerObservation {
    /// The incarnation that reported it.
    pub incarnation: String,
    /// The generation that incarnation was launched at.
    pub launch_generation: u64,
    /// The snapshot.
    pub snapshot: RunnerSnapshot,
    /// When the control plane accepted it.
    pub received_at: DateTime<Utc>,
    /// The desired generation of a pause this runner acknowledged with a
    /// safe point — the precondition for removing its compute (KTD2).
    #[serde(default)]
    pub safe_point_generation: Option<u64>,
}

/// What became of a queued owner input.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    /// Waiting for the runner to poll it.
    Queued,
    /// Handed to the runner; redelivered until it acknowledges.
    Delivered,
    /// The runner accepted it.
    Consumed,
    /// The runner refused it (its own validation).
    Rejected,
    /// The runner moved on to another request before it was delivered; it
    /// was never handed over and the runner was never touched.
    Obsolete,
}

impl InputStatus {
    /// True once nothing more can happen to the input.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Consumed | Self::Rejected | Self::Obsolete)
    }
}

/// One owner answer queued for the runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedInput {
    /// Control-plane id, unique within the task.
    pub input_id: String,
    /// Owner-chosen key; a retry with it returns this input.
    pub idempotency_key: String,
    /// The runner request it answers.
    pub pending_input_id: String,
    /// Its status.
    pub status: InputStatus,
    /// The answer itself, dropped once settled: the control plane relays
    /// input, it does not keep a copy of the conversation.
    pub delivery: Option<HumanInputDelivery>,
    /// When the owner submitted it.
    pub submitted_at: DateTime<Utc>,
    /// When its status last changed.
    pub updated_at: DateTime<Utc>,
    /// The runner's reason, when it rejected the input.
    pub detail: Option<String>,
}

/// An owner input as submitted.
#[derive(Debug, Clone, PartialEq)]
pub struct InputRequest {
    /// The runner request it answers.
    pub pending_input_id: String,
    /// Owner-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// The answer.
    pub delivery: HumanInputDelivery,
}

/// The result of queueing an owner input.
#[derive(Debug, Clone, PartialEq)]
pub struct InputOutcome {
    /// The queued input.
    pub input: QueuedInput,
    /// True when this was a retry answered from the existing input.
    pub replayed: bool,
}

/// The result of a runner snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerReportOutcome {
    /// The task as it is now.
    pub task: TaskRecord,
    /// False when the snapshot was a duplicate and changed nothing.
    pub applied: bool,
}

/// Everything needed to create a task.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTask {
    /// Identity of the task.
    pub task_id: TaskId,
    /// Its owner.
    pub owner: OwnerRef,
    /// Backend it is assigned to.
    pub backend_id: BackendId,
    /// Immutable launch input.
    pub launch_spec: TaskLaunchSpec,
    /// Queue priority, default 0.
    pub priority: i32,
}

impl NewTask {
    /// A task request with the default priority.
    pub fn new(
        task_id: TaskId,
        owner: OwnerRef,
        backend_id: BackendId,
        launch_spec: TaskLaunchSpec,
    ) -> Self {
        Self {
            task_id,
            owner,
            backend_id,
            launch_spec,
            priority: 0,
        }
    }

    /// Sets the queue priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// An owner command as submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    /// The command.
    pub command: OwnerCommand,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Optimistic concurrency: when set, the command is refused unless the
    /// task is still at this desired generation.
    pub expected_generation: Option<u64>,
}

impl CommandRequest {
    /// A command without an expected generation.
    pub fn new(command: OwnerCommand, idempotency_key: impl Into<String>) -> Self {
        Self {
            command,
            idempotency_key: idempotency_key.into(),
            expected_generation: None,
        }
    }

    /// Requires the task to be at `generation`.
    pub fn expecting_generation(mut self, generation: u64) -> Self {
        self.expected_generation = Some(generation);
        self
    }
}

/// The result of an owner command.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutcome {
    /// The receipt — identical for the first delivery and every retry.
    pub receipt: CommandReceipt,
    /// The task as it is now.
    pub task: TaskRecord,
    /// True when this delivery was a retry answered from the receipt.
    pub replayed: bool,
}

/// One report from a controller (KTD3).
#[derive(Debug, Clone, PartialEq)]
pub struct ControllerReport {
    /// Task it is about.
    pub task_id: TaskId,
    /// The reporting controller's lease.
    pub lease: LeaseToken,
    /// Desired generation the controller acted on; must be the current one.
    pub desired_generation: u64,
    /// What was observed.
    pub observation: Observation,
    /// The resource it was about, if any exists.
    pub resource: Option<ResourceIdentity>,
    /// Normalized summary.
    pub summary: ObservedSummary,
    /// When the backend observed it.
    pub observed_at: DateTime<Utc>,
}

/// The result of a controller report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportOutcome {
    /// The task as it is now.
    pub task: TaskRecord,
    /// False when the report was a duplicate and changed nothing.
    pub applied: bool,
}
