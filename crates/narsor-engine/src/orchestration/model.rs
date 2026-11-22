//! Records and messages of the control plane's task orchestration.
//!
//! Everything here is plain data: the transition rules live in
//! [`super::lifecycle`] and the durable mutation protocol in
//! [`super::control_plane`].

use crate::automations::HookPhase;
use crate::domain::human_input::HumanInputDelivery;
pub use crate::domain::runner_snapshot::{
    PendingInputRef, RunnerSnapshot, RETIRED_CONDUCTOR_HARNESS,
};
use crate::domain::{
    BackendId, EnvironmentDiff, ExecutionStatus, TaskId, TaskLaunchSpec, WorkspaceHeader,
};
use chrono::{DateTime, Utc};
use narsor_protocol::api::{ConversationState, Timeline};
use narsor_protocol::controller::{MoveTaskTarget, STORAGE_DELETED_AT_ARCHIVE};
use serde::{Deserialize, Serialize};

pub use narsor_protocol::controller::{
    ArchiveTombstone, AttentionItem, AttentionKind, CommandActor, CommandReceipt,
    ConversationAttemptSummary, ConversationAvailability, ExecutionCursor, GateAutomationPhase,
    GateWatch, HarnessUnavailable, InputStatus, InputUpdate, MaterialChange, Observation,
    ObservedSummary, OperatorCommand, OperatorCommandStatus, OwnerCommand, OwnerRef, QueueReason,
    ResourceIdentity, ResumeExhaustion, ResumeMessage, ResumeState, RunnerProblem, RunnerState,
    SlotReleaseReason, TaskEvent, TaskEventKind, TaskProvenance, TaskState, WorkloadLoss,
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

/// An owner's reading of a stopped task, as the control plane remembers it
/// (D63).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderLease {
    /// When the lease began: a reader credential issued before this belongs
    /// to an earlier reading and is not the one an owner now waits for.
    pub since: DateTime<Utc>,
    /// The lease holds until then, unless a read moves it later.
    pub until: DateTime<Utc>,
}

/// What the execution status of a task's last snapshot means for its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotReleaseClass {
    /// The execution ended (`Completed`, `Cancelled`): the slot is freed at
    /// once. Not [`ExecutionStatus::is_terminal`], which also counts
    /// `Failed`.
    Final,
    /// The execution waits for its owner (`InputNeeded`, `Failed`) or for an
    /// external event (`WaitingForEvent`, a `wait` step): the slot is freed
    /// after a wait. A matching event resumes the task like an owner's
    /// answer does.
    Transient,
}

impl SlotReleaseClass {
    /// The class of an execution status; `None` for any other status: the
    /// slot is kept.
    pub fn of(status: ExecutionStatus) -> Option<Self> {
        match status {
            ExecutionStatus::Completed | ExecutionStatus::Cancelled => Some(Self::Final),
            ExecutionStatus::InputNeeded
            | ExecutionStatus::Failed
            | ExecutionStatus::WaitingForEvent => Some(Self::Transient),
            _ => None,
        }
    }

