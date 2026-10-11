//! Records and messages of the control plane's task orchestration.
//!
//! Everything here is plain data: the transition rules live in
//! [`super::lifecycle`] and the durable mutation protocol in
//! [`super::control_plane`].

use crate::domain::{BackendId, ExecutionStatus, TaskDisplay, TaskId, TaskLaunchSpec};
use crate::engine::{HumanInputDelivery, PendingHumanInput, Timeline};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version of [`TaskRecord`].
///
/// Version 2 replaced the owner `priority` with the queue position
/// [`TaskRecord::rank`]; the store migrates a version 1 record when it first
/// reads it (see [`super::store`]).
pub const TASK_RECORD_VERSION: u32 = 2;

/// Current schema version of [`BackendLease`].
pub const BACKEND_LEASE_VERSION: u32 = 1;

/// Current schema version of [`DefaultBackendSelection`].
pub const DEFAULT_BACKEND_SELECTION_VERSION: u32 = 1;

/// Orchestration state of a task (the plan's state diagram).
///
/// Deliberately separate from [`ExecutionStatus`]: this is what the control
/// plane wants and last saw of the *infrastructure*, while the runner's
/// execution status stays the work-state detail on task storage.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycle {
    /// Waiting for a capacity reservation, or holding one and waiting for
    /// its controller to accept the start (U10, see [`super::scheduler`]).
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
    /// Retry an interrupted or unknown task (an explicit owner decision;
    /// the resume policy issues `Resume` instead, D58).
    Start,
    /// Bring a running task to a safe point and release its compute.
    Pause,
    /// Continue a paused, interrupted or unknown task on the same storage.
    Resume,
    /// Delete workload and storage (R34). Wins over every other state.
    Archive,
}

