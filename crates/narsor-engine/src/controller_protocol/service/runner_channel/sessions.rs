//! Runner credentials and sessions.

use crate::controller_protocol::credentials::{ControllerIdentity, RunnerIdentity, RunnerRole};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::{RunnerState, TaskRecord};

impl ProtocolService {
    /// Issues the credential of a new runner incarnation, replacing the
    /// task's previous one (KTD3).
    ///
    /// Only the backend's current lease holder may, only for the task's
    /// current generation, and only while a runner may be launched
    /// (Provisioning or Running).
    pub fn issue_runner_credential(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
        request: RunnerCredentialRequest,
    ) -> ProtocolResult<RunnerCredentialResponse> {
        self.issue_session(identity, task_id, request, RunnerRole::Worker, |record| {
            if matches!(
                record.runner_state,
                RunnerState::Provisioning | RunnerState::Running
            ) {
                return Ok(());
            }
            Err(ProtocolError::conflict(
                "runner_launch_not_allowed",
                format!(
                    "task '{}' cannot launch a runner while {:?}",
                    record.task_id, record.runner_state
                ),
            ))
        })
    }

    /// Issues a session credential of `role` to the lease holder for the
    /// task's current generation, once `gate` accepts the task's record.
    pub(crate) fn issue_session(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
        request: RunnerCredentialRequest,
        role: RunnerRole,
        gate: impl FnOnce(&TaskRecord) -> ProtocolResult<()>,
    ) -> ProtocolResult<RunnerCredentialResponse> {
        let record = self.backend_task(identity, task_id)?;
        let lease = self.require_lease(identity)?;
        if lease.epoch != request.lease_epoch {
            return Err(ProtocolError::conflict(
                "stale_lease",
                format!(
                    "lease epoch {} is not the current epoch {}",
                    request.lease_epoch, lease.epoch
                ),
            ));
        }
        if request.generation != record.generation {
            return Err(ProtocolError::conflict(
                "stale_generation",
                format!(
                    "task '{}' is at generation {}, not {}",
                    record.task_id, record.generation, request.generation
                ),
            ));
        }
        gate(&record)?;
        let incarnation = self.checked_incarnation(&request.incarnation)?;
        let (session, credential) = self.credentials.issue_runner_session(
            role,
            &record.task_id,
            incarnation,
            record.generation,
            lease.epoch,
            self.config.runner_credential_ttl,
        )?;
        Ok(RunnerCredentialResponse {
            task_id: session.task_id,
            generation: session.launch_generation,
            incarnation: session.incarnation,
            credential: credential.expose().to_string(),
            expires_at: session.expires_at,
        })
    }

    /// The incarnation a controller names for a credential, trimmed, or a
    /// `400` when it is not one.
    fn checked_incarnation<'a>(&self, incarnation: &'a str) -> ProtocolResult<&'a str> {
        let incarnation = incarnation.trim();
        if incarnation.is_empty() || incarnation.len() > MAX_INCARNATION_LEN {
            return Err(ProtocolError::bad_request(
                "invalid_incarnation",
                "an incarnation is a non-empty identifier of at most 256 characters",
            ));
        }
        Ok(incarnation)
    }

    /// Refuses a call that writes the task's work state when the caller is
    /// a read-only runner (D63): its credential reads and nothing else.
    pub(crate) fn require_worker(identity: &RunnerIdentity) -> ProtocolResult<()> {
        match identity.role {
            RunnerRole::Worker => Ok(()),
            RunnerRole::Reader => Err(ProtocolError::Forbidden),
        }
    }

    /// Extends the runner's credential.
    pub fn runner_renew(&self, identity: &RunnerIdentity) -> ProtocolResult<RunnerRenewResponse> {
        let session = self
            .credentials
            .touch_runner(identity, Some(self.config.runner_credential_ttl))?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(RunnerRenewResponse {
            expires_at: session.expires_at,
        })
    }
}

/// Longest incarnation identifier accepted (a Pod UID is 36 characters).
const MAX_INCARNATION_LEN: usize = 256;
