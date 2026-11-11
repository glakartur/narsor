//! Enrollment codes and enrolling a controller with one.

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::lease::bounded_ttl;
use crate::controller_protocol::service::{parse_identifier, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::domain::BackendId;
use crate::environment::BackendKind;

impl ProtocolService {
    /// Creates a one-time code enrolling one controller for `backend_id`.
    pub fn create_enrollment(
        &self,
        backend_id: &str,
        request: CreateEnrollmentRequest,
    ) -> ProtocolResult<EnrollmentResponse> {
        let id = parse_identifier(BackendId::parse(backend_id))?;
        let record = self
            .launch
            .backend_record(&id)
            .map_err(|error| ProtocolError::bad_request(error.code(), error.to_string()))?
            .ok_or(ProtocolError::NotFound)?;
        // Judged by what the backend is, not by its id: the daemon's own
        // `local` asked for over HTTP is one more such backend (R15). Its
        // trust scope does not count either: a `local` backend declared with
        // another one after the server started runs tasks the same way.
        if record.kind == BackendKind::Local && !request.accept_workstation_risk {
            return Err(ProtocolError::bad_request(
                WORKSTATION_RISK_NOT_ACCEPTED,
                WORKSTATION_RISK,
            ));
        }
        self.issue_enrollment(backend_id, request)
    }

    /// A one-time code for the controller a trusted daemon starts for its
    /// built-in `local` backend. Not reachable over HTTP: the daemon calls
    /// it in process, for a controller it starts itself (KTD11).
    pub fn create_builtin_local_enrollment(&self) -> ProtocolResult<EnrollmentResponse> {
        self.issue_enrollment(
            crate::environment::BackendRecord::LOCAL_ID,
            CreateEnrollmentRequest::default(),
        )
    }

    fn issue_enrollment(
        &self,
        backend_id: &str,
        request: CreateEnrollmentRequest,
    ) -> ProtocolResult<EnrollmentResponse> {
        let backend_id = parse_identifier(BackendId::parse(backend_id))?;
        let ttl = bounded_ttl(
            request.ttl_secs,
            self.config.default_enrollment_ttl,
            self.config.max_enrollment_ttl,
        );
        let (record, code) = self
            .credentials
            .create_enrollment(&backend_id, request.name, ttl)?;
        Ok(EnrollmentResponse {
            enrollment_code: code.expose().to_string(),
            backend_id: record.backend_id.to_string(),
            expires_at: record.expires_at,
        })
    }

    /// Redeems an enrollment code into a controller identity bound to the
    /// request's public key.
    pub fn enroll(&self, request: EnrollRequest) -> ProtocolResult<EnrollResponse> {
        let record = self
            .credentials
            .redeem_enrollment(
                &request.enrollment_code,
                &request.public_key,
                &request.proof,
            )?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(EnrollResponse {
            controller_id: record.controller_id,
            backend_id: record.backend_id,
            protocol_version: PROTOCOL_VERSION,
        })
    }
}
