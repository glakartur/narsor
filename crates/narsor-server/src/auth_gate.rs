//! The HTTP side of authentication: the owner gate, the claim and identity
//! endpoints, the trusted-proxy policy, CORS and the stream guard.
//!
//! # Refusals
//!
//! Like every other gate of this API, refusals carry no body: `401` when the
//! caller is not authenticated (no token, a wrong one, an expired one, an
//! untrusted issuer — indistinguishable on purpose), `403` when an
//! authenticated identity is not the installation owner. Neither reveals
//! anything about tasks.
//!
//! # Streams end with the credential
//!
//! A request is authorized once, when it arrives; an SSE stream then lives
//! on. So the gate wraps every event-stream response it lets through: the
//! stream ends when the token that opened it expires, and as soon as its
//! identity stops being the owner (R33's rebinding). A former owner keeps
//! nothing — reconnecting with an old cursor goes through the gate again.

use std::future::Future as _;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures::Stream;
use tower_http::cors::{AllowOrigin, CorsLayer};

use narsor_engine::controller_protocol::credentials::constant_time_eq;
use narsor_engine::controller_protocol::LOCAL_OWNER;
use narsor_engine::orchestration::OwnerRef;

use narsor_engine::auth::oidc::{OidcIdentity, OidcVerifier, VerifiedToken};
use narsor_engine::auth::owner::{ClaimOutcome, OwnerRecord, OwnerStore};
use narsor_engine::auth::settings::{CorsPolicy, ProxyPolicy};

pub use narsor_engine::auth::{ClaimRequest, OwnerBindingSource, OwnerView, WhoAmIResponse};

/// How often an open stream re-checks that its identity is still the owner.
const STREAM_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The header a TLS-terminating proxy sets to the scheme the client used.
const FORWARDED_PROTO: &str = "x-forwarded-proto";

/// Marks a request whose caller may administer the installation: enroll,
/// list and revoke controllers. In v1 that is exactly the owner (R38); the
/// marker keeps the role separate from ownership of a task so it can
/// diverge later (R33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operator;

/// Who the owner gate let through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// Trusted mode's local token holder.
    LocalOperator,
    /// Network mode's installation owner.
    Owner {
        /// The verified identity.
        identity: OidcIdentity,
        /// When the token it presented expires.
        expires_at: DateTime<Utc>,
    },
}

/// Network mode's user authentication: the token verifier and the owner.
#[derive(Debug)]
pub struct NetworkAuth {
    verifier: OidcVerifier,
    owners: OwnerStore,
}

impl NetworkAuth {
    /// Tokens verified by `verifier`, the owner kept by `owners`.
    pub fn new(verifier: OidcVerifier, owners: OwnerStore) -> Self {
        Self { verifier, owners }
    }

    /// The token verifier.
    pub fn verifier(&self) -> &OidcVerifier {
        &self.verifier
    }

    /// The installation owner.
    pub fn owners(&self) -> &OwnerStore {
        &self.owners
    }

    /// Verifies a bearer token. It takes the token by value, copied out of
    /// the request beforehand: a request body is not `Sync`, so a borrow of
    /// the request must not live across the verification's await.
    async fn authenticate(&self, token: Option<String>) -> Option<VerifiedToken> {
        match self.verifier.verify(&token?).await {
            Ok(verified) => Some(verified),
            Err(error) => {
                tracing::debug!(error = %error, "auth: refused a token");
                None
            }
        }
    }
}

/// How owner routes authenticate.
#[derive(Debug, Clone)]
pub enum OwnerAuth {
    /// The process's random token (trusted mode).
    Trusted {
        /// The token every request must present.
        token: String,
    },
    /// An OIDC token of the installation owner (network mode).
    Network(Arc<NetworkAuth>),
}

impl OwnerAuth {
    /// The mode's name, as clients see it.
    pub fn mode(&self) -> &'static str {
        match self {
            OwnerAuth::Trusted { .. } => narsor_engine::config::settings::SERVER_MODE_TRUSTED,
            OwnerAuth::Network(_) => narsor_engine::config::settings::SERVER_MODE_NETWORK,
        }
    }
}

fn bearer(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// The owner gate, in front of every owner route.
///
/// An accepted request carries the caller's [`OwnerRef`] (which every
/// control-plane route scopes its tasks to), its [`Principal`] and — the
/// owner being the operator in v1 — [`Operator`].
pub async fn require_owner(
    State(auth): State<OwnerAuth>,
    request: Request,
    next: Next,
) -> Response {
    match auth {
        OwnerAuth::Trusted { token } => {
            let expected = format!("Bearer {token}");
            let presented = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok());
            match presented {
                Some(value) if constant_time_eq(value.as_bytes(), expected.as_bytes()) => {
                    let mut request = request;
                    let extensions = request.extensions_mut();
                    extensions.insert(OwnerRef::new(LOCAL_OWNER));
                    extensions.insert(Principal::LocalOperator);
                    extensions.insert(Operator);
                    next.run(request).await
                }
                _ => StatusCode::UNAUTHORIZED.into_response(),
            }
        }
        OwnerAuth::Network(network) => {
            let Some(verified) = network
                .authenticate(bearer(&request).map(str::to_owned))
                .await
            else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            if !network.owners.is_owner(&verified.identity) {
                return StatusCode::FORBIDDEN.into_response();
            }
            let mut request = request;
            let extensions = request.extensions_mut();
            extensions.insert(verified.identity.owner_ref());
            extensions.insert(Principal::Owner {
                identity: verified.identity.clone(),
                expires_at: verified.expires_at,
            });
            extensions.insert(Operator);
            let response = next.run(request).await;
            if is_event_stream(&response) {
                let identity = verified.identity;
                guard_stream(response, verified.expires_at, move || {
                    network.owners.is_owner(&identity)
                })
            } else {
                response
            }
        }
    }
}