/// Who issued a lifecycle command: the owner, or the control plane's own
/// resume policy acting on the owner's behalf after a workload loss.
///
/// Only an owner command counts as the owner's decision: it resets the
/// task's [`ResumeState`], a policy command never does.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CommandActor {
    /// The task's owner, through a client. What every receipt written
    /// before the resume policy existed was.
    #[default]
    Owner,
    /// The automatic resume policy of the control plane.
    Policy,
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
    /// The workload of an `Unknown` task was checked again and found absent
    /// or dead while its storage is still there (Unknown → Interrupted), so
    /// a resume cannot end up with two runners on one storage.
    LossConfirmed,
    /// Workload and storage deletion acknowledged, or both found absent
    /// (Archiving → Archived).
    ResourcesDeleted,
    /// A status refresh with no lifecycle meaning of its own.
    Status,
    /// An observation of a newer version this one does not know. Never
    /// written: it only lets a client decode a newer server's event or
    /// summary instead of dropping it, and the control plane refuses it in
    /// a report.
    #[serde(other)]
    Unrecognized,
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
    /// Who issued it; receipts from before the resume policy read `Owner`.
    #[serde(default)]
    pub actor: CommandActor,
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
        /// What the runner says waits on the owner (U4). A change of this
        /// list is what a client patches a list row with, without a read.
        #[serde(default)]
        attention: Vec<AttentionItem>,
    },
    /// The owner queued an answer for a pending input request.
    InputQueued {
        /// Control-plane id of the queued input.
        input_id: String,
        /// The runner's pending-input id it answers.
        pending_input_id: String,
    },
    /// The owner queued a command for the runner of a running task (U3).
    /// Carries the command itself and who sent it: for a forced transition
    /// that is the audit of the override, its reason included.
    OperatorCommandQueued {
        /// Control-plane id of the queued command.
        command_id: String,
        /// What it asks the runner to do.
        command: OperatorCommand,
        /// The owner who sent it.
        requested_by: OwnerRef,
    },
    /// The runner settled an operator command, or the control plane made
    /// it obsolete before the runner ever saw it (U3).
    OperatorCommandResolved {
        /// The command.
        command_id: String,
        /// How it ended.
        status: OperatorCommandStatus,
        /// Why it was rejected; `None` when applied.
        detail: Option<String>,
    },
    /// The owner changed the queue priority of a queued task. No longer
    /// written — `rank` and [`Self::Moved`] replaced it — and kept only so
    /// that a log written by an older version still reads.
    PriorityChanged {
        /// The priority it was changed to.
        priority: i32,
    },
    /// The owner moved a queued task within its backend's queue.
    Moved {
        /// The task's new 1-based position.
        rank: u32,
        /// The backend's whole order after the move, first to last, so a
        /// client patches its list without reading it again.
        order: Vec<TaskId>,
    },
    /// The scheduler's verdict on a queued task changed: it was admitted,
    /// or why it waits changed (U10). Carried in the stream so a client
    /// shows the reason without polling.
    QueueChanged {
        /// The new verdict.
        reason: QueueReason,
    },
    /// Queued inputs changed status (delivered, consumed, rejected or made
    /// obsolete by the runner moving on).
    InputsUpdated {
        /// Every input whose status changed, with its new status.
        updates: Vec<InputUpdate>,
    },
    /// A task material changed (U9). Counts only: a material's file names
    /// and bytes never travel in an event, so the durable log and every SSE
    /// stream replaying it stay free of material content (R30, KTD4).
    MaterialChanged {
        /// The material.
        material_id: String,
        /// What happened to it.
        change: MaterialChange,
        /// How many files it holds.
        files: u32,
        /// Their total size in bytes.
        total_bytes: u64,
    },
    /// The resume policy resumed the task after its workload was lost: a
    /// `Resume` receipt with [`CommandActor::Policy`] was written in the
    /// same mutation.
    AutoResumed {
        /// Which consecutive attempt this is, starting at 1; 0 when the loss
        /// was found by a controller that took over since the launch and the
        /// policy did not count it (see `ResumeState::last_loss_uncounted`).
        attempt: u32,
        /// How many consecutive attempts the policy makes at most.
        limit: u32,
        /// What the controller said about the loss, if anything.
        #[serde(default)]
        loss_reason: Option<String>,
    },
    /// The resume policy gave up: the task stays interrupted and waits for
    /// its owner. Announced once per exhaustion.
    AutoResumeExhausted {
        /// How many consecutive attempts were made.
        attempts: u32,
        /// Why, for people (see [`ResumeExhaustion::reason`]).
        reason: String,
    },
    /// An event of a newer version this one does not know. Never written:
    /// it only lets a client keep reading a newer server's stream and log,
    /// taking the lifecycle and generation from the envelope.
    #[serde(other)]
    Unrecognized,
}

/// What the control plane remembers to resume a task after its workload was
/// lost (see `ControlPlane::report` and the resume policy).
///
/// Empty on every task that never lost its workload and on every record
/// written before the policy existed. Only an owner start or resume resets
/// it; another controller report never touches the exhaustion it holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResumeState {
    /// Losses in a row without a runner that stayed up long enough to count
    /// as a successful start.
    pub consecutive_losses: u32,
    /// When the policy may resume the task next; `None` while no automatic
    /// resume is planned (none needed, waiting for the owner, exhausted).
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// The last loss, recorded when the workload was lost — also when it
    /// went to `Unknown` first, so a later confirmation counts the loss
    /// from where it actually happened.
    pub last_loss: Option<WorkloadLoss>,
    /// Set once the policy gave up; the task waits for its owner.
    pub exhausted: Option<ResumeExhaustion>,
    /// Losses found by a controller that took over since the launch (a
    /// restart) that the policy left uncounted since the count last started
    /// anew. Capped at the policy's limit: a controller that keeps
    /// restarting gets no endless free attempts.
    pub uncounted_losses: u32,
    /// Whether the last loss was one of those: the resume after it is no
    /// counted attempt.
    pub last_loss_uncounted: bool,
}

impl ResumeState {
    /// True when nothing about a lost workload is remembered.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Where and when a task lost its workload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadLoss {
    /// The lifecycle the task was in when the workload was lost.
    pub from: TaskLifecycle,
    /// When the control plane accepted the loss report.
    pub at: DateTime<Utc>,
    /// Lease epoch of the controller that reported it.
    pub lease_epoch: u64,
    /// What the controller said about it, if anything.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The resume policy gave up on a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeExhaustion {
    /// Why, for people: names the last loss.
    pub reason: String,
    /// When the limit was reached.
    pub at: DateTime<Utc>,
    /// Whether [`TaskEventKind::AutoResumeExhausted`] was published for it,
    /// so the event appears exactly once.
    #[serde(default)]
    pub announced: bool,
}