    /// Why a slot was freed in this class.
    pub fn release_reason(self) -> SlotReleaseReason {
        match self {
            Self::Final => SlotReleaseReason::Terminal,
            Self::Transient => SlotReleaseReason::GraceElapsed,
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
/// 8. execution `WaitingForEvent` (a `wait` step) is `Running`: the work
///    goes on by itself when the event comes, so the owner
///    is not told that it waits for them;
/// 9. `Stopped` or `PauseRequested` is `Paused`;
/// 10. `Provisioning` without a snapshot is `Queued`;
/// 11. `Interrupted` with no attempt planned (lost while a pause was
///     requested, which the resume policy leaves to the owner) is `Paused`;
/// 12. the rest is `Running`: `Running`, `Provisioning` with a snapshot, and
///     `Interrupted` while the resume policy still waits to try again.
///
/// Rules 5 to 8 come before 9, so a task stopped to free its slot keeps the
/// state its work reached. Rule 9 reads `PauseRequested` as the owner's
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
        Some(ExecutionStatus::WaitingForEvent) => TaskState::Running,
        _ => match runner_state {
            RunnerState::PauseRequested if pause_by_policy => TaskState::Running,
            RunnerState::Stopped | RunnerState::PauseRequested => TaskState::Paused,
            RunnerState::Provisioning if execution.is_none() => TaskState::Queued,
            // Lost while pausing: no attempt is planned, nothing runs.
            RunnerState::Interrupted if resume.next_attempt_at.is_none() => TaskState::Paused,
            _ => TaskState::Running,
        },
    }
}

/// Why the owner cannot refresh the environment of a task now; `None` when
/// they can (R4, R5). The one rule the control plane's mutation and the
/// action offer both ask, so what a client is offered and what the control
/// plane accepts cannot disagree.
///
/// Only a task without a live workload is refreshed: `Stopped`,
/// `Interrupted` (its workload is gone, whether or not the resume policy
/// still tries) and `Queued` without a reservation. A task whose work ended
/// (`Finished`), or that is archived or being archived, is refused whatever
/// its runner state. `holds_slot` is [`TaskRecord::holds_capacity`]: for a
/// `Queued` task, whether the scheduler admitted it. The reasons are for the
/// owner, in Polish like the rest of the offer.
pub fn environment_refresh_refusal(
    runner_state: RunnerState,
    task_state: TaskState,
    holds_slot: bool,
) -> Option<String> {
    if runner_state.is_archival() {
        return Some("zadanie jest archiwizowane".to_string());
    }
    if task_state == TaskState::Finished {
        return Some("zadanie jest zakończone".to_string());
    }
    let reason = match runner_state {
        RunnerState::Stopped | RunnerState::Interrupted => return None,
        RunnerState::Queued if !holds_slot => return None,
        RunnerState::Queued => {
            "zadanie zostało dopuszczone i zaraz ruszy — gdy ruszy, wstrzymaj je, a potem \
             odśwież środowisko"
        }
        RunnerState::Provisioning | RunnerState::Running => {
            "zadanie działa — wstrzymaj je, a potem odśwież środowisko"
        }
        RunnerState::PauseRequested => {
            "zadanie jest wstrzymywane — poczekaj, aż się zatrzyma, a potem odśwież środowisko"
        }
        RunnerState::Unknown => {
            "nie wiadomo, czy zadanie jeszcze działa — poczekaj, aż kontroler to ustali, \
             a potem odśwież środowisko"
        }
        RunnerState::Archiving | RunnerState::Archived => "zadanie jest archiwizowane",
    };
    Some(reason.to_string())
}

/// What [`super::ControlPlane::refresh_environment`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentRefreshOutcome {
    /// The task's launch spec carries the new environment under a new
    /// generation; this is what changed.
    Refreshed(Box<EnvironmentDiff>),
    /// The new environment has the identity of the stored one: nothing was
    /// written and nothing announced (R8).
    Unchanged,
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
    /// When the task entered its current owner-visible state, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<DateTime<Utc>>,
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
    /// `Queued`, from where its runner state itself holds the slot.
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
    /// Until when a task stopped by its owner's `Pause` or `Stop` keeps its
    /// slot. Set from `slot_release_due_at` when the owner stops a task that
    /// waits for the owner and the deadline did not pass; the same deadline
    /// ends the hold, and so do `ReleaseSlot`, a resume and an archive. While
    /// it is set, a `Stopped` task holds capacity (see
    /// [`TaskRecord::holds_capacity`]).
    #[serde(default)]
    pub slot_hold_until: Option<DateTime<Utc>>,
    /// The control plane accepted the safe point of this task's pause for a
    /// lowered limit: the task goes back to the queue once its compute is
    /// gone. Set under the backend's schedule lock, so the accepted stops
    /// never exceed the overshoot.
    #[serde(default)]
    pub limit_stop_accepted: bool,
    /// The control plane's want of a read-only runner for the stopped task
    /// (D63): set by a read of its details, moved later by the next one,
    /// removed when it ran out and by any command that resumes or archives
    /// the task, in that command's own mutation (R15). The controller starts
    /// and removes the reader by it; the reader is no slot.
    #[serde(default)]
    pub reader_lease: Option<ReaderLease>,
    /// The owner's comment on the pause, for the runner of the next launch.
    /// The runner takes a key once, so the record needs no mark of its own.
    #[serde(default)]
    pub resume_message: Option<ResumeMessage>,
    /// The keys of the comments a launch took off the record, so a late
    /// retry of any of them still reads as a replay.
    #[serde(default)]
    pub resume_message_taken: Vec<String>,
    /// The wait episode of the `decision` gate with a `watch` block the
    /// task waits at, as its runner reported it; `None` while it waits at
    /// no such gate. The control plane keeps one queue entry for it
    /// (KTD-4): a snapshot that changes it ends the old episode's entry and
    /// begins the new one's ([`GateChange`]). Only a runner with an
    /// execution changes it, so a runner that is still starting (after a
    /// pause, a loss or a restart) does not end the episode (R17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watched_gate: Option<WatchedGate>,
    /// Watched gate episodes the task left while their exit scripts (`out`)
    /// had not all ended: the runner left the gate by an exit the control
    /// plane did not hold (R10, defensive). Each one stays here, phase
    /// `exiting`, until every exit script ended; its queue entry stays with
    /// it, as the index the hook claim walks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exiting_gates: Vec<WatchedGate>,
    /// The observer and run that created the task (R19);
    /// `None` for a task its owner created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<TaskProvenance>,
    /// The run as the runner last reported it when the owner archived the
    /// task, trimmed to its step headers ([`archived_timeline`]). Set once,
    /// in the mutation that sets [`Self::archive`], so a runner report
    /// accepted while the task is `Archiving` cannot change it; `None` for
    /// a task archived without a runner snapshot and for records archived
    /// before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_run: Option<ArchivedRun>,
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
    /// its slot; `None` before the first snapshot, and for a status that
    /// keeps the slot.
    pub fn slot_release_class(&self) -> Option<SlotReleaseClass> {
        self.runner
            .as_ref()
            .and_then(|runner| SlotReleaseClass::of(runner.snapshot.execution_status))
    }

    /// The deadline of a running task whose execution waits in a transient
    /// state (the only one a read of its details moves later); `None` when
    /// it is not waiting so, or has no deadline yet.
    pub fn slot_release_waiting(&self) -> Option<DateTime<Utc>> {
        if self.runner_state == RunnerState::Running
            && self.slot_release_class() == Some(SlotReleaseClass::Transient)
        {
            self.slot_release_due_at
        } else {
            None
        }
    }

