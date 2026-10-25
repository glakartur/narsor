//! Typed clients of the controller and runner protocol.
//!
//! Both talk to a control plane by URL, not by socket address: a controller
//! or a runner lives in another environment and reaches the control plane
//! outbound (R5). Failures map onto the same [`ClientError`] the owner
//! client uses, so a refused report reads `Refused { code: "stale_lease", … }`
//! exactly like a refused owner command.

use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::wire::*;
use crate::auth::pop::{rotation_message, ControllerCredential, ControllerKey};
use crate::materials::{
    MaterialAckRequest, MaterialAckResponse, MaterialUploadResponse, MaterialView,
    BUNDLE_CONTENT_TYPE,
};
use crate::orchestration::RunnerSnapshot;
use narsor_shared::api_error::{handle_json_response, ClientError};

/// Bound of a plain request; a long-poll adds its own wait on top.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound of one material blob download or publication (U9).
const MATERIAL_TRANSFER_TIMEOUT: Duration = Duration::from_secs(300);

/// How a controller's client behaves beyond its URL and credential.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    /// Bound of `release_lease`, when it must be shorter than a plain
    /// request's: a controller stopping under a supervisor's deadline gives
    /// up on a dead control plane in time (KTD8).
    pub release_lease_timeout: Option<Duration>,
    /// Never follow a redirect: a control plane answers where it was asked,
    /// and a redirect to another host would carry signed requests there
    /// (R16).
    pub refuse_redirects: bool,
}

impl ClientOptions {
    fn http(&self) -> reqwest::Client {
        if !self.refuse_redirects {
            return reqwest::Client::new();
        }
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    }
}

/// How a client proves who it is.
#[derive(Clone)]
enum Credential {
    /// A runner's bearer secret (or, for a credential that is not a valid
    /// controller credential, the text as given — refused by the server).
    Bearer(String),
    /// A controller's key: every request is signed. Boxed: it is much
    /// larger than the other variants.
    Signed(Box<ControllerCredential>),
    /// Nothing (enrollment).
    None,
}

/// A small HTTP helper shared by both clients.
#[derive(Clone)]
struct Transport {
    base_url: String,
    credential: Credential,
    http: reqwest::Client,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credential is a secret: never in a log line.
        f.debug_struct("Transport")
            .field("base_url", &self.base_url)
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl Transport {
    fn new(base_url: impl Into<String>, credential: Credential) -> Self {
        Self::with_http(base_url, credential, reqwest::Client::new())
    }

    fn with_http(base_url: impl Into<String>, credential: Credential, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            credential,
            http,
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
        let request = self
            .http
            .get(self.url(path))
            .query(query)
            .timeout(REQUEST_TIMEOUT + wait);
        self.send(request, Vec::new()).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        self.post_within(path, body, REQUEST_TIMEOUT).await
    }

    /// [`Self::post`] bounded by `timeout`.
    async fn post_within<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T, ClientError> {
        let body = serde_json::to_vec(body)
            .map_err(|error| ClientError::Server(format!("cannot encode the request: {error}")))?;
        let request = self
            .http
            .request(Method::POST, self.url(path))
            .header(CONTENT_TYPE, "application/json")
            .timeout(timeout);
        self.send(request, body).await
    }

    /// `GET path`, answered with raw bytes (a material blob).
    async fn get_bytes(&self, path: &str) -> Result<bytes::Bytes, ClientError> {
        let request = self
            .http
            .get(self.url(path))
            .timeout(MATERIAL_TRANSFER_TIMEOUT);
        let response = self.send_raw(request, Vec::new()).await?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(
                match handle_json_response::<serde_json::Value>(response).await {
                    Err(error) => error,
                    Ok(_) => ClientError::Server("unexpected answer to a download".to_string()),
                },
            );
        }
        response
            .bytes()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))
    }

    /// `POST path` with a body of `content_type`, decoding a JSON answer.
    async fn post_raw<T: DeserializeOwned>(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<T, ClientError> {
        let request = self
            .http
            .request(Method::POST, self.url(path))
            .header(CONTENT_TYPE, content_type)
            .timeout(MATERIAL_TRANSFER_TIMEOUT);
        self.send(request, body).await
    }

    /// Builds the request, authenticates it — a controller signs the exact
    /// method, path with query and body bytes that go on the wire — and
    /// decodes the answer.
    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        body: Vec<u8>,
    ) -> Result<T, ClientError> {
        let response = self.send_raw(request, body).await?;
        handle_json_response(response).await
    }

    /// [`Self::send`] without decoding the answer.
    async fn send_raw(
        &self,
        request: reqwest::RequestBuilder,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ClientError> {
        let mut request = request
            .body(body.clone())
            .build()
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        let authorization = match &self.credential {
            Credential::Bearer(token) => Some(format!("Bearer {token}")),
            Credential::Signed(credential) => {
                let url = request.url();
                let path_and_query = match url.query() {
                    Some(query) => format!("{}?{query}", url.path()),
                    None => url.path().to_string(),
                };
                Some(credential.authorization(request.method().as_str(), &path_and_query, &body))
            }
            Credential::None => None,
        };
        if let Some(authorization) = authorization {
            let value =
                HeaderValue::from_str(&authorization).map_err(|_| ClientError::Unauthorized)?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        self.http
            .execute(request)
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))
    }
}

