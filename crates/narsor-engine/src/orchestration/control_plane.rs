//! The control plane's task orchestration API (KTD1, KTD3).
//!
//! Two kinds of caller mutate a task, with different authority:
//!
//! * the **owner** changes desired state ([`ControlPlane::command`]). No
//!   controller lease is involved — desired state belongs to the control
//!   plane alone — but every command is idempotent by key and may carry the
//!   generation it expects;
//! * a **controller** reports what it observed ([`ControlPlane::report`]),
//!   fenced by its backend's lease epoch, the task's current desired
//!   generation and the resource's own version, so stale, reordered and
//!   duplicated reports are refused or absorbed.
//!
//! Every mutation of a task runs under that task's lock, reads the record,
//! applies a pure transition ([`super::lifecycle`]) and commits through the
//! store's protocol (see [`super::store`]), so concurrent callers serialize
//! and none loses another's update.

use super::error::{OrchestrationError, Result};
use super::lifecycle::{command_effect, observation_effect, CommandEffect, ObservationEffect};
use super::model::{
    slot_release_extension_threshold, ArchiveTombstone, BackendLease, CommandActor, CommandOutcome,
    CommandReceipt, CommandRequest, ControllerReport, LeaseToken, MaterialChange, NewTask,
    Observation, ObservedState, OwnerCommand, ReaderLease, ReportOutcome, ResumeState, RunnerState,
    SlotReleaseClass, SlotReleaseReason, TaskAssignment, TaskEvent, TaskEventKind, TaskRecord,
    WorkloadLoss, BACKEND_LEASE_VERSION, TASK_RECORD_VERSION,
};
use super::resume::{
    auto_resume_key, count_loss, loss_from_history, slot_release_key, ResumePolicy,
};
use super::store::OrchestrationStore;
use crate::domain::{BackendId, TaskId};
use crate::persistence::{ExecutionStore, StatePaths};
use chrono::{DateTime, Duration, Utc};
use std::path::PathBuf;
use std::sync::Arc;

/// Source of "now" for timestamps and lease expiry; injectable for tests.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current time.
    fn now(&self) -> DateTime<Utc>;
}

/// The wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// What [`ControlPlane::recover`] did at startup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrchestrationRecovery {
    /// Orphaned temporary files of interrupted writes that were removed.
    pub removed_temp_files: Vec<PathBuf>,
    /// Events that had been committed in a record but not yet appended to
    /// the log, and were appended now: `(task, sequence)`.
    pub republished_events: Vec<(TaskId, u64)>,
    /// Leftovers of the material store (U9) that were removed: uploads a
    /// crash interrupted before they became materials, and materials of
    /// tasks archived before their purge completed.
    pub removed_material_leftovers: Vec<PathBuf>,
    /// Tasks interrupted (or unknown) before losses were remembered, whose
    /// loss was read from their event log so the resume policy can judge
    /// it like any other.
    pub adopted_losses: Vec<TaskId>,
}

/// Whether a mutation changed the record.
pub(crate) enum Mutation<T> {
    /// Nothing to write; the value is returned as is.
    Unchanged(T),
    /// Commit a new revision described by this event.
    Changed(TaskEventKind, T),
}

/// Durable, backend-neutral task orchestration on the state root.
#[derive(Debug, Clone)]
pub struct ControlPlane {
    store: OrchestrationStore,
    clock: Arc<dyn Clock>,
    resume_policy: ResumePolicy,
}

impl ControlPlane {
    /// A control plane on `paths`, using the wall clock.
    pub fn new(paths: StatePaths) -> Self {
        Self::with_clock(paths, Arc::new(SystemClock))
    }

    /// A control plane with an explicit clock.
    pub fn with_clock(paths: StatePaths, clock: Arc<dyn Clock>) -> Self {
        Self {
            store: OrchestrationStore::new(paths),
            clock,
            resume_policy: ResumePolicy::default(),
        }
    }

    /// Replaces how workload losses are counted for the automatic resume.
    pub fn with_resume_policy(mut self, policy: ResumePolicy) -> Self {
        self.resume_policy = policy;
        self
    }

    /// How workload losses are counted for the automatic resume.
    pub fn resume_policy(&self) -> &ResumePolicy {
        &self.resume_policy
    }

    /// The underlying file store.
    pub fn store(&self) -> &OrchestrationStore {
        &self.store
    }

