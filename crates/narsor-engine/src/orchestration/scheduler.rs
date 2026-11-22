//! Admission of queued tasks under backend availability and a global task
//! budget (U10: R3, R6, R10, R11).
//!
//! The control plane — not a controller — decides which queued task may
//! start. A queued task is offered `Start` only once it holds a
//! [`CapacityReservation`], and a controller's `StartAccepted` for a task
//! without one is refused, so no controller can exceed the shared
//! `server.max_concurrent_tasks` budget however stale or eager it is.
//!
//! # One global scheduling decision at a time
//!
//! [`ControlPlane::schedule`] and
//! [`ControlPlane::command_with_capacity`] run under the global schedule
//! lock, so admissions on different backends share one consistent count.
//! Scheduling also takes the backend lock to preserve that backend's queue
//! order. These are the only ways into a capacity-holding state from a free
//! one; resuming a paused task goes through `command_with_capacity`.
//! Every other transition either keeps a task's slot (an admitted start
//! becoming `Provisioning`, a retry of an interrupted task) or frees one,
//! so a count taken under the lock can only be too high, never too low: a
//! decision may under-admit for a moment, and the re-evaluation that the
//! release triggers corrects it, but it never over-admits.
//!
//! # Order and non-preemption
//!
//! The queue of a backend is its tasks that are not archived, by position
//! key, lowest first, and by creation order (`queue_sequence`) among equal
//! keys, which only legacy data can have. Waiting tasks are admitted in
//! that order. Only the owner's [`ControlPlane::move_task`] changes it:
//! a resume, an answer, a limit stop or any other change of a task keeps
//! its key, and so its place. A reservation, once granted, is never taken
//! back — not by a task moved ahead of it or a controller going away —
//! and nothing here ever stops a task that holds compute (R11).
//!
//! Two exceptions come with a limit that goes below the slots in use. A
//! reservation over the limit is taken back at once: nothing started on it,
//! so it costs nothing. A task that runs is asked to stop, and only at a safe
//! point: [`ControlPlane::decide_limit_stop`] accepts exactly as many safe
//! points as the limit is exceeded by, the first ones that arrive, and
//! answers "continue" to the others. A stopped task goes back to the queue
//! with its own key.
//!
//! # What holds a slot
//!
//! See [`TaskRecord::holds_capacity`]: everything that has or may have
//! compute, conservatively including `Interrupted`, `Unknown` and
//! `Archiving`. A `Stopped` task released its compute; it keeps its slot only
//! after the owner's `Pause` or `Stop` of a task that waited for the owner,
//! until that wait's deadline, `ReleaseSlot`, a resume or an archive ends it.
//!
//! # An offline controller
//!
//! While no controller holds an unexpired lease of the backend (it crashed,
//! lost its connection or was revoked, which expires its lease), new
//! reservations are withheld and waiting tasks show
//! [`QueueReason::ControllerOffline`]. Tasks already admitted keep their
//! reservation: they are first in line when a controller returns, and the
//! owner can still archive them. Withholding instead of granting keeps the
//! decision as late as possible, so a move made meanwhile still
//! counts.

use super::error::{OrchestrationError, Result};
use super::lifecycle::{command_effect, CommandEffect};
use super::model::{
    BackendAvailability, CapacityReservation, CommandActor, CommandOutcome, CommandRequest,
    DefaultBackendSelection, MoveTarget, OwnerCommand, QueueReason, QueueStatus, RunnerState,
    SchedulingPolicy, SlotReleaseReason, TaskEventKind, TaskRecord,
    DEFAULT_BACKEND_SELECTION_VERSION,
};
use super::position;
use super::resume::limit_stop_key;
use super::store::queue_order;
use super::{control_plane::Mutation, ControlPlane};
use crate::domain::{BackendId, TaskId};
use std::collections::HashMap;

