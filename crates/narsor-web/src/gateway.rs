//! The HTTP side of `narsor-web`: the files of the interface, the proxy to
//! the API of `narsor-server`, and the guards in front of both.
//!
//! # What goes through
//!
//! Only the routes of the owner API the interface uses: `/control/v1/…`,
//! `/settings/…`, `/auth/v1/whoami` and `POST /tasks`. Anything else is a
//! `404`, so `narsor-web` is never an open relay. Requests and answers flow
//! through as they are; an event stream (SSE) is passed on chunk by chunk.
//!
//! # Tokens
//!
//! - [`TokenMode::Server`]: the browser presents the session token of this
//!   process (`Authorization: Bearer <session>`); `narsor-web` checks it in
//!   constant time and sends the request on with the server's own token,
//!   re-read after a `401`. The token of the server never reaches the
//!   browser.
//! - [`TokenMode::Passthrough`]: the browser's `Authorization` goes to the
//!   server unchanged, and so does `X-Forwarded-Proto` of the TLS proxy in
//!   front of `narsor-web` (a server in network mode lists `narsor-web` in
//!   `trusted_proxies`).
//!
//! # Other sites and DNS rebinding
//!
//! In the server mode a request whose `Host` is not this loopback address,
//! or whose `Origin` is another site, is refused with `403` before anything
//! else: a page of another site, even one whose name an attacker pointed at
//! 127.0.0.1, can neither read the interface nor act through it.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::future::BoxFuture;
use narsor_api_client::api::{ApiClient, ClientError, ForwardAuth};
use tokio::sync::RwLock;

use crate::settings::TokenMode;

/// The largest request body passed on (the interface sends small JSON).
const BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Request headers passed on to the server, besides `Authorization`.
const REQUEST_HEADERS: [HeaderName; 3] = [
    header::CONTENT_TYPE,
    header::ACCEPT,
    HeaderName::from_static("last-event-id"),
];

/// Answer headers passed back to the browser.
const RESPONSE_HEADERS: [HeaderName; 2] = [header::CONTENT_TYPE, header::CACHE_CONTROL];

const FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// Makes a fresh client after the upstream stopped answering (the local
/// daemon restarted on another port with another token).
pub type Reconnect = Arc<dyn Fn() -> BoxFuture<'static, Result<ApiClient, String>> + Send + Sync>;

/// The server the API goes to.
pub struct Upstream {
    client: RwLock<ApiClient>,
    reconnect: Option<Reconnect>,
}

impl Upstream {
    /// A server that stays where it is.
    pub fn fixed(client: ApiClient) -> Self {
        Self {
            client: RwLock::new(client),
            reconnect: None,
        }
    }

    /// A server that `reconnect` finds again when it stops answering.
    pub fn reconnecting(client: ApiClient, reconnect: Reconnect) -> Self {
        Self {
            client: RwLock::new(client),
            reconnect: Some(reconnect),
        }
    }

    async fn forward(&self, request: &Forwarded) -> Result<reqwest::Response, ClientError> {
        let current = self.client.read().await.clone();
        let first = send(current, request).await;
        match (&first, &self.reconnect) {
            (Err(ClientError::Unavailable(_)), Some(reconnect)) => {
                let fresh = reconnect().await.map_err(ClientError::Unavailable)?;
                *self.client.write().await = fresh.clone();
                send(fresh, request).await
            }
            _ => first,
        }
    }
}

/// One request on its way to the server.
struct Forwarded {
    method: reqwest::Method,
    path_and_query: String,
    headers: reqwest::header::HeaderMap,
    body: bytes::Bytes,
    auth: ForwardAuth,
}

async fn send(client: ApiClient, request: &Forwarded) -> Result<reqwest::Response, ClientError> {
    client
        .forward(
            request.method.clone(),
            &request.path_and_query,
            request.headers.clone(),
            request.body.clone(),
            request.auth.clone(),
        )
        .await
}

/// What the gateway knows to answer a request.
pub struct Gateway {
    upstream: Upstream,
    mode: TokenMode,
    /// The session token of the server mode.
    session: Option<String>,
    /// `Host` values accepted in the server mode.
    hosts: Vec<String>,
}

impl Gateway {
    /// A gateway to `upstream`. `port` is the port `narsor-web` listens on;
    /// `session` is required in the server mode.
    pub fn new(upstream: Upstream, mode: TokenMode, session: Option<String>, port: u16) -> Self {
        let hosts = ["127.0.0.1", "localhost", "[::1]"]
            .iter()
            .map(|host| format!("{host}:{port}"))
            .collect();
        Self {
            upstream,
            mode,
            session,
            hosts,
        }
    }

    /// The routes: the interface, its mode, the proxy.
    pub fn router(self) -> Router {
        let state = Arc::new(self);
        Router::new()
            .merge(crate::routes::<Arc<Gateway>>())
            .route("/gateway/v1/info", get(info))
            .fallback(proxy)
            .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
            .with_state(state)
    }

