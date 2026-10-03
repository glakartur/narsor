//! Refusals of the control-plane protocol and their HTTP shape.
//!
//! Authentication refusals and "not found" carry **no body** (the existing
//! API's rule): a caller cannot tell a wrong credential from a missing one,
//! nor a task of another backend or owner from one that does not exist.
//! Everything a legitimate caller can act on — a fenced lease, a stale
//! generation, an obsolete input — is a `409` (or `400`) with a stable
//! machine-readable `code`, in the same [`RequestErrorResponse`] shape the
//! rest of the API uses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::api::RequestErrorResponse;
use crate::orchestration::OrchestrationError;

/// Why a protocol request was refused.
#[derive(Debug)]
pub enum ProtocolError {
    /// No, a wrong, a revoked or an outdated credential. Bare `401`.
    Unauthorized,
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
    /// The server failed. Bare `500`, details in the log only.
    Internal(String),
}

impl ProtocolError {
    pub(crate) fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
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
            Self::NotFound => f.write_str("not found"),
            Self::BadRequest { code, message } | Self::Conflict { code, message } => {
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
            E::TaskNotFound(_) | E::WrongBackend { .. } | E::InputNotFound { .. } => Self::NotFound,
            E::InvalidIdentifier(_) => Self::bad_request("invalid_identifier", message),
            E::InvalidLaunchSpec(_) => Self::bad_request("invalid_launch_spec", message),
            E::TaskConflict(_) => Self::conflict("task_conflict", message),
            E::IdempotencyKeyReused { .. } | E::InputKeyReused { .. } => {
                Self::conflict("idempotency_key_reused", message)
            }
            E::StaleGeneration { .. } => Self::conflict("stale_generation", message),
            E::IllegalCommand { .. } => Self::conflict("illegal_command", message),
            E::IllegalObservation { .. } => Self::conflict("illegal_observation", message),
            E::StaleObservation { .. } => Self::conflict("stale_observation", message),
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
            E::Persistence(_) | E::UnsupportedRecordVersion { .. } | E::Lock { .. } => {
                Self::Internal(message)
            }
        }
    }
}

impl IntoResponse for ProtocolError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::BadRequest { code, message } => (
                StatusCode::BAD_REQUEST,
                Json(RequestErrorResponse {
                    code: code.to_owned(),
                    message,
                }),
            )
                .into_response(),
            Self::Conflict { code, message } => (
                StatusCode::CONFLICT,
                Json(RequestErrorResponse {
                    code: code.to_owned(),
                    message,
                }),
            )
                .into_response(),
            Self::Internal(message) => {
                tracing::warn!(message, "control plane: internal error serving request");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Result type of the protocol service.
pub type ProtocolResult<T> = Result<T, ProtocolError>;