/// What one scheduling pass decided for one queued task.
enum Verdict {
    /// Grant a reservation (or keep the one it holds).
    Admit,
    /// Take the reservation back (the limit went below the slots in use) and
    /// keep waiting, for this reason.
    Revoke(QueueReason),
    /// Keep waiting, for this reason.
    Wait(QueueReason),
}

impl ControlPlane {
    /// Re-evaluates admission on `backend_id` and returns every task whose
    /// record changed (a reservation granted, a reason updated), so the
    /// caller can announce them.
    ///
    /// The caller reads backend availability and the global task limit fresh
    /// for every pass. Idempotent: a pass over an unchanged backend writes
    /// nothing.
    pub fn schedule(
        &self,
        backend_id: &BackendId,
        policy: &SchedulingPolicy,
    ) -> Result<Vec<TaskId>> {
        self.schedule_with(backend_id, || policy.clone())
    }

    /// [`Self::schedule`] reading its policy after the schedule locks are
    /// taken, so a decision never uses a limit that changed while it waited
    /// for the lock (R15).
    pub fn schedule_with(
        &self,
        backend_id: &BackendId,
        policy: impl FnOnce() -> SchedulingPolicy,
    ) -> Result<Vec<TaskId>> {
        let _global = self.store().lock_global_schedule()?;
        let _lock = self.store().lock_schedule(backend_id)?;
        let policy = policy();
        let tasks = self.queue(backend_id)?;
        let online = self.controller_online(backend_id)?;
        let mut in_use = slots_in_use(&self.store().list_tasks()?);

        // Reservations over a lowered limit are taken back, those of the
        // lowest priority first. A task that holds compute is never one.
        let mut revoked: Vec<&TaskId> = Vec::new();
        if let Some(limit) = policy.max_concurrent_tasks {
            let mut reserved: Vec<&TaskRecord> = tasks
                .iter()
                .filter(|task| {
                    task.runner_state == RunnerState::Queued && task.reservation.is_some()
                })
                .collect();
            reserved
                .sort_by_key(|task| std::cmp::Reverse((task.position_key, task.queue_sequence)));
            for task in reserved {
                if in_use <= limit {
                    break;
                }
                in_use -= 1;
                revoked.push(&task.task_id);
            }
        }

        let mut waiting: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|task| {
                task.runner_state == RunnerState::Queued
                    && (task.reservation.is_none() || revoked.contains(&&task.task_id))
            })
            .collect();
        waiting.sort_by_key(|task| (task.position_key, task.queue_sequence));

        let mut verdicts: Vec<(TaskId, Verdict)> = tasks
            .iter()
            .filter(|task| {
                task.runner_state == RunnerState::Queued
                    && task.reservation.is_some()
                    && !revoked.contains(&&task.task_id)
            })
            .map(|task| (task.task_id.clone(), Verdict::Admit))
            .collect();
        let mut still_waiting = Vec::new();
        for task in waiting {
            match (&policy.backend, policy.max_concurrent_tasks) {
                (BackendAvailability::Available, limit)
                    if online && limit.is_none_or(|limit| in_use < limit) =>
                {
                    in_use += 1;
                    verdicts.push((task.task_id.clone(), Verdict::Admit));
                }
                _ => still_waiting.push(task),
            }
        }
        // Positions and the slot count are those *after* this pass's
        // grants, so every reason describes the same, final state.
        for (index, task) in still_waiting.into_iter().enumerate() {
            let position = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let reason = match (&policy.backend, policy.max_concurrent_tasks) {
                (BackendAvailability::Unavailable(detail), _) => QueueReason::BackendUnknown {
                    detail: detail.clone(),
                },
                (BackendAvailability::Available, _) if !online => QueueReason::ControllerOffline,
                (BackendAvailability::Available, limit) => QueueReason::WaitingForCapacity {
                    position,
                    running: in_use,
                    limit: limit.unwrap_or(u32::MAX),
                },
            };
            let verdict = if revoked.contains(&&task.task_id) {
                Verdict::Revoke(reason)
            } else {
                Verdict::Wait(reason)
            };
            verdicts.push((task.task_id.clone(), verdict));
        }