/// A client for one enrolled controller identity.
///
/// It holds the identity's private key and signs every request with it
/// ([`crate::auth::pop`]); no secret ever travels.
#[derive(Debug, Clone)]
pub struct ControllerClient {
    transport: Transport,
    release_lease_timeout: Option<Duration>,
}

impl ControllerClient {
    /// A client presenting the persisted controller `credential`
    /// (`nsk.…`, see [`ControllerCredential`]) to the control plane at
    /// `base_url`. Anything that is not such a credential is sent as given
    /// and refused by the server.
    pub fn new(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        let credential = credential.into();
        let credential = match ControllerCredential::parse(&credential) {
            Some(parsed) => Credential::Signed(Box::new(parsed)),
            None => Credential::Bearer(credential),
        };
        Self {
            transport: Transport::new(base_url, credential),
            release_lease_timeout: None,
        }
    }

    /// The same identity, behaving as `options` say.
    pub fn with_options(self, options: &ClientOptions) -> Self {
        Self {
            transport: Transport::with_http(
                self.transport.base_url,
                self.transport.credential,
                options.http(),
            ),
            release_lease_timeout: options.release_lease_timeout,
        }
    }

    /// Generates a key pair, redeems a one-time enrollment code binding its
    /// public half, and returns a client for the new identity together with
    /// its details. Persist [`Self::credential`] to reconnect later.
    pub async fn enroll(
        base_url: impl Into<String>,
        enrollment_code: &str,
    ) -> Result<(Self, EnrollResponse), ClientError> {
        Self::enroll_with_options(base_url, enrollment_code, &ClientOptions::default()).await
    }

    /// [`Self::enroll`], the enrollment request and the returned client
    /// behaving as `options` say.
    pub async fn enroll_with_options(
        base_url: impl Into<String>,
        enrollment_code: &str,
        options: &ClientOptions,
    ) -> Result<(Self, EnrollResponse), ClientError> {
        let anonymous = Transport::with_http(base_url, Credential::None, options.http());
        let key = ControllerKey::generate();
        let request = crate::auth::pop::signed_enrollment_request(enrollment_code, &key);
        let enrolled: EnrollResponse = anonymous.post("/controller/v1/enroll", &request).await?;
        let credential = ControllerCredential::new(enrolled.controller_id.clone(), key);
        let client = Self {
            transport: Transport::with_http(
                anonymous.base_url,
                Credential::Signed(Box::new(credential)),
                anonymous.http,
            ),
            release_lease_timeout: options.release_lease_timeout,
        };
        Ok((client, enrolled))
    }

    /// The persistable credential of this identity (its id and private
    /// key), `None` for a client built from something else. A secret: store
    /// it `0600`, never log it.
    pub fn credential(&self) -> Option<String> {
        match &self.transport.credential {
            Credential::Signed(credential) => Some(credential.expose()),
            _ => None,
        }
    }

