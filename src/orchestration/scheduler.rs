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
//! back — not by a task moved ahead of it, a lowered limit or a controller
//! going away — and nothing here ever stops a task that holds compute
//! (R11).
//!
//! # What holds a slot
//!
//! See [`TaskRecord::holds_capacity`]: everything that has or may have
//! compute, conservatively including `Interrupted`, `Unknown` and
//! `Archiving`; a `Paused` task released its compute and its slot.
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
    BackendCapacity, CapacityReservation, CommandOutcome, CommandRequest, DefaultBackendSelection,
    MoveTarget, OwnerCommand, QueueReason, QueueStatus, TaskEventKind, TaskLifecycle, TaskRecord,
    DEFAULT_BACKEND_SELECTION_VERSION,
};
use super::{control_plane::Mutation, ControlPlane};
use crate::domain::{BackendId, TaskId};

/// What one scheduling pass decided for one queued task.
enum Verdict {
    /// Grant a reservation (or keep the one it holds).
    Admit,
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
        let _lock = self.store().lock_schedule(backend_id)?;
        let tasks = self.backend_tasks(backend_id)?;
        let online = self.controller_online(backend_id)?;
        let mut in_use = slots_in_use(&tasks);

        let mut waiting: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|task| task.lifecycle == TaskLifecycle::Queued && task.reservation.is_none())
            .collect();
        waiting.sort_by_key(|task| (task.rank, task.queue_sequence));

        let mut verdicts: Vec<(TaskId, Verdict)> = tasks
            .iter()
            .filter(|task| task.lifecycle == TaskLifecycle::Queued && task.reservation.is_some())
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
            verdicts.push((task.task_id.clone(), Verdict::Wait(reason)));
        }

        let mut changed = Vec::new();
        for (task_id, verdict) in verdicts {
            if self.apply_verdict(&task_id, verdict)? {
                changed.push(task_id);
            }
        }
        Ok(changed)
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

    /// [`ControlPlane::command`], refusing a command that would take a
    /// capacity slot the backend does not have free.
    ///
    /// Only resuming a `Paused` task needs one — every other command keeps
    /// or frees the task's slot — and it is decided under the backend's
    /// schedule lock, like an admission. A full backend refuses it with
    /// [`OrchestrationError::BackendAtCapacity`] rather than stopping
    /// anything (R11). A retry of an accepted command is answered from its
    /// receipt as always, whatever the capacity now.
    pub fn command_with_capacity(
        &self,
        task_id: &TaskId,
        request: CommandRequest,
        capacity: &BackendCapacity,
    ) -> Result<CommandOutcome> {
        let backend_id = self.task(task_id)?.assignment.backend_id;
        let _lock = self.store().lock_schedule(&backend_id)?;
        let record = self.task(task_id)?;
        let takes_slot = record.receipt(&request.idempotency_key).is_none()
            && !record.holds_capacity()
            && matches!(
                (
                    request.command,
                    command_effect(record.lifecycle, request.command)
                ),
                (
                    OwnerCommand::Start | OwnerCommand::Resume,
                    CommandEffect::Transition(TaskLifecycle::Provisioning)
                )
            );
        if takes_slot {
            match capacity {
                BackendCapacity::Unavailable(detail) => {
                    return Err(OrchestrationError::BackendUnavailable {
                        backend_id,
                        detail: detail.clone(),
                    })
                }
                BackendCapacity::Limit(limit) => {
                    let running = self.capacity_in_use(&backend_id)?;
                    if running >= *limit {
                        return Err(OrchestrationError::BackendAtCapacity {
                            backend_id,
                            running,
                            limit: *limit,
                        });
                    }
                }
            }
        }
        self.command(task_id, request)
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
        let tasks = self.backend_tasks(&backend_id)?;
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
        let order: Vec<TaskId> = after.iter().map(|id| (*id).clone()).collect();

        // The moved task first: it is the one that can still be refused
        // (a command raced in), and a crash after it leaves only equal
        // ranks, which queue order breaks by creation.
        let rank = rank_of(task_id);
        let (record, _) = self.mutate(task_id, |record, _now| {
            check_movable(record, expected_generation)?;
            record.rank = rank;
            Ok(Mutation::Changed(
                TaskEventKind::Moved {
                    rank,
                    order: order.clone(),
                },
                (),
            ))
        })?;
        for task in tasks.iter().filter(|task| task.task_id != *task_id) {
            let rank = rank_of(&task.task_id);
            if task.rank != rank {
                self.set_rank(&task.task_id, rank)?;
            }
        }
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

    /// Writes a task's new rank without an event: the move that caused it
    /// announced the whole order already.
    fn set_rank(&self, task_id: &TaskId, rank: u32) -> Result<()> {
        let _lock = self.store().lock_task(task_id)?;
        let mut record = self.store().load_task(task_id)?;
        record.rank = rank;
        self.store().save_task(&record)
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
            if record.lifecycle != TaskLifecycle::Queued {
                return Ok(Mutation::Unchanged(false));
            }
            let mut granted = false;
            let reason = match verdict {
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
            if !granted && record.queue.as_ref().map(|status| &status.reason) == Some(&reason) {
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
    if record.lifecycle != TaskLifecycle::Queued {
        return Err(OrchestrationError::TaskNotQueued {
            task_id: record.task_id.clone(),
            lifecycle: record.lifecycle,
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

fn slots_in_use(tasks: &[TaskRecord]) -> u32 {
    let count = tasks.iter().filter(|task| task.holds_capacity()).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}