        let mut changed = Vec::new();
        for (task_id, verdict) in verdicts {
            if self.apply_verdict(&task_id, verdict)? {
                changed.push(task_id);
            }
        }
        Ok(changed)
    }

    /// Asks the task running at `generation` to stop at a safe point,
    /// because the global task limit is exceeded: a `Pause` of
    /// [`CommandActor::Policy`], keyed [`limit_stop_key`]`(generation)`.
    ///
    /// Allowed only while [`TaskRecord::limit_stop_candidate`]. An owner
    /// command that came first moves the generation or the state, so the
    /// policy's is refused with `StaleGeneration` or `IllegalCommand`.
    pub fn request_limit_stop(&self, task_id: &TaskId, generation: u64) -> Result<CommandOutcome> {
        self.command(
            task_id,
            CommandRequest::new(OwnerCommand::Pause, limit_stop_key(generation))
                .expecting_generation(generation)
                .issued_by(CommandActor::Policy),
        )
    }

    /// Decides what happens to the task whose runner reports a safe point
    /// while it waits for a pause a lowered limit asked for.
    ///
    /// Under the schedule locks, with the limit read after they are
    /// taken: the stops accepted so far and not yet carried out count
    /// against the overshoot, so two reports at once never stop two tasks
    /// for an overshoot of one. With an overshoot left the safe point is
    /// accepted ([`TaskRecord::limit_stop_accepted`]) and the runner is told
    /// to stop. Otherwise — the overshoot went, the limit is unknown, or the
    /// owner has work for the runner — the pause is taken back: the task is
    /// `Running` again and the runner is told to continue. Any other task,
    /// and a repeated report, is left as it is.
    pub fn decide_limit_stop(
        &self,
        task_id: &TaskId,
        policy: impl FnOnce() -> SchedulingPolicy,
    ) -> Result<LimitStopDecision> {
        let backend_id = self.task(task_id)?.assignment.backend_id;
        let _global = self.store().lock_global_schedule()?;
        let _lock = self.store().lock_schedule(&backend_id)?;
        let policy = policy();
        let load = BackendLoad::of(&self.store().list_tasks()?);
        let (_, decision) = self.mutate(task_id, |record, _now| {
            if !record.pause_requested_by_limit() {
                return Ok(Mutation::Unchanged(LimitStopDecision::NotApplicable));
            }
            if record.limit_stop_accepted {
                return Ok(Mutation::Unchanged(LimitStopDecision::Accepted));
            }
            let overshoot = match (&policy.backend, policy.max_concurrent_tasks) {
                (BackendAvailability::Available, Some(limit)) => load.overshoot(limit),
                _ => 0,
            };
            if overshoot > 0 && !record.owner_work_pending() {
                record.limit_stop_accepted = true;
                return Ok(Mutation::Changed(
                    TaskEventKind::SlotReleased {
                        reason: SlotReleaseReason::LimitLowered,
                    },
                    LimitStopDecision::Accepted,
                ));
            }
            record.generation += 1;
            record.runner_state = RunnerState::Running;
            Ok(Mutation::Changed(
                TaskEventKind::LimitStopWithdrawn,
                LimitStopDecision::Withdrawn,
            ))
        })?;
        Ok(decision)
    }

    /// What all tasks together hold, whatever the backend.
    pub fn total_load(&self) -> Result<BackendLoad> {
        Ok(BackendLoad::of(&self.store().list_tasks()?))
    }

    /// Slots of `backend_id` in use right now (see
    /// [`TaskRecord::holds_capacity`]).
    pub fn capacity_in_use(&self, backend_id: &BackendId) -> Result<u32> {
        Ok(slots_in_use(&self.backend_tasks(backend_id)?))
    }

    /// Slots in use right now across every backend.
    pub fn total_capacity_in_use(&self) -> Result<u32> {
        Ok(slots_in_use(&self.store().list_tasks()?))
    }

    /// Whether a controller holds an unexpired lease of `backend_id` — the
    /// only condition under which new starts are admitted.
    ///
    /// The lease, not the heartbeat, is the protocol's liveness: a
    /// controller renews it well before expiry, and revoking a controller
    /// expires it at once.
    pub fn controller_online(&self, backend_id: &BackendId) -> Result<bool> {
        let now = self.now();
        Ok(self
            .lease(backend_id)?
            .is_some_and(|lease| now < lease.expires_at))
    }

    /// [`ControlPlane::command`], deciding what a command that would take a
    /// capacity slot does when the backend has none free.
    ///
    /// Only resuming a `Stopped` task needs a slot — every other command
    /// keeps or frees the task's slot — and it is decided under the
    /// backend's schedule lock, like an admission. With a free slot the
    /// resume goes straight to `Provisioning`. On a full backend it does not
    /// fail and stops nothing (R11): the task goes to `Queued` holding
    /// nothing, at its own place in the queue, and a later
    /// [`ControlPlane::schedule`] pass admits it when its turn comes.
    /// A retry of an accepted command is answered from its receipt as
    /// always, whatever the capacity now.
    pub fn command_with_capacity(
        &self,
        task_id: &TaskId,
        request: CommandRequest,
        policy: &SchedulingPolicy,
    ) -> Result<CommandOutcome> {
        self.command_with_capacity_from(task_id, request, || policy.clone())
    }

    /// [`Self::command_with_capacity`] reading the limit after the schedule
    /// lock is taken, like [`Self::schedule_with`].
    pub fn command_with_capacity_from(
        &self,
        task_id: &TaskId,
        request: CommandRequest,
        policy: impl FnOnce() -> SchedulingPolicy,
    ) -> Result<CommandOutcome> {
        let backend_id = self.task(task_id)?.assignment.backend_id;
        let _global = self.store().lock_global_schedule()?;
        let _lock = self.store().lock_schedule(&backend_id)?;
        let record = self.task(task_id)?;
        let takes_slot = record.receipt(&request.idempotency_key).is_none()
            && !record.holds_capacity()
            && matches!(
                (
                    request.command,
                    command_effect(record.runner_state, request.command)
                ),
                (
                    OwnerCommand::Start | OwnerCommand::Resume,
                    CommandEffect::Transition(RunnerState::Provisioning)
                )
            );
        let mut queue_resume = false;
        if takes_slot {
            let policy = policy();
            match &policy.backend {
                BackendAvailability::Unavailable(detail) => {
                    return Err(OrchestrationError::BackendUnavailable {
                        backend_id,
                        detail: detail.clone(),
                    })
                }
                BackendAvailability::Available => {
                    if let Some(limit) = policy.max_concurrent_tasks {
                        queue_resume = self.total_capacity_in_use()? >= limit;
                    }
                }
            }
        }
        self.command_routed(task_id, request, queue_resume)
    }

    /// Moves a queued task within its backend's queue.
    ///
    /// The moved task gets a key between those of its new neighbours; when
    /// no integer is free there, a few neighbours get new keys first (see
    /// [`position::plan_move`]).
    ///
    /// Only a `Queued` task can be moved — admission order never touches a
    /// task that holds compute. An archived anchor is refused: it has no
    /// place in the queue. A move that leaves the order as it is (`Bottom`
    /// for the last task, say) writes and announces nothing. A move that
    /// finds no free key anywhere fails with
    /// [`OrchestrationError::NoRoomToMove`] and writes nothing.
    /// `expected_generation` makes the move conditional, like an owner
    /// command; the desired generation itself does not move, because nothing
    /// a controller acts on changed. Returns the record and whether the
    /// order changed; the caller re-evaluates admission.
    pub fn move_task(
        &self,
        task_id: &TaskId,
        target: &MoveTarget,
        expected_generation: Option<u64>,
    ) -> Result<(TaskRecord, bool)> {
        let backend_id = self.task(task_id)?.assignment.backend_id;
        let _lock = self.store().lock_schedule(&backend_id)?;
        let all = self.backend_tasks(&backend_id)?;
        let moved = all
            .iter()
            .find(|task| task.task_id == *task_id)
            .ok_or_else(|| OrchestrationError::TaskNotFound(task_id.clone()))?;
        check_movable(moved, expected_generation)?;
        if let MoveTarget::Before(anchor) | MoveTarget::After(anchor) = target {
            if all
                .iter()
                .any(|task| task.task_id == *anchor && !in_queue(task))
            {
                return Err(OrchestrationError::InvalidMoveAnchor {
                    task_id: task_id.clone(),
                    anchor: anchor.clone(),
                    reason: "the anchor is archived and has no place in the queue",
                });
            }
        }
        let tasks: Vec<&TaskRecord> = all.iter().filter(|task| in_queue(task)).collect();

        let before: Vec<&TaskId> = tasks.iter().map(|task| &task.task_id).collect();
        let after = reordered(&before, task_id, target)?;
        if after == before {
            return Ok((moved.clone(), false));
        }
        let slot = after
            .iter()
            .position(|id| *id == task_id)
            .expect("the moved task is in the new order");
        let keys: Vec<(TaskId, u64)> = tasks
            .iter()
            .map(|task| (task.task_id.clone(), task.position_key))
            .collect();
        let upper = position::upper_bound(
            position::key_at(self.now()),
            keys.last().map(|(_, key)| *key),
        );
        let mut writes = position::plan_move(&keys, task_id, slot, 0, upper).map_err(|_| {
            OrchestrationError::NoRoomToMove {
                task_id: task_id.clone(),
            }
        })?;
        let (_, key) = writes
            .pop()
            .expect("a plan ends with the moved task's write");
        let rank = u32::try_from(slot + 1).unwrap_or(u32::MAX);
        // The event goes to the mover's own stream, so it names only the
        // mover's tasks: the ids of another owner's tasks on a shared
        // backend are not theirs to learn. The position is still that in
        // the whole queue.
        let order: Vec<TaskId> = after
            .iter()
            .filter(|id| {
                tasks
                    .iter()
                    .any(|task| &task.task_id == **id && task.owner == moved.owner)
            })
            .map(|id| (*id).clone())
            .collect();

        // The neighbours first, the moved task last: its `Moved` event is
        // the one commit point. Each neighbour write keeps the order, so a
        // crash or a refusal before the commit leaves the queue as it was,
        // with some neighbours spaced further apart.
        for (neighbour, key) in &writes {
            self.set_position_key(neighbour, *key)?;
        }
        let (record, ()) = self.mutate(task_id, |record, _now| {
            check_movable(record, expected_generation)?;
            record.position_key = key;
            Ok(Mutation::Changed(
                TaskEventKind::Moved {
                    rank,
                    order: order.clone(),
                },
                (),
            ))
        })?;
        Ok((record, true))
    }

    /// The owner's default-backend override, if one was ever set (R10).
    pub fn default_backend(&self) -> Result<Option<DefaultBackendSelection>> {
        self.store().load_default_backend()
    }

    /// Durably sets (`Some`) or clears (`None`) the default-backend
    /// override. Repeating the current value writes nothing.
    ///
    /// Validating the backend against the catalogue is the caller's job:
    /// the control plane does not read configuration.
    pub fn set_default_backend(
        &self,
        backend: Option<BackendId>,
    ) -> Result<DefaultBackendSelection> {
        let _lock = self.store().lock_default_backend()?;
        let current = self.store().load_default_backend()?;
        if let Some(current) = &current {
            if current.backend == backend {
                return Ok(current.clone());
            }
        }
        let selection = DefaultBackendSelection {
            record_version: DEFAULT_BACKEND_SELECTION_VERSION,
            revision: current.map_or(0, |current| current.revision) + 1,
            backend,
            updated_at: Some(self.now()),
        };
        self.store().save_default_backend(&selection)?;
        Ok(selection)
    }

    /// Writes a task's new key without an event: only a move makes room
    /// with it, and the move announces the new order.
    fn set_position_key(&self, task_id: &TaskId, key: u64) -> Result<()> {
        let _lock = self.store().lock_task(task_id)?;
        let mut record = self.store().load_task(task_id)?;
        record.position_key = key;
        self.store().save_task(&mut record)
    }

    /// The 1-based position of `record` in its backend's queue, 0 for an
    /// archived task, which has none (see [`queue_positions`]).
    pub fn queue_position(&self, record: &TaskRecord) -> Result<u32> {
        if !in_queue(record) {
            return Ok(0);
        }
        let tasks = self.backend_tasks(&record.assignment.backend_id)?;
        Ok(queue_positions(&tasks)
            .get(&record.task_id)
            .copied()
            .unwrap_or(0))
    }

    /// The backend's queue: its tasks that are not archived, in queue
    /// order.
    fn queue(&self, backend_id: &BackendId) -> Result<Vec<TaskRecord>> {
        let mut tasks = self.backend_tasks(backend_id)?;
        tasks.retain(in_queue);
        Ok(tasks)
    }

    /// Every task of `backend_id`, archived ones too, in key order.
    fn backend_tasks(&self, backend_id: &BackendId) -> Result<Vec<TaskRecord>> {
        Ok(self
            .list_tasks()?
            .into_iter()
            .filter(|task| task.assignment.backend_id == *backend_id)
            .collect())
    }

    /// Writes one verdict, re-checked under the task's lock: a task that
    /// left `Queued` meanwhile (archived, say) is left alone.
    fn apply_verdict(&self, task_id: &TaskId, verdict: Verdict) -> Result<bool> {
        let (_, changed) = self.mutate(task_id, |record, now| {
            if record.runner_state != RunnerState::Queued {
                return Ok(Mutation::Unchanged(false));
            }
            let mut granted = false;
            let mut revoked = false;
            let reason = match verdict {
                // A controller may already hold the `Start` this reservation
                // offered. Its `StartAccepted` is then refused (`NotAdmitted`),
                // and nothing is lost: the protocol has it create no resource
                // before that report is accepted.
                Verdict::Revoke(reason) => {
                    revoked = record.reservation.take().is_some();
                    reason
                }
                Verdict::Admit => {
                    if record.reservation.is_none() {
                        record.reservation = Some(CapacityReservation { granted_at: now });
                        granted = true;
                    }
                    QueueReason::Admitted
                }
                // Only this lock grants reservations, so one cannot have
                // appeared since the pass read the record; the guard keeps
                // a granted slot from ever being reported as waiting.
                Verdict::Wait(_) if record.reservation.is_some() => QueueReason::Admitted,
                Verdict::Wait(reason) => reason,
            };
            if !granted
                && !revoked
                && record.queue.as_ref().map(|status| &status.reason) == Some(&reason)
            {
                return Ok(Mutation::Unchanged(false));
            }
            record.queue = Some(QueueStatus {
                reason: reason.clone(),
                since: now,
            });
            Ok(Mutation::Changed(
                TaskEventKind::QueueChanged { reason },
                true,
            ))
        })?;
        Ok(changed)
    }
}