/// What happened to a task material (U9).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialChange {
    /// The owner uploaded it; it waits for the runner.
    Uploaded,
    /// The runner published it from the task's outbox.
    Published,
    /// The runner wrote it onto task storage and acknowledged it.
    Delivered,
    /// The owner deleted it.
    Deleted,
    /// Its time to live ran out and the control plane removed it.
    Expired,
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
    /// 1-based position among the tasks of the backend, the queue order:
    /// the scheduler admits waiting tasks by it, lowest first. Dense —
    /// renumbered under the backend's schedule lock on every move — and
    /// kept by a task once it is admitted or finished.
    #[serde(default)]
    pub rank: u32,
    /// The priority a version 1 record carried, read only to number such a
    /// record once and never written back.
    #[serde(default, rename = "priority", skip_serializing)]
    pub(crate) legacy_priority: Option<i32>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// Creation order across all tasks, the tie-breaker among equal ranks
    /// (U10). Strictly increasing, never reused.
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
    /// Operator commands queued for the runner, oldest first; the audit
    /// trail of every interrupt, transition and restart the owner asked for
    /// (U3).
    #[serde(default)]
    pub operator_commands: Vec<QueuedOperatorCommand>,
    /// The highest number a command id was ever given (`command-<n>`), so
    /// ids stay unique after settled commands are forgotten. Records from
    /// before it was kept read 0; the highest id still on the record
    /// covers them.
    #[serde(default)]
    pub operator_command_counter: u64,
    /// Idempotency keys of commands forgotten since, newest last, so that a
    /// late retry of one is refused rather than carried out a second time.
    #[serde(default)]
    pub retired_command_keys: Vec<String>,
    /// The capacity slot the scheduler granted while the task is queued
    /// (U10). Only a task holding one is offered `Start`, and only such a
    /// task may be accepted by its controller; cleared once the task leaves
    /// `Queued`, from where its lifecycle itself holds the slot.
    #[serde(default)]
    pub reservation: Option<CapacityReservation>,
    /// Why the queued task waits, or that it was admitted; `None` outside
    /// `Queued` and before the scheduler first looked at it.
    #[serde(default)]
    pub queue: Option<QueueStatus>,
    /// Digest of the owner's create request, so a retry of the identical
    /// request returns this task without resolving the configuration again
    /// — which may have changed meanwhile (a new default backend).
    #[serde(default)]
    pub request_digest: Option<String>,
    /// When the runner of the current launch last reported ready; `None`
    /// before that and on records written before it was kept. Tells a loss
    /// right after a start from one after a run long enough to count.
    #[serde(default)]
    pub running_since: Option<DateTime<Utc>>,
    /// The newest controller lease epoch known to have the current workload
    /// in hand: the backend's epoch when a command ordered the launch,
    /// raised by every report about the live workload. A loss reported
    /// under a newer epoch was found by a controller that took over (or
    /// restarted) since, and does not count against the resume limit.
    /// `None` before the first launch and on records written before it was
    /// kept; such a loss counts.
    #[serde(default)]
    pub workload_epoch: Option<u64>,
    /// Automatic resume after a workload loss.
    #[serde(default)]
    pub resume: ResumeState,
    /// The event of revision [`Self::revision`]: an outbox of one, so a crash
    /// between writing the record and appending the event is repaired from
    /// the record itself (see `OrchestrationStore::publish_last_event`).
    pub last_event: TaskEvent,
}

impl TaskRecord {
    /// Refuses a request made against another generation than the task's
    /// current one.
    pub fn check_generation(&self, presented: u64) -> super::error::Result<()> {
        if presented == self.generation {
            return Ok(());
        }
        Err(super::error::OrchestrationError::StaleGeneration {
            task_id: self.task_id.clone(),
            current: self.generation,
            presented,
        })
    }

