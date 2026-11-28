//! The resume of tasks that waited for their harness's usage limit: a task
//! whose step stopped on an exhausted limit comes back at the planned time,
//! without its owner.
//!
//! The control plane plans the time when a runner snapshot first reports
//! the wait (see [`crate::orchestration::ControlPlane::schedule_usage_wait`])
//! and keeps it in the task record, so a restart of the daemon neither
//! loses nor moves it. The slot-release policy frees the waiting task's
//! slot at once; this pass only acts on what is due. It runs on the
//! daemon's scheduler tick, beside the resume policy's pass.

use std::collections::{BTreeSet, HashMap};

use crate::controller_protocol::service::ProtocolService;
use crate::domain::{BackendId, TaskId};
use crate::orchestration::{
    usage_resume_key, CommandActor, CommandRequest, OrchestrationError, OwnerCommand, TaskRecord,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageResumePass {
    pub resumed: Vec<TaskId>,
}

impl ProtocolService {
    /// Resumes every task whose wait for the usage limit is due.
    ///
    /// A task is resumed only while a controller holds its backend's lease,
    /// like the resume policy does: otherwise the wait stays due and the
    /// next pass with the controller back takes it. Each resume is a
    /// `Resume` of the policy keyed [`usage_resume_key`] of the waiting
    /// generation and expecting it, so two passes write one receipt and an
    /// owner command that came first wins silently. The resume takes a
    /// slot like the owner's does: on a full backend the task goes back to
    /// the queue at its own place (D84).
    ///
    /// Failures are logged; the next pass retries.
    pub fn usage_resume_pass(&self) -> UsageResumePass {
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "usage wait: cannot list tasks");
                return UsageResumePass::default();
            }
        };
        let now = self.control.now();
        let mut pass = UsageResumePass::default();
        let mut online: HashMap<BackendId, bool> = HashMap::new();
        let mut resumed_on = BTreeSet::new();
        for task in tasks.iter().filter(|task| task.usage_resume_due(now)) {
            let backend_id = &task.assignment.backend_id;
            let controller_online = *online
                .entry(backend_id.clone())
                .or_insert_with(|| self.controller_online(backend_id));
            if controller_online && self.usage_resume(task) {
                pass.resumed.push(task.task_id.clone());
                resumed_on.insert(backend_id.clone());
            }
        }
        for backend_id in resumed_on {
            self.reschedule(&backend_id);
        }
        pass
    }

    fn usage_resume(&self, task: &TaskRecord) -> bool {
        let backend_id = &task.assignment.backend_id;
        let request = CommandRequest::new(OwnerCommand::Resume, usage_resume_key(task.generation))
            .expecting_generation(task.generation)
            .issued_by(CommandActor::Policy);
        match self
            .control
            .command_with_capacity_from(&task.task_id, request, || {
                self.scheduling_policy(backend_id)
            }) {
            Ok(outcome) if outcome.replayed => false,
            Ok(_) => {
                tracing::info!(task = %task.task_id, "usage wait: resumed at the planned time");
                self.notify(&task.task_id);
                true
            }
            Err(
                error @ (OrchestrationError::StaleGeneration { .. }
                | OrchestrationError::IllegalCommand { .. }),
            ) => {
                tracing::debug!(task = %task.task_id, %error, "usage wait: overtaken");
                false
            }
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "usage wait: resume failed");
                false
            }
        }
    }
}
