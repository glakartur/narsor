//! Typed clients of the controller and runner protocol.
//!
//! Both talk to a control plane by URL, not by socket address: a controller
//! or a runner lives in another environment and reaches the control plane
//! outbound (R5). Failures map onto the same [`ClientError`] the owner
//! client uses, so a refused report reads `Refused { code: "stale_lease", … }`
//! exactly like a refused owner command.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::wire::*;
use crate::api::{handle_json_response, ClientError};
use crate::orchestration::RunnerSnapshot;

/// Bound of a plain request; a long-poll adds its own wait on top.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A small HTTP helper shared by both clients.
#[derive(Clone)]
struct Transport {
    base_url: String,
    credential: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credential is a bearer secret: never in a log line.
        f.debug_struct("Transport")
            .field("base_url", &self.base_url)
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl Transport {
    fn new(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            credential: credential.into(),
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn get<Q: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        query: &Q,
        wait: Duration,
    ) -> Result<T, ClientError> {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.credential)
            .query(query)
            .timeout(REQUEST_TIMEOUT + wait)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.credential)
            .json(body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }
}

/// A client for one enrolled controller identity.
#[derive(Debug, Clone)]
pub struct ControllerClient {
    transport: Transport,
}

impl ControllerClient {
    /// A client presenting `credential` to the control plane at `base_url`
    /// (`http://host:port`).
    pub fn new(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            transport: Transport::new(base_url, credential),
        }
    }

    /// Redeems a one-time enrollment code and returns a client for the new
    /// identity together with its details.
    pub async fn enroll(
        base_url: impl Into<String>,
        enrollment_code: &str,
    ) -> Result<(Self, EnrollResponse), ClientError> {
        let anonymous = Transport::new(base_url, String::new());
        let response = anonymous
            .http
            .post(anonymous.url("/controller/v1/enroll"))
            .json(&EnrollRequest {
                enrollment_code: enrollment_code.to_string(),
            })
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        let enrolled: EnrollResponse = handle_json_response(response).await?;
        let client = Self::new(anonymous.base_url, enrolled.credential.clone());
        Ok((client, enrolled))
    }

    /// `POST /controller/v1/heartbeat`.
    pub async fn heartbeat(&self) -> Result<HeartbeatResponse, ClientError> {
        self.transport
            .post("/controller/v1/heartbeat", &serde_json::json!({}))
            .await
    }

    /// `POST /controller/v1/lease`.
    pub async fn acquire_lease(&self, ttl_secs: Option<u64>) -> Result<LeaseView, ClientError> {
        self.transport
            .post("/controller/v1/lease", &AcquireLeaseRequest { ttl_secs })
            .await
    }

    /// `POST /controller/v1/lease/renew`.
    pub async fn renew_lease(
        &self,
        epoch: u64,
        ttl_secs: Option<u64>,
    ) -> Result<LeaseView, ClientError> {
        self.transport
            .post(
                "/controller/v1/lease/renew",
                &LeaseEpochRequest { epoch, ttl_secs },
            )
            .await
    }

    /// `POST /controller/v1/lease/release`.
    pub async fn release_lease(&self, epoch: u64) -> Result<LeaseView, ClientError> {
        self.transport
            .post(
                "/controller/v1/lease/release",
                &LeaseEpochRequest {
                    epoch,
                    ttl_secs: None,
                },
            )
            .await
    }

    /// `GET /controller/v1/commands` — waits up to `wait_secs` for something
    /// new; `all` asks for a full resync.
    pub async fn commands(
        &self,
        wait_secs: u64,
        all: bool,
    ) -> Result<CommandsResponse, ClientError> {
        self.transport
            .get(
                "/controller/v1/commands",
                &CommandsQuery {
                    wait_secs: Some(wait_secs),
                    all,
                },
                Duration::from_secs(wait_secs),
            )
            .await
    }

    /// `POST /controller/v1/acks`.
    pub async fn acknowledge(&self, acks: Vec<CommandAck>) -> Result<AckResponse, ClientError> {
        self.transport
            .post("/controller/v1/acks", &AckRequest { acks })
            .await
    }

    /// `POST /controller/v1/reports`.
    pub async fn report(&self, report: &ReportRequest) -> Result<ReportResponse, ClientError> {
        self.transport.post("/controller/v1/reports", report).await
    }

    /// `POST /controller/v1/tasks/:task_id/runner-credentials`.
    pub async fn issue_runner_credential(
        &self,
        task_id: &str,
        request: &RunnerCredentialRequest,
    ) -> Result<RunnerCredentialResponse, ClientError> {
        self.transport
            .post(
                &format!("/controller/v1/tasks/{task_id}/runner-credentials"),
                request,
            )
            .await
    }
}

/// A client for one runner incarnation.
#[derive(Debug, Clone)]
pub struct RunnerClient {
    transport: Transport,
}

impl RunnerClient {
    /// A client presenting the runner `credential` (read from its mounted
    /// file) to the control plane at `base_url`.
    pub fn new(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            transport: Transport::new(base_url, credential),
        }
    }

    /// `POST /runner/v1/snapshots`.
    pub async fn snapshot(
        &self,
        snapshot: &RunnerSnapshot,
    ) -> Result<RunnerSnapshotResponse, ClientError> {
        self.transport.post("/runner/v1/snapshots", snapshot).await
    }

    /// `GET /runner/v1/poll` — waits up to `wait_secs` for an answer to
    /// `pending_input_id` or a directive other than `continue`.
    pub async fn poll(
        &self,
        pending_input_id: Option<&str>,
        wait_secs: u64,
    ) -> Result<RunnerPollResponse, ClientError> {
        self.transport
            .get(
                "/runner/v1/poll",
                &RunnerPollQuery {
                    pending_input_id: pending_input_id.map(str::to_string),
                    wait_secs: Some(wait_secs),
                },
                Duration::from_secs(wait_secs),
            )
            .await
    }

    /// `POST /runner/v1/renew`.
    pub async fn renew(&self) -> Result<RunnerRenewResponse, ClientError> {
        self.transport
            .post("/runner/v1/renew", &serde_json::json!({}))
            .await
    }

    /// `POST /runner/v1/inputs/:input_id/ack`.
    pub async fn ack_input(
        &self,
        input_id: &str,
        accepted: bool,
        detail: Option<String>,
    ) -> Result<InputAckResponse, ClientError> {
        self.transport
            .post(
                &format!("/runner/v1/inputs/{input_id}/ack"),
                &InputAckRequest { accepted, detail },
            )
            .await
    }
}