    /// Whether the resume policy may resume the task now: it is
    /// interrupted, its planned attempt is due and the policy has not given
    /// up on it.
    pub fn auto_resume_due(&self, now: DateTime<Utc>) -> bool {
        self.lifecycle == TaskLifecycle::Interrupted
            && self.resume.exhausted.is_none()
            && self.resume.next_attempt_at.is_some_and(|at| at <= now)
    }

    /// The receipt of the start or resume that produced `launch_generation`
    /// (see [`Self::launch_command`]).
    pub fn launch_receipt(&self, launch_generation: u64) -> Option<&CommandReceipt> {
        self.commands.iter().find(|receipt| {
            matches!(receipt.command, OwnerCommand::Start | OwnerCommand::Resume)
                && receipt.generation_after == launch_generation
        })
    }

    /// Whether the task occupies one of its backend's capacity slots.
    ///
    /// Conservative on purpose (R6, R11, R13): a slot is free only once the
    /// control plane *knows* the task has no compute.
    ///
    /// * `Provisioning`, `Running`, `PauseRequested` — compute exists or is
    ///   being created;
    /// * `Queued` with a reservation — admitted, the controller may create
    ///   compute any moment;
    /// * `Interrupted`, `Unknown` — the workload may still exist (a dead Pod
    ///   object, a node that is merely unreachable), and only a start/resume
    ///   (the owner's, or the resume policy's for a confirmed loss, D58) or
    ///   an archive resolves that; both keep or release the slot
    ///   explicitly, so a retry never has to wait for capacity;
    /// * `Archiving` — deletion is requested but not confirmed.
    ///
    /// `Paused` releases its slot: the safe point was acknowledged and the
    /// compute removed (R12). `Archived` and an unadmitted `Queued` task
    /// hold nothing.
    pub fn holds_capacity(&self) -> bool {
        match self.lifecycle {
            TaskLifecycle::Queued => self.reservation.is_some(),
            TaskLifecycle::Provisioning
            | TaskLifecycle::Running
            | TaskLifecycle::PauseRequested
            | TaskLifecycle::Interrupted
            | TaskLifecycle::Unknown
            | TaskLifecycle::Archiving => true,
            TaskLifecycle::Paused | TaskLifecycle::Archived => false,
        }
    }

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

    /// The command that launched the runner of `launch_generation`, when a
    /// command did (KTD3, D58).
    ///
    /// `None` for the first launch out of `Queued`, which no command
    /// produces. A runner that finds its task storage left behind by an
    /// unexpected stop reads this to learn that a newer launch generation
    /// was recorded by the control plane — its owner's start/resume or the
    /// resume policy's `Resume` — the only thing that lets it run ordinary
    /// recovery on that storage.
    pub fn launch_command(&self, launch_generation: u64) -> Option<OwnerCommand> {
        self.launch_receipt(launch_generation)
            .map(|receipt| receipt.command)
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

    /// Finds an operator command by id.
    pub fn operator_command(&self, command_id: &str) -> Option<&QueuedOperatorCommand> {
        self.operator_commands
            .iter()
            .find(|command| command.command_id == command_id)
    }

    /// Why an operator command cannot be queued for this task's runner
    /// right now, or `None` when it can: only a running task with a runner
    /// of its current launch has someone to carry it out.
    pub fn operator_command_refusal(&self) -> Option<String> {
        let reason = match self.lifecycle {
            TaskLifecycle::Running => None,
            TaskLifecycle::Queued | TaskLifecycle::Provisioning => {
                Some("the task has not started yet")
            }
            TaskLifecycle::PauseRequested => Some("the task is pausing"),
            TaskLifecycle::Paused => Some("the task is paused; resume it first"),
            TaskLifecycle::Interrupted | TaskLifecycle::Unknown => {
                Some("the task's runner is gone; start or resume the task first")
            }
            TaskLifecycle::Archiving | TaskLifecycle::Archived => Some("the task is archived"),
        };
        if let Some(reason) = reason {
            return Some(format!("{reason} (lifecycle: {:?})", self.lifecycle));
        }
        match &self.runner {
            None => Some("the task's runner has not reported yet".to_string()),
            Some(runner) if self.supersedes_launch(runner.launch_generation) => {
                Some("the task's new runner has not reported yet".to_string())
            }
            Some(_) => None,
        }
    }
}

/// A capacity slot granted to a queued task (U10).
///
/// Durable on the task record itself, so a reservation and the task it
/// belongs to are committed together and survive a restart: what counts
/// against a backend's limit is always derivable from the records alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityReservation {
    /// When the scheduler granted it.
    pub granted_at: DateTime<Utc>,
}

