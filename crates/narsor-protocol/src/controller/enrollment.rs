//! Controller enrollment, key rotation, heartbeat and runner credential DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controller::LeaseView;
use crate::identifier::{BackendId, TaskId};

/// A controller's protocol-visible identity after enrollment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollResponse {
    pub controller_id: String,
    pub backend_id: BackendId,
    pub protocol_version: u32,
}

/// Proof-of-possession body for controller enrollment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollRequest {
    pub enrollment_code: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub proof: String,
}

/// Request to replace a controller's public key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateKeyRequest {
    pub public_key: String,
    pub proof: String,
}

/// Controller identity after key rotation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateKeyResponse {
    pub controller_id: String,
    pub rotated_at: DateTime<Utc>,
}

/// Controller heartbeat response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatResponse {
    pub controller_id: String,
    pub backend_id: BackendId,
    pub server_time: DateTime<Utc>,
    pub lease: Option<LeaseView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCredentialRequest {
    pub lease_epoch: u64,
    pub generation: u64,
    pub incarnation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCredentialResponse {
    pub task_id: TaskId,
    pub generation: u64,
    pub incarnation: String,
    pub credential: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerRenewResponse {
    pub expires_at: DateTime<Utc>,
}

/// The refusal code of an enrollment for a `local` backend, whatever its
/// trust scope, asked for without [`CreateEnrollmentRequest::accept_workstation_risk`].
pub const WORKSTATION_RISK_NOT_ACCEPTED: &str = "workstation_risk_not_accepted";

/// What the owner accepts when enrolling a controller of a `local` backend
/// on a `workstation` (R15).
pub const WORKSTATION_RISK: &str = "a controller of a 'local' backend runs every task the \
server sends as the user of its computer, without a container: a server that is taken over \
can run any program there and read every file that user can read. Accept this explicitly \
(accept_workstation_risk; narsor-cli controller enroll-code --accept-workstation-risk)";

/// Request to create a one-time controller enrollment code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateEnrollmentRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// The owner accepts that a controller of a `local` backend on a
    /// `workstation` runs whatever the server sends as that computer's
    /// user, without a container (R15). Required for such a backend; not
    /// sent when not given.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_workstation_risk: bool,
}

/// One-time controller enrollment code and its backend scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollmentResponse {
    pub enrollment_code: String,
    pub backend_id: String,
    pub expires_at: DateTime<Utc>,
}

/// Controller identity details without its credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerView {
    pub controller_id: String,
    pub backend_id: String,
    pub name: Option<String>,
    pub enrolled_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub holds_lease: bool,
}

/// All controller identities visible to an owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerListResponse {
    pub controllers: Vec<ControllerView>,
}