    /// "Now" by this control plane's clock — the one lease expiry and
    /// observation staleness are judged by.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// Creates a queued task, idempotently.
    ///
    /// Repeating the same request returns the existing record unchanged; the
    /// same task id with a different launch spec, owner or backend
    /// is a [`OrchestrationError::TaskConflict`] — a retry must never
    /// silently re-point an existing task.
    pub fn create_task(&self, request: NewTask) -> Result<TaskRecord> {
        if !request.launch_spec.digest_is_valid() {
            return Err(OrchestrationError::InvalidLaunchSpec(request.task_id));
        }
        let _creation = self.store.lock_creation()?;
        if self.store.task_exists(&request.task_id) {
            let existing = self.store.load_task(&request.task_id)?;
            return if same_creation(&existing, &request) || same_request(&existing, &request) {
                Ok(existing)
            } else {
                Err(OrchestrationError::TaskConflict(request.task_id))
            };
        }

        // Records are never deleted (archive keeps a tombstone), so the
        // maximum over all of them only ever grows: no separate counter file
        // that could drift from the records it numbers.
        let _schedule = self.store.lock_schedule(&request.backend_id)?;
        let tasks = self.store.list_tasks()?;
        let queue_sequence = tasks
            .iter()
            .map(|task| task.queue_sequence)
            .max()
            .unwrap_or(0)
            + 1;
        // A new task joins the end of its backend's queue; the schedule
        // lock keeps a concurrent move from renumbering under it.
        let rank = tasks
            .iter()
            .filter(|task| task.assignment.backend_id == request.backend_id)
            .map(|task| task.rank)
            .max()
            .unwrap_or(0)
            + 1;
        let now = self.clock.now();
        let record = TaskRecord {
            record_version: TASK_RECORD_VERSION,
            owner: request.owner,
            assignment: TaskAssignment {
                backend_id: request.backend_id,
                resource_key: request.task_id.to_string(),
            },
            launch_spec: request.launch_spec,
            rank,
            legacy_priority: None,
            created_at: now,
            queue_sequence,
            runner_state: RunnerState::Queued,
            generation: 1,
            revision: 1,
            resources_may_exist: false,
            last_report_epoch: None,
            observed: None,
            archive: None,
            commands: Vec::new(),
            runner: None,
            inputs: Vec::new(),
            operator_commands: Vec::new(),
            operator_command_counter: 0,
            retired_command_keys: Vec::new(),
            reservation: None,
            queue: None,
            request_digest: request.request_digest,
            running_since: None,
            workload_epoch: None,
            resume: ResumeState::default(),
            slot_release_due_at: None,
            limit_stop_accepted: false,
            reader_lease: None,
            resume_message: None,
            resume_message_taken: Vec::new(),
            last_event: TaskEvent {
                sequence: 1,
                task_id: request.task_id.clone(),
                at: now,
                runner_state: RunnerState::Queued,
                generation: 1,
                kind: TaskEventKind::Created,
            },
            task_id: request.task_id,
        };

        let _task = self.store.lock_task(&record.task_id)?;
        if !self.store.create_task(&record)? {
            // Unreachable while every creator holds the creation lock, but a
            // record that appeared anyway must not be overwritten.
            return Err(OrchestrationError::TaskConflict(record.task_id));
        }
        self.store.publish_last_event(&record)?;
        Ok(record)
    }

    /// Loads one task.
    pub fn task(&self, task_id: &TaskId) -> Result<TaskRecord> {
        self.store.load_task(task_id)
    }