/// The scheduler's verdict on a queued task (U10), for humans and clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum QueueReason {
    /// Holds a reservation; its controller is offered the start.
    Admitted,
    /// Every slot of the backend is taken.
    WaitingForCapacity {
        /// 1-based place among the backend's waiting tasks, by `rank`.
        position: u32,
        /// Slots in use: tasks holding compute plus admitted tasks whose
        /// start was not accepted yet.
        running: u32,
        /// The backend's `max_concurrent_tasks`, as configured right now.
        limit: u32,
    },
    /// No controller holds an unexpired lease of the backend (it is down,
    /// unreachable or was revoked), so no new start is admitted: a slot
    /// handed out now could only be used once a controller returns, and by
    /// then the owner may have reordered the queue. Carries no position on
    /// purpose: while nothing can start, the order is not news, and every
    /// archive would otherwise re-announce every waiting task.
    ControllerOffline,
    /// The backend is not registered any more, or its record is unusable.
    BackendUnknown {
        /// What is wrong with it.
        detail: String,
    },
}

impl QueueReason {
    /// A one-line explanation for people.
    pub fn describe(&self) -> String {
        match self {
            QueueReason::Admitted => "admitted; waiting for the controller to start it".into(),
            QueueReason::WaitingForCapacity {
                position,
                running,
                limit,
            } => format!(
                "waiting for capacity: position {position} in the queue, \
                 {running} of {limit} slots in use"
            ),
            QueueReason::ControllerOffline => {
                "waiting for the backend's controller to come online".into()
            }
            QueueReason::BackendUnknown { detail } => {
                format!("cannot be scheduled: {detail}")
            }
        }
    }
}

/// The current [`QueueReason`] of a queued task and since when it holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueStatus {
    /// The verdict.
    pub reason: QueueReason,
    /// When the scheduler reached it.
    pub since: DateTime<Utc>,
}

/// Capacity of a backend as configuration states it right now (U10).
///
/// Read fresh by the caller on every scheduling decision, so a changed
/// limit applies to the next decision without a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendCapacity {
    /// `max_concurrent_tasks` of a usable backend record.
    Limit(u32),
    /// The backend cannot be used; why.
    Unavailable(String),
}

/// The durable owner choice of the default backend (R10).
///
/// Only an *override* of the configured `default_backend`; absent or
/// `backend: None` means the configured value applies. It is read when a
/// task is created and never again for that task, so changing it moves
/// nothing that already exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultBackendSelection {
    /// Schema version.
    pub record_version: u32,
    /// Grows with every change.
    pub revision: u64,
    /// The chosen backend, `None` after a reset.
    pub backend: Option<BackendId>,
    /// When it last changed.
    pub updated_at: Option<DateTime<Utc>>,
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

/// What a task waits on its owner for.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// A question to answer: an agent asked, or a human step waits.
    Question,
    /// An agent result held for an approval decision.
    Approval,
    /// A step ended without a usable output the owner must settle, or the
    /// runner reports a condition that keeps it from working.
    Problem,
    /// A step that failed for good and waits for the owner (the waiting id
    /// is the failure's key). With a conductor it comes with a diagnosis
    /// and proposals, without one it is the bare failure.
    FailedStep,
    /// A proposal of the conductor's from the chat, waiting for the owner's
    /// confirmation (the waiting id is the proposal's id).
    ConductorProposal,
    /// The backend could not provision the task; no execution exists
    /// (added by the control plane, never by a runner).
    ProvisioningFailed,
}