    /// Whether a read-only runner is wanted for the task now: its lease
    /// has not run out and the task has no workload of its own (`Stopped`,
    /// `Interrupted`). The same test decides what the controller is told and
    /// whether the reader's credential is issued and its directive
    /// `Continue`, so they never disagree.
    pub fn reader_lease_active(&self, now: DateTime<Utc>) -> bool {
        self.reader_may_read()
            && self
                .reader_lease
                .as_ref()
                .is_some_and(|lease| lease.until > now)
    }

    /// Whether the task is in a state a read-only runner may read it in:
    /// stopped, or interrupted with its workload known to be gone. `Unknown`
    /// is not: its previous runner may still be alive.
    pub fn reader_may_read(&self) -> bool {
        matches!(
            self.runner_state,
            RunnerState::Stopped | RunnerState::Interrupted
        )
    }

    /// Whether a lease is recorded that no longer wants a reader (it ran
    /// out, or the task left the states it is read in): the periodic pass
    /// removes it.
    pub fn reader_lease_stale(&self, now: DateTime<Utc>) -> bool {
        self.reader_lease.is_some() && !self.reader_lease_active(now)
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

    /// The idempotency key of the first piece of owner work that waits for
    /// the runner (an unsettled answer, else a queued operator command): the
    /// key the work's resume derives from, so a retry resumes once.
    pub fn pending_work_key(&self) -> Option<&str> {
        self.inputs
            .iter()
            .find(|input| !input.status.is_settled())
            .map(|input| input.idempotency_key.as_str())
            .or_else(|| {
                self.operator_commands
                    .iter()
                    .find(|command| command.status == OperatorCommandStatus::Queued)
                    .map(|command| command.idempotency_key.as_str())
            })
    }

    /// Whether the slot-release policy may stop the task now: it runs, its
    /// execution still is in a state that frees the slot, the deadline came
    /// and nothing of the owner's waits for the runner.
    pub fn slot_release_due(&self, now: DateTime<Utc>) -> bool {
        self.runner_state == RunnerState::Running
            && self.slot_release_due_at.is_some_and(|at| at <= now)
            && self.slot_release_class().is_some()
            && !self.owner_work_pending()
    }

    /// Whether the task waits for the owner so that its slot could be freed
    /// now: it runs, its execution is in a state that frees the slot, a
    /// deadline is set, and nothing of the owner's waits for the runner. The
    /// deadline need not have passed.
    pub fn slot_release_pending(&self) -> bool {
        self.runner_state == RunnerState::Running
            && self.slot_release_due_at.is_some()
            && self.slot_release_class().is_some()
            && !self.owner_work_pending()
    }

    /// Whether the task is `Stopped` and still holds its slot until
    /// `slot_hold_until`; the owner can end that wait with `ReleaseSlot`.
    pub fn holds_stopped_slot(&self) -> bool {
        self.runner_state == RunnerState::Stopped && self.slot_hold_until.is_some()
    }

    /// Whether the hold of a `Stopped` task is over at `now`.
    pub fn slot_hold_expired(&self, now: DateTime<Utc>) -> bool {
        self.holds_stopped_slot() && self.slot_hold_until.is_some_and(|until| until <= now)
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

    /// Whether the pause the task now waits to complete is the one a lowered
    /// limit of concurrent tasks asked for.
    pub fn pause_requested_by_limit(&self) -> bool {
        self.runner_state == RunnerState::PauseRequested
            && self.commands.iter().rev().any(|receipt| {
                receipt.command == OwnerCommand::Pause
                    && receipt.generation_after == self.generation
                    && receipt.actor == CommandActor::Policy
                    && receipt
                        .idempotency_key
                        .starts_with(super::resume::LIMIT_STOP_KEY_PREFIX)
            })
    }

    /// Whether a lowered limit may ask the task to stop now: it runs and
    /// nothing of the owner's waits for the runner. `Interrupted` and
    /// `Unknown` are not asked, until the control plane knows their state.
    pub fn limit_stop_candidate(&self) -> bool {
        self.runner_state == RunnerState::Running && !self.owner_work_pending()
    }

    /// Whether the task holds a slot with compute that exists or may exist,
    /// as opposed to a reservation that nothing started on yet.
    pub fn holds_compute_slot(&self) -> bool {
        self.holds_capacity()
            && !matches!(
                self.runner_state,
                RunnerState::Queued | RunnerState::Stopped
            )
    }

    /// The receipt of the start or resume that produced `launch_generation`
    /// (see [`Self::launch_command`]).
    pub fn launch_receipt(&self, launch_generation: u64) -> Option<&CommandReceipt> {
        self.commands.iter().find(|receipt| {
            matches!(receipt.command, OwnerCommand::Start | OwnerCommand::Resume)
                && receipt.generation_after == launch_generation
        })
    }

    /// Whether the task occupies one slot in the global task budget.
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
    /// `Stopped` has no compute: the safe point was acknowledged and the
    /// compute removed (R12). It keeps the slot only while
    /// [`Self::slot_hold_until`] is set, that is after the owner's `Pause` or
    /// `Stop` of a task that waited for the owner, until that wait ends
    /// (its deadline, `ReleaseSlot`, a resume or an archive). A resume of
    /// such a task needs no new slot. `Archived` and an unadmitted `Queued`
    /// task hold nothing.
    pub fn holds_capacity(&self) -> bool {
        match self.runner_state {
            RunnerState::Queued => self.reservation.is_some(),
            RunnerState::Provisioning
            | RunnerState::Running
            | RunnerState::PauseRequested
            | RunnerState::Interrupted
            | RunnerState::Unknown
            | RunnerState::Archiving => true,
            RunnerState::Stopped => self.slot_hold_until.is_some(),
            RunnerState::Archived => false,
        }
    }

    /// What the owner sees of the task; see [`derive_task_state`].
    pub fn task_state(&self) -> TaskState {
        derive_task_state(
            self.runner_state,
            // While a resumed launch has not reported, the snapshot is the
            // superseded launch's: it says nothing of the task now.
            if self.runner_state == RunnerState::Provisioning
                && self.awaited_launch_generation().is_some()
            {
                None
            } else {
                self.runner
                    .as_ref()
                    .map(|runner| runner.snapshot.execution_status)
            },
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

    /// Why the owner cannot refresh the task's environment now; see
    /// [`environment_refresh_refusal`].
    pub fn environment_refresh_refusal(&self) -> Option<String> {
        environment_refresh_refusal(self.runner_state, self.task_state(), self.holds_capacity())
    }

    /// Whether the task can take no more answers: finished, archived (or
    /// being archived).
    pub fn has_ended(&self) -> bool {
        self.archive.is_some()
            || self.runner_state.is_archival()
            || self.task_state() == TaskState::Finished
    }

    /// The gate of `episode` whose exit scripts (`out`) run now (R10,
    /// R18): an episode the task left with exit scripts to run, its watched
    /// gate in phase `exiting`, or the watched gate of a task that ended
    /// there with exit scripts not yet run (one attempt, R18).
    pub fn exiting_gate(&self, episode: &str) -> Option<&WatchedGate> {
        if let Some(gate) = self
            .exiting_gates
            .iter()
            .find(|gate| gate.episode == episode)
        {
            return Some(gate);
        }
        let gate = self
            .watched_gate
            .as_ref()
            .filter(|gate| gate.episode == episode)?;
        let automation = gate.automation.as_ref()?;
        let exiting = automation.phase == GateAutomationPhase::Exiting
            || (self.has_ended() && automation.exit_due());
        exiting.then_some(gate)
    }

    /// The item (an answer or an operator command) the watched gate holds
    /// back from the runner while its exit scripts run; `None` when none is
    /// held.
    pub fn held_exit_item(&self) -> Option<&HeldExitItem> {
        self.watched_gate
            .as_ref()?
            .automation
            .as_ref()
            .filter(|automation| automation.phase == GateAutomationPhase::Exiting)?
            .exit
            .as_ref()?
            .held
            .as_ref()
    }

    /// The id of the answer the watched gate holds back from the runner,
    /// if it holds one ([`Self::held_exit_item`]).
    pub fn held_input_id(&self) -> Option<&str> {
        match self.held_exit_item()? {
            HeldExitItem::Input { input_id } => Some(input_id),
            HeldExitItem::Command { .. } => None,
        }
    }

    /// The id of the answer the watched gate's exit holds, while the gate
    /// is `exiting` or `released`: once released it is still queued until
    /// the runner's next poll, so it is not obsolete for a report or a
    /// poll without a request meanwhile. A runner that left the gate
    /// changed the watched gate first, so its answer is not kept.
    pub fn exit_input_id(&self) -> Option<&str> {
        let automation = self.watched_gate.as_ref()?.automation.as_ref()?;
        if !matches!(
            automation.phase,
            GateAutomationPhase::Exiting | GateAutomationPhase::Released
        ) {
            return None;
        }
        match automation.exit.as_ref()?.held.as_ref()? {
            HeldExitItem::Input { input_id } => Some(input_id),
            HeldExitItem::Command { .. } => None,
        }
    }

    /// The id of the operator command the watched gate holds back from the
    /// runner, if it holds one ([`Self::held_exit_item`]).
    pub fn held_command_id(&self) -> Option<&str> {
        match self.held_exit_item()? {
            HeldExitItem::Command { command_id } => Some(command_id),
            HeldExitItem::Input { .. } => None,
        }
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

    /// The launch generation of a launch that was ordered and whose runner
    /// has not reported yet: the task was resumed (by its owner or by work
    /// sent to it) and the runner of the older launch is superseded.
    /// `None` when no such launch waits.
    pub fn awaited_launch_generation(&self) -> Option<u64> {
        let previous = self.runner.as_ref()?;
        if !matches!(
            self.runner_state,
            RunnerState::Queued | RunnerState::Provisioning | RunnerState::Running
        ) || !self.supersedes_launch(previous.launch_generation)
        {
            return None;
        }
        self.commands
            .iter()
            .rev()
            .find(|receipt| matches!(receipt.command, OwnerCommand::Start | OwnerCommand::Resume))
            .map(|receipt| receipt.generation_after)
    }

    /// The launch generation an operator command accepted now is meant for:
    /// the running runner's, or that of the launch it waits for — for a
    /// `Stopped` task the one its resume will order (every resume raises
    /// the desired generation by one). `None` when nothing can take it.
    pub fn operator_command_launch(&self) -> Option<u64> {
        match self.runner_state {
            RunnerState::Stopped => Some(self.generation + 1),
            _ => self.awaited_launch_generation().or_else(|| {
                (self.runner_state == RunnerState::Running)
                    .then(|| self.runner.as_ref().map(|runner| runner.launch_generation))
                    .flatten()
            }),
        }
    }

    /// Why `command` cannot be queued for this task right now, or `None`
    /// when it can.
    ///
    /// A running task with a runner of its current launch carries out
    /// anything. A task without a runner — paused, or resumed and waiting
    /// for its new runner or a slot — takes the work an owner can send it
    /// (a message to the conductor, an undo, a restart, a transition): it
    /// is kept and carried out after the next start (R13, R14). What acts
    /// on a live runner (interrupt, confirm or reject a proposal) is
    /// refused, with the reason.
    pub fn operator_command_refusal(&self, command: &OperatorCommand) -> Option<String> {
        let stop_while_pausing = matches!(command, OperatorCommand::StopStep)
            && self.runner_state == RunnerState::PauseRequested;
        let reason = match self.runner_state {
            RunnerState::Running | RunnerState::Stopped => None,
            RunnerState::PauseRequested if stop_while_pausing => None,
            RunnerState::Queued | RunnerState::Provisioning => self
                .awaited_launch_generation()
                .is_none()
                .then_some("the task has not started yet"),
            RunnerState::PauseRequested => Some("the task is pausing"),
            RunnerState::Interrupted | RunnerState::Unknown => {
                Some("the task's runner is gone; start or resume the task first")
            }
            RunnerState::Archiving | RunnerState::Archived => Some("the task is archived"),
        };
        if let Some(reason) = reason {
            return Some(format!("{reason} (runner state: {:?})", self.runner_state));
        }
        if self.runner.is_none() {
            return Some("the task's runner has not reported yet".to_string());
        }
        if (self.runner_state != RunnerState::Running && !stop_while_pausing)
            || self.awaited_launch_generation().is_some()
        {
            // No live runner to act on: only work that waits for the next
            // start can be taken.
            if matches!(
                command,
                OperatorCommand::Interrupt
                    | OperatorCommand::StopStep
                    | OperatorCommand::ConfirmProposal { .. }
                    | OperatorCommand::RejectProposal { .. }
            ) {
                return Some(format!(
                    "the task has no running runner; this command acts on a live runner \
                     (runner state: {:?})",
                    self.runner_state
                ));
            }
        }
        None
    }
}

/// A capacity slot granted to a queued task (U10).
///
/// Durable on the task record itself, so a reservation and the task it
/// belongs to are committed together and survive a restart: what counts
/// against the global task limit is always derivable from the records alone.
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

/// Scheduler inputs, read fresh for each admission decision.
///
/// Backend availability determines whether a task can start on that backend;
/// `server.max_concurrent_tasks` is one shared budget across all backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulingPolicy {
    /// Whether this task's backend can currently accept work.
    pub backend: BackendAvailability,
    /// Maximum tasks holding capacity across all backends; `None` is unlimited.
    pub max_concurrent_tasks: Option<u32>,
}

/// Whether a backend can accept new work, independently of the global limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAvailability {
    /// The backend is configured and usable.
    Available,
    /// The backend cannot be used; why.
    Unavailable(String),
}

/// The durable owner choice of the default backend (R10).
///
/// Only an *override* of the configured `server.defaults.backend`; absent or
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

/// What an archived task keeps of its run: the step headers of the last
/// reported timeline and the workspace headers, frozen at archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedRun {
    /// The last reported timeline, trimmed ([`archived_timeline`]).
    #[serde(default)]
    pub timeline: Option<Timeline>,
    /// The last reported workspace headers, as reported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceHeader>,
}

impl ArchivedRun {
    /// The frozen copy of what `snapshot` reported.
    pub fn of(snapshot: &RunnerSnapshot) -> Self {
        Self {
            timeline: snapshot.timeline.as_ref().map(archived_timeline),
            workspaces: snapshot.workspaces.clone(),
        }
    }
}

/// `timeline` reduced to what outlives the task storage: one header row per
/// step with its labels, status, times, error, harness, model, effort,
/// usage, cost and decision fields as reported. The attempt ids, session
/// ids and event links point into the storage that archive deletes, so
/// they are dropped, and every step reads as having no conversation — a
/// client starts no read of it.
pub fn archived_timeline(timeline: &Timeline) -> Timeline {
    let mut trimmed = timeline.clone();
    trimmed.links.clear();
    for position in &mut trimmed.positions {
        position.attempts.clear();
        // `Position::attempt_ids` falls back to it.
        position.action_execution_id = None;
        position.resumed_harness_session = None;
        position.harness_session_id = None;
        position.conversation = ConversationState::NotCreated {
            reason: STORAGE_DELETED_AT_ARCHIVE.to_string(),
        };
    }
    trimmed
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
    /// The observer and its run, when an observer's script gave the answer
    /// (`task decide`, R15); `None` for the owner's. The runner records it
    /// in the answer's event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer: Option<TaskProvenance>,
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
    /// The watched gate episodes this snapshot ended and began; empty when
    /// the task still waits in the episode it was known to wait in.
    pub gate_change: GateChange,
}

/// A wait episode of a `decision` gate with a `watch` block: the episode
/// (the id of the gate's pending input request) and the gate's rendered
/// `watch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedGate {
    /// The episode: the id of the pending input request.
    pub episode: String,
    /// The gate's `watch`, as the runner rendered it.
    pub watch: GateWatch,
    /// When the control plane first saw the episode. An entry added back
    /// later keeps it as its start, so a script still sees what changed
    /// since the episode began. `None` in a record from before it was
    /// kept: the time of the restore is used then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub began_at: Option<DateTime<Utc>>,
    /// Where the automations of the gate's queue stand in this episode:
    /// its phase and its entry (and, later, exit) scripts. Set by the
    /// snapshot that begins the episode, from the catalogue as it is then;
    /// a later snapshot of the same episode (pause, `Interrupted`,
    /// auto-resume, runner restart, a runner problem) keeps it (R19).
    /// `None` in a record from before entry scripts, or written without the
    /// catalogue: the episode counts as [`GateAutomationPhase::Ready`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<GateAutomation>,
}

impl WatchedGate {
    /// Whether `check` runs may check the episode (R9): every entry script
    /// succeeded, or there is none.
    pub fn automation_ready(&self) -> bool {
        self.automation
            .as_ref()
            .is_none_or(|automation| automation.phase == GateAutomationPhase::Ready)
    }
}

/// Most attempts of one entry or exit script in one episode (R12, R15).
pub const GATE_HOOK_MAX_ATTEMPTS: u32 = 3;

/// The wait before the second and the third attempt of a failed entry or
/// exit script, in seconds.
pub const GATE_HOOK_RETRY_DELAYS_SECS: [u64; 2] = [10, 30];

/// How long one attempt of an entry or exit script may run, in seconds.
pub const GATE_HOOK_TIMEOUT_SECS: u64 = 120;

/// The automations of a watched gate's queue in one episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateAutomation {
    /// Where the episode stands.
    pub phase: GateAutomationPhase,
    /// The entry (and exit) scripts of the episode, one per automation and
    /// script phase, in the queue file's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<GateHook>,
    /// How the episode is left, once an exit came while the queue has exit
    /// scripts (R10, R11): the choice, its author and the item held back
    /// from the runner until every exit script ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<GateExit>,
    /// Counts the exits of the episode that did not happen and gave way:
    /// raised when a held exit is put back ([`Self::reopen_exit`]) or
    /// replaced by a new one. An `out` run carries the value of its claim,
    /// so the end of a run for an earlier exit does not count for the
    /// current one. `0` in a record from before it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exit_generation: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The automations of a queue whose entry (`in`) and exit (`out`) scripts
/// run for an episode, as the catalogue says when the episode begins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateScripts {
    /// The automations with an `in` script, in the queue file's order.
    pub entry: Vec<String>,
    /// The automations with an `out` script, in the queue file's order.
    pub exit: Vec<String>,
}