    /// Every task, in creation order.
    pub fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        self.store.list_tasks()
    }

    /// The task's control-plane events after the cursor `after` (0 = all).
    pub fn events_since(&self, task_id: &TaskId, after: u64) -> Result<Vec<TaskEvent>> {
        self.store.events_since(task_id, after)
    }

    /// Applies an owner command, idempotently by its key.
    ///
    /// A retry with the same key and command returns the original receipt and
    /// writes nothing; the same key for a different command is
    /// [`OrchestrationError::IdempotencyKeyReused`]. Archive wins over every
    /// state; on a task no controller ever accepted it completes at once,
    /// since resources that were never created count as already deleted.
    ///
    /// An owner command that takes the task out of `Interrupted`/`Unknown`
    /// is the owner's decision and resets its resume state (R6); one the
    /// resume policy issued ([`super::model::CommandActor::Policy`]) only
    /// consumes the planned attempt and keeps the count. A policy command is
    /// a `Resume` of a task whose attempt is due
    /// ([`TaskRecord::auto_resume_due`]) and nothing else; its receipt is
    /// announced as [`TaskEventKind::AutoResumed`] instead of
    /// `OwnerCommandAccepted`, in the same mutation.
    pub fn command(&self, task_id: &TaskId, request: CommandRequest) -> Result<CommandOutcome> {
        self.command_routed(task_id, request, false)
    }

    /// [`Self::command`] with the scheduler's decision on where a resume of
    /// a `Stopped` task goes: `queue_resume` sends it to the front of the
    /// queue (`Queued`, no reservation) instead of straight to
    /// `Provisioning`, because the backend has no free slot (R20). Decided
    /// by the caller under the backend's schedule lock.
    pub(crate) fn command_routed(
        &self,
        task_id: &TaskId,
        request: CommandRequest,
        queue_resume: bool,
    ) -> Result<CommandOutcome> {
        let (task, (receipt, replayed)) = self.mutate(task_id, |record, now| {
            if let Some(receipt) = record.receipt(&request.idempotency_key) {
                if receipt.command != request.command
                    || receipt.expected_generation != request.expected_generation
                {
                    return Err(OrchestrationError::IdempotencyKeyReused {
                        task_id: record.task_id.clone(),
                        key: request.idempotency_key.clone(),
                        original: receipt.command,
                        requested: request.command,
                    });
                }
                return Ok(Mutation::Unchanged((receipt.clone(), true)));
            }
            if let Some(expected) = request.expected_generation {
                record.check_generation(expected)?;
            }
            let policy_may_act = match request.command {
                OwnerCommand::Resume => record.auto_resume_due(now),
                OwnerCommand::Pause => {
                    record.slot_release_due(now)
                        || (request
                            .idempotency_key
                            .starts_with(super::resume::LIMIT_STOP_KEY_PREFIX)
                            && record.limit_stop_candidate())
                }
                _ => false,
            };
            if request.actor == CommandActor::Policy && !policy_may_act {
                return Err(OrchestrationError::IllegalCommand {
                    task_id: record.task_id.clone(),
                    command: request.command,
                    runner_state: record.runner_state,
                });
            }
            let event = match request.actor {
                CommandActor::Owner => TaskEventKind::OwnerCommandAccepted {
                    command: request.command,
                    idempotency_key: request.idempotency_key.clone(),
                },
                // A lowered limit asks the task to stop: planned now, decided
                // at the safe point (see `decide_limit_stop`).
                CommandActor::Policy
                    if request.command == OwnerCommand::Pause
                        && request
                            .idempotency_key
                            .starts_with(super::resume::LIMIT_STOP_KEY_PREFIX) =>
                {
                    TaskEventKind::SlotReleaseScheduled { due_at: now }
                }
                CommandActor::Policy if request.command == OwnerCommand::Pause => {
                    TaskEventKind::SlotReleased {
                        reason: record
                            .slot_release_class()
                            .map(SlotReleaseClass::release_reason)
                            .unwrap_or(SlotReleaseReason::GraceElapsed),
                    }
                }
                CommandActor::Policy => TaskEventKind::AutoResumed {
                    attempt: if record.resume.last_loss_uncounted {
                        0
                    } else {
                        record.resume.consecutive_losses.max(1)
                    },
                    limit: self.resume_policy.limit(),
                    loss_reason: record
                        .resume
                        .last_loss
                        .as_ref()
                        .and_then(|loss| loss.reason.clone()),
                },
            };

            match command_effect(record.runner_state, request.command) {
                CommandEffect::Illegal => {
                    return Err(OrchestrationError::IllegalCommand {
                        task_id: record.task_id.clone(),
                        command: request.command,
                        runner_state: record.runner_state,
                    })
                }
                // Recorded for the audit trail; the tombstone already holds.
                CommandEffect::ArchiveConverged => {}
                CommandEffect::Transition(next) => {
                    let previous = record.runner_state;
                    // No free slot: the resume waits in the queue, ahead of
                    // everyone, holding nothing (R20). The ranks of the
                    // others are shifted by the caller under the schedule
                    // lock; rank 0 is before every dense rank.
                    let next = if queue_resume
                        && previous == RunnerState::Stopped
                        && next == RunnerState::Provisioning
                    {
                        record.rank = 0;
                        record.reservation = None;
                        record.queue = None;
                        RunnerState::Queued
                    } else {
                        next
                    };
                    record.generation += 1;
                    record.runner_state = next;
                    // The accepted safe point belongs to the pause it was
                    // accepted for; any other way out of it ends it.
                    if next != RunnerState::PauseRequested {
                        record.limit_stop_accepted = false;
                    }
                    // A new desired state ends whatever the last snapshot
                    // made the policy wait for.
                    record.slot_release_due_at = None;
                    // Nothing else may read the storage the task's own
                    // runner is about to take (R15): the reader's lease
                    // ends in the very mutation that resumes or archives.
                    record.reader_lease = None;
                    if matches!(previous, RunnerState::Interrupted | RunnerState::Unknown) {
                        leave_loss(record, request.actor);
                    }
                    if matches!(next, RunnerState::Provisioning | RunnerState::Queued) {
                        self.order_launch(record)?;
                    }
                    if request.command == OwnerCommand::Archive {
                        archive(record, &request.idempotency_key, now);
                    }
                }
            }

            let receipt = CommandReceipt {
                idempotency_key: request.idempotency_key.clone(),
                command: request.command,
                expected_generation: request.expected_generation,
                accepted_at: now,
                generation_after: record.generation,
                runner_state_after: record.runner_state,
                revision: record.revision + 1,
                actor: request.actor,
            };
            record.commands.push(receipt.clone());
            Ok(Mutation::Changed(event, (receipt, false)))
        })?;
        Ok(CommandOutcome {
            receipt,
            task,
            replayed,
        })
    }

    /// The resume policy resumes a task interrupted at `generation` whose
    /// attempt is due: a `Resume` with [`CommandActor::Policy`], keyed
    /// [`auto_resume_key`]`(generation)` and expecting that generation.
    ///
    /// Idempotent like any command: a second pass before the controller
    /// acted replays the receipt. An owner command that came first moved
    /// the generation, so the policy's is refused with `StaleGeneration`.
    /// No capacity check: an interrupted task still holds its slot, and the
    /// resume keeps it.
    pub fn auto_resume(&self, task_id: &TaskId, generation: u64) -> Result<CommandOutcome> {
        self.command(
            task_id,
            CommandRequest::new(OwnerCommand::Resume, auto_resume_key(generation))
                .expecting_generation(generation)
                .issued_by(CommandActor::Policy),
        )
    }

    /// The slot-release policy frees the slot of the task running at
    /// `generation`: a `Pause` with [`CommandActor::Policy`], keyed
    /// [`slot_release_key`]`(generation)` and expecting that generation.
    ///
    /// Allowed only while [`TaskRecord::slot_release_due`]; the pause then
    /// goes the owner's way (`Running → PauseRequested`, a safe point, the
    /// compute removed, `Stopped`) and `holds_capacity` is unchanged until
    /// the safe point is acknowledged. A command of the owner that came
    /// first moved the generation or the state, so the policy's is refused
    /// with `StaleGeneration` or `IllegalCommand`. Needs no capacity.
    pub fn release_slot(&self, task_id: &TaskId, generation: u64) -> Result<CommandOutcome> {
        self.command(
            task_id,
            CommandRequest::new(OwnerCommand::Pause, slot_release_key(generation))
                .expecting_generation(generation)
                .issued_by(CommandActor::Policy),
        )
    }

    /// Plans the slot release of a running task whose last snapshot is in a
    /// state that frees the slot but whose deadline was never set (a task
    /// that was `Provisioning` when the snapshot came, or a record from
    /// before the policy existed). `grace` is the wait for a transient
    /// state. Announces the deadline; `None` when nothing was missing.
    pub fn arm_slot_release(
        &self,
        task_id: &TaskId,
        grace: Duration,
    ) -> Result<Option<TaskRecord>> {
        let (record, armed) = self.mutate(task_id, |record, now| {
            if record.slot_release_due_at.is_some() {
                return Ok(Mutation::Unchanged(false));
            }
            refresh_slot_release_due(record, now, grace);
            match record.slot_release_due_at {
                Some(due_at) => Ok(Mutation::Changed(
                    TaskEventKind::SlotReleaseScheduled { due_at },
                    true,
                )),
                None => Ok(Mutation::Unchanged(false)),
            }
        })?;
        Ok(armed.then_some(record))
    }

    /// A read of the task's details moves its slot-release deadline to
    /// `read_extension` from now, if the task waits in a transient state
    /// and that is later than the deadline by more than
    /// [`slot_release_extension_threshold`]. Returns whether it moved it,
    /// so frequent reads write the record at most once per threshold.
    pub fn extend_slot_release(&self, task_id: &TaskId, read_extension: Duration) -> Result<bool> {
        let (_, moved) = self.mutate(task_id, |record, now| {
            let Some(due_at) = record.slot_release_waiting() else {
                return Ok(Mutation::Unchanged(false));
            };
            let extended = now + read_extension;
            if extended - due_at <= slot_release_extension_threshold() {
                return Ok(Mutation::Unchanged(false));
            }
            record.slot_release_due_at = Some(extended);
            Ok(Mutation::Changed(
                TaskEventKind::SlotReleaseScheduled { due_at: extended },
                true,
            ))
        })?;
        Ok(moved)
    }

    /// A read of the stopped task's details wants a read-only runner (D63):
    /// leases it until `idle` from now, or moves a lease already there later,
    /// unless that would move it by no more than
    /// [`slot_release_extension_threshold`] (a client that refreshes often
    /// does not write the record each time). Returns the lease in force
    /// afterwards, and whether this call wrote it; `None` for a task that
    /// may not be read this way (running, queued, `Unknown`, archival).
    pub fn lease_reader(
        &self,
        task_id: &TaskId,
        idle: Duration,
    ) -> Result<Option<(ReaderLease, bool)>> {
        let (_, leased) = self.mutate(task_id, |record, now| {
            if !record.reader_may_read() {
                return Ok(Mutation::Unchanged(None));
            }
            let until = now + idle;
            let current = record
                .reader_lease
                .filter(|_| record.reader_lease_active(now));
            if let Some(current) = current {
                if until - current.until <= slot_release_extension_threshold() {
                    return Ok(Mutation::Unchanged(Some((current, false))));
                }
            }
            let lease = ReaderLease {
                since: current.map_or(now, |current| current.since),
                until,
            };
            record.reader_lease = Some(lease);
            Ok(Mutation::Changed(
                TaskEventKind::ReaderLeased { until },
                Some((lease, true)),
            ))
        })?;
        Ok(leased)
    }

    /// Removes the reader lease of a task that has a stale one (see
    /// [`TaskRecord::reader_lease_stale`]). Returns whether it did.
    pub fn end_reader_lease(&self, task_id: &TaskId) -> Result<bool> {
        let (_, ended) = self.mutate(task_id, |record, now| {
            if !record.reader_lease_stale(now) {
                return Ok(Mutation::Unchanged(false));
            }
            record.reader_lease = None;
            Ok(Mutation::Changed(TaskEventKind::ReaderLeaseEnded, true))
        })?;
        Ok(ended)
    }

    /// Publishes [`TaskEventKind::AutoResumeExhausted`] for the task's
    /// exhaustion once: `None` when there is none, or it was announced.
    pub fn announce_exhaustion(&self, task_id: &TaskId) -> Result<Option<TaskRecord>> {
        let (record, announced) = self.mutate(task_id, |record, _| {
            // The loss past the last attempt exhausted the limit.
            let attempts = record.resume.consecutive_losses.saturating_sub(1);
            let Some(exhaustion) = record
                .resume
                .exhausted
                .as_mut()
                .filter(|exhaustion| !exhaustion.announced)
            else {
                return Ok(Mutation::Unchanged(false));
            };
            exhaustion.announced = true;
            Ok(Mutation::Changed(
                TaskEventKind::AutoResumeExhausted {
                    attempts,
                    reason: exhaustion.reason.clone(),
                },
                true,
            ))
        })?;
        Ok(announced.then_some(record))
    }

    /// Acquires the backend's lease with a new, higher fencing epoch.
    ///
    /// Granted when no lease exists, the current one expired, or the same
    /// controller asks again (a restarted controller fences its own previous
    /// incarnation). Refused with [`OrchestrationError::LeaseHeld`] while
    /// another controller holds an unexpired lease.
    pub fn acquire_lease(
        &self,
        backend_id: &BackendId,
        controller_id: &str,
        ttl: Duration,
    ) -> Result<BackendLease> {
        let _lock = self.store.lock_backend(backend_id)?;
        let now = self.clock.now();
        let current = self.store.load_lease(backend_id)?;
        if let Some(current) = &current {
            if current.controller_id != controller_id && now < current.expires_at {
                return Err(OrchestrationError::LeaseHeld {
                    backend_id: backend_id.clone(),
                    holder: current.controller_id.clone(),
                    expires_at: current.expires_at,
                });
            }
        }
        let lease = BackendLease {
            record_version: BACKEND_LEASE_VERSION,
            backend_id: backend_id.clone(),
            controller_id: controller_id.to_string(),
            epoch: current.map_or(1, |lease| lease.epoch + 1),
            acquired_at: now,
            expires_at: now + ttl,
        };
        self.store.save_lease(&lease)?;
        Ok(lease)
    }

    /// Extends the current lease, keeping its epoch.
    ///
    /// Allowed even after expiry as long as nobody took the backend over:
    /// expiry only *permits* a takeover, it does not fence by itself.
    pub fn renew_lease(&self, token: &LeaseToken, ttl: Duration) -> Result<BackendLease> {
        let _lock = self.store.lock_backend(&token.backend_id)?;
        let mut lease = self.current_lease(token)?;
        lease.expires_at = self.clock.now() + ttl;
        self.store.save_lease(&lease)?;
        Ok(lease)
    }

    /// The backend's lease, if one was ever acquired.
    pub fn lease(&self, backend_id: &BackendId) -> Result<Option<BackendLease>> {
        self.store.load_lease(backend_id)
    }

    /// Gives the lease up voluntarily (a controller shutting down cleanly),
    /// so another controller may take over at once instead of waiting for
    /// expiry. The epoch stays; the next acquisition increments it, which
    /// fences every report still in flight under this one.
    pub fn release_lease(&self, token: &LeaseToken) -> Result<BackendLease> {
        let _lock = self.store.lock_backend(&token.backend_id)?;
        let mut lease = self.current_lease(token)?;
        lease.expires_at = lease.expires_at.min(self.clock.now());
        self.store.save_lease(&lease)?;
        Ok(lease)
    }

    /// Ends `controller_id`'s hold on the backend's lease, if it holds it —
    /// the lease half of revoking a controller identity (R37).
    ///
    /// Expiring the lease is enough to fence it: reports under an expired
    /// lease are refused, and the revoked identity can no longer
    /// authenticate to renew it. Returns the lease as it is afterwards, or
    /// `None` when `controller_id` did not hold it.
    pub fn revoke_lease(
        &self,
        backend_id: &BackendId,
        controller_id: &str,
    ) -> Result<Option<BackendLease>> {
        let _lock = self.store.lock_backend(backend_id)?;
        let Some(mut lease) = self.store.load_lease(backend_id)? else {
            return Ok(None);
        };
        if lease.controller_id != controller_id {
            return Ok(None);
        }
        lease.expires_at = lease.expires_at.min(self.clock.now());
        self.store.save_lease(&lease)?;
        Ok(Some(lease))
    }

    /// The backend's lease if `controller_id` holds it unexpired.
    pub fn held_lease(
        &self,
        backend_id: &BackendId,
        controller_id: &str,
    ) -> Result<Option<BackendLease>> {
        Ok(self.store.load_lease(backend_id)?.filter(|lease| {
            lease.controller_id == controller_id && self.clock.now() < lease.expires_at
        }))
    }

    /// Accepts a controller observation.
    ///
    /// Refused unless the lease is the backend's current, unexpired one and
    /// not older than the last accepted report; the desired generation is the
    /// current one; the resource version is not older than the last accepted
    /// one of the same incarnation; and the observation is legal from the
    /// current runner state; `PausedAtSafePoint` additionally needs the runner's
    /// safe-point acknowledgement for the current generation
    /// ([`OrchestrationError::SafePointNotAcknowledged`]). A pure redelivery
    /// is absorbed with `applied: false`.
    ///
    /// A workload loss is remembered with where, when and under which lease
    /// epoch it happened, and counted once the task enters `Interrupted`
    /// (see [`super::resume`]): the report plans the resume policy's next
    /// attempt or gives up, but never replaces the workload itself. `Unknown`
    /// is counted only once a controller confirms the loss.
    pub fn report(&self, report: ControllerReport) -> Result<ReportOutcome> {
        let (task, applied) = self.mutate(&report.task_id, |record, now| {
            let lease = self.valid_lease(&report.lease)?;
            if record.assignment.backend_id != lease.backend_id {
                return Err(OrchestrationError::WrongBackend {
                    task_id: record.task_id.clone(),
                    assigned: record.assignment.backend_id.clone(),
                    presented: lease.backend_id.clone(),
                });
            }
            if let Some(last) = record.last_report_epoch {
                if report.lease.epoch < last {
                    return Err(OrchestrationError::StaleLease {
                        backend_id: lease.backend_id.clone(),
                        controller_id: report.lease.controller_id.clone(),
                        presented: report.lease.epoch,
                        current: Some(last),
                    });
                }
            }
            record.check_generation(report.desired_generation)?;
            check_resource_order(record, &report)?;

            let effect = observation_effect(record.runner_state, report.observation);
            let observed = ObservedState {
                observation: report.observation,
                summary: report.summary.clone(),
                resource: report.resource.clone(),
                lease_epoch: report.lease.epoch,
                desired_generation: report.desired_generation,
                observed_at: report.observed_at,
                received_at: now,
            };
            match effect {
                ObservationEffect::Illegal => {
                    return Err(OrchestrationError::IllegalObservation {
                        task_id: record.task_id.clone(),
                        observation: report.observation,
                        runner_state: record.runner_state,
                    })
                }
                ObservationEffect::Unchanged if is_redelivery(record, &observed) => {
                    return Ok(Mutation::Unchanged(false))
                }
                ObservationEffect::Unchanged => {}
                ObservationEffect::Transition(next) => {
                    // Checked here, not only at the protocol edge, so no
                    // writer — an HTTP controller or an in-process adapter —
                    // can remove compute on its own word (KTD2).
                    if report.observation == Observation::PausedAtSafePoint
                        && !safe_point_acknowledged(record)
                    {
                        return Err(OrchestrationError::SafePointNotAcknowledged {
                            task_id: record.task_id.clone(),
                            generation: record.generation,
                        });
                    }
                    // Checked here for the same reason: admission is the
                    // control plane's decision (U10), so no controller —
                    // a stale one, one that never read the commands — can
                    // start a task the scheduler did not give a slot.
                    if report.observation == Observation::StartAccepted
                        && record.reservation.is_none()
                    {
                        return Err(OrchestrationError::NotAdmitted(record.task_id.clone()));
                    }
                    let previous = record.runner_state;
                    // The safe point a lowered limit asked for: the compute
                    // is gone, and the task goes back to the queue on its
                    // own rank, with no reservation, as the resume that
                    // starts it again.
                    let next = if next == RunnerState::Stopped && record.limit_stop_accepted {
                        record.limit_stop_accepted = false;
                        record.slot_release_due_at = None;
                        record.reservation = None;
                        record.queue = None;
                        record.generation += 1;
                        record.commands.push(CommandReceipt {
                            idempotency_key: super::resume::limit_requeue_key(record.generation),
                            command: OwnerCommand::Resume,
                            expected_generation: None,
                            accepted_at: now,
                            generation_after: record.generation,
                            runner_state_after: RunnerState::Queued,
                            revision: record.revision + 1,
                            actor: CommandActor::Policy,
                        });
                        self.order_launch(record)?;
                        RunnerState::Queued
                    } else {
                        next
                    };
                    record.runner_state = next;
                    if next != RunnerState::PauseRequested {
                        record.limit_stop_accepted = false;
                    }
                    match report.observation {
                        Observation::StartAccepted => record.resources_may_exist = true,
                        Observation::RunnerReady => record.running_since = Some(now),
                        Observation::ResourcesDeleted => {
                            if let Some(tombstone) = &mut record.archive {
                                tombstone.completed_at = Some(now);
                            }
                        }
                        _ => {}
                    }
                    // A confirmation counts the loss remembered when the
                    // task went `Unknown`; only one without such a memory
                    // (from before losses were kept) is taken as new.
                    let new_loss = match report.observation {
                        Observation::WorkloadLost { .. } => true,
                        Observation::LossConfirmed => record.resume.last_loss.is_none(),
                        _ => false,
                    };
                    if new_loss {
                        record.resume.last_loss = Some(WorkloadLoss {
                            from: previous,
                            at: now,
                            lease_epoch: report.lease.epoch,
                            reason: report.summary.detail.clone(),
                        });
                    }
                    if next == RunnerState::Interrupted {
                        count_loss(record, &self.resume_policy, now);
                    }
                }
            }
            if holds_workload(record.runner_state) && !reports_loss(report.observation) {
                record.workload_epoch = record.workload_epoch.max(Some(report.lease.epoch));
            }
            record.observed = Some(observed);
            record.last_report_epoch = Some(report.lease.epoch);
            Ok(Mutation::Changed(
                TaskEventKind::ControllerReported {
                    observation: report.observation,
                    lease_epoch: report.lease.epoch,
                },
                true,
            ))
        })?;
        Ok(ReportOutcome { task, applied })
    }

    /// Copies the task's launch spec to its task storage once (R18).
    ///
    /// `Ok(false)` is the reconnect case: the identical spec is already
    /// there, so the caller reconciles the existing runner instead of
    /// bootstrapping a second one. An archival task is never bootstrapped.
    pub fn bootstrap_task_storage(
        &self,
        task_id: &TaskId,
        task_storage: &ExecutionStore,
    ) -> Result<bool> {
        let record = self.store.load_task(task_id)?;
        if record.runner_state.is_archival() {
            return Err(OrchestrationError::IllegalCommand {
                task_id: task_id.clone(),
                command: OwnerCommand::Start,
                runner_state: record.runner_state,
            });
        }
        Ok(task_storage.initialize_task_launch_spec(task_id, &record.launch_spec)?)
    }

    /// Records that a material of the task changed (U9): one event carrying
    /// the material id and counts, nothing of its content.
    ///
    /// Desired state is untouched; the event exists so clients following
    /// the stream learn that the task's materials changed and re-read them.
    pub fn record_material_change(
        &self,
        task_id: &TaskId,
        material_id: &str,
        change: MaterialChange,
        files: u32,
        total_bytes: u64,
    ) -> Result<TaskRecord> {
        let (record, ()) = self.mutate(task_id, |_, _| {
            Ok(Mutation::Changed(
                TaskEventKind::MaterialChanged {
                    material_id: material_id.to_string(),
                    change,
                    files,
                    total_bytes,
                },
                (),
            ))
        })?;
        Ok(record)
    }

    /// Startup repair of the orchestration directory.
    ///
    /// Removes orphaned temporary files and appends any event a record
    /// committed but a crash kept out of the log. Nothing else is judged:
    /// unlike local recovery, a remote task's runner state changes only through
    /// controller reconciliation, never because this process restarted.
    ///
    /// The material store (U9) is repaired here too, so the one startup
    /// recovery the daemon runs covers it: interrupted uploads are dropped
    /// and an archived task's materials removed. A failure there is logged
    /// rather than returned — it must not keep task records unrepaired.
    pub fn recover(&self) -> Result<OrchestrationRecovery> {
        let removed_temp_files = self.store.cleanup_orphaned_temp_files()?;
        let mut republished_events = Vec::new();
        let mut adopted_losses = Vec::new();
        for task_id in self.store.list_task_ids()? {
            let _lock = self.store.lock_task(&task_id)?;
            let mut record = self.store.load_task(&task_id)?;
            if self.store.publish_last_event(&record)? {
                republished_events.push((task_id.clone(), record.revision));
            }
            // One task's unreadable history must not cost every other task
            // its rank repair and material cleanup: it is skipped, and
            // adopted at a later start once readable.
            match self.adopt_loss(&mut record) {
                Ok(true) => adopted_losses.push(task_id),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    task_id = %task_id,
                    %error,
                    "recovery: cannot adopt the task's workload loss; skipped"
                ),
            }
        }
        self.normalise_ranks()?;
        let materials = crate::materials::MaterialStore::new(
            self.store.paths(),
            crate::materials::MaterialLimits::default(),
        );
        let removed_material_leftovers = materials
            .recover(|task_id| match self.store.load_task(task_id) {
                Ok(record) => record.runner_state.is_archival(),
                // Materials of a task that does not exist belong to nobody.
                Err(OrchestrationError::TaskNotFound(_)) => true,
                // An unreadable record is no proof of archival: keep them.
                Err(_) => false,
            })
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "material store recovery failed");
                Vec::new()
            });
        Ok(OrchestrationRecovery {
            removed_temp_files,
            republished_events,
            removed_material_leftovers,
            adopted_losses,
        })
    }

    /// Gives a task interrupted (or unknown) before losses were remembered
    /// the loss its event log shows, counted as a fresh one, so the resume
    /// policy resumes it — unless it was lost while pausing. Call under the
    /// task's lock. Like rank repair, it announces nothing: the record's
    /// runner state does not change, and clients' next read shows the state.
    fn adopt_loss(&self, record: &mut TaskRecord) -> Result<bool> {
        let lost = matches!(
            record.runner_state,
            RunnerState::Interrupted | RunnerState::Unknown
        );
        if !lost || record.resume.last_loss.is_some() {
            return Ok(false);
        }
        let events = self.store.events_since(&record.task_id, 0)?;
        record.resume.last_loss = Some(loss_from_history(record, &events));
        if record.runner_state == RunnerState::Interrupted {
            count_loss(record, &self.resume_policy, self.clock.now());
        }
        self.store.save_task(record)?;
        Ok(true)
    }

    /// A command ordered a new launch: no runner of it was ready yet, and
    /// it is ordered under the backend's current lease epoch.
    fn order_launch(&self, record: &mut TaskRecord) -> Result<()> {
        record.running_since = None;
        if let Some(lease) = self.store.load_lease(&record.assignment.backend_id)? {
            record.workload_epoch = record.workload_epoch.max(Some(lease.epoch));
        }
        Ok(())
    }

    /// The serialized read-modify-write of one task.
    pub(crate) fn mutate<T>(
        &self,
        task_id: &TaskId,
        operation: impl FnOnce(&mut TaskRecord, DateTime<Utc>) -> Result<Mutation<T>>,
    ) -> Result<(TaskRecord, T)> {
        // Checked before locking so a typo never leaves an empty task
        // directory behind.
        if !self.store.task_exists(task_id) {
            return Err(OrchestrationError::TaskNotFound(task_id.clone()));
        }
        let _lock = self.store.lock_task(task_id)?;
        let mut record = self.store.load_task(task_id)?;
        // Repair an interrupted publish first, so the log never skips a
        // revision once a newer one is appended.
        self.store.publish_last_event(&record)?;

        let now = self.clock.now();
        match operation(&mut record, now)? {
            Mutation::Unchanged(value) => Ok((record, value)),
            Mutation::Changed(kind, value) => {
                // A reservation and a queue verdict mean something only
                // while queued: once the task leaves `Queued` its runner state
                // holds (or releases) the slot by itself (U10).
                if record.runner_state != RunnerState::Queued {
                    record.reservation = None;
                    record.queue = None;
                }
                record.revision += 1;
                record.last_event = TaskEvent {
                    sequence: record.revision,
                    task_id: record.task_id.clone(),
                    at: now,
                    runner_state: record.runner_state,
                    generation: record.generation,
                    kind,
                };
                self.store.save_task(&record)?;
                self.store.publish_last_event(&record)?;
                Ok((record, value))
            }
        }
    }

    /// The backend's current lease if `token` is it; otherwise fenced.
    fn current_lease(&self, token: &LeaseToken) -> Result<BackendLease> {
        let current = self.store.load_lease(&token.backend_id)?;
        match current {
            Some(lease)
                if lease.epoch == token.epoch && lease.controller_id == token.controller_id =>
            {
                Ok(lease)
            }
            other => Err(OrchestrationError::StaleLease {
                backend_id: token.backend_id.clone(),
                controller_id: token.controller_id.clone(),
                presented: token.epoch,
                current: other.map(|lease| lease.epoch),
            }),
        }
    }

    /// [`Self::current_lease`], additionally refusing an expired one.
    ///
    /// Called under the task lock but without the backend lock: the lease
    /// file is replaced atomically and its epoch only grows, so a report
    /// validated just before a takeover is ordered before it, and every
    /// report validated after it is fenced. `last_report_epoch` then keeps the
    /// older epoch out of this task for good once the new controller reported.
    fn valid_lease(&self, token: &LeaseToken) -> Result<BackendLease> {
        let lease = self.current_lease(token)?;
        if self.clock.now() >= lease.expires_at {
            return Err(OrchestrationError::LeaseExpired {
                backend_id: lease.backend_id,
                epoch: lease.epoch,
                expires_at: lease.expires_at,
            });
        }
        Ok(lease)
    }
}