/// One thing a task waits on its owner for — metadata only; the content
/// (question text, proposal) is read with the task's details.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionItem {
    /// What kind of waiting it is.
    pub kind: AttentionKind,
    /// The workflow state it concerns, when it concerns one.
    #[serde(default)]
    pub state_id: Option<String>,
    /// Since when it waits.
    pub since: DateTime<Utc>,
    /// Identity of the wait: the pending-input id for a question, approval
    /// or settle-the-step problem, the problem code for a runner problem,
    /// the proposal or failure key for the conductor's. Stable while the
    /// same wait lasts; an answer names it.
    pub waiting_id: String,
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
    /// Why the runner cannot do its work, when it cannot — for instance a
    /// base image that breaks the runner contract (R22), or a storage it
    /// will not touch before a newer launch generation authorizes it (the
    /// owner's or the resume policy's command, D58). Diagnosable text, never
    /// a secret.
    #[serde(default)]
    pub problem: Option<RunnerProblem>,
    /// What waits on the owner, oldest first (U4). Computed by the runner
    /// from its pending input and problem; the conductor's items (U5) join
    /// it. Provisioning failures are not here: only the control plane knows
    /// them.
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
    /// Whether the runner hosts a conductor (U5): its harness is usable in
    /// this task's environment. Without one a failed step goes to the
    /// owner as it is, with no diagnosis and no proposals.
    #[serde(default)]
    pub conductor_available: bool,
    /// Why there is no conductor, when there is none.
    #[serde(default)]
    pub conductor_unavailable_reason: Option<String>,
    /// The installation's default harness this task cannot switch to, while
    /// it cannot (U7, R16c): its backend does not run it, or its binary is
    /// not in the task's environment. The task keeps its harness meanwhile;
    /// this is information for the owner, never a [`RunnerProblem`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness_unavailable: Option<HarnessUnavailable>,
    /// The workflow's `display` rendered against the execution's inputs and
    /// current variables. `None` when the runner could not render one (no
    /// execution yet), which leaves what the control plane rendered at
    /// creation in place; the last report that has one wins.
    #[serde(default)]
    pub display: Option<TaskDisplay>,
    /// When the runner took this snapshot.
    pub observed_at: DateTime<Utc>,
}

/// A default harness a task cannot follow (U7, R16c).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessUnavailable {
    /// The installation's default the task cannot switch to.
    pub harness: String,
    /// The harness the task keeps running on meanwhile.
    pub kept: String,
    /// Why it cannot switch, for the owner.
    pub reason: String,
}

/// A condition that keeps a runner from doing its work.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProblem {
    /// Stable machine-readable code (`base_image_contract`,
    /// `owner_decision_required`, `initialization_failed`…).
    pub code: String,
    /// What exactly is wrong, for the owner.
    pub message: String,
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

/// What an owner asks the runner of a running task to do (U3).
///
/// The conductor's commands (chat, proposals, undo, harness) ride the same
/// queue, delivery and acknowledgement as the step commands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperatorCommand {
    /// Cut the step in flight short; the execution is left recoverable.
    Interrupt,
    /// A manual transition out of the current state.
    Transition {
        /// The state to go to: one a declared transition of the current
        /// state leads to, or — with `forced` — any state of the workflow.
        target: String,
        /// Whether this is an override of the declared transitions; needs
        /// the current state's `allow_manual_override` and a `reason`.
        #[serde(default, rename = "override")]
        forced: bool,
        /// Why the owner overrides; recorded as the audit of the override.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Start the failed or interrupted step again.
    RestartStep {
        /// An instruction for the agent's next attempt.
        #[serde(default)]
        note: Option<String>,
        /// The key of the step failure being answered; the runner refuses
        /// the command when the execution has moved on from it.
        #[serde(default)]
        failure_key: Option<String>,
    },
    /// Write to the conductor; its answer follows as further chat entries.
    ConductorChat {
        /// What the owner writes.
        text: String,
    },
    /// Carry out a proposal of the conductor's.
    ConfirmProposal {
        /// The proposal: the id of its chat entry.
        proposal_id: String,
    },
    /// Turn a proposal of the conductor's down.
    RejectProposal {
        /// The proposal: the id of its chat entry.
        proposal_id: String,
    },
    /// Take back a chosen output of the conductor's.
    UndoDecision {
        /// The decision to undo.
        decision_id: String,
    },
    /// Historical: run the conductor on another harness. No longer accepted
    /// (the conductor follows the installation's default harness); kept so
    /// that event logs and queues written before still decode. The control
    /// plane refuses it, a runner that finds one queued settles it unapplied.
    SetConductorHarness {
        /// The harness name.
        harness: String,
    },
}

