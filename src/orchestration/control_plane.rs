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
    ArchiveTombstone, BackendLease, CommandOutcome, CommandReceipt, CommandRequest,
    ControllerReport, LeaseToken, MaterialChange, NewTask, Observation, ObservedState,
    OwnerCommand, ReportOutcome, TaskAssignment, TaskEvent, TaskEventKind, TaskLifecycle,
    TaskRecord, BACKEND_LEASE_VERSION, TASK_RECORD_VERSION,
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
        }
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
            lifecycle: TaskLifecycle::Queued,
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
            reservation: None,
            queue: None,
            request_digest: request.request_digest,
            last_event: TaskEvent {
                sequence: 1,
                task_id: request.task_id.clone(),
                at: now,
                lifecycle: TaskLifecycle::Queued,
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
    pub fn command(&self, task_id: &TaskId, request: CommandRequest) -> Result<CommandOutcome> {
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
                if expected != record.generation {
                    return Err(OrchestrationError::StaleGeneration {
                        task_id: record.task_id.clone(),
                        current: record.generation,
                        presented: expected,
                    });
                }
            }

            match command_effect(record.lifecycle, request.command) {
                CommandEffect::Illegal => {
                    return Err(OrchestrationError::IllegalCommand {
                        task_id: record.task_id.clone(),
                        command: request.command,
                        lifecycle: record.lifecycle,
                    })
                }
                // Recorded for the audit trail; the tombstone already holds.
                CommandEffect::ArchiveConverged => {}
                CommandEffect::Transition(next) => {
                    record.generation += 1;
                    record.lifecycle = next;
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
                lifecycle_after: record.lifecycle,
                revision: record.revision + 1,
            };
            record.commands.push(receipt.clone());
            Ok(Mutation::Changed(
                TaskEventKind::OwnerCommandAccepted {
                    command: request.command,
                    idempotency_key: request.idempotency_key.clone(),
                },
                (receipt, false),
            ))
        })?;
        Ok(CommandOutcome {
            receipt,
            task,
            replayed,
        })
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
    /// current lifecycle; `PausedAtSafePoint` additionally needs the runner's
    /// safe-point acknowledgement for the current generation
    /// ([`OrchestrationError::SafePointNotAcknowledged`]). A pure redelivery
    /// is absorbed with `applied: false`. Workload loss never schedules a replacement — the
    /// task waits in Interrupted/Unknown for an owner command (R13).
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
            if report.desired_generation != record.generation {
                return Err(OrchestrationError::StaleGeneration {
                    task_id: record.task_id.clone(),
                    current: record.generation,
                    presented: report.desired_generation,
                });
            }
            check_resource_order(record, &report)?;

            let effect = observation_effect(record.lifecycle, report.observation);
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
                        lifecycle: record.lifecycle,
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
                    record.lifecycle = next;
                    match report.observation {
                        Observation::StartAccepted => record.resources_may_exist = true,
                        Observation::ResourcesDeleted => {
                            if let Some(tombstone) = &mut record.archive {
                                tombstone.completed_at = Some(now);
                            }
                        }
                        _ => {}
                    }
                }
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
        if record.lifecycle.is_archival() {
            return Err(OrchestrationError::IllegalCommand {
                task_id: task_id.clone(),
                command: OwnerCommand::Start,
                lifecycle: record.lifecycle,
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
    /// unlike local recovery, a remote task's lifecycle changes only through
    /// controller reconciliation, never because this process restarted.
    ///
    /// The material store (U9) is repaired here too, so the one startup
    /// recovery the daemon runs covers it: interrupted uploads are dropped
    /// and an archived task's materials removed. A failure there is logged
    /// rather than returned — it must not keep task records unrepaired.
    pub fn recover(&self) -> Result<OrchestrationRecovery> {
        let removed_temp_files = self.store.cleanup_orphaned_temp_files()?;
        let mut republished_events = Vec::new();
        for task_id in self.store.list_task_ids()? {
            let _lock = self.store.lock_task(&task_id)?;
            let record = self.store.load_task(&task_id)?;
            if self.store.publish_last_event(&record)? {
                republished_events.push((task_id, record.revision));
            }
        }
        let materials = crate::materials::MaterialStore::new(
            self.store.paths(),
            crate::materials::MaterialLimits::default(),
        );
        let removed_material_leftovers = materials
            .recover(|task_id| match self.store.load_task(task_id) {
                Ok(record) => record.lifecycle.is_archival(),
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
        })
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
                // while queued: once the task leaves `Queued` its lifecycle
                // holds (or releases) the slot by itself (U10).
                if record.lifecycle != TaskLifecycle::Queued {
                    record.reservation = None;
                    record.queue = None;
                }
                record.revision += 1;
                record.last_event = TaskEvent {
                    sequence: record.revision,
                    task_id: record.task_id.clone(),
                    at: now,
                    lifecycle: record.lifecycle,
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
        record.lifecycle = TaskLifecycle::Archived;
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