/// Sets or clears the slot-release deadline from the runner state and the
/// last snapshot (D62). Only a `Running` task has one:
///
/// * a final execution state makes it due now (or keeps an earlier one);
/// * a transient one plans `now + grace`, unless a deadline exists already,
///   so repeated snapshots of the same wait do not push it back;
/// * anything else clears it.
pub(crate) fn refresh_slot_release_due(
    record: &mut TaskRecord,
    now: DateTime<Utc>,
    grace: Duration,
) {
    let class = record.slot_release_class();
    record.slot_release_due_at = match (record.runner_state, class) {
        (RunnerState::Running, Some(SlotReleaseClass::Final)) => Some(
            record
                .slot_release_due_at
                .map_or(now, |due_at| due_at.min(now)),
        ),
        (RunnerState::Running, Some(SlotReleaseClass::Transient)) => {
            record.slot_release_due_at.or(Some(now + grace))
        }
        _ => None,
    };
}

/// A command takes the task out of `Interrupted`/`Unknown`: the planned
/// attempt is used up either way, and the owner's decision forgets the
/// losses altogether (R6).
fn leave_loss(record: &mut TaskRecord, actor: CommandActor) {
    match actor {
        CommandActor::Owner => record.resume = ResumeState::default(),
        CommandActor::Policy => record.resume.next_attempt_at = None,
    }
}