fn is_event_stream(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

/// Ends `response`'s body at `expires_at`, or as soon as `still_valid`
/// turns false (checked every [`STREAM_RECHECK_INTERVAL`]).
pub fn guard_stream(
    response: Response,
    expires_at: DateTime<Utc>,
    still_valid: impl Fn() -> bool + Send + Sync + 'static,
) -> Response {
    let remaining = (expires_at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
    let (parts, body) = response.into_parts();
    let guarded = GuardedStream {
        inner: Box::pin(body.into_data_stream()),
        deadline: Box::pin(tokio::time::sleep(remaining)),
        recheck: tokio::time::interval_at(
            tokio::time::Instant::now() + STREAM_RECHECK_INTERVAL,
            STREAM_RECHECK_INTERVAL,
        ),
        still_valid: Box::new(still_valid),
        done: false,
    };
    Response::from_parts(parts, Body::from_stream(guarded))
}

type DataStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

struct GuardedStream {
    inner: DataStream,
    deadline: Pin<Box<tokio::time::Sleep>>,
    recheck: tokio::time::Interval,
    still_valid: Box<dyn Fn() -> bool + Send + Sync>,
    done: bool,
}

impl Stream for GuardedStream {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            self.done = true;
            return Poll::Ready(None);
        }
        while self.recheck.poll_tick(cx).is_ready() {
            if !(self.still_valid)() {
                self.done = true;
                return Poll::Ready(None);
            }
        }
        self.inner.as_mut().poll_next(cx)
    }
}

// ---------------------------------------------------------------------------
// Claim and identity
// ---------------------------------------------------------------------------

fn owner_view(record: OwnerRecord) -> OwnerView {
    OwnerView {
        issuer: record.issuer,
        subject: record.subject,
        bound_by: record.bound_by,
        bound_at: record.bound_at,
        revision: record.revision,
    }
}

/// The identity routes, each authenticating on its own: a caller who is
/// not (yet) the owner must still be able to claim and to ask who it is.
pub fn auth_routes<S>(auth: OwnerAuth) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/auth/v1/claim", post(post_claim))
        .route("/auth/v1/whoami", get(get_whoami))
        .with_state(auth)
}

async fn post_claim(
    State(auth): State<OwnerAuth>,
    request: Request,
) -> Result<Json<OwnerView>, StatusCode> {
    let OwnerAuth::Network(network) = auth else {
        // Trusted mode has no owner to claim.
        return Err(StatusCode::NOT_FOUND);
    };
    let verified = network
        .authenticate(bearer(&request).map(str::to_owned))
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let body = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let claim: ClaimRequest = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    match network
        .owners
        .claim(&verified.identity, claim.claim_code.trim())
    {
        Ok(ClaimOutcome::Claimed(record)) => {
            tracing::info!(subject = %record.subject, "auth: the installation was claimed");
            Ok(Json(owner_view(record)))
        }
        Ok(ClaimOutcome::AlreadyOwner(record)) => Ok(Json(owner_view(record))),
        Ok(ClaimOutcome::Refused) => Err(StatusCode::FORBIDDEN),
        Err(error) => {
            tracing::warn!(error = %error, "auth: a claim could not be recorded");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_whoami(
    State(auth): State<OwnerAuth>,
    request: Request,
) -> Result<Json<WhoAmIResponse>, StatusCode> {
    match &auth {
        OwnerAuth::Trusted { token } => {
            let presented = bearer(&request).unwrap_or_default();
            if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok(Json(WhoAmIResponse {
                mode: auth.mode().to_string(),
                issuer: None,
                subject: None,
                owner: true,
                claimed: true,
                expires_at: None,
            }))
        }
        OwnerAuth::Network(network) => {
            let verified = network
                .authenticate(bearer(&request).map(str::to_owned))
                .await
                .ok_or(StatusCode::UNAUTHORIZED)?;
            Ok(Json(WhoAmIResponse {
                mode: auth.mode().to_string(),
                owner: network.owners.is_owner(&verified.identity),
                claimed: network.owners.is_claimed(),
                issuer: Some(verified.identity.issuer),
                subject: Some(verified.identity.subject),
                expires_at: Some(verified.expires_at),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Transport policy
// ---------------------------------------------------------------------------

/// The TLS-termination policy, in front of everything in network mode: a
/// bare `403` for a peer that is not a trusted proxy, or a request the proxy
/// does not say arrived over HTTPS.
pub async fn require_trusted_proxy(
    State(policy): State<Arc<ProxyPolicy>>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip());
    let https = request
        .headers()
        .get(FORWARDED_PROTO)
        .and_then(|value| value.to_str().ok())
        // A chain of proxies lists one scheme per hop, and a proxy that
        // appends lets the client forge the leading entries. Only a list in
        // which every hop is https cannot have been downgraded anywhere.
        .is_some_and(|value| {
            value
                .split(',')
                .all(|scheme| scheme.trim().eq_ignore_ascii_case("https"))
        });
    match peer {
        Some(peer) if policy.allows(peer) && https => next.run(request).await,
        _ => StatusCode::FORBIDDEN.into_response(),
    }
}

/// The CORS layer of `policy`, or `None` when it denies everything: then no
/// CORS header is ever sent and browsers refuse every cross-origin call.
pub fn cors_layer(policy: &CorsPolicy) -> Option<CorsLayer> {
    if policy.allowed_origins.is_empty() {
        return None;
    }
    let origins: Vec<HeaderValue> = policy
        .allowed_origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                header::HeaderName::from_static("last-event-id"),
            ]),
    )
}
