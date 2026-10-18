//! The slot-release policy's triggers (D62, R1-R7, R17): a task whose work
//! ended, or waits for its owner, gives its slot back to the queue.
//!
//! The control plane records a deadline on the task when a runner snapshot
//! shows such a state (see
//! [`crate::orchestration::ControlPlane::record_runner_snapshot_with`]); this
//! module only acts on it. A final execution state (completed, cancelled) is
//! due at once, so the snapshot's own request frees the slot; a transient
//! one (input needed, failed) is due after the backend's wait, which the
//! periodic pass finds, also after a restart of the daemon. A read of the
//! task's details moves the deadline later.
//!
//! Freeing a slot is the owner's pause, issued by the policy
//! ([`crate::orchestration::ControlPlane::release_slot`]): the runner stops
//! at a safe point, the controller removes the compute, and the report of
//! that safe point already re-runs admission for the backend.

use std::collections::HashMap;
use std::time::Duration as StdDuration;

use chrono::Duration;

use super::ProtocolService;
use crate::domain::{BackendId, TaskId};
use crate::environment::SlotReleaseTimings;
use crate::orchestration::{OrchestrationError, RunnerState, TaskRecord};

/// What one pass of the slot-release policy did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotReleasePass {
    /// Tasks whose slot this pass freed (a replayed receipt is not counted).
    pub released: Vec<TaskId>,
    /// Tasks whose missing deadline this pass planned.
    pub scheduled: Vec<TaskId>,
}

/// Converts a configured period; one too large for the calendar counts as
/// the largest one.
pub(super) fn chrono_duration(period: StdDuration) -> Duration {
    Duration::from_std(period).unwrap_or(Duration::MAX)
}

impl ProtocolService {
    /// Frees the slot of every running task whose deadline came, and plans
    /// the deadline of one that has none yet.
    ///
    /// The second part is the self-repair: a runner that reported a
    /// final or transient state while still provisioning, or a record from
    /// before the policy existed, has no deadline, and nothing else would
    /// ever set it. Periods are read from each backend as configured now.
    /// Failures are logged; the next pass retries.
    pub fn slot_release_pass(&self) -> SlotReleasePass {
        if !self.config.slot_release {
            return SlotReleasePass::default();
        }
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "slot release: cannot list tasks");
                return SlotReleasePass::default();
            }
        };
        let mut pass = SlotReleasePass::default();
        let mut timings: HashMap<BackendId, SlotReleaseTimings> = HashMap::new();
        for task in tasks {
            if task.runner_state != RunnerState::Running || task.slot_release_class().is_none() {
                continue;
            }
            let mut task = task;
            if task.slot_release_due_at.is_none() {
                let grace = timings
                    .entry(task.assignment.backend_id.clone())
                    .or_insert_with(|| self.slot_release_timings(&task.assignment.backend_id))
                    .grace;
                match self
                    .control
                    .arm_slot_release(&task.task_id, chrono_duration(grace))
                {
                    Ok(Some(armed)) => {
                        pass.scheduled.push(task.task_id.clone());
                        self.notify(&task.task_id);
                        task = armed;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(task = %task.task_id, %error, "slot release: cannot plan the deadline");
                        continue;
                    }
                }
            }
            if self.release_slot_if_due(&task) {
                pass.released.push(task.task_id.clone());
            }
        }
        pass
    }

    /// Frees the task's slot if its deadline came; true when this call did.
    ///
    /// The trigger after a runner snapshot and the periodic pass both call
    /// it, so a task is released by whichever sees it first, and the
    /// command's key (one per run) makes the other a replay.
    pub(super) fn release_slot_if_due(&self, task: &TaskRecord) -> bool {
        if !self.config.slot_release || !task.slot_release_due(self.control.now()) {
            return false;
        }
        match self.control.release_slot(&task.task_id, task.generation) {
            Ok(outcome) if outcome.replayed => false,
            Ok(_) => {
                tracing::info!(task = %task.task_id, "slot release: asked the runner to stop");
                self.notify(&task.task_id);
                true
            }
            // The owner (or the other trigger) acted first: a pause already
            // waits for its safe point, or the task moved on.
            Err(
                error @ (OrchestrationError::StaleGeneration { .. }
                | OrchestrationError::IllegalCommand { .. }),
            ) => {
                tracing::debug!(task = %task.task_id, %error, "slot release: overtaken");
                false
            }
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "slot release: could not stop the task");
                false
            }
        }
    }

    /// A read of the task's details: moves a transient task's deadline later
    /// (R7). Writes only when the deadline moves by more than the threshold,
    /// so a client that refreshes often does not write the record each time.
    pub(super) fn note_details_read(&self, task: &TaskRecord) {
        let Some(due_at) = task.slot_release_waiting() else {
            return;
        };
        let extension = chrono_duration(
            self.slot_release_timings(&task.assignment.backend_id)
                .read_extension,
        );
        // The cheap test on the record just read; the mutation tests again
        // under the task's lock.
        if self.control.now() + extension - due_at
            <= crate::orchestration::slot_release_extension_threshold()
        {
            return;
        }
        match self.control.extend_slot_release(&task.task_id, extension) {
            Ok(true) => self.notify(&task.task_id),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "slot release: cannot extend the wait");
            }
        }
    }

    /// The backend's slot-release periods as configured now; the defaults
    /// when the backend cannot be read (the next call reads it again).
    pub(super) fn slot_release_timings(&self, backend_id: &BackendId) -> SlotReleaseTimings {
        self.launch
            .slot_release_timings(backend_id)
            .unwrap_or_else(|error| {
                tracing::debug!(backend = %backend_id, %error, "slot release: using default periods");
                SlotReleaseTimings::default()
            })
    }
}
