//! Typed HTTP client for the embedded API (U5), the one thing U7's task view
//! is meant to depend on for events, the timeline and conversations.
//!
//! # Why a distinct "API unavailable" error
//!
//! The TUI's task view (U7) reads by calling this client on every manual
//! refresh (R8: no auto-refresh) — a request can fail for reasons that call
//! for very different messages to the operator: the *server* is simply gone
//! (the loopback port refused the connection, or the request timed out —
//! [`ClientError::Unavailable`]) versus the *answer* being a well-formed
//! `401` or `404` from a server that is very much up. Collapsing all of
//! that into one generic "request failed" would make "is my API server even
//! running" indistinguishable from "this execution id is wrong", which is
//! exactly the distinction an operator needs when the TUI is the process
//! that started the server in the first place.
//!
//! # Why a timeout
//!
//! The server only ever reads local files, so a slow response almost never
//! means "give it more time" — it means the process is gone, wedged, or the
//! wrong port. A bounded timeout turns that into a prompt, readable
//! [`ClientError::Unavailable`] instead of a TUI that appears to hang.

use std::net::SocketAddr;
use std::time::Duration;

use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use crate::domain::Event;
use crate::engine::Timeline;

use super::routes::ConversationResponse;

/// How long a single request may take before it is treated as the server
/// being unavailable.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A distinguishable failure of a client call.
#[derive(Debug)]
pub enum ClientError {
    /// The request never got a well-formed answer at all: connection
    /// refused, timed out, or some other transport-level failure. This is
    /// the "API unavailable" case the plan calls for — distinct from a
    /// `401`/`404` that a live server chose to send.
    Unavailable(String),
    /// The token was missing or wrong.
    Unauthorized,
    /// The execution (or, for conversations, the enclosing execution) is
    /// not known to the server.
    NotFound,
    /// The server answered with an unexpected status or a body this client
    /// could not parse.
    Server(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unavailable(detail) => write!(f, "API unavailable: {detail}"),
            ClientError::Unauthorized => write!(f, "API rejected the request: bad token"),
            ClientError::NotFound => write!(f, "API: not found"),
            ClientError::Server(detail) => write!(f, "API error: {detail}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// A client bound to one running server, over `http://127.0.0.1:<port>`.
pub struct ApiClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl ApiClient {
    /// Builds a client for the server at `addr`, presenting `token` on every
    /// request.
    pub fn new(addr: SocketAddr, token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with only a timeout configured never fails to build");
        Self {
            base_url: format!("http://{addr}"),
            token: token.into(),
            http,
        }
    }

    /// Events of `execution_id` with `sequence > after`.
    pub async fn events(&self, execution_id: &str, after: u64) -> Result<Vec<Event>, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/events?after={after}",
            self.base_url
        );
        self.get_json(&url).await
    }

    /// The step timeline of `execution_id`.
    pub async fn timeline(&self, execution_id: &str) -> Result<Timeline, ClientError> {
        let url = format!("{}/executions/{execution_id}/timeline", self.base_url);
        self.get_json(&url).await
    }

    /// Conversation entries of one attempt, from entry `from` onward, up to
    /// `byte_budget` bytes.
    pub async fn conversation(
        &self,
        execution_id: &str,
        attempt_id: &str,
        from: u64,
        byte_budget: usize,
    ) -> Result<ConversationResponse, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/attempts/{attempt_id}/conversation?from={from}&byte_budget={byte_budget}",
            self.base_url
        );
        self.get_json(&url).await
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;

        match response.status() {
            StatusCode::OK => response
                .json::<T>()
                .await
                .map_err(|error| ClientError::Server(error.to_string())),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ClientError::Unauthorized),
            StatusCode::NOT_FOUND => Err(ClientError::NotFound),
            status => Err(ClientError::Server(format!("unexpected status {status}"))),
        }
    }
}