/// Whether `task` has a place in its backend's queue: every task but an
/// archived one.
fn in_queue(task: &TaskRecord) -> bool {
    task.runner_state != RunnerState::Archived
}

/// The 1-based position of every task in its backend's queue, counted
/// among the backend's tasks that are not archived. An archived task has
/// none. `tasks` may hold any number of backends, in any order.
pub fn queue_positions(tasks: &[TaskRecord]) -> HashMap<TaskId, u32> {
    let mut queued: Vec<&TaskRecord> = tasks.iter().filter(|task| in_queue(task)).collect();
    queued.sort_by(|left, right| {
        left.assignment
            .backend_id
            .cmp(&right.assignment.backend_id)
            .then_with(|| queue_order(left, right))
    });
    let mut positions = HashMap::with_capacity(queued.len());
    let mut counts: HashMap<&BackendId, u32> = HashMap::new();
    for task in queued {
        let count = counts.entry(&task.assignment.backend_id).or_insert(0);
        *count = count.saturating_add(1);
        positions.insert(task.task_id.clone(), *count);
    }
    positions
}

/// Whether `record` may be moved now.
fn check_movable(record: &TaskRecord, expected_generation: Option<u64>) -> Result<()> {
    if let Some(expected) = expected_generation {
        record.check_generation(expected)?;
    }
    if record.runner_state != RunnerState::Queued {
        return Err(OrchestrationError::TaskNotQueued {
            task_id: record.task_id.clone(),
            runner_state: record.runner_state,
        });
    }
    Ok(())
}

