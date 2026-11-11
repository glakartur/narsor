//! Backend leases: acquiring, renewing and releasing them.

use chrono::Duration;

use crate::controller_protocol::credentials::ControllerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::{BackendLease, LeaseToken};

impl ProtocolService {
    /// Acquires the controller's backend lease (a new fencing epoch).
    pub fn acquire_lease(
        &self,
        identity: &ControllerIdentity,
        request: AcquireLeaseRequest,
    ) -> ProtocolResult<LeaseView> {
        let ttl = self.lease_ttl(request.ttl_secs);
        let lease =
            self.control
                .acquire_lease(&identity.backend_id, &identity.controller_id, ttl)?;
        self.credentials.record_heartbeat(&identity.controller_id)?;
        // A controller is back: the backend's capacity is available again.
        self.reschedule(&identity.backend_id);
        Ok(lease_view(lease))
    }

    /// Renews the lease at `request.epoch`.
    pub fn renew_lease(
        &self,
        identity: &ControllerIdentity,
        request: LeaseEpochRequest,
    ) -> ProtocolResult<LeaseView> {
        let ttl = self.lease_ttl(request.ttl_secs);
        let lease = self
            .control
            .renew_lease(&lease_token(identity, request.epoch), ttl)?;
        self.credentials.record_heartbeat(&identity.controller_id)?;
        // A renewal may revive a lease that had lapsed.
        self.reschedule(&identity.backend_id);
        Ok(lease_view(lease))
    }

    /// Releases the lease at `request.epoch`.
    pub fn release_lease(
        &self,
        identity: &ControllerIdentity,
        request: LeaseEpochRequest,
    ) -> ProtocolResult<LeaseView> {
        let lease = self
            .control
            .release_lease(&lease_token(identity, request.epoch))?;
        self.reschedule(&identity.backend_id);
        Ok(lease_view(lease))
    }

    pub(crate) fn require_lease(
        &self,
        identity: &ControllerIdentity,
    ) -> ProtocolResult<BackendLease> {
        self.control
            .held_lease(&identity.backend_id, &identity.controller_id)?
            .ok_or_else(|| {
                ProtocolError::conflict(
                    "lease_not_held",
                    format!(
                        "controller '{}' does not hold an unexpired lease of backend '{}'",
                        identity.controller_id, identity.backend_id
                    ),
                )
            })
    }

    fn lease_ttl(&self, requested: Option<u64>) -> Duration {
        bounded_ttl(
            requested,
            self.config.default_lease_ttl,
            self.config.max_lease_ttl,
        )
    }
}

pub(crate) fn lease_token(identity: &ControllerIdentity, epoch: u64) -> LeaseToken {
    LeaseToken {
        backend_id: identity.backend_id.clone(),
        controller_id: identity.controller_id.clone(),
        epoch,
    }
}

pub(crate) fn lease_view(lease: BackendLease) -> LeaseView {
    LeaseView {
        backend_id: lease.backend_id,
        controller_id: lease.controller_id,
        epoch: lease.epoch,
        acquired_at: lease.acquired_at,
        expires_at: lease.expires_at,
    }
}

pub(crate) fn bounded_ttl(requested: Option<u64>, default: Duration, max: Duration) -> Duration {
    match requested {
        Some(secs) => {
            let max_secs = u64::try_from(max.num_seconds()).unwrap_or(1).max(1);
            Duration::seconds(secs.clamp(1, max_secs) as i64)
        }
        None => default,
    }
}
