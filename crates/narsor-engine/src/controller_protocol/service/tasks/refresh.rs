//! Refreshing the catalogue-derived environment of an owner's task.

use super::launch_refusal;
use crate::controller_protocol::error::ProtocolResult;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::OwnerRef;

impl ProtocolService {
    /// Re-resolves the environment of a stopped task from the current
    /// catalogue and stores it under a new generation (R2, R7). A catalogue
    /// that does not resolve refuses the refresh with its reason and leaves
    /// the task as it was (R6); an environment equal to the stored one
    /// changes nothing (R8). The event names `owner`, never a field of the
    /// request (R9).
    ///
    /// The control plane's guard runs once before the catalogue is read, so
    /// a task that cannot be refreshed is refused without resolving
    /// anything, and again under the task lock.
    pub fn refresh_environment(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: RefreshEnvironmentRequest,
    ) -> ProtocolResult<RefreshEnvironmentResponse> {
        let record = self.owned_task(owner, task_id)?;
        record.check_environment_refresh(request.expected_generation)?;
        let environment = self
            .launch
            .resolve_environment(&record.assignment.backend_id, &record.launch_spec)
            .map_err(launch_refusal)?;
        let (task, outcome) = self.control.refresh_environment(
            &record.task_id,
            environment,
            owner.clone(),
            request.expected_generation,
        )?;
        let task = match outcome {
            EnvironmentRefreshResult::Refreshed { .. } => {
                self.notify(&task.task_id);
                // A pending start or resume of the old generation is stale
                // now; the queue decides again at the new one.
                self.reschedule(&task.assignment.backend_id);
                self.owned_summary(owner, &task.task_id)?
            }
            _ => self.summarize(&task)?,
        };
        Ok(RefreshEnvironmentResponse { outcome, task })
    }
}