/// `order` with `task_id` taken out and put at `target`.
fn reordered<'a>(
    order: &[&'a TaskId],
    task_id: &'a TaskId,
    target: &'a MoveTarget,
) -> Result<Vec<&'a TaskId>> {
    let mut rest: Vec<&TaskId> = order.iter().copied().filter(|id| *id != task_id).collect();
    let index = match target {
        MoveTarget::Top => 0,
        MoveTarget::Bottom => rest.len(),
        MoveTarget::Before(anchor) | MoveTarget::After(anchor) => {
            let reason = if anchor == task_id {
                "a task cannot be moved relative to itself"
            } else {
                "the anchor is not a task of the same backend"
            };
            let position = rest.iter().position(|id| *id == anchor).ok_or_else(|| {
                OrchestrationError::InvalidMoveAnchor {
                    task_id: task_id.clone(),
                    anchor: anchor.clone(),
                    reason,
                }
            })?;
            position + usize::from(matches!(target, MoveTarget::After(_)))
        }
    };
    rest.insert(index, task_id);
    Ok(rest)
}

/// The outcome of [`ControlPlane::decide_limit_stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitStopDecision {
    /// The task's pause is not one a lowered limit asked for.
    NotApplicable,
    /// The safe point is accepted: the runner stops, the task goes back to
    /// the queue.
    Accepted,
    /// The pause was taken back: the runner continues.
    Withdrawn,
}