/// Why a [`OperatorCommand::SetConductorHarness`] is refused or settled
/// unapplied: the harness is no longer chosen per task.
pub const RETIRED_CONDUCTOR_HARNESS: &str =
    "the conductor's harness is no longer chosen per task; the conductor follows the \
     installation's default harness";

impl QueuedOperatorCommand {
    /// Drops what a settled command no longer needs: the text of a conductor
    /// message, which is up to thousands of characters the record, its
    /// views and its events would carry from then on.
    pub fn forget_text(&mut self) {
        if let OperatorCommand::ConductorChat { text } = &mut self.command {
            text.clear();
        }
    }
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

/// What became of a queued operator command.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OperatorCommandStatus {
    /// Waiting for the runner; handed over on every poll until it settles.
    Queued,
    /// The runner carried it out.
    Applied,
    /// The runner refused it, or it became obsolete before the runner saw it.
    Rejected,
}

impl OperatorCommandStatus {
    /// True once nothing more can happen to the command.
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Queued)
    }
}

/// One operator command queued for a runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedOperatorCommand {
    /// Control-plane id, unique within the task.
    pub command_id: String,
    /// Owner-chosen key; a retry with it returns this command.
    pub idempotency_key: String,
    /// What it asks.
    pub command: OperatorCommand,
    /// The owner who sent it.
    pub requested_by: OwnerRef,
    /// The desired generation the owner expected, if it sent one.
    pub expected_generation: Option<u64>,
    /// The launch generation of the runner it is meant for: a runner of
    /// another launch never carries it out.
    pub launch_generation: u64,
    /// Its status.
    pub status: OperatorCommandStatus,
    /// When it was queued.
    pub submitted_at: DateTime<Utc>,
    /// When its status last changed.
    pub updated_at: DateTime<Utc>,
    /// Why it was rejected.
    pub detail: Option<String>,
}

/// An operator command as submitted.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorCommandRequest {
    /// What it asks.
    pub command: OperatorCommand,
    /// Owner-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Optimistic concurrency: refused unless the task is still at this
    /// desired generation.
    pub expected_generation: Option<u64>,
    /// The owner sending it.
    pub requested_by: OwnerRef,
}

/// The result of queueing an operator command.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorCommandOutcome {
    /// The queued command.
    pub command: QueuedOperatorCommand,
    /// True when this was a retry answered from the existing command.
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
    /// Digest of the owner request it came from, if any (see
    /// [`TaskRecord::request_digest`]).
    pub request_digest: Option<String>,
}

impl NewTask {
    /// A task request; the task joins the end of its backend's queue.
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
            request_digest: None,
        }
    }

    /// Records the digest of the owner request.
    pub fn with_request_digest(mut self, digest: impl Into<String>) -> Self {
        self.request_digest = Some(digest.into());
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
    /// Who issues it; recorded on the receipt.
    pub actor: CommandActor,
}

impl CommandRequest {
    /// An owner command without an expected generation.
    pub fn new(command: OwnerCommand, idempotency_key: impl Into<String>) -> Self {
        Self {
            command,
            idempotency_key: idempotency_key.into(),
            expected_generation: None,
            actor: CommandActor::Owner,
        }
    }

    /// Issued by `actor` instead of the owner.
    pub fn issued_by(mut self, actor: CommandActor) -> Self {
        self.actor = actor;
        self
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

/// Where [`super::ControlPlane::move_task`] puts a queued task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "to", content = "task_id")]
pub enum MoveTarget {
    /// Directly before the named task.
    Before(TaskId),
    /// Directly after the named task.
    After(TaskId),
    /// First of the backend's tasks.
    Top,
    /// Last of the backend's tasks.
    Bottom,
}