/// Whether a task in `runner_state` has (or is getting) a workload.
fn holds_workload(runner_state: RunnerState) -> bool {
    matches!(
        runner_state,
        RunnerState::Provisioning | RunnerState::Running | RunnerState::PauseRequested
    )
}

/// Whether `observation` says the workload was lost.
fn reports_loss(observation: Observation) -> bool {
    matches!(
        observation,
        Observation::WorkloadLost { .. } | Observation::LossConfirmed
    )
}

/// Sets the archive tombstone; completes at once when nothing was created.
fn archive(record: &mut TaskRecord, idempotency_key: &str, now: DateTime<Utc>) {
    let never_created = !record.resources_may_exist;
    record.archive = Some(ArchiveTombstone {
        idempotency_key: idempotency_key.to_string(),
        generation: record.generation,
        requested_at: now,
        completed_at: never_created.then_some(now),
        resources_never_created: never_created,
    });
    if never_created {
        record.runner_state = RunnerState::Archived;
    }
}

/// Whether a stored record was created from exactly this request.
fn same_creation(record: &TaskRecord, request: &NewTask) -> bool {
    record.launch_spec == request.launch_spec
        && record.owner == request.owner
        && record.assignment.backend_id == request.backend_id
}

/// Whether the stored record was created from a request with the same
/// digest — a retry of the identical owner request, which must return the
/// task even if the configuration it once resolved against changed since.
fn same_request(record: &TaskRecord, request: &NewTask) -> bool {
    record.owner == request.owner
        && record.request_digest.is_some()
        && record.request_digest == request.request_digest
}

