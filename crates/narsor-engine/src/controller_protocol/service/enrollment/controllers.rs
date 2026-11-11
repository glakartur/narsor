//! Enrolled controllers: listing, revocation, key rotation and presence.

use chrono::{DateTime, Utc};

use crate::controller_protocol::credentials::{ControllerIdentity, ControllerRecord};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::lease::lease_view;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::BackendId;

impl ProtocolService {
    /// Every controller identity.
    pub fn controllers(&self) -> ProtocolResult<ControllerListResponse> {
        let controllers = self
            .credentials
            .controllers()?
            .iter()
            .map(|record| self.controller_view(record))
            .collect::<ProtocolResult<_>>()?;
        Ok(ControllerListResponse { controllers })
    }

    /// Revokes a controller identity and fences its lease (R37).
    pub fn revoke_controller(&self, controller_id: &str) -> ProtocolResult<ControllerView> {
        let record = self
            .credentials
            .revoke_controller(controller_id)?
            .ok_or(ProtocolError::NotFound)?;
        // Its lease expired with it: new starts are withheld until another
        // controller holds the backend, and the waiting tasks say so.
        self.reschedule(&record.backend_id);
        self.controller_view(&record)
    }

    /// Replaces the calling controller's key with the request's new one,
    /// which must sign the rotation itself (proof of possession).
    pub fn rotate_controller_key(
        &self,
        identity: &ControllerIdentity,
        request: RotateKeyRequest,
    ) -> ProtocolResult<RotateKeyResponse> {
        let message =
            crate::auth::pop::rotation_message(&identity.controller_id, &request.public_key);
        if !crate::auth::pop::verify(&request.public_key, &message, &request.proof) {
            return Err(ProtocolError::bad_request(
                "invalid_key_proof",
                "the new key must sign the rotation of this identity",
            ));
        }
        let record = self
            .credentials
            .rotate_controller_key(&identity.controller_id, &request.public_key)?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(RotateKeyResponse {
            controller_id: record.controller_id,
            rotated_at: record.key_rotated_at.unwrap_or_else(|| self.control.now()),
        })
    }

    /// Records a heartbeat and answers with the server's view.
    pub fn heartbeat(&self, identity: &ControllerIdentity) -> ProtocolResult<HeartbeatResponse> {
        self.credentials
            .record_heartbeat(&identity.controller_id)?
            .ok_or(ProtocolError::Unauthorized)?;
        let lease = self
            .control
            .held_lease(&identity.backend_id, &identity.controller_id)?;
        Ok(HeartbeatResponse {
            controller_id: identity.controller_id.clone(),
            backend_id: identity.backend_id.clone(),
            server_time: self.control.now(),
            lease: lease.map(lease_view),
        })
    }

    pub(crate) fn controller_presence(
        &self,
        backend_id: &BackendId,
        now: DateTime<Utc>,
    ) -> ProtocolResult<Option<ControllerPresence>> {
        let Some(lease) = self.control.lease(backend_id)? else {
            return Ok(None);
        };
        let record = self.credentials.controller(&lease.controller_id)?;
        let last_heartbeat_at = record.as_ref().and_then(|record| record.last_heartbeat_at);
        let live = now < lease.expires_at
            && record
                .as_ref()
                .is_some_and(|record| record.revoked_at.is_none())
            && last_heartbeat_at
                .is_some_and(|heartbeat| now - heartbeat <= self.config.controller_stale_after);
        Ok(Some(ControllerPresence {
            controller_id: lease.controller_id,
            lease_epoch: lease.epoch,
            lease_expires_at: lease.expires_at,
            last_heartbeat_at,
            freshness: if live {
                Freshness::Live
            } else {
                Freshness::Stale
            },
        }))
    }

    fn controller_view(&self, record: &ControllerRecord) -> ProtocolResult<ControllerView> {
        let holds_lease = self
            .control
            .held_lease(&record.backend_id, &record.controller_id)?
            .is_some();
        Ok(ControllerView {
            controller_id: record.controller_id.clone(),
            backend_id: record.backend_id.to_string(),
            name: record.name.clone(),
            enrolled_at: record.enrolled_at,
            revoked_at: record.revoked_at,
            last_heartbeat_at: record.last_heartbeat_at,
            holds_lease: holds_lease && record.revoked_at.is_none(),
        })
    }
}
