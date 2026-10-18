//! Records and messages of the control plane's task orchestration.
//!
//! Everything here is plain data: the transition rules live in
//! [`super::lifecycle`] and the durable mutation protocol in
//! [`super::control_plane`].

use crate::domain::{BackendId, ExecutionStatus, TaskDisplay, TaskId, TaskLaunchSpec};
use crate::engine::{HumanInputDelivery, PendingHumanInput, Timeline};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use narsor_protocol::controller::{
    ArchiveTombstone, AttentionItem, AttentionKind, CommandActor, CommandReceipt,
    ConversationAttemptSummary, ConversationAvailability, ExecutionCursor, HarnessUnavailable,
    InputStatus, InputUpdate, MaterialChange, Observation, ObservedSummary, OperatorCommand,
    OperatorCommandStatus, OwnerCommand, OwnerRef, QueueReason, ResourceIdentity, ResumeExhaustion,
    ResumeState, RunnerProblem, RunnerState, SlotReleaseReason, TaskEvent, TaskEventKind,
    TaskState, WorkloadLoss,
};

/// Current schema version of [`TaskRecord`].
///
/// Version 2 replaced the owner `priority` with the queue position
/// [`TaskRecord::rank`]; the store migrates a version 1 record when it first
/// reads it (see [`super::store`]).
pub const TASK_RECORD_VERSION: u32 = 2;

/// How much a read of a task's details adds to its slot-release wait at
/// least: a read that would move the deadline by less changes nothing, so
/// a client that refreshes often does not write the record each time.
pub fn slot_release_extension_threshold() -> chrono::Duration {
    chrono::Duration::seconds(30)
}

/// What the execution status of a task's last snapshot means for its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotReleaseClass {
    /// The execution ended (`Completed`, `Cancelled`): the slot is freed at
    /// once. Not [`ExecutionStatus::is_terminal`], which also counts
    /// `Failed`.
    Final,
    /// The execution waits for its owner (`InputNeeded`, `Failed`): the slot
    /// is freed after a wait.
    Transient,
    /// Any other status: the slot is kept.
    Other,
}

impl SlotReleaseClass {
    /// The class of an execution status.
    pub fn of(status: ExecutionStatus) -> Self {
        match status {
            ExecutionStatus::Completed | ExecutionStatus::Cancelled => Self::Final,
            ExecutionStatus::InputNeeded | ExecutionStatus::Failed => Self::Transient,
            _ => Self::Other,
        }
    }

    /// Why a slot was freed in this class, `None` for [`Self::Other`].
    pub fn release_reason(self) -> Option<SlotReleaseReason> {
        match self {
            Self::Final => Some(SlotReleaseReason::Terminal),
            Self::Transient => Some(SlotReleaseReason::GraceElapsed),
            Self::Other => None,
        }
    }
}

/// Works out the [`TaskState`] of a task from its runner state, the execution
/// status of the runner's last snapshot (`None`: no snapshot yet) and the
/// resume policy (D58). The first matching rule wins:
///
/// 1. `Archived` is `Archived`; 2. `Archiving` is `Finished`;
/// 3. `Queued` is `Queued`, whatever the last snapshot says;
/// 4. `Unknown`, or `Interrupted` with the resume limit used up, is `Error`;
/// 5. execution `Completed` or `Cancelled` is `Finished`;
/// 6. execution `Failed` is `Error`;
/// 7. execution `InputNeeded` is `HumanNeeded`;
/// 8. `Stopped` or `PauseRequested` is `Paused`;
/// 9. `Provisioning` without a snapshot is `Queued`;
/// 10. the rest is `Running`: `Running`, `Provisioning` with a snapshot, and
///     `Interrupted` while the resume policy still waits to try again.
///
/// Rules 5 to 7 come before 8, so a task stopped to free its slot keeps the
/// state its work reached. Rule 8 reads `PauseRequested` as the owner's
/// pause only when `pause_by_policy` is false: a stop the slot-release
/// policy asked for is no decision of the owner, so a task whose work moved
/// on meanwhile (execution `Running`) stays `Running`.
pub fn derive_task_state(
    runner_state: RunnerState,
    execution: Option<ExecutionStatus>,
    resume: &ResumeState,
    pause_by_policy: bool,
) -> TaskState {
    match runner_state {
        RunnerState::Archived => return TaskState::Archived,
        RunnerState::Archiving => return TaskState::Finished,
        RunnerState::Queued => return TaskState::Queued,
        RunnerState::Unknown => return TaskState::Error,
        RunnerState::Interrupted if resume.exhausted.is_some() => return TaskState::Error,
        _ => {}
    }
    match execution {
        Some(ExecutionStatus::Completed | ExecutionStatus::Cancelled) => TaskState::Finished,
        Some(ExecutionStatus::Failed) => TaskState::Error,
        Some(ExecutionStatus::InputNeeded) => TaskState::HumanNeeded,
        _ => match runner_state {
            RunnerState::PauseRequested if pause_by_policy => TaskState::Running,
            RunnerState::Stopped | RunnerState::PauseRequested => TaskState::Paused,
            RunnerState::Provisioning if execution.is_none() => TaskState::Queued,
            _ => TaskState::Running,
        },
    }
}

