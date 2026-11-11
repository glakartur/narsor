//! Controller reports of what became of its runners.

use crate::controller_protocol::credentials::ControllerIdentity;
use crate::controller_protocol::error::ProtocolResult;
use crate::controller_protocol::service::lease::lease_token;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::{ControllerReport, Observation, RunnerState, TaskRecord};

impl ProtocolService {
    /// Accepts a controller's observation, fenced by its lease epoch.
    ///
    /// `PausedAtSafePoint` additionally requires the runner to have
    /// acknowledged a safe point for the current generation (KTD2): compute
    /// is removed only after that, never on the controller's word alone.
    /// [`ControlPlane::report`](crate::orchestration::ControlPlane::report) enforces it, so every writer is held to it.
    pub fn report(
        &self,
        identity: &ControllerIdentity,
        request: ReportRequest,
    ) -> ProtocolResult<ReportResponse> {
        let record = self.backend_task(identity, &request.task_id)?;
        if request.observation == Observation::StartAccepted
            && record.runner_state == RunnerState::Queued
            && record.reservation.is_none()
        {
            // A start may only be accepted with a reservation; give the
            // scheduler its chance first rather than refuse a controller
            // that merely reported before any pass ran.
            self.reschedule(&identity.backend_id);
        }
        let outcome = self.control.report(ControllerReport {
            task_id: record.task_id.clone(),
            lease: lease_token(identity, request.lease_epoch),
            desired_generation: request.desired_generation,
            observation: request.observation,
            resource: request.resource,
            summary: request.summary,
            observed_at: request.observed_at,
        })?;
        if outcome.applied {
            self.notify(&record.task_id);
            if record.holds_capacity() && !outcome.task.holds_capacity() {
                // A released global slot can admit queued work on any backend.
                self.reschedule_all();
            } else {
                self.reschedule(&identity.backend_id);
            }
            if record.pause_requested_by_policy() {
                self.resume_for_stranded_work(&outcome.task, request.observation);
            }
        }
        Ok(ReportResponse {
            applied: outcome.applied,
            runner_state: outcome.task.runner_state,
            generation: outcome.task.generation,
        })
    }

    /// An answer accepted while the slot-release policy's pause was pending
    /// (the owner asked for no pause; an owner's own pause stands) is stored on a task that now is `Stopped`: nothing
    /// else would resume it, so it is resumed here as `submit_input` would
    /// have, under a key of the work's own (a retry resumes once). A failure
    /// is logged only: the report is applied, and a retry of the work
    /// repairs the resume.
    fn resume_for_stranded_work(&self, task: &TaskRecord, observation: Observation) {
        if observation != Observation::PausedAtSafePoint
            || task.runner_state != RunnerState::Stopped
        {
            return;
        }
        let Some(work_key) = task.pending_work_key() else {
            return;
        };
        if let Err(error) = self.resume_for_work(task, work_key) {
            tracing::warn!(task = %task.task_id, %error, "resume for work stored during a pause failed");
        }
    }
}
