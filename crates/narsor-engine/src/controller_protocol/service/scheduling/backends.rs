//! The default backend and rescheduling of waiting tasks.

use crate::controller_protocol::error::ProtocolResult;
use crate::controller_protocol::service::tasks::launch_refusal;
use crate::controller_protocol::service::{parse_identifier, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::domain::BackendId;
use crate::orchestration::RunnerState;

impl ProtocolService {
    /// The default backend of new tasks (R10).
    pub fn default_backend(&self) -> ProtocolResult<DefaultBackendView> {
        let selection = self.control.default_backend()?;
        let configured = self.launch.configured_default_backend();
        let override_backend_id = selection
            .as_ref()
            .and_then(|selection| selection.backend.clone());
        Ok(DefaultBackendView {
            effective: override_backend_id
                .as_ref()
                .map_or_else(|| configured.clone(), ToString::to_string),
            configured,
            override_backend: override_backend_id.map(|backend| backend.to_string()),
            revision: selection.as_ref().map_or(0, |selection| selection.revision),
            updated_at: selection.and_then(|selection| selection.updated_at),
        })
    }

    /// Durably chooses the default backend of new tasks, or clears the
    /// choice (R10). The backend must be registered and usable now; tasks
    /// that exist keep their backend.
    pub fn set_default_backend(
        &self,
        request: SetDefaultBackendRequest,
    ) -> ProtocolResult<DefaultBackendView> {
        let backend = match request.backend_id {
            Some(backend) => {
                let backend = parse_identifier(BackendId::parse(backend))?;
                self.launch
                    .check_backend(&backend)
                    .map_err(launch_refusal)?;
                Some(backend)
            }
            None => None,
        };
        self.control.set_default_backend(backend)?;
        // Moves nothing — queued tasks keep their backend — but a client
        // watching the queue gets fresh reasons all the same.
        self.reschedule_all();
        self.default_backend()
    }

    /// Re-evaluates admission on every backend that has a queued task.
    ///
    /// The daemon calls it periodically, because some inputs change without
    /// any request: a controller's lease runs out, the operator edits a
    /// global task limit. Failures are logged; the next pass retries.
    pub fn reschedule_all(&self) {
        let backends: std::collections::BTreeSet<BackendId> = match self.control.list_tasks() {
            Ok(tasks) => tasks
                .into_iter()
                .filter(|task| task.runner_state == RunnerState::Queued)
                .map(|task| task.assignment.backend_id)
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "scheduler: cannot list tasks");
                return;
            }
        };
        for backend in backends {
            self.reschedule(&backend);
        }
    }

    /// Re-evaluates admission on `backend_id` against its current availability
    /// and the shared task budget, announcing every task that changed.
    ///
    /// Runs after the mutation that prompted it was committed, so a failure
    /// here must not fail that request: it is logged, and the next trigger
    /// (or the daemon's periodic pass) decides again.
    pub fn reschedule(&self, backend_id: &BackendId) {
        // Read under the schedule lock: a limit changed meanwhile applies.
        match self
            .control
            .schedule_with(backend_id, || self.scheduling_policy(backend_id))
        {
            Ok(changed) => {
                for task_id in changed {
                    self.notify(&task_id);
                }
            }
            Err(error) => {
                tracing::warn!(backend = %backend_id, %error, "scheduler: admission pass failed");
            }
        }
    }
}