/// What the tasks of a backend hold, as the limit's overshoot reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackendLoad {
    /// Slots held by compute that exists or may exist. A reservation that
    /// nothing started on is not counted: the schedule takes it back.
    pub compute: u32,
    /// Safe points already accepted whose compute is not removed yet.
    pub stopping: u32,
    /// Tasks that wait for a pause a lowered limit asked for, safe point not
    /// yet decided.
    pub stop_requested: u32,
}

impl BackendLoad {
    fn of(tasks: &[TaskRecord]) -> Self {
        let count = |keep: &dyn Fn(&TaskRecord) -> bool| {
            u32::try_from(tasks.iter().filter(|task| keep(task)).count()).unwrap_or(u32::MAX)
        };
        Self {
            compute: count(&|task| task.holds_compute_slot()),
            stopping: count(&|task| {
                task.limit_stop_accepted && task.runner_state == RunnerState::PauseRequested
            }),
            stop_requested: count(&|task| {
                task.pause_requested_by_limit() && !task.limit_stop_accepted
            }),
        }
    }

    /// Tasks that still have to stop for `limit`: the slots held, less the
    /// limit and the stops already accepted.
    pub fn overshoot(&self, limit: u32) -> u32 {
        self.compute
            .saturating_sub(limit)
            .saturating_sub(self.stopping)
    }
}

fn slots_in_use(tasks: &[TaskRecord]) -> u32 {
    let count = tasks.iter().filter(|task| task.holds_capacity()).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}