impl GateAutomation {
    /// A new episode of a queue with `scripts`: `entering` with one pending
    /// `in` each, or `ready` at once when there is none (R17). The `out`
    /// scripts are recorded now, pending, so a catalogue change during the
    /// episode does not change which automations run `out`.
    pub fn entering(scripts: GateScripts) -> Self {
        let entering = !scripts.entry.is_empty();
        let hooks: Vec<GateHook> = scripts
            .entry
            .into_iter()
            .map(|automation| GateHook::pending(automation, HookPhase::In))
            .chain(
                scripts
                    .exit
                    .into_iter()
                    .map(|automation| GateHook::pending(automation, HookPhase::Out)),
            )
            .collect();
        Self {
            phase: if entering {
                GateAutomationPhase::Entering
            } else {
                GateAutomationPhase::Ready
            },
            hooks,
            exit: None,
            exit_generation: 0,
        }
    }

    /// Whether an exit script of the episode has not ended yet (none ran,
    /// or one waits for its next attempt).
    pub fn exit_pending(&self) -> bool {
        self.hooks
            .iter()
            .any(|hook| hook.hook == HookPhase::Out && hook.status == GateHookStatus::Pending)
    }

    /// The episode is left by `exit`: phase `exiting` while an exit script
    /// is pending, `released` when none is. An entry script not run yet
    /// never runs (`skipped`); one in progress ends, but its result no
    /// longer changes the phase (R14).
    pub fn begin_exit(&mut self, exit: GateExit) {
        for hook in self
            .hooks
            .iter_mut()
            .filter(|hook| hook.hook == HookPhase::In && hook.status == GateHookStatus::Pending)
        {
            hook.status = GateHookStatus::Skipped;
        }
        self.phase = if self.exit_pending() {
            GateAutomationPhase::Exiting
        } else {
            GateAutomationPhase::Released
        };
        self.exit = Some(exit);
    }