/// Current schema version of [`BackendLease`].
pub const BACKEND_LEASE_VERSION: u32 = 1;

/// Current schema version of [`DefaultBackendSelection`].
pub const DEFAULT_BACKEND_SELECTION_VERSION: u32 = 1;

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
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
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
    /// `Queued`, from where its runner_state itself holds the slot.
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
    /// When the control plane frees the slot of the running task, because
    /// its work ended or waits for its owner (D62). Set by the runner
    /// snapshot that shows such a state, cleared by any other; lives in the
    /// record so a restart of the daemon neither loses nor shortens it.
    #[serde(default)]
    pub slot_release_due_at: Option<DateTime<Utc>>,
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
        self.runner_state == RunnerState::Interrupted
            && self.resume.exhausted.is_none()
            && self.resume.next_attempt_at.is_some_and(|at| at <= now)
    }

    /// How the last runner snapshot classifies the task's execution for
    /// its slot; `None` before the first snapshot.
    pub fn slot_release_class(&self) -> Option<SlotReleaseClass> {
        self.runner
            .as_ref()
            .map(|runner| SlotReleaseClass::of(runner.snapshot.execution_status))
    }

    /// Whether the owner's work waits for the runner: an answer not yet
    /// settled or an operator command not yet resolved. The slot is never
    /// freed under it.
    pub fn owner_work_pending(&self) -> bool {
        self.inputs.iter().any(|input| !input.status.is_settled())
            || self
                .operator_commands
                .iter()
                .any(|command| command.status == OperatorCommandStatus::Queued)
    }

    /// Whether the slot-release policy may stop the task now: it runs, its
    /// execution still is in a state that frees the slot, the deadline came
    /// and nothing of the owner's waits for the runner.
    pub fn slot_release_due(&self, now: DateTime<Utc>) -> bool {
        self.runner_state == RunnerState::Running
            && self.slot_release_due_at.is_some_and(|at| at <= now)
            && self
                .slot_release_class()
                .is_some_and(|class| class != SlotReleaseClass::Other)
            && !self.owner_work_pending()
    }

    /// Whether the pause the task now waits to complete is the slot-release
    /// policy's, not the owner's.
    pub fn pause_requested_by_policy(&self) -> bool {
        self.runner_state == RunnerState::PauseRequested
            && self.commands.iter().rev().any(|receipt| {
                receipt.command == OwnerCommand::Pause
                    && receipt.generation_after == self.generation
                    && receipt.actor == CommandActor::Policy
            })
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
    /// `Stopped` releases its slot: the safe point was acknowledged and the
    /// compute removed (R12). `Archived` and an unadmitted `Queued` task
    /// hold nothing.
    pub fn holds_capacity(&self) -> bool {
        match self.runner_state {
            RunnerState::Queued => self.reservation.is_some(),
            RunnerState::Provisioning
            | RunnerState::Running
            | RunnerState::PauseRequested
            | RunnerState::Interrupted
            | RunnerState::Unknown
            | RunnerState::Archiving => true,
            RunnerState::Stopped | RunnerState::Archived => false,
        }
    }

    /// What the owner sees of the task; see [`derive_task_state`].
    pub fn task_state(&self) -> TaskState {
        derive_task_state(
            self.runner_state,
            self.runner
                .as_ref()
                .map(|runner| runner.snapshot.execution_status),
            &self.resume,
            self.pause_requested_by_policy(),
        )
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
        let reason = match self.runner_state {
            RunnerState::Running => None,
            RunnerState::Queued | RunnerState::Provisioning => Some("the task has not started yet"),
            RunnerState::PauseRequested => Some("the task is pausing"),
            RunnerState::Stopped => Some("the task is paused; resume it first"),
            RunnerState::Interrupted | RunnerState::Unknown => {
                Some("the task's runner is gone; start or resume the task first")
            }
            RunnerState::Archiving | RunnerState::Archived => Some("the task is archived"),
        };
        if let Some(reason) = reason {
            return Some(format!("{reason} (lifecycle: {:?})", self.runner_state));
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

#[cfg(test)]
mod tests {
    use super::*;
    use RunnerState as R;
    use TaskState as T;

    fn exhausted() -> ResumeState {
        ResumeState {
            exhausted: Some(ResumeExhaustion {
                reason: "the limit was used up".to_string(),
                at: Utc::now(),
                announced: false,
            }),
            ..ResumeState::default()
        }
    }

    fn derive(runner: RunnerState, execution: Option<ExecutionStatus>) -> TaskState {
        derive_task_state(runner, execution, &ResumeState::default(), false)
    }

    #[test]
    fn a_finished_run_is_finished_with_or_without_a_runner() {
        assert_eq!(
            derive(R::Running, Some(ExecutionStatus::Completed)),
            T::Finished
        );
        assert_eq!(
            derive(R::Stopped, Some(ExecutionStatus::Completed)),
            T::Finished
        );
        assert_eq!(
            derive(R::Running, Some(ExecutionStatus::Cancelled)),
            T::Finished
        );
        assert_eq!(
            derive(R::PauseRequested, Some(ExecutionStatus::Completed)),
            T::Finished
        );
    }

    #[test]
    fn a_run_waiting_for_a_human_needs_one_with_or_without_a_runner() {
        assert_eq!(
            derive(R::Running, Some(ExecutionStatus::InputNeeded)),
            T::HumanNeeded
        );
        assert_eq!(
            derive(R::Stopped, Some(ExecutionStatus::InputNeeded)),
            T::HumanNeeded
        );
    }

    #[test]
    fn a_stopped_task_with_work_left_is_paused() {
        assert_eq!(
            derive(R::Stopped, Some(ExecutionStatus::Running)),
            T::Paused
        );
        assert_eq!(derive(R::Stopped, None), T::Paused);
        assert_eq!(
            derive(R::PauseRequested, Some(ExecutionStatus::Running)),
            T::Paused
        );
    }

    #[test]
    fn a_queued_task_is_queued_whatever_its_last_snapshot_says() {
        assert_eq!(
            derive(R::Queued, Some(ExecutionStatus::InputNeeded)),
            T::Queued
        );
        assert_eq!(
            derive(R::Queued, Some(ExecutionStatus::Completed)),
            T::Queued
        );
        assert_eq!(derive(R::Queued, None), T::Queued);
    }

    #[test]
    fn provisioning_is_queued_until_the_runner_has_reported() {
        assert_eq!(derive(R::Provisioning, None), T::Queued);
        assert_eq!(
            derive(R::Provisioning, Some(ExecutionStatus::Running)),
            T::Running
        );
    }

    #[test]
    fn a_lost_or_failed_task_is_an_error_unless_the_policy_still_retries() {
        assert_eq!(derive(R::Unknown, Some(ExecutionStatus::Running)), T::Error);
        assert_eq!(derive(R::Running, Some(ExecutionStatus::Failed)), T::Error);
        assert_eq!(
            derive_task_state(
                R::Interrupted,
                Some(ExecutionStatus::Running),
                &exhausted(),
                false
            ),
            T::Error
        );
        assert_eq!(
            derive_task_state(
                R::Interrupted,
                Some(ExecutionStatus::Running),
                &ResumeState::default(),
                false
            ),
            T::Running
        );
    }

    #[test]
    fn a_stop_the_policy_asked_for_does_not_read_as_a_pause() {
        let derive = |pause_by_policy| {
            derive_task_state(
                R::PauseRequested,
                Some(ExecutionStatus::Running),
                &ResumeState::default(),
                pause_by_policy,
            )
        };
        assert_eq!(derive(false), T::Paused);
        assert_eq!(derive(true), T::Running);
        // The state the work reached still wins over both.
        assert_eq!(
            derive_task_state(
                R::PauseRequested,
                Some(ExecutionStatus::InputNeeded),
                &ResumeState::default(),
                true
            ),
            T::HumanNeeded
        );
    }

    #[test]
    fn only_completed_and_cancelled_free_a_slot_at_once() {
        use SlotReleaseClass as C;
        assert_eq!(C::of(ExecutionStatus::Completed), C::Final);
        assert_eq!(C::of(ExecutionStatus::Cancelled), C::Final);
        assert_eq!(C::of(ExecutionStatus::InputNeeded), C::Transient);
        assert_eq!(C::of(ExecutionStatus::Failed), C::Transient);
        for other in [
            ExecutionStatus::Running,
            ExecutionStatus::Paused,
            ExecutionStatus::WaitingForEvent,
            ExecutionStatus::Unknown,
        ] {
            assert_eq!(C::of(other), C::Other);
        }
    }

    #[test]
    fn archival_wins_over_everything() {
        assert_eq!(
            derive(R::Archiving, Some(ExecutionStatus::Running)),
            T::Finished
        );
        assert_eq!(
            derive(R::Archived, Some(ExecutionStatus::Failed)),
            T::Archived
        );
    }
}