    /// `POST /controller/v1/key` — rotates to a freshly generated key and
    /// returns a client holding it. From the server's answer on, this
    /// client's key is refused.
    pub async fn rotate_key(&self) -> Result<(Self, RotateKeyResponse), ClientError> {
        let Credential::Signed(current) = &self.transport.credential else {
            return Err(ClientError::Unauthorized);
        };
        let key = ControllerKey::generate();
        let public_key = key.public_key();
        let request = RotateKeyRequest {
            proof: key.sign(&rotation_message(current.controller_id(), &public_key)),
            public_key,
        };
        let response: RotateKeyResponse =
            self.transport.post("/controller/v1/key", &request).await?;
        let credential = ControllerCredential::new(current.controller_id(), key);
        let client = Self {
            transport: Transport::with_http(
                self.transport.base_url.clone(),
                Credential::Signed(Box::new(credential)),
                self.transport.http.clone(),
            ),
            release_lease_timeout: self.release_lease_timeout,
        };
        Ok((client, response))
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

    /// `POST /controller/v1/lease/release`, bounded by the
    /// [`ClientOptions::release_lease_timeout`] when one was set.
    pub async fn release_lease(&self, epoch: u64) -> Result<LeaseView, ClientError> {
        self.transport
            .post_within(
                "/controller/v1/lease/release",
                &LeaseEpochRequest {
                    epoch,
                    ttl_secs: None,
                },
                self.release_lease_timeout.unwrap_or(REQUEST_TIMEOUT),
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

    /// `POST /controller/v1/tasks/:task_id/reader-credentials`: the
    /// credential of a read-only runner for a stopped task an owner is
    /// reading (D63). Refused unless the control plane wants the reader.
    pub async fn issue_reader_credential(
        &self,
        task_id: &str,
        request: &RunnerCredentialRequest,
    ) -> Result<RunnerCredentialResponse, ClientError> {
        self.transport
            .post(
                &format!("/controller/v1/tasks/{task_id}/reader-credentials"),
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
            transport: Transport::new(base_url, Credential::Bearer(credential.into())),
        }
    }

    /// `POST /runner/v1/snapshots`.
    pub async fn snapshot(
        &self,
        snapshot: &RunnerSnapshot,
    ) -> Result<RunnerSnapshotResponse, ClientError> {
        let request = super::wire::runner_snapshot_request(snapshot)
            .map_err(|error| ClientError::Server(format!("cannot encode the snapshot: {error}")))?;
        self.transport.post("/runner/v1/snapshots", &request).await
    }

    /// `GET /runner/v1/poll` — waits up to `wait_secs` for an answer to
    /// `pending_input_id` or a directive other than `continue`.
    pub async fn poll(
        &self,
        pending_input_id: Option<&str>,
        wait_secs: u64,
    ) -> Result<RunnerPollResponse, ClientError> {
        self.poll_with(&RunnerPollQuery {
            pending_input_id: pending_input_id.map(str::to_string),
            wait_secs: Some(wait_secs),
            ..RunnerPollQuery::default()
        })
        .await
    }

    /// `GET /runner/v1/poll` with every option of [`RunnerPollQuery`].
    pub async fn poll_with(
        &self,
        query: &RunnerPollQuery,
    ) -> Result<RunnerPollResponse, ClientError> {
        let wait = Duration::from_secs(query.wait_secs.unwrap_or(0));
        self.transport.get("/runner/v1/poll", query, wait).await
    }

    /// `POST /runner/v1/renew`.
    pub async fn renew(&self) -> Result<RunnerRenewResponse, ClientError> {
        self.transport
            .post("/runner/v1/renew", &serde_json::json!({}))
            .await
    }

    /// `POST /runner/v1/conversation-pages/:request_id`.
    pub async fn upload_conversation_page(
        &self,
        request_id: &str,
        page: &ConversationPage,
    ) -> Result<ConversationPageAccepted, ClientError> {
        self.transport
            .post(&format!("/runner/v1/conversation-pages/{request_id}"), page)
            .await
    }

    /// `POST /runner/v1/execution-events/:request_id`.
    pub async fn upload_execution_events(
        &self,
        request_id: &str,
        page: &ExecutionEventsPage,
    ) -> Result<ConversationPageAccepted, ClientError> {
        self.transport
            .post(&format!("/runner/v1/execution-events/{request_id}"), page)
            .await
    }

    /// `POST /runner/v1/conductor-reads/:request_id`.
    pub async fn upload_conductor_read(
        &self,
        request_id: &str,
        view: &crate::conductor::ConductorView,
    ) -> Result<ConversationPageAccepted, ClientError> {
        self.transport
            .post(&format!("/runner/v1/conductor-reads/{request_id}"), view)
            .await
    }

    /// `POST /runner/v1/relay-failures/:request_id` — the read an owner
    /// waits for cannot be answered.
    pub async fn upload_relay_failure(
        &self,
        request_id: &str,
        failure: &RelayFailure,
    ) -> Result<ConversationPageAccepted, ClientError> {
        self.transport
            .post(&format!("/runner/v1/relay-failures/{request_id}"), failure)
            .await
    }

    /// `GET /runner/v1/materials/:material_id` — a material of this
    /// runner's task, with its manifest.
    pub async fn material(&self, material_id: &str) -> Result<MaterialView, ClientError> {
        self.transport
            .get(
                &format!("/runner/v1/materials/{material_id}"),
                &(),
                Duration::ZERO,
            )
            .await
    }

    /// `GET /runner/v1/materials/:material_id/blobs/:sha256` — unverified
    /// bytes; the caller checks them against the manifest.
    pub async fn material_blob(
        &self,
        material_id: &str,
        sha256: &str,
    ) -> Result<bytes::Bytes, ClientError> {
        self.transport
            .get_bytes(&format!(
                "/runner/v1/materials/{material_id}/blobs/{sha256}"
            ))
            .await
    }

    /// `POST /runner/v1/materials/:material_id/ack`.
    pub async fn ack_material(
        &self,
        material_id: &str,
        manifest_digest: &str,
    ) -> Result<MaterialAckResponse, ClientError> {
        self.transport
            .post(
                &format!("/runner/v1/materials/{material_id}/ack"),
                &MaterialAckRequest {
                    manifest_digest: manifest_digest.to_string(),
                },
            )
            .await
    }

    /// `POST /runner/v1/materials` — publishes an encoded bundle from the
    /// task's outbox.
    pub async fn publish_material(
        &self,
        bundle: Vec<u8>,
    ) -> Result<MaterialUploadResponse, ClientError> {
        self.transport
            .post_raw("/runner/v1/materials", BUNDLE_CONTENT_TYPE, bundle)
            .await
    }

    /// `POST /runner/v1/commands/:command_id/ack`.
    pub async fn ack_command(
        &self,
        command_id: &str,
        applied: bool,
        detail: Option<String>,
    ) -> Result<OperatorCommandAckResponse, ClientError> {
        self.transport
            .post(
                &format!("/runner/v1/commands/{command_id}/ack"),
                &OperatorCommandAckRequest { applied, detail },
            )
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