    /// Replaces the exit of an episode still left (`exiting` or
    /// `released`) whose item gave way: `exit` is the new one, and a run
    /// of the old exit no longer counts.
    pub fn replace_exit(&mut self, exit: GateExit) {
        self.exit = Some(exit);
        self.exit_generation += 1;
    }

    /// Puts the episode back to where it stood before its exit, which did
    /// not happen: `ready` (`entry_failed` when an entry script failed for
    /// good), no exit, and every exit script pending again with no
    /// attempts. A run of the old exit no longer counts.
    pub fn reopen_exit(&mut self) {
        self.exit = None;
        self.exit_generation += 1;
        for hook in self
            .hooks
            .iter_mut()
            .filter(|hook| hook.hook == HookPhase::Out)
        {
            hook.status = GateHookStatus::Pending;
            hook.log.clear();
            hook.waiting_for = None;
        }
        self.phase = self.entry_outcome().unwrap_or(GateAutomationPhase::Ready);
    }

    /// Whether the episode is not left yet and an exit script of it has
    /// not ended: an exit not held by the control plane starts them.
    pub fn exit_due(&self) -> bool {
        self.phase.before_exit() && self.exit_pending()
    }

    /// Leaves the episode by an exit no item waits for (the task ended at
    /// the gate, or the runner left it by itself), when [`Self::exit_due`]:
    /// returns whether the exit began.
    pub fn begin_unheld_exit(&mut self, author: GateExitAuthor, at: DateTime<Utc>) -> bool {
        if !self.exit_due() {
            return false;
        }
        self.begin_exit(GateExit::unheld(author, at));
        true
    }

