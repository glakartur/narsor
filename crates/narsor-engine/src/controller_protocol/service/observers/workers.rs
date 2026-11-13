//! The observer worker's credential (PO5): the owner creates and revokes
//! it; the worker presents it to claim runs and report their results, and
//! for nothing else. The server keeps its SHA-256 digest only
//! ([`crate::controller_protocol::credentials`]).

use narsor_protocol::controller::{
    CreateObserverWorkerRequest, ObserverWorkerCreated, ObserverWorkerListResponse,
    ObserverWorkerView,
};
use narsor_protocol::display::without_unsafe_chars;

use crate::controller_protocol::credentials::{ObserverWorkerIdentity, ObserverWorkerRecord};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::orchestration::OwnerRef;

/// Longest label of a worker credential, in characters.
pub const MAX_WORKER_NAME_CHARS: usize = 100;

fn worker_view(record: &ObserverWorkerRecord) -> ObserverWorkerView {
    ObserverWorkerView {
        worker_id: record.worker_id.clone(),
        name: record.name.clone(),
        created_at: record.created_at,
        revoked_at: record.revoked_at,
    }
}

impl ProtocolService {
    /// Creates a worker credential of `owner`, shown this once.
    pub fn create_observer_worker(
        &self,
        owner: &OwnerRef,
        request: CreateObserverWorkerRequest,
    ) -> ProtocolResult<ObserverWorkerCreated> {
        let name = match request.name.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(name)
                if name.chars().count() > MAX_WORKER_NAME_CHARS
                    || without_unsafe_chars(name) != name =>
            {
                return Err(ProtocolError::bad_request(
                    "invalid_worker_name",
                    format!(
                        "a worker name has at most {MAX_WORKER_NAME_CHARS} characters and no \
                         control characters"
                    ),
                ));
            }
            Some(name) => Some(name.to_string()),
        };
        let (record, credential) = self.credentials.create_observer_worker(owner, name)?;
        tracing::info!(worker = %record.worker_id, "observers: worker credential created");
        Ok(ObserverWorkerCreated {
            worker: worker_view(&record),
            credential: credential.expose().to_string(),
        })
    }

    /// The worker credentials of `owner`, revoked ones included.
    pub fn observer_workers(&self, owner: &OwnerRef) -> ProtocolResult<ObserverWorkerListResponse> {
        Ok(ObserverWorkerListResponse {
            workers: self
                .credentials
                .observer_workers(owner)?
                .iter()
                .map(worker_view)
                .collect(),
        })
    }

    /// Revokes `owner`'s worker credential `worker_id`: from now on it is
    /// refused. A run it claimed and did not report is lost at its
    /// deadline. Not found for a credential of another owner.
    pub fn revoke_observer_worker(
        &self,
        owner: &OwnerRef,
        worker_id: &str,
    ) -> ProtocolResult<ObserverWorkerView> {
        let record = self
            .credentials
            .revoke_observer_worker(owner, worker_id)?
            .ok_or(ProtocolError::NotFound)?;
        tracing::info!(worker = %record.worker_id, "observers: worker credential revoked");
        Ok(worker_view(&record))
    }

    /// Who an observer worker credential proves; `None` for every refusal.
    pub fn authenticate_observer_worker(&self, bearer: &str) -> Option<ObserverWorkerIdentity> {
        self.credentials.authenticate_observer_worker(bearer)
    }
}
