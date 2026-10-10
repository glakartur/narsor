//! The resume policy's pass (R1, R7): tasks whose workload was lost come
//! back without their owner.
//!
//! The control plane counts every loss and plans the next attempt when the
//! task enters `Interrupted` (see [`crate::orchestration::resume`]); this
//! pass only acts on what is due. It runs on the daemon's scheduler tick,
//! beside [`ProtocolService::reschedule_all`], so it behaves the same for
//! every backend and needs nothing from the controller but its presence.

use std::collections::{BTreeSet, HashMap};

use super::ProtocolService;
use crate::domain::{BackendId, TaskId};
use crate::orchestration::{OrchestrationError, TaskRecord};

/// What one pass of the resume policy did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoResumePass {
    /// Tasks resumed by this pass (a replayed receipt is not counted).
    pub resumed: Vec<TaskId>,
    /// Tasks whose exhaustion this pass announced.
    pub exhausted: Vec<TaskId>,
}

impl ProtocolService {
    /// Resumes every interrupted task whose planned attempt is due, and
    /// announces every exhaustion not announced yet.
    ///
    /// A task is resumed only while a controller holds its backend's lease:
    /// with nobody to launch the workload, the history would show a resume
    /// that never happened. Its attempt stays due and the next pass with
    /// the controller back takes it. Each resume is a `Resume` receipt of
    /// the policy ([`crate::orchestration::ControlPlane::auto_resume`]): the
    /// receipt, the used-up attempt and the `AutoResumed` event are one
    /// mutation, keyed by the interrupted generation, so two passes before
    /// the controller acts write one receipt and an owner command that came
    /// first wins silently.
    ///
    /// Failures are logged; the next pass retries.
    pub fn auto_resume_pass(&self) -> AutoResumePass {
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "resume policy: cannot list tasks");
                return AutoResumePass::default();
            }
        };
        let now = self.control.now();
        let mut pass = AutoResumePass::default();
        let mut online: HashMap<BackendId, bool> = HashMap::new();
        let mut resumed_on = BTreeSet::new();
        for task in &tasks {
            if self.announce_exhaustion(task) {
                pass.exhausted.push(task.task_id.clone());
            }
            if !task.auto_resume_due(now) {
                continue;
            }
            let backend_id = &task.assignment.backend_id;
            let controller_online = *online
                .entry(backend_id.clone())
                .or_insert_with(|| self.controller_online(backend_id));
            if controller_online && self.auto_resume(task) {
                pass.resumed.push(task.task_id.clone());
                resumed_on.insert(backend_id.clone());
            }
        }
        for backend_id in resumed_on {
            self.reschedule(&backend_id);
        }
        pass
    }

    /// Resumes one due task; true when this call did.
    fn auto_resume(&self, task: &TaskRecord) -> bool {
        match self.control.auto_resume(&task.task_id, task.generation) {
            Ok(outcome) if outcome.replayed => false,
            Ok(_) => {
                tracing::info!(task = %task.task_id, "resume policy: resumed after workload loss");
                self.notify(&task.task_id);
                true
            }
            // The owner (or another pass) acted first; nothing to do.
            Err(
                error @ (OrchestrationError::StaleGeneration { .. }
                | OrchestrationError::IllegalCommand { .. }),
            ) => {
                tracing::debug!(task = %task.task_id, %error, "resume policy: overtaken");
                false
            }
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "resume policy: resume failed");
                false
            }
        }
    }

    /// Announces the task's exhaustion if it has one not announced yet;
    /// true when this call did.
    fn announce_exhaustion(&self, task: &TaskRecord) -> bool {
        let unannounced = task
            .resume
            .exhausted
            .as_ref()
            .is_some_and(|exhaustion| !exhaustion.announced);
        if !unannounced {
            return false;
        }
        match self.control.announce_exhaustion(&task.task_id) {
            Ok(Some(_)) => {
                tracing::info!(task = %task.task_id, "resume policy: gave up; waiting for the owner");
                self.notify(&task.task_id);
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "resume policy: cannot announce exhaustion");
                false
            }
        }
    }

    /// Whether a controller holds the backend's lease; an unreadable lease
    /// counts as offline.
    fn controller_online(&self, backend_id: &BackendId) -> bool {
        self.control
            .controller_online(backend_id)
            .unwrap_or_else(|error| {
                tracing::warn!(backend = %backend_id, %error, "resume policy: cannot read the lease");
                false
            })
    }
}