    /// The phase the exit scripts give: `released` once none is pending
    /// (each succeeded, failed its last attempt or was skipped, R15);
    /// `None` while one is.
    pub fn exit_outcome(&self) -> Option<GateAutomationPhase> {
        (!self.exit_pending()).then_some(GateAutomationPhase::Released)
    }

    /// The phase the scripts of `hook` give once none of them is pending
    /// ([`Self::exit_outcome`] for `out`, [`Self::entry_outcome`]
    /// otherwise); `None` while one is.
    pub fn outcome(&self, hook: HookPhase) -> Option<GateAutomationPhase> {
        match hook {
            HookPhase::Out => self.exit_outcome(),
            HookPhase::In | HookPhase::Check => self.entry_outcome(),
        }
    }

    /// Most attempts of one script of `hook`: one for an exit script of a
    /// task that ended at the gate (R18), [`GATE_HOOK_MAX_ATTEMPTS`]
    /// otherwise.
    pub fn max_attempts(&self, hook: HookPhase) -> u32 {
        let task_end = self
            .exit
            .as_ref()
            .is_some_and(|exit| exit.author == GateExitAuthor::TaskEnd);
        if hook == HookPhase::Out && task_end {
            1
        } else {
            GATE_HOOK_MAX_ATTEMPTS
        }
    }