/// Whether the runner acknowledged a safe point for the task's current
/// desired generation — the only permission to complete a pause.
pub(crate) fn safe_point_acknowledged(record: &TaskRecord) -> bool {
    record
        .runner
        .as_ref()
        .is_some_and(|runner| runner.safe_point_generation == Some(record.generation))
}

/// Refuses a report about an older version of the same resource incarnation.
fn check_resource_order(record: &TaskRecord, report: &ControllerReport) -> Result<()> {
    let previous = record
        .observed
        .as_ref()
        .and_then(|observed| observed.resource.as_ref());
    if let (Some(previous), Some(incoming)) = (previous, report.resource.as_ref()) {
        if previous.uid == incoming.uid
            && incoming.resource_generation < previous.resource_generation
        {
            return Err(OrchestrationError::StaleObservation {
                task_id: record.task_id.clone(),
                uid: incoming.uid.clone(),
                accepted: previous.resource_generation,
                presented: incoming.resource_generation,
            });
        }
    }
    Ok(())
}

/// A report that repeats the last accepted observation in every respect but
/// its timestamps (at-least-once delivery) changes nothing.
fn is_redelivery(record: &TaskRecord, incoming: &ObservedState) -> bool {
    record.observed.as_ref().is_some_and(|previous| {
        previous.observation == incoming.observation
            && previous.summary == incoming.summary
            && previous.resource == incoming.resource
            && previous.lease_epoch == incoming.lease_epoch
            && previous.desired_generation == incoming.desired_generation
    })
}
