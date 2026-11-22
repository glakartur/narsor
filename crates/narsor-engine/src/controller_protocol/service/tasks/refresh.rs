//! Refreshing the catalogue-derived environment of an owner's task.

use super::launch_refusal;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::{EnvironmentRefreshOutcome, OwnerRef, TaskRecord};

impl ProtocolService {
    /// Re-resolves the environment of a stopped task from the current
    /// catalogue and stores it under a new generation (R2, R7). A catalogue
    /// that does not resolve refuses the refresh with its reason and leaves
    /// the task as it was (R6); an environment equal to the stored one
    /// changes nothing (R8). The event names `owner`, never a field of the
    /// request (R9).
    pub fn refresh_environment(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: RefreshEnvironmentRequest,
    ) -> ProtocolResult<RefreshEnvironmentResponse> {
        let record = self.owned_task(owner, task_id)?;
        require_refreshable(&record, request.expected_generation)?;
        let environment = self
            .launch
            .resolve_environment(&record.assignment.backend_id, &record.launch_spec)
            .map_err(launch_refusal)?;
        let old_identity = record.launch_spec.environment.identity.clone();
        let (task, outcome) = self.control.refresh_environment(
            &record.task_id,
            environment,
            owner.clone(),
            request.expected_generation,
        )?;
        let outcome = match outcome {
            EnvironmentRefreshOutcome::Refreshed(diff) => {
                self.notify(&task.task_id);
                // A pending start or resume of the old generation is stale
                // now; the queue decides again at the new one.
                self.reschedule(&task.assignment.backend_id);
                EnvironmentRefreshResult::Refreshed {
                    old_identity,
                    new_identity: task.launch_spec.environment.identity.clone(),
                    generation: task.generation,
                    diff,
                }
            }
            EnvironmentRefreshOutcome::Unchanged => EnvironmentRefreshResult::Unchanged {
                identity: task.launch_spec.environment.identity.clone(),
            },
        };
        Ok(RefreshEnvironmentResponse {
            outcome,
            task: self.owned_summary(owner, &task.task_id)?,
        })
    }
}

/// Refuses a refresh of a task that has a live workload or is finished or
/// archival, before the catalogue is read. Silent where the task has moved
/// on from `expected_generation`: the control plane refuses that with
/// `stale_generation`, the code clients know.
fn require_refreshable(
    record: &TaskRecord,
    expected_generation: Option<u64>,
) -> ProtocolResult<()> {
    if expected_generation.is_some_and(|expected| expected != record.generation) {
        return Ok(());
    }
    match record.environment_refresh_refusal() {
        Some(reason) => Err(ProtocolError::conflict("action_unavailable", reason)),
        None => Ok(()),
    }
}