    /// The phase the entry scripts give once none is pending: `ready` when
    /// every one succeeded or was skipped, `entry_failed` when one failed
    /// its last attempt; `None` while one is still pending.
    pub fn entry_outcome(&self) -> Option<GateAutomationPhase> {
        let mut entry = self.hooks.iter().filter(|hook| hook.hook == HookPhase::In);
        if entry
            .clone()
            .any(|hook| hook.status == GateHookStatus::Pending)
        {
            return None;
        }
        Some(if entry.any(|hook| hook.status == GateHookStatus::Failed) {
            GateAutomationPhase::EntryFailed
        } else {
            GateAutomationPhase::Ready
        })
    }
}

/// How a watched gate episode is left while its queue has exit scripts
/// (R10, R11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateExit {
    /// The choice it is left by; `None` when the task ended at the gate or
    /// the runner left it by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    /// Who chose it.
    pub author: GateExitAuthor,
    /// The automation run that answered, for an `automation` author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The answer or operator command held back from the runner while the
    /// exit scripts run; `None` when nothing waits for them (the task
    /// ended at the gate, or the runner already left it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<HeldExitItem>,
    /// When the exit came.
    pub at: DateTime<Utc>,
}

impl GateExit {
    /// An exit by `author` at `at` that holds nothing back and has no
    /// choice: the task ended at the gate, or the runner left it.
    pub fn unheld(author: GateExitAuthor, at: DateTime<Utc>) -> Self {
        Self {
            choice: None,
            author,
            run_id: None,
            held: None,
            at,
        }
    }
}

/// Who left a watched gate episode (R11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateExitAuthor {
    /// The owner answered the gate.
    Person,
    /// A `check` run answered it (`task decide`).
    Automation,
    /// The owner sent an operator command (a transition, a step).
    Operator,
    /// The task ended at the gate (archive, R18).
    TaskEnd,
    /// The runner left the gate with no exit the control plane held.
    Runner,
    /// An author from a newer build.
    #[serde(other)]
    Unrecognized,
}

impl GateExitAuthor {
    /// The wire name, as an exit script reads it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Automation => "automation",
            Self::Operator => "operator",
            Self::TaskEnd => "task_end",
            Self::Runner => "runner",
            Self::Unrecognized => "unrecognized",
        }
    }
}

/// The item a watched gate holds back from the runner while its exit
/// scripts run. The item stays where it is ([`TaskRecord::inputs`],
/// [`TaskRecord::operator_commands`]); delivery skips it until the gate is
/// `released`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HeldExitItem {
    /// A queued answer.
    Input {
        /// Its id.
        input_id: String,
    },
    /// A queued operator command.
    Command {
        /// Its id.
        command_id: String,
    },
}

/// One entry or exit script of one automation in one episode.
///
/// The run in progress is not here: it is the (queue, automation) pair's
/// hook run (`AutomationRecord::active_hook`). The task changes when an
/// attempt ends, so a claim writes no task event and a lost claim leaves
/// nothing behind on the task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateHook {
    /// The automation.
    pub automation: String,
    /// `in` or `out`.
    pub hook: HookPhase,
    /// Where it stands.
    pub status: GateHookStatus,
    /// Every attempt that ended, oldest first: what the task view lists
    /// under the gate's step (R20). At most the script's attempts, plus a
    /// skip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<GateHookAttemptLog>,
    /// The catalogue file (relative to the catalogue root) a pending
    /// script waits for: it cannot be used now, so the script is neither
    /// run nor skipped. Set once per file with a task warning; cleared when
    /// an attempt of the script ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