    fn allows_host(&self, headers: &HeaderMap) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let host_ok = host.is_some_and(|host| self.hosts.iter().any(|allowed| allowed == host));
        let origin_ok = origin.is_none_or(|origin| {
            self.hosts
                .iter()
                .any(|allowed| origin == format!("http://{allowed}"))
        });
        host_ok && origin_ok
    }

    fn session_matches(&self, headers: &HeaderMap) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        constant_time_eq(presented.as_bytes(), session.as_bytes())
    }
}

/// Whether `a` equals `b`, in a time that does not depend on where they differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Refuses other sites in the server mode, for every route.
async fn guard(State(gateway): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    if gateway.mode == TokenMode::Server && !gateway.allows_host(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// `GET /gateway/v1/info`: what the sign-in form tells the operator.
async fn info(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "token": gateway.mode.word() }))
}

/// The path and query to send when the interface may reach it; else `None`.
///
/// The check runs on the path as a URL parser normalizes it, and the same
/// normalized text is what goes to the server: a dot segment written with
/// percent escapes (`/control/v1/%2e%2e/auth/v1/claim`) is resolved before
/// the allowlist sees it, so the checked path and the sent path are one.
pub fn checked_path(raw: &str) -> Option<String> {
    // A backslash, plain or escaped, is a separator to some servers.
    if raw.contains('\\') || raw.to_ascii_lowercase().contains("%5c") {
        return None;
    }
    let parsed = reqwest::Url::parse(&format!("http://gateway.invalid{raw}")).ok()?;
    if parsed.host_str() != Some("gateway.invalid") || !is_proxied(parsed.path()) {
        return None;
    }
    Some(match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    })
}

/// Whether the interface may reach `path` on the server.
pub fn is_proxied(path: &str) -> bool {
    path.starts_with("/control/v1/")
        || path.starts_with("/settings/")
        || path == "/auth/v1/whoami"
        || path == "/tasks"
}

async fn proxy(State(gateway): State<Arc<Gateway>>, request: Request) -> Response {
    let raw = request.uri().path_and_query().map_or_else(
        || request.uri().path().to_string(),
        |value| value.as_str().to_string(),
    );
    let Some(path_and_query) = checked_path(&raw) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let auth = match gateway.mode {
        TokenMode::Server => {
            if !gateway.session_matches(request.headers()) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            ForwardAuth::Own
        }
        // The browser's own token or nothing: never narsor-web's.
        TokenMode::Passthrough => match request.headers().get(header::AUTHORIZATION) {
            Some(value) => ForwardAuth::Header(value.clone()),
            None => return StatusCode::UNAUTHORIZED.into_response(),
        },
    };
    let mut headers = reqwest::header::HeaderMap::new();
    for name in REQUEST_HEADERS {
        if let Some(value) = request.headers().get(&name) {
            headers.insert(name, value.clone());
        }
    }
    if gateway.mode == TokenMode::Passthrough {
        if let Some(value) = request.headers().get(&FORWARDED_PROTO) {
            headers.insert(FORWARDED_PROTO, value.clone());
        }
    }
    let method = request.method().clone();
    let body = match to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let forwarded = Forwarded {
        method: reqwest_method(&method),
        path_and_query,
        headers,
        body,
        auth,
    };
    match gateway.upstream.forward(&forwarded).await {
        Ok(answer) => relay(answer),
        Err(error) => {
            tracing::warn!(%error, "narsor-web: the server did not answer");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

fn reqwest_method(method: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

/// The server's answer, passed back as it arrives.
fn relay(answer: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(answer.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for name in RESPONSE_HEADERS {
        if let Some(value) = answer.headers().get(&name) {
            if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
                response = response.header(name, value);
            }
        }
    }
    response
        .body(Body::from_stream(answer.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_routes_of_the_interface_are_proxied() {
        for path in [
            "/control/v1/tasks",
            "/settings/agent-harness",
            "/auth/v1/whoami",
            "/tasks",
        ] {
            assert!(is_proxied(path), "{path}");
        }
        for path in [
            "/auth/v1/claim",
            "/executions/x",
            "/control/v1",
            "/tasks/x",
            "/metrics",
        ] {
            assert!(!is_proxied(path), "{path}");
        }
    }

    #[test]
    fn a_dot_segment_cannot_leave_the_allowlist() {
        for raw in [
            "/control/v1/%2e%2e/%2e%2e/auth/v1/claim",
            "/control/v1/%2E%2E/%2E%2E/executions",
            "/control/v1/../../auth/v1/claim",
            "/settings/%2e%2e/auth/v1/claim",
            "/control/v1/..%5c..%5cauth",
            "/control/v1\\..\\auth",
            "//evil.example/control/v1/tasks",
        ] {
            assert_eq!(checked_path(raw), None, "{raw}");
        }
        assert_eq!(
            checked_path("/control/v1/tasks?archived=false").as_deref(),
            Some("/control/v1/tasks?archived=false")
        );
        assert_eq!(
            checked_path("/control/v1/events/stream?cursor=a%3A1%2Cb%3A2").as_deref(),
            Some("/control/v1/events/stream?cursor=a%3A1%2Cb%3A2")
        );
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
