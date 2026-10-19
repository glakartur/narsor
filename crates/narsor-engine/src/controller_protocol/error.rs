//! Refusals of the control-plane protocol and their HTTP shape.
//!
//! Authentication refusals and "not found" carry **no body** (the existing
//! API's rule): a caller cannot tell a wrong credential from a missing one,
//! nor a task of another backend or owner from one that does not exist.
//! Everything a legitimate caller can act on — a fenced lease, a stale
//! generation, an obsolete input — is a `409` (or `400`) with a stable
//! machine-readable `code`, in the same [`RequestErrorResponse`] shape the
//! rest of the API uses.

use crate::orchestration::OrchestrationError;

/// Why a protocol request was refused.
#[derive(Debug)]
pub enum ProtocolError {
    /// No, a wrong, a revoked or an outdated credential. Bare `401`.
    Unauthorized,
    /// The caller is authenticated but may not do this (an owner who is not
    /// the installation's operator). Bare `403`.
    Forbidden,
    /// The thing does not exist *for this caller*. Bare `404`.
    NotFound,
    /// The request is malformed. `400` with a reason.
    BadRequest {
        /// Stable reason code.
        code: &'static str,
        /// Explanation without secrets.
        message: String,
    },
    /// The request conflicts with current state. `409` with a reason.
    Conflict {
        /// Stable reason code.
        code: &'static str,
        /// Explanation without secrets.
        message: String,
    },
    /// Something that should answer did not (a live runner that missed its
    /// deadline). `503` with a reason; the caller may retry.
    Unavailable {
        /// Stable reason code.
        code: &'static str,
        /// Explanation without secrets.
        message: String,
    },
    /// The server failed. Bare `500`, details in the log only.
    Internal(String),
}

impl ProtocolError {
    /// Constructs a typed conflict for an owner/controller/runner request.
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }

    /// Constructs a retryable service-unavailable response.
    pub fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::Unavailable {
            code,
            message: message.into(),
        }
    }

    /// Constructs a typed bad-request response.
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::Forbidden => f.write_str("forbidden"),
            Self::NotFound => f.write_str("not found"),
            Self::BadRequest { code, message }
            | Self::Conflict { code, message }
            | Self::Unavailable { code, message } => {
                write!(f, "{code}: {message}")
            }
            Self::Internal(message) => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<OrchestrationError> for ProtocolError {
    fn from(error: OrchestrationError) -> Self {
        use OrchestrationError as E;
        let message = error.to_string();
        match error {
            // A task of another backend is indistinguishable from none.
            E::TaskNotFound(_)
            | E::WrongBackend { .. }
            | E::InputNotFound { .. }
            | E::OperatorCommandNotFound { .. } => Self::NotFound,
            E::InvalidIdentifier(_) => Self::bad_request("invalid_identifier", message),
            E::InvalidLaunchSpec(_) => Self::bad_request("invalid_launch_spec", message),
            E::TaskConflict(_) => Self::conflict("task_conflict", message),
            E::IdempotencyKeyReused { .. }
            | E::InputKeyReused { .. }
            | E::OperatorCommandKeyReused { .. } => {
                Self::conflict("idempotency_key_reused", message)
            }
            E::StaleGeneration { .. } => Self::conflict("stale_generation", message),
            E::IllegalCommand { .. } => Self::conflict("illegal_command", message),
            E::IllegalObservation { .. } => Self::conflict("illegal_observation", message),
            E::StaleObservation { .. } => Self::conflict("stale_observation", message),
            E::SafePointNotAcknowledged { .. } => {
                Self::conflict("safe_point_not_acknowledged", message)
            }
            E::LeaseHeld { .. } => Self::conflict("lease_held", message),
            E::StaleLease { .. } => Self::conflict("stale_lease", message),
            E::LeaseExpired { .. } => Self::conflict("lease_expired", message),
            // The runner authenticated a moment before a resume superseded
            // it: from now on its credential is refused anyway.
            E::RunnerSuperseded { .. } => Self::Unauthorized,
            E::StaleRunnerSnapshot { .. } => Self::conflict("stale_snapshot", message),
            E::ObsoleteInput { .. } => Self::conflict("obsolete_input", message),
            E::InputAlreadyQueued { .. } => Self::conflict("input_already_queued", message),
            E::InputAlreadySettled { .. } => Self::conflict("input_already_settled", message),
            E::TaskNotAcceptingInput { .. } => Self::conflict("task_not_accepting_input", message),
            E::OperatorCommandRefused { .. } => Self::conflict("operator_command_refused", message),
            E::OperatorCommandAlreadySettled { .. } => {
                Self::conflict("operator_command_already_settled", message)
            }
            E::NotAdmitted(_) => Self::conflict("not_admitted", message),
            E::BackendUnavailable { .. } => Self::conflict("backend_unavailable", message),
            E::TaskNotQueued { .. } => Self::conflict("task_not_queued", message),
            E::InvalidMoveAnchor { .. } => Self::bad_request("invalid_move_anchor", message),
            E::Persistence(_)
            | E::UnsupportedRecordVersion { .. }
            | E::LegacyRecordAppeared { .. }
            | E::Lock { .. } => Self::Internal(message),
        }
    }
}

impl From<crate::materials::MaterialError> for ProtocolError {
    fn from(error: crate::materials::MaterialError) -> Self {
        use crate::materials::MaterialError as E;
        let message = error.to_string();
        match error {
            E::NotFound => Self::NotFound,
            E::InvalidPath { .. } => Self::bad_request("invalid_material_path", message),
            E::InvalidManifest(_) => Self::bad_request("invalid_material_manifest", message),
            E::TooLarge(_) => Self::bad_request("material_too_large", message),
            E::DigestMismatch(_) => Self::bad_request("material_digest_mismatch", message),
            E::Incomplete(_) => Self::bad_request("material_incomplete", message),
            E::Refused(_) => Self::bad_request("material_refused", message),
            E::QuotaExceeded(_) => Self::conflict("material_quota_exceeded", message),
            E::IdReused(_) => Self::conflict("material_id_reused", message),
            E::TaskArchived(_) => Self::conflict("task_archived", message),
            E::Persistence(_) | E::Io { .. } | E::Internal(_) => Self::Internal(message),
        }
    }
}

/// Result type of the protocol service.
pub type ProtocolResult<T> = Result<T, ProtocolError>;