/// One ended attempt of an entry or exit script, as the task view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateHookAttemptLog {
    /// The attempt, from 1.
    pub attempt: u32,
    /// When it ended.
    pub at: DateTime<Utc>,
    /// Why it failed; `None` when it succeeded or was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why no script ran; `None` when the script ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// When the next automatic attempt runs; `None` when none follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
}

impl GateHook {
    /// A script not tried yet.
    pub fn pending(automation: String, hook: HookPhase) -> Self {
        Self {
            automation,
            hook,
            status: GateHookStatus::Pending,
            log: Vec::new(),
            waiting_for: None,
        }
    }

    /// Attempts that ended (a lost run counts; a skip does not).
    pub fn attempts(&self) -> u32 {
        let attempts = self.log.iter().filter(|entry| entry.skipped.is_none());
        u32::try_from(attempts.count()).unwrap_or(u32::MAX)
    }

    /// Why the latest failed attempt failed.
    pub fn last_error(&self) -> Option<&str> {
        self.log
            .iter()
            .rev()
            .find_map(|entry| entry.error.as_deref())
    }

    /// The earliest time of the next attempt of a pending script, after a
    /// failed one; `None`: at once, or no attempt follows.
    pub fn next_attempt_at(&self) -> Option<DateTime<Utc>> {
        if self.status != GateHookStatus::Pending {
            return None;
        }
        self.log.last().and_then(|entry| entry.retry_at)
    }
}

/// Where one entry or exit script stands. A run in progress is still
/// `pending` here: the pair's record has the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateHookStatus {
    /// Not tried yet, or failed with attempts left.
    Pending,
    /// An attempt succeeded.
    Succeeded,
    /// The last attempt failed: no more automatic attempts.
    Failed,
    /// No script ran: the automation left the queue or lost the script.
    Skipped,
    /// A status from a newer build: never run again by this one.
    #[serde(other)]
    Unrecognized,
}

/// How one attempt of an entry or exit script ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateHookOutcome {
    /// The script exited 0.
    Succeeded,
    /// It failed, timed out, could not start, or was lost; the reason.
    Failed(String),
    /// No script ran (the automation left the queue or lost the script);
    /// the reason.
    Skipped(String),
}

/// How a runner snapshot moved the task between watched gate episodes
/// (R16): the episode it left and the one it entered. A snapshot that
/// leaves one episode and enters the next (a review loop) has both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateChange {
    /// The watched episode that ended: its gate exited.
    pub ended: Option<WatchedGate>,
    /// The watched episode that began.
    pub began: Option<WatchedGate>,
}

impl GateChange {
    /// Whether the snapshot left the task in the episode it was in.
    pub fn is_empty(&self) -> bool {
        self.ended.is_none() && self.began.is_none()
    }
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
    /// The observer that creates it, if one does (R19).
    pub provenance: Option<TaskProvenance>,
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
            provenance: None,
        }
    }

    /// Records the digest of the owner request.
    pub fn with_request_digest(mut self, digest: impl Into<String>) -> Self {
        self.request_digest = Some(digest.into());
        self
    }

    /// Records the observer that creates the task.
    pub fn with_provenance(mut self, provenance: Option<TaskProvenance>) -> Self {
        self.provenance = provenance;
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
    fn a_run_waiting_for_an_external_event_runs_and_never_needs_a_human() {
        // Running, stopped to free its slot, or stopping for it: the task
        // waits for the event by itself, not for its owner (U4).
        for runner in [R::Running, R::Stopped, R::PauseRequested] {
            let state = derive(runner, Some(ExecutionStatus::WaitingForEvent));
            assert_eq!(state, T::Running, "{runner:?}");
            assert_ne!(state, T::HumanNeeded);
        }
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
        let planned = ResumeState {
            next_attempt_at: Some(Utc::now()),
            ..ResumeState::default()
        };
        assert_eq!(
            derive_task_state(
                R::Interrupted,
                Some(ExecutionStatus::Running),
                &planned,
                false
            ),
            T::Running
        );
    }

    #[test]
    fn a_task_lost_while_pausing_has_no_attempt_planned_and_is_paused() {
        // `count_loss` plans nothing for a loss from `PauseRequested`.
        for execution in [Some(ExecutionStatus::Running), None] {
            assert_eq!(
                derive_task_state(R::Interrupted, execution, &ResumeState::default(), false),
                T::Paused
            );
        }
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
        assert_eq!(C::of(ExecutionStatus::Completed), Some(C::Final));
        assert_eq!(C::of(ExecutionStatus::Cancelled), Some(C::Final));
        assert_eq!(C::of(ExecutionStatus::InputNeeded), Some(C::Transient));
        assert_eq!(C::of(ExecutionStatus::Failed), Some(C::Transient));
        // A `wait` step frees its slot like a wait for the owner (U4).
        assert_eq!(C::of(ExecutionStatus::WaitingForEvent), Some(C::Transient));
        for other in [
            ExecutionStatus::Running,
            ExecutionStatus::Paused,
            ExecutionStatus::Unknown,
        ] {
            assert_eq!(C::of(other), None);
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

impl From<MoveTarget> for MoveTaskTarget {
    fn from(target: MoveTarget) -> Self {
        match target {
            MoveTarget::Before(task_id) => Self::Before(task_id.to_string()),
            MoveTarget::After(task_id) => Self::After(task_id.to_string()),
            MoveTarget::Top => Self::Top,
            MoveTarget::Bottom => Self::Bottom,
        }
    }
}
