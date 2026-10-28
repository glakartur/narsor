//! Admission of queued tasks by backend capacity (U10: R3, R6, R10, R11).
//!
//! The control plane — not a controller — decides which queued task may
//! start. A queued task is offered `Start` only once it holds a
//! [`CapacityReservation`], and a controller's `StartAccepted` for a task
//! without one is refused, so no controller can exceed a backend's
//! `max_concurrent_tasks` however stale or eager it is.
//!
//! # One decision per backend at a time
//!
//! [`ControlPlane::schedule`] runs under the backend's schedule lock and
//! is the only place that grants reservations; the only other way into a
//! capacity-holding state from a free one — resuming a paused task — goes
//! through [`ControlPlane::command_with_capacity`] under the same lock.
//! Every other transition either keeps a task's slot (an admitted start
//! becoming `Provisioning`, a retry of an interrupted task) or frees one,
//! so a count taken under the lock can only be too high, never too low: a
//! decision may under-admit for a moment, and the re-evaluation that the
//! release triggers corrects it, but it never over-admits.
//!
//! # Order and non-preemption
//!
//! Waiting tasks are admitted by `rank`, lowest first, and by creation
//! order (`queue_sequence`) among equal ranks, which only a crash in the
//! middle of a renumbering can leave. The owner reorders them with
//! [`ControlPlane::move_task`]. A reservation, once granted, is never taken
//! back — not by a task moved ahead of it or a controller going away —
//! and nothing here ever stops a task that holds compute (R11).
//!
//! Two exceptions come with a limit that goes below the slots in use. A
//! reservation over the limit is taken back at once: nothing started on it,
//! so it costs nothing. A task that runs is asked to stop, and only at a safe
//! point: [`ControlPlane::decide_limit_stop`] accepts exactly as many safe
//! points as the limit is exceeded by, the first ones that arrive, and
//! answers "continue" to the others. A stopped task goes back to the queue
//! on its own rank.
//!
//! # What holds a slot
//!
//! See [`TaskRecord::holds_capacity`]: everything that has or may have
//! compute, conservatively including `Interrupted`, `Unknown` and
//! `Archiving`; a `Stopped` task released its compute and its slot.
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
    BackendCapacity, CapacityReservation, CommandActor, CommandOutcome, CommandRequest,
    DefaultBackendSelection, MoveTarget, OwnerCommand, QueueReason, QueueStatus, RunnerState,
    SlotReleaseReason, TaskEventKind, TaskRecord, DEFAULT_BACKEND_SELECTION_VERSION,
};
use super::resume::limit_stop_key;
use super::{control_plane::Mutation, ControlPlane};
use crate::domain::{BackendId, TaskId};

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
    /// `capacity` is the backend's limit as configuration states it *now*;
    /// the caller reads it fresh for every pass. Idempotent: a pass over an
    /// unchanged backend writes nothing.
    pub fn schedule(
        &self,
        backend_id: &BackendId,
        capacity: &BackendCapacity,
    ) -> Result<Vec<TaskId>> {
        self.schedule_with(backend_id, || capacity.clone())
    }

    /// [`Self::schedule`] reading the limit *after* the schedule lock is
    /// taken, so a decision never uses a limit that a change made while it
    /// waited for the lock has already replaced (R15).
    pub fn schedule_with(
        &self,
        backend_id: &BackendId,
        capacity: impl FnOnce() -> BackendCapacity,
    ) -> Result<Vec<TaskId>> {
        let _lock = self.store().lock_schedule(backend_id)?;
        let capacity = &capacity();
        let tasks = self.dense_backend_tasks(backend_id)?;
        let online = self.controller_online(backend_id)?;
        let mut in_use = slots_in_use(&tasks);

        // Reservations over a lowered limit are taken back, those of the
        // lowest priority first. A task that holds compute is never one.
        let mut revoked: Vec<&TaskId> = Vec::new();
        if let BackendCapacity::Limit(limit) = capacity {
            let mut reserved: Vec<&TaskRecord> = tasks
                .iter()
                .filter(|task| {
                    task.runner_state == RunnerState::Queued && task.reservation.is_some()
                })
                .collect();
            reserved.sort_by_key(|task| std::cmp::Reverse((task.rank, task.queue_sequence)));
            for task in reserved {
                if in_use <= *limit {
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
        waiting.sort_by_key(|task| (task.rank, task.queue_sequence));

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
            match capacity {
                BackendCapacity::Limit(limit) if online && in_use < *limit => {
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
            let reason = match capacity {
                BackendCapacity::Unavailable(detail) => QueueReason::BackendUnknown {
                    detail: detail.clone(),
                },
                BackendCapacity::Limit(_) if !online => QueueReason::ControllerOffline,
                BackendCapacity::Limit(limit) => QueueReason::WaitingForCapacity {
                    position,
                    running: in_use,
                    limit: *limit,
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

    /// What the backend's tasks hold right now, for the limit's overshoot.
    pub fn backend_load(&self, backend_id: &BackendId) -> Result<BackendLoad> {
        Ok(BackendLoad::of(&self.backend_tasks(backend_id)?))
    }

    /// Asks the task running at `generation` to stop at a safe point,
    /// because the backend's limit is exceeded: a `Pause` of
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
    /// Under the backend's schedule lock, with the limit read after it is
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
        capacity: impl FnOnce() -> BackendCapacity,
    ) -> Result<LimitStopDecision> {
        let backend_id = self.task(task_id)?.assignment.backend_id;
        let _lock = self.store().lock_schedule(&backend_id)?;
        let capacity = capacity();
        let load = BackendLoad::of(&self.backend_tasks(&backend_id)?);
        let (_, decision) = self.mutate(task_id, |record, _now| {
            if !record.pause_requested_by_limit() {
                return Ok(Mutation::Unchanged(LimitStopDecision::NotApplicable));
            }
            if record.limit_stop_accepted {
                return Ok(Mutation::Unchanged(LimitStopDecision::Accepted));
            }
            let overshoot = match &capacity {
                BackendCapacity::Limit(limit) => load.overshoot(*limit),
                BackendCapacity::Unavailable(_) => 0,
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

    /// Slots of `backend_id` in use right now (see
    /// [`TaskRecord::holds_capacity`]).
    pub fn capacity_in_use(&self, backend_id: &BackendId) -> Result<u32> {
        Ok(slots_in_use(&self.backend_tasks(backend_id)?))
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
    /// nothing, at the front of the queue (like [`MoveTarget::Top`]), and
    /// the next [`ControlPlane::schedule`] pass gives it the first slot
    /// released (R20). A second such resume goes in front of the first.
    /// A retry of an accepted command is answered from its receipt as
    /// always, whatever the capacity now.
    pub fn command_with_capacity(
        &self,
        task_id: &TaskId,
        request: CommandRequest,
        capacity: &BackendCapacity,
    ) -> Result<CommandOutcome> {
        self.command_with_capacity_from(task_id, request, || capacity.clone())
    }

    /// [`Self::command_with_capacity`] reading the limit after the schedule
    /// lock is taken, like [`Self::schedule_with`].
    pub fn command_with_capacity_from(
        &self,
        task_id: &TaskId,
        request: CommandRequest,
        capacity: impl FnOnce() -> BackendCapacity,
    ) -> Result<CommandOutcome> {
        let backend_id = self.task(task_id)?.assignment.backend_id;
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
            match &capacity() {
                BackendCapacity::Unavailable(detail) => {
                    return Err(OrchestrationError::BackendUnavailable {
                        backend_id,
                        detail: detail.clone(),
                    })
                }
                BackendCapacity::Limit(limit) => {
                    queue_resume = self.capacity_in_use(&backend_id)? >= *limit;
                }
            }
        }
        let outcome = self.command_routed(task_id, request, queue_resume)?;
        if queue_resume && !outcome.replayed {
            self.put_first_in_queue(&backend_id, task_id)?;
            return Ok(CommandOutcome {
                task: self.task(task_id)?,
                ..outcome
            });
        }
        Ok(outcome)
    }

    /// Gives `task_id` (just sent to the queue at rank 0) rank 1 and
    /// renumbers the others behind it in their queue order. Under the
    /// backend's schedule lock. The task's own rank is written last, so a
    /// crash in between leaves at worst equal ranks, which the next pass
    /// renumbers.
    fn put_first_in_queue(&self, backend_id: &BackendId, task_id: &TaskId) -> Result<()> {
        let mut others = self.backend_tasks(backend_id)?;
        others.retain(|task| task.task_id != *task_id);
        others.sort_by_key(|task| (task.rank, task.queue_sequence));
        for (index, task) in others.iter().enumerate() {
            let rank = u32::try_from(index + 2).unwrap_or(u32::MAX);
            if task.rank != rank {
                self.set_rank(&task.task_id, rank)?;
            }
        }
        self.set_rank(task_id, 1)
    }

    /// Moves a queued task within its backend's queue.
    ///
    /// The queue is the backend's tasks in `rank` order; the move takes the
    /// task out and puts it at `target`, then renumbers whoever shifted, all
    /// under the backend's schedule lock so a concurrent admission, creation
    /// or move sees either the old order or the new one. Admitted and
    /// finished tasks keep their place in the list: only the moved task and
    /// those it passed change rank, so the order they hold relative to each
    /// other never changes.
    ///
    /// Only a `Queued` task can be moved — admission order never touches a
    /// task that holds compute. A move that leaves the order as it is
    /// (`Bottom` for the last task, say) writes and announces nothing.
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
        let tasks = self.dense_backend_tasks(&backend_id)?;
        let moved = tasks
            .iter()
            .find(|task| task.task_id == *task_id)
            .ok_or_else(|| OrchestrationError::TaskNotFound(task_id.clone()))?;
        check_movable(moved, expected_generation)?;

        let before: Vec<&TaskId> = tasks.iter().map(|task| &task.task_id).collect();
        let after = reordered(&before, task_id, target)?;
        if after == before {
            return Ok((moved.clone(), false));
        }
        let rank_of = |id: &TaskId| {
            let position = after.iter().position(|candidate| *candidate == id);
            u32::try_from(position.unwrap_or(0) + 1).unwrap_or(u32::MAX)
        };
        // The event goes to the mover's own stream, so it names only the
        // mover's tasks: the ids of another owner's tasks on a shared
        // backend are not theirs to learn. Ranks are still those of the
        // whole queue.
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
        // the one commit point. A crash or failure before it announces
        // nothing and leaves at worst equal ranks, which the next pass under
        // this lock renumbers; one after it leaves the order announced.
        let neighbours: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|task| task.task_id != *task_id && task.rank != rank_of(&task.task_id))
            .collect();
        for (written, task) in neighbours.iter().enumerate() {
            if let Err(error) = self.set_rank(&task.task_id, rank_of(&task.task_id)) {
                self.restore_ranks(&neighbours[..written]);
                return Err(error);
            }
        }
        let rank = rank_of(task_id);
        let committed = self.mutate(task_id, |record, _now| {
            check_movable(record, expected_generation)?;
            record.rank = rank;
            Ok(Mutation::Changed(
                TaskEventKind::Moved {
                    rank,
                    order: order.clone(),
                },
                (),
            ))
        });
        match committed {
            Ok((record, ())) => Ok((record, true)),
            Err(error) => {
                // Refused (a command raced in): the queue stays as it was.
                self.restore_ranks(&neighbours);
                Err(error)
            }
        }
    }

    /// Puts `tasks` back to the ranks they were read with, as far as that
    /// works: the caller already has an error to report, and whatever stays
    /// unrestored is equal ranks the next pass renumbers.
    fn restore_ranks(&self, tasks: &[&TaskRecord]) {
        for task in tasks {
            if let Err(error) = self.set_rank(&task.task_id, task.rank) {
                tracing::warn!(task_id = %task.task_id, %error, "could not restore a rank");
            }
        }
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

    /// Writes a task's new rank without an event: the move that caused it
    /// announced the whole order already.
    fn set_rank(&self, task_id: &TaskId, rank: u32) -> Result<()> {
        let _lock = self.store().lock_task(task_id)?;
        let mut record = self.store().load_task(task_id)?;
        record.rank = rank;
        self.store().save_task(&mut record)
    }

    /// Renumbers equal ranks of every backend (see
    /// [`Self::dense_backend_tasks`]), each under its own schedule lock.
    pub(crate) fn normalise_ranks(&self) -> Result<()> {
        let mut backends: Vec<BackendId> = Vec::new();
        for task in self.list_tasks()? {
            if !backends.contains(&task.assignment.backend_id) {
                backends.push(task.assignment.backend_id);
            }
        }
        for backend_id in backends {
            let _lock = self.store().lock_schedule(&backend_id)?;
            self.dense_backend_tasks(&backend_id)?;
        }
        Ok(())
    }

    /// [`Self::backend_tasks`] with equal ranks renumbered to 1, 2, 3, ...
    /// in queue order (creation order among the equal ones).
    ///
    /// Equal ranks are what a renumbering interrupted by a crash leaves.
    /// Call under the backend's schedule lock. Renumbering announces
    /// nothing, like the moves it repairs; the clients' next read sees it.
    pub(crate) fn dense_backend_tasks(&self, backend_id: &BackendId) -> Result<Vec<TaskRecord>> {
        let mut tasks = self.backend_tasks(backend_id)?;
        let has_equal_ranks = tasks.windows(2).any(|pair| pair[0].rank == pair[1].rank);
        if has_equal_ranks {
            for (index, task) in tasks.iter_mut().enumerate() {
                let rank = u32::try_from(index + 1).unwrap_or(u32::MAX);
                if task.rank != rank {
                    self.set_rank(&task.task_id, rank)?;
                    task.rank = rank;
                }
            }
        }
        Ok(tasks)
    }

    /// Every task of `backend_id`, in queue order.
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
