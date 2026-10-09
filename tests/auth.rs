//! Integration tests for U7: the trusted and network modes, OIDC owner
//! authentication with the one-time claim, per-task owner authorization,
//! controller proof-of-possession, and streams that end with the credential
//! that opened them.
//!
//! Nothing reaches the network. Network-mode servers trust an in-process
//! mock OIDC issuer (discovery document and JWKS on a loopback port) whose
//! keys the test generates; requests come from loopback, which the servers
//! list as their TLS-terminating proxy, and carry the `X-Forwarded-Proto`
//! such a proxy sets.

mod environment_support;

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::Utc;
use futures::StreamExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use narsor::api::{self, ApiServices, ClientError, DiscoveryError, ServerSecurity};
use narsor::auth::{
    parse_proxy, ControllerCredential, CorsPolicy, NetworkAuth, OidcIdentity, OidcSettings,
    OidcVerifier, OwnerBindingSource, OwnerStore, ProxyPolicy, SecuritySettings, StartupOwnership,
};
use narsor::config::Flags;
use narsor::controller_protocol::{
    ControllerClient, CreateEnrollmentRequest, CreateTaskRequest, LaunchRequest, ProtocolService,
    ReportRequest, RunnerClient, RunnerCredentialRequest,
};
use narsor::domain::{ExecutionStatus, TaskId};
use narsor::engine::Engine;
use narsor::orchestration::{
    ControlPlane, ConversationAvailability, ExecutionCursor, Observation, ObservedSummary,
    OwnerRef, ResourceIdentity, RunnerSnapshot,
};
use narsor::persistence::paths::StatePaths;
use narsor::persistence::{DaemonInfo, ExecutionStore};
use reqwest::{Method, StatusCode};

/// The audience every network-mode server here demands.
const AUDIENCE: &str = "narsor-api";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-auth-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// One signing key of the mock issuer and its published JWK.
#[derive(Clone)]
struct IssuerKey {
    kid: String,
    alg: Algorithm,
    encoding: EncodingKey,
    jwk: serde_json::Value,
    /// The raw public key bytes, for the algorithm-confusion attack.
    public: Vec<u8>,
}

impl IssuerKey {
    fn es256(kid: &str) -> Self {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        use p256::pkcs8::EncodePrivateKey;
        let secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let der = secret.to_pkcs8_der().expect("pkcs8");
        let point = secret.public_key().to_encoded_point(false);
        Self {
            kid: kid.to_string(),
            alg: Algorithm::ES256,
            encoding: EncodingKey::from_ec_der(der.as_bytes()),
            jwk: serde_json::json!({
                "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
                "x": b64(point.x().expect("x")), "y": b64(point.y().expect("y")),
            }),
            public: point.as_bytes().to_vec(),
        }
    }

    fn eddsa(kid: &str) -> Self {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let der = signing.to_pkcs8_der().expect("pkcs8");
        let public = signing.verifying_key().as_bytes().to_vec();
        Self {
            kid: kid.to_string(),
            alg: Algorithm::EdDSA,
            encoding: EncodingKey::from_ed_der(der.as_bytes()),
            jwk: serde_json::json!({
                "kty": "OKP", "crv": "Ed25519", "use": "sig", "alg": "EdDSA", "kid": kid,
                "x": b64(&public),
            }),
            public,
        }
    }
}

#[derive(Clone)]
struct IssuerState {
    url: String,
    keys: Arc<Mutex<Vec<serde_json::Value>>>,
    jwks_hits: Arc<AtomicUsize>,
}

/// An OIDC issuer on a loopback port: discovery and JWKS only.
struct MockIssuer {
    state: IssuerState,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockIssuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockIssuer {
    async fn start(keys: &[&IssuerKey]) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the issuer");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let state = IssuerState {
            url,
            keys: Arc::new(Mutex::new(keys.iter().map(|key| key.jwk.clone()).collect())),
            jwks_hits: Arc::new(AtomicUsize::new(0)),
        };
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the issuer");
        });
        Self { state, task }
    }

    fn url(&self) -> &str {
        &self.state.url
    }

    /// Publishes another key: what the issuer does when it rotates.
    fn publish(&self, key: &IssuerKey) {
        self.state.keys.lock().unwrap().push(key.jwk.clone());
    }

    fn jwks_hits(&self) -> usize {
        self.state.jwks_hits.load(Ordering::SeqCst)
    }

    /// Standard claims for `subject`, valid for five minutes.
    fn claims(&self, subject: &str) -> serde_json::Value {
        let now = Utc::now().timestamp();
        serde_json::json!({
            "iss": self.url(), "sub": subject, "aud": AUDIENCE,
            "iat": now, "nbf": now, "exp": now + 300,
        })
    }

    fn token(&self, key: &IssuerKey, subject: &str) -> String {
        sign(key, &self.claims(subject))
    }

    fn token_expiring_in(&self, key: &IssuerKey, subject: &str, secs: i64) -> String {
        let mut claims = self.claims(subject);
        claims["exp"] = serde_json::json!(Utc::now().timestamp() + secs);
        sign(key, &claims)
    }

    fn identity(&self, subject: &str) -> OidcIdentity {
        OidcIdentity::new(self.url(), subject)
    }
}

async fn discovery(State(state): State<IssuerState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "issuer": state.url,
        "jwks_uri": format!("{}/jwks", state.url),
    }))
}

async fn jwks(State(state): State<IssuerState>) -> Json<serde_json::Value> {
    state.jwks_hits.fetch_add(1, Ordering::SeqCst);
    let keys = state.keys.lock().unwrap().clone();
    Json(serde_json::json!({ "keys": keys }))
}

fn sign(key: &IssuerKey, claims: &serde_json::Value) -> String {
    let mut header = Header::new(key.alg);
    header.kid = Some(key.kid.clone());
    jsonwebtoken::encode(&header, claims, &key.encoding).expect("sign a token")
}

/// Loopback as the trusted proxy: the test client plays the proxy.
fn loopback_proxy() -> ProxyPolicy {
    ProxyPolicy {
        trusted: vec![parse_proxy("127.0.0.1").unwrap()],
    }
}

/// A network-mode server and what a test needs to look behind it.
struct NetworkServer {
    root: TempRoot,
    handle: api::ApiHandle,
    auth: Arc<NetworkAuth>,
    service: Arc<ProtocolService>,
    /// The claim code, when the installation started unclaimed.
    claim_code: Option<String>,
    http: reqwest::Client,
}

struct NetworkOptions {
    owner_subject: Option<String>,
    proxy: ProxyPolicy,
    cors: CorsPolicy,
    min_refresh_interval: Duration,
    /// Clock-skew leeway of token validation; `None` keeps the default.
    leeway: Option<Duration>,
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            owner_subject: None,
            proxy: loopback_proxy(),
            cors: CorsPolicy::deny(),
            min_refresh_interval: Duration::from_secs(60),
            leeway: None,
        }
    }
}

impl NetworkServer {
    async fn start(issuer: &MockIssuer, options: NetworkOptions) -> Self {
        let root = TempRoot::new();
        let paths = StatePaths::new(root.path());
        let defaults = OidcSettings::new(issuer.url(), AUDIENCE);
        let verifier = OidcVerifier::new(OidcSettings {
            min_refresh_interval: options.min_refresh_interval,
            leeway: options.leeway.unwrap_or(defaults.leeway),
            ..defaults
        })
        .expect("verifier");
        let (owners, ownership) =
            OwnerStore::open(&paths, issuer.url(), options.owner_subject.as_deref())
                .expect("owner store");
        let claim_code = match ownership {
            StartupOwnership::Unclaimed { code, .. } => Some(code.expose().to_string()),
            StartupOwnership::Owned(_) => None,
        };
        let auth = Arc::new(NetworkAuth::new(verifier, owners));
        let service = Arc::new(ProtocolService::new(
            ControlPlane::new(paths),
            environment_support::seeded_resolver(root.path()),
        ));
        let handle = api::serve(
            Arc::new(Engine::new(ExecutionStore::new(root.path()))),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ApiServices {
                control: Some(Arc::clone(&service)),
                ..ApiServices::default()
            },
            ServerSecurity::Network {
                bind: SocketAddr::from(([127, 0, 0, 1], 0)),
                auth: Arc::clone(&auth),
                proxy: options.proxy,
                cors: options.cors,
            },
        )
        .await
        .expect("network server starts");
        Self {
            root,
            handle,
            auth,
            service,
            claim_code,
            http: reqwest::Client::new(),
        }
    }

    async fn owned_by(issuer: &MockIssuer, subject: &str) -> Self {
        Self::start(
            issuer,
            NetworkOptions {
                owner_subject: Some(subject.to_string()),
                ..NetworkOptions::default()
            },
        )
        .await
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.handle.addr())
    }

    /// A request as the TLS-terminating proxy forwards it.
    fn request(&self, method: Method, path: &str, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, self.url(path))
            .header("x-forwarded-proto", "https");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let mut request = self.request(method, path, token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("request");
        let status = response.status();
        (status, response.text().await.expect("body"))
    }

    async fn create_task(&self, token: &str, task: &str) -> StatusCode {
        let body = serde_json::to_value(create_request(task, "k8s")).unwrap();
        self.call(Method::POST, "/control/v1/tasks", Some(token), Some(body))
            .await
            .0
    }

    async fn claim(&self, token: &str, code: &str) -> StatusCode {
        self.call(
            Method::POST,
            "/auth/v1/claim",
            Some(token),
            Some(serde_json::json!({ "claim_code": code })),
        )
        .await
        .0
    }
}

fn create_request(task: &str, backend: &str) -> CreateTaskRequest {
    CreateTaskRequest {
        task_id: Some(task.to_string()),
        backend_id: Some(backend.to_string()),
        launch: LaunchRequest {
            workflow_id: "implement-feature".to_string(),
            workflow_version: 1,
            inputs: serde_json::json!({ "ticket": "NAR-1" }),
            workspace: None,
            profile: None,
            harness: None,
            resources: None,
        },
    }
}

fn input_body() -> serde_json::Value {
    serde_json::json!({
        "pending_input_id": "episode-1",
        "idempotency_key": "input-1",
        "delivery": { "comment": "approve" },
    })
}

/// Reads an SSE response until it ends or `limit` passes; whether it ended.
async fn ends_within(response: reqwest::Response, limit: Duration) -> bool {
    let mut body = response.bytes_stream();
    tokio::time::timeout(limit, async {
        while let Some(chunk) = body.next().await {
            if chunk.is_err() {
                break;
            }
        }
    })
    .await
    .is_ok()
}

// ---------------------------------------------------------------------------
// Trusted mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trusted_mode_refuses_a_non_loopback_bind_and_keeps_the_local_token() {
    for refused in [
        "0.0.0.0:0",
        "[::]:0",
        "[::ffff:127.0.0.1]:0",
        "192.0.2.10:0",
    ] {
        let settings = SecuritySettings {
            bind: refused.to_string(),
            ..SecuritySettings::trusted_loopback()
        };
        let error = settings.validate().expect_err(refused);
        assert!(error.to_string().contains("loopback"), "{refused}: {error}");
    }
    // Even a hand-built configuration cannot expose the token-only API.
    let root = TempRoot::new();
    for refused in ["0.0.0.0:0", "[::ffff:127.0.0.1]:0"] {
        let started = api::serve(
            Arc::new(Engine::new(ExecutionStore::new(root.path()))),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ApiServices::default(),
            ServerSecurity::Trusted {
                bind: refused.parse().unwrap(),
                cors: CorsPolicy::deny(),
            },
        )
        .await;
        assert!(started.is_err(), "{refused} must be refused");
    }

    // Loopback keeps today's behaviour: the process token, nothing else.
    let handle = api::start(
        Arc::new(Engine::new(ExecutionStore::new(root.path()))),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await
    .expect("trusted server");
    assert!(handle.addr().ip().is_loopback());
    assert!(handle.client().list_executions().await.is_ok());
    let stranger = api::ApiClient::new(handle.addr(), "not-the-token");
    assert!(matches!(
        stranger.list_executions().await,
        Err(ClientError::Unauthorized)
    ));
}

#[tokio::test]
async fn a_discovery_file_others_can_read_is_not_trusted() {
    let root = TempRoot::new();
    let paths = StatePaths::new(root.path());
    let info = DaemonInfo {
        pid: std::process::id(),
        addr: "127.0.0.1:9".parse().unwrap(),
        token: "secret".to_string(),
        started_at: Utc::now(),
    };
    info.write(&paths.daemon_file()).expect("write");
    fs::set_permissions(paths.daemon_file(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        api::ensure_daemon_running(&paths, &Flags::default()).await,
        Err(DiscoveryError::Unprotected(_))
    ));
    assert!(
        paths.daemon_file().exists(),
        "not this client's file to remove"
    );

    // A network-mode daemon publishes no local token.
    DaemonInfo {
        token: String::new(),
        ..info
    }
    .write(&paths.daemon_file())
    .expect("write");
    assert!(matches!(
        api::ensure_daemon_running(&paths, &Flags::default()).await,
        Err(DiscoveryError::NetworkMode)
    ));
}

// ---------------------------------------------------------------------------
// Network mode: owner and claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_token_claims_an_unowned_installation_once() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::start(&issuer, NetworkOptions::default()).await;
    let code = server.claim_code.clone().expect("unclaimed at start");
    let paths = StatePaths::new(server.root.path());
    let mode = fs::metadata(paths.claim_code_file())
        .expect("the code is stored for the operator")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        fs::read_to_string(paths.claim_code_file()).unwrap().trim(),
        code
    );

    let alice = issuer.token(&key, "alice");
    let bob = issuer.token(&key, "bob");

    // Unclaimed: a valid identity has no access yet.
    let (status, body) = server
        .call(Method::GET, "/control/v1/tasks", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.is_empty());
    assert_eq!(
        server.claim(&alice, "nsclaim-wrong").await,
        StatusCode::FORBIDDEN
    );

    assert_eq!(server.claim(&alice, &code).await, StatusCode::OK);
    assert!(!paths.claim_code_file().exists(), "the code is destroyed");
    assert!(!paths.claim_file().exists());
    assert_eq!(
        server.claim(&alice, &code).await,
        StatusCode::OK,
        "a retry by the owner is harmless"
    );
    assert_eq!(server.claim(&bob, &code).await, StatusCode::FORBIDDEN);

    assert_eq!(
        server
            .call(Method::GET, "/control/v1/tasks", Some(&alice), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        server
            .call(Method::GET, "/control/v1/tasks", Some(&bob), None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (_, whoami) = server
        .call(Method::GET, "/auth/v1/whoami", Some(&bob), None)
        .await;
    let whoami: serde_json::Value = serde_json::from_str(&whoami).unwrap();
    assert_eq!(whoami["owner"], false);
    assert_eq!(whoami["claimed"], true);
    let owner = server.auth.owners().owner().expect("owned");
    assert_eq!(owner.identity(), issuer.identity("alice"));
    assert_eq!(owner.bound_by, OwnerBindingSource::Claim);
}

#[tokio::test]
async fn a_concurrent_claim_has_exactly_one_winner() {
    let key = IssuerKey::eddsa("key-1");
    let issuer = Arc::new(MockIssuer::start(&[&key]).await);
    let server = Arc::new(NetworkServer::start(&issuer, NetworkOptions::default()).await);
    let code = server.claim_code.clone().unwrap();

    let claims = (0..8).map(|index| {
        let server = Arc::clone(&server);
        let token = issuer.token(&key, &format!("user-{index}"));
        let code = code.clone();
        tokio::spawn(async move { (index, server.claim(&token, &code).await) })
    });
    let outcomes = futures::future::join_all(claims).await;
    let winners: Vec<_> = outcomes
        .into_iter()
        .map(|outcome| outcome.expect("join"))
        .filter(|(_, status)| *status == StatusCode::OK)
        .collect();
    assert_eq!(winners.len(), 1, "exactly one claim wins: {winners:?}");
    let owner = server.auth.owners().owner().unwrap();
    assert_eq!(owner.subject, format!("user-{}", winners[0].0));
}

#[tokio::test]
async fn a_second_identity_cannot_read_or_mutate_the_owners_tasks() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::owned_by(&issuer, "alice").await;
    assert!(server.claim_code.is_none(), "bound by configuration");
    let alice = issuer.token(&key, "alice");
    let mallory = issuer.token(&key, "mallory");
    assert_eq!(
        server.create_task(&alice, "task-1").await,
        StatusCode::CREATED
    );

    let attempts = [
        (Method::GET, "/control/v1/tasks", None),
        (Method::GET, "/control/v1/tasks/task-1", None),
        (Method::GET, "/control/v1/tasks/task-1/conversation", None),
        (Method::GET, "/control/v1/tasks/task-1/events", None),
        (Method::GET, "/control/v1/events/stream", None),
        (
            Method::POST,
            "/control/v1/tasks/task-1/commands",
            Some(serde_json::json!({ "command": "pause", "idempotency_key": "k" })),
        ),
        (
            Method::POST,
            "/control/v1/tasks/task-1/input",
            Some(input_body()),
        ),
        (
            Method::POST,
            "/control/v1/tasks",
            Some(serde_json::to_value(create_request("task-1", "k8s")).unwrap()),
        ),
        (
            Method::POST,
            "/control/v1/backends/k8s/enrollments",
            Some(serde_json::json!({})),
        ),
        (Method::GET, "/control/v1/controllers", None),
        (Method::GET, "/control/v1/capabilities", None),
        (Method::GET, "/control/v1/workflows", None),
        (
            Method::POST,
            "/control/v1/workflows/validate",
            Some(serde_json::json!({ "path": "/etc/passwd.yaml" })),
        ),
        (Method::GET, "/control/v1/repositories", None),
        (
            Method::POST,
            "/control/v1/repositories",
            Some(serde_json::json!({ "id": "x", "remote": "https://example.com/x.git" })),
        ),
        (Method::DELETE, "/control/v1/repositories/x", None),
        (Method::GET, "/control/v1/tasks?archived=true", None),
        (Method::GET, "/control/v1/executions", None),
        (
            Method::POST,
            "/control/v1/executions/exec-1/recover",
            Some(serde_json::json!({})),
        ),
        (Method::GET, "/executions", None),
        (Method::GET, "/events/stream", None),
    ];
    for (method, path, body) in attempts {
        let (status, text) = server
            .call(method.clone(), path, Some(&mallory), body)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(text.is_empty(), "{method} {path} revealed: {text}");
    }
    let task = server
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(task.generation, 1, "nothing was mutated");
    assert!(task.inputs.is_empty());

    // Inside the gate, a task of another owner is indistinguishable from
    // none — even when creating one under its id.
    let other = OwnerRef::new("someone-else");
    assert!(matches!(
        server
            .service
            .create_task(&other, create_request("task-1", "k8s")),
        Err(narsor::controller_protocol::ProtocolError::NotFound)
    ));
    assert!(matches!(
        server.service.task(&other, "task-1"),
        Err(narsor::controller_protocol::ProtocolError::NotFound)
    ));
}

#[tokio::test]
async fn invalid_tokens_are_refused_without_revealing_anything() {
    let key = IssuerKey::es256("key-1");
    let ed_key = IssuerKey::eddsa("key-ed");
    let issuer = MockIssuer::start(&[&key, &ed_key]).await;
    let server = NetworkServer::owned_by(&issuer, "alice").await;
    let alice = issuer.token(&key, "alice");
    assert_eq!(
        server.create_task(&alice, "task-1").await,
        StatusCode::CREATED
    );
    let now = Utc::now().timestamp();

    let with = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut claims = issuer.claims("alice");
        change(&mut claims);
        sign(&key, &claims)
    };
    let wrong_issuer = with(&|claims| claims["iss"] = "https://evil.example".into());
    let wrong_audience = with(&|claims| claims["aud"] = "another-app".into());
    let expired = with(&|claims| claims["exp"] = (now - 600).into());
    let not_yet_valid = with(&|claims| claims["nbf"] = (now + 600).into());
    let issued_in_future = with(&|claims| claims["iat"] = (now + 600).into());
    let no_subject = with(&|claims| {
        claims.as_object_mut().unwrap().remove("sub");
    });
    // HS256 "signed" with the public key as the secret: the classic
    // algorithm-confusion attack.
    let mut hs_header = Header::new(Algorithm::HS256);
    hs_header.kid = Some(key.kid.clone());
    let confused = jsonwebtoken::encode(
        &hs_header,
        &issuer.claims("alice"),
        &EncodingKey::from_secret(&key.public),
    )
    .unwrap();
    let unsigned = format!(
        "{}.{}.",
        b64(br#"{"alg":"none","typ":"JWT"}"#),
        b64(issuer.claims("alice").to_string().as_bytes())
    );
    // A real EdDSA signature presented under the ES256 key's id: the key
    // was not published for that algorithm.
    let mut mismatched_header = Header::new(Algorithm::EdDSA);
    mismatched_header.kid = Some(key.kid.clone());
    let mismatched = jsonwebtoken::encode(
        &mismatched_header,
        &issuer.claims("alice"),
        &ed_key.encoding,
    )
    .unwrap();
    let unknown_key = sign(
        &IssuerKey::es256("key-unpublished"),
        &issuer.claims("alice"),
    );
    let tampered = {
        let mut parts: Vec<String> = alice.split('.').map(str::to_string).collect();
        let mut claims = issuer.claims("mallory");
        claims["sub"] = "alice".into();
        claims["extra"] = "x".into();
        parts[1] = b64(claims.to_string().as_bytes());
        parts.join(".")
    };

    let refused = [
        ("wrong issuer", wrong_issuer),
        ("wrong audience", wrong_audience),
        ("expired", expired),
        ("not yet valid", not_yet_valid),
        ("issued in the future", issued_in_future),
        ("no subject", no_subject),
        ("HS256 with the public key", confused),
        ("alg none", unsigned),
        ("algorithm not the key's", mismatched),
        ("unknown key", unknown_key),
        ("tampered claims", tampered),
        ("garbage", "not-a-jwt".to_string()),
    ];
    for (what, token) in refused {
        for path in [
            "/control/v1/tasks",
            "/control/v1/tasks/task-1",
            "/auth/v1/whoami",
        ] {
            let (status, body) = server.call(Method::GET, path, Some(&token), None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{what} on {path}");
            assert!(body.is_empty(), "{what} on {path} revealed: {body}");
        }
    }
    let (status, body) = server
        .call(Method::GET, "/control/v1/tasks", None, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.is_empty());

    // Sanity: the owner's own tokens work, with either published key.
    for token in [alice, issuer.token(&ed_key, "alice")] {
        assert_eq!(
            server
                .call(Method::GET, "/control/v1/tasks/task-1", Some(&token), None)
                .await
                .0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn rotated_keys_are_fetched_but_unknown_ids_only_once_per_interval() {
    let first = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&first]).await;
    let server = NetworkServer::start(
        &issuer,
        NetworkOptions {
            owner_subject: Some("alice".to_string()),
            min_refresh_interval: Duration::from_secs(1),
            ..NetworkOptions::default()
        },
    )
    .await;
    let whoami = |token: String| {
        let server = &server;
        async move {
            server
                .call(Method::GET, "/auth/v1/whoami", Some(&token), None)
                .await
                .0
        }
    };

    assert_eq!(whoami(issuer.token(&first, "alice")).await, StatusCode::OK);
    assert_eq!(issuer.jwks_hits(), 1);
    assert_eq!(whoami(issuer.token(&first, "alice")).await, StatusCode::OK);
    assert_eq!(issuer.jwks_hits(), 1, "keys are cached");

    // A flood of unknown key ids right after a fetch causes no refetch.
    let bogus = IssuerKey::es256("key-bogus");
    for _ in 0..5 {
        assert_eq!(
            whoami(issuer.token(&bogus, "alice")).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(issuer.jwks_hits(), 1, "refetches are rate-limited");

    // The issuer rotates; once the interval passed, the new key is fetched.
    let second = IssuerKey::eddsa("key-2");
    issuer.publish(&second);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(whoami(issuer.token(&second, "alice")).await, StatusCode::OK);
    assert_eq!(issuer.jwks_hits(), 2);
    for _ in 0..5 {
        whoami(issuer.token(&bogus, "alice")).await;
    }
    assert_eq!(
        issuer.jwks_hits(),
        2,
        "still at most one fetch per interval"
    );
}

// ---------------------------------------------------------------------------
// Network mode: transport policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn network_mode_serves_only_https_requests_from_trusted_proxies() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let alice = issuer.token(&key, "alice");

    let server = NetworkServer::owned_by(&issuer, "alice").await;
    let plain = |proto: Option<&'static str>| {
        let mut request = server
            .http
            .get(server.url("/control/v1/tasks"))
            .bearer_auth(&alice);
        if let Some(proto) = proto {
            request = request.header("x-forwarded-proto", proto);
        }
        request.send()
    };
    assert_eq!(plain(None).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        plain(Some("http")).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(plain(Some("https")).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        plain(Some("https, http")).await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "a client-forged leading https cannot hide a plain-http hop"
    );
    assert_eq!(
        plain(Some("https, https")).await.unwrap().status(),
        StatusCode::OK
    );

    let elsewhere = NetworkServer::start(
        &issuer,
        NetworkOptions {
            owner_subject: Some("alice".to_string()),
            proxy: ProxyPolicy {
                trusted: vec![parse_proxy("10.0.0.0/8").unwrap()],
            },
            ..NetworkOptions::default()
        },
    )
    .await;
    assert_eq!(
        elsewhere
            .call(Method::GET, "/control/v1/tasks", Some(&alice), None)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "a peer that is not a trusted proxy is refused"
    );
}

#[tokio::test]
async fn cors_is_denied_by_default_and_exact_when_configured() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let alice = issuer.token(&key, "alice");
    let preflight = |server: &NetworkServer, origin: &str| {
        server
            .request(Method::OPTIONS, "/control/v1/tasks", None)
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization")
            .send()
    };
    let allowed = |response: &reqwest::Response| {
        response
            .headers()
            .get("access-control-allow-origin")
            .map(|value| value.to_str().unwrap().to_string())
    };

    let closed = NetworkServer::owned_by(&issuer, "alice").await;
    let response = closed
        .request(Method::GET, "/control/v1/tasks", Some(&alice))
        .header("origin", "https://app.example")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed(&response), None);
    assert_eq!(
        allowed(&preflight(&closed, "https://app.example").await.unwrap()),
        None
    );

    let open = NetworkServer::start(
        &issuer,
        NetworkOptions {
            owner_subject: Some("alice".to_string()),
            cors: CorsPolicy {
                allowed_origins: vec!["https://app.example".to_string()],
            },
            ..NetworkOptions::default()
        },
    )
    .await;
    assert_eq!(
        allowed(&preflight(&open, "https://app.example").await.unwrap()).as_deref(),
        Some("https://app.example")
    );
    assert_eq!(
        allowed(&preflight(&open, "https://evil.example").await.unwrap()),
        None
    );
}

// ---------------------------------------------------------------------------
// Network mode: streams and former owners
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_active_stream_ends_when_its_token_expires() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::owned_by(&issuer, "alice").await;
    let mut short = String::new();
    for path in ["/control/v1/events/stream", "/events/stream"] {
        // `exp` has whole seconds: this token lives for 2 to 3 more seconds.
        short = issuer.token_expiring_in(&key, "alice", 3);
        let started = Instant::now();
        let response = server
            .request(Method::GET, path, Some(&short))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert!(
            ends_within(response, Duration::from_secs(10)).await,
            "{path} must end at the token's expiry"
        );
        let lasted = started.elapsed();
        assert!(
            lasted >= Duration::from_millis(1000) && lasted < Duration::from_secs(10),
            "{path} lasted {lasted:?}"
        );
    }
    // Reconnecting with the expired token yields no stream time: within the
    // clock-skew leeway it is still let in, but its stream ends at once.
    let reopened = server
        .request(Method::GET, "/control/v1/events/stream", Some(&short))
        .send()
        .await
        .unwrap();
    if reopened.status() == StatusCode::OK {
        assert!(ends_within(reopened, Duration::from_secs(2)).await);
    }
}

#[tokio::test]
async fn a_former_owner_cannot_replay_the_stream_or_send_input() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::owned_by(&issuer, "alice").await;
    let alice = issuer.token(&key, "alice");
    let bob = issuer.token(&key, "bob");
    assert_eq!(
        server.create_task(&alice, "task-1").await,
        StatusCode::CREATED
    );

    let (_, listed) = server
        .call(Method::GET, "/control/v1/tasks", Some(&alice), None)
        .await;
    let cursor = serde_json::from_str::<serde_json::Value>(&listed).unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let stream = server
        .request(Method::GET, "/control/v1/events/stream", Some(&alice))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);

    // A moment later the operator rebinds the installation to another
    // identity (R33); the stream stays open until then, and not after.
    let auth = Arc::clone(&server.auth);
    let bob_identity = issuer.identity("bob");
    let started = Instant::now();
    let rebind = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        auth.owners()
            .rebind(&bob_identity, OwnerBindingSource::Configuration)
            .expect("rebind");
    });
    assert!(
        ends_within(stream, Duration::from_secs(6)).await,
        "the former owner's open stream ends"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1100),
        "the stream lived until the rebinding"
    );
    rebind.await.expect("rebind task");

    let replay = format!("/control/v1/events/stream?cursor={cursor}");
    for (method, path, body) in [
        (Method::GET, replay.as_str(), None),
        (Method::GET, "/control/v1/tasks/task-1/conversation", None),
        (
            Method::POST,
            "/control/v1/tasks/task-1/input",
            Some(input_body()),
        ),
    ] {
        let (status, text) = server.call(method.clone(), path, Some(&alice), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(text.is_empty());
    }
    let task = server
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert!(task.inputs.is_empty(), "no input reached the task");

    // The new owner does not inherit the old owner's tasks either.
    assert_eq!(
        server
            .call(Method::GET, "/control/v1/tasks/task-1", Some(&bob), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// Controllers and runners
// ---------------------------------------------------------------------------

/// A trusted-mode server with a control plane: the controller trust plane
/// is the same in both modes.
struct ControlServer {
    _root: TempRoot,
    handle: api::ApiHandle,
    owner: api::ApiClient,
}

impl ControlServer {
    async fn start() -> Self {
        let root = TempRoot::new();
        let service = Arc::new(ProtocolService::new(
            ControlPlane::new(StatePaths::new(root.path())),
            environment_support::seeded_resolver(root.path()),
        ));
        let handle = api::start_with_services(
            Arc::new(Engine::new(ExecutionStore::new(root.path()))),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ApiServices {
                control: Some(service),
                ..ApiServices::default()
            },
        )
        .await
        .expect("server");
        let owner = handle.client();
        Self {
            _root: root,
            handle,
            owner,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.handle.addr())
    }

    async fn enroll(&self, backend: &str) -> ControllerClient {
        let code = self
            .owner
            .create_enrollment(backend, &CreateEnrollmentRequest::default())
            .await
            .expect("code");
        ControllerClient::enroll(self.base_url(), &code.enrollment_code)
            .await
            .expect("enroll")
            .0
    }
}

fn report(task: &str, epoch: u64, observation: Observation, resource: bool) -> ReportRequest {
    ReportRequest {
        task_id: task.to_string(),
        lease_epoch: epoch,
        desired_generation: 1,
        observation,
        resource: resource.then(|| ResourceIdentity {
            uid: "pod-1".to_string(),
            resource_generation: 1,
        }),
        summary: ObservedSummary::default(),
        observed_at: Utc::now(),
    }
}

fn snapshot() -> RunnerSnapshot {
    RunnerSnapshot {
        snapshot_sequence: 1,
        execution_status: ExecutionStatus::Running,
        execution_cursor: ExecutionCursor::default(),
        event_cursor: 1,
        pending_input: None,
        conversation: ConversationAvailability::Live {
            attempts: Vec::new(),
        },
        timeline: None,
        safe_point: false,
        problem: None,
        attention: Vec::new(),
        conductor_available: false,
        conductor_unavailable_reason: None,
        display: None,
        observed_at: Utc::now(),
    }
}

#[tokio::test]
async fn a_controller_proves_its_key_on_every_request_and_revocation_ends_it() {
    let server = ControlServer::start().await;
    let controller = server.enroll("k8s").await;
    let stored = controller.credential().expect("credential");
    assert!(stored.starts_with("nsk."));
    let heartbeat = controller.heartbeat().await.expect("signed heartbeat");

    // A captured request cannot be replayed, nor reused with another body.
    let credential = ControllerCredential::parse(&stored).unwrap();
    let raw = reqwest::Client::new();
    let url = format!("{}/controller/v1/heartbeat", server.base_url());
    let header = credential.authorization("POST", "/controller/v1/heartbeat", b"{}");
    let send = |body: &'static str| {
        raw.post(&url)
            .header("authorization", &header)
            .header("content-type", "application/json")
            .body(body)
            .send()
    };
    assert_eq!(send("{}").await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        send("{}").await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
        "a replayed request is refused"
    );
    let fresh = credential.authorization("POST", "/controller/v1/heartbeat", b"{}");
    let tampered = raw
        .post(&url)
        .header("authorization", &fresh)
        .body("{\"x\":1}")
        .send()
        .await
        .unwrap();
    assert_eq!(tampered.status(), StatusCode::UNAUTHORIZED);
    let as_bearer = raw.post(&url).bearer_auth(&stored).send().await.unwrap();
    assert_eq!(
        as_bearer.status(),
        StatusCode::UNAUTHORIZED,
        "the private key never works as a bearer token"
    );

    // Rotation: the new key works, the old one is refused from then on.
    let (rotated, _) = controller.rotate_key().await.expect("rotate");
    assert!(matches!(
        controller.heartbeat().await,
        Err(ClientError::Unauthorized)
    ));
    rotated.heartbeat().await.expect("the new key works");

    // Revocation ends the identity for good, and its lease with it.
    rotated.acquire_lease(None).await.expect("lease");
    server
        .owner
        .revoke_controller(&heartbeat.controller_id)
        .await
        .expect("revoke");
    assert!(matches!(
        rotated.heartbeat().await,
        Err(ClientError::Unauthorized)
    ));
    assert!(matches!(
        rotated.commands(0, true).await,
        Err(ClientError::Unauthorized)
    ));
    assert!(matches!(
        rotated.rotate_key().await,
        Err(ClientError::Unauthorized)
    ));
}

#[tokio::test]
async fn a_backend_scoped_controller_cannot_reach_another_backends_tasks() {
    let server = ControlServer::start().await;
    server
        .owner
        .create_control_task(&create_request("task-b", "backend-b"))
        .await
        .expect("create");
    let controller = server.enroll("backend-a").await;
    let lease = controller.acquire_lease(None).await.expect("lease");
    assert_eq!(lease.backend_id.as_str(), "backend-a");

    let commands = controller.commands(0, true).await.expect("commands");
    assert!(commands.commands.is_empty(), "backend-b's task is not sent");
    assert!(matches!(
        controller
            .report(&report(
                "task-b",
                lease.epoch,
                Observation::StartAccepted,
                false
            ))
            .await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        controller
            .issue_runner_credential(
                "task-b",
                &RunnerCredentialRequest {
                    lease_epoch: lease.epoch,
                    generation: 1,
                    incarnation: "pod-1".to_string(),
                },
            )
            .await,
        Err(ClientError::NotFound)
    ));
}

#[tokio::test]
async fn a_stale_lease_or_an_old_runner_incarnation_cannot_report() {
    let server = ControlServer::start().await;
    server
        .owner
        .create_control_task(&create_request("task-1", "k8s"))
        .await
        .expect("create");
    let controller = server.enroll("k8s").await;
    let old = controller.acquire_lease(None).await.expect("lease").epoch;
    controller.release_lease(old).await.expect("release");
    let current = controller.acquire_lease(None).await.expect("lease").epoch;
    assert!(current > old);

    let stale = controller
        .report(&report("task-1", old, Observation::StartAccepted, false))
        .await;
    assert!(
        matches!(&stale, Err(ClientError::Refused { code, .. }) if code == "stale_lease"),
        "{stale:?}"
    );
    controller
        .report(&report(
            "task-1",
            current,
            Observation::StartAccepted,
            false,
        ))
        .await
        .expect("current lease reports");

    let issue = |incarnation: &'static str| {
        let controller = &controller;
        async move {
            controller
                .issue_runner_credential(
                    "task-1",
                    &RunnerCredentialRequest {
                        lease_epoch: current,
                        generation: 1,
                        incarnation: incarnation.to_string(),
                    },
                )
                .await
        }
    };
    let first = issue("pod-1").await.expect("first incarnation");
    let second = issue("pod-2").await.expect("second incarnation");
    let old_runner = RunnerClient::new(server.base_url(), first.credential);
    assert!(matches!(
        old_runner.snapshot(&snapshot()).await,
        Err(ClientError::Unauthorized)
    ));
    let runner = RunnerClient::new(server.base_url(), second.credential);
    runner.snapshot(&snapshot()).await.expect("current runner");
}

#[tokio::test]
async fn owner_records_survive_a_restart_and_a_foreign_issuer_is_refused() {
    let root = TempRoot::new();
    let paths = StatePaths::new(root.path());
    let (store, ownership) =
        OwnerStore::open(&paths, "https://issuer.example", Some("alice")).unwrap();
    assert!(matches!(ownership, StartupOwnership::Owned(_)));
    assert!(store.is_owner(&OidcIdentity::new("https://issuer.example", "alice")));
    drop(store);

    // Configuration removed: the recorded owner stays.
    let (store, _) = OwnerStore::open(&paths, "https://issuer.example", None).unwrap();
    assert_eq!(store.owner().unwrap().subject, "alice");
    drop(store);

    // Rebinding by configuration bumps the revision.
    let (store, _) = OwnerStore::open(&paths, "https://issuer.example", Some("bob")).unwrap();
    let owner = store.owner().unwrap();
    assert_eq!((owner.subject.as_str(), owner.revision), ("bob", 2));
    drop(store);

    // An owner of another issuer could never log in: refused at startup.
    let error = OwnerStore::open(&paths, "https://other.example", None).unwrap_err();
    assert!(error.to_string().contains("owner_subject"), "{error}");
}

// ---------------------------------------------------------------------------
// The remote-aware client (U8)
// ---------------------------------------------------------------------------

/// What the forwarding proxy saw of one request.
#[derive(Debug, Clone)]
struct Forwarded {
    path_and_query: String,
    authorization: Option<String>,
    status: u16,
}

#[derive(Clone)]
struct ProxyState {
    upstream: String,
    http: reqwest::Client,
    seen: Arc<Mutex<Vec<Forwarded>>>,
}

/// A TLS-terminating proxy stand-in on a loopback port: forwards method,
/// path, query, body and the bearer token unchanged, adds
/// `X-Forwarded-Proto: https`, and streams the answer back — what the
/// network-mode client talks to instead of the daemon itself.
struct ForwardingProxy {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Forwarded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ForwardingProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ForwardingProxy {
    async fn start(upstream: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let addr = listener.local_addr().expect("proxy address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = ProxyState {
            upstream: format!("http://{upstream}"),
            http: reqwest::Client::new(),
            seen: Arc::clone(&seen),
        };
        let router = Router::new().fallback(forward).with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the proxy");
        });
        Self { addr, seen, task }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> Vec<Forwarded> {
        self.seen.lock().unwrap().clone()
    }
}

async fn forward(
    State(proxy): State<ProxyState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.expect("body");
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or("/".to_string(), |value| value.to_string());
    let authorization = parts
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut upstream = proxy
        .http
        .request(parts.method, format!("{}{path_and_query}", proxy.upstream))
        .header("x-forwarded-proto", "https")
        .body(body);
    for name in ["authorization", "content-type", "last-event-id"] {
        if let Some(value) = parts.headers.get(name) {
            upstream = upstream.header(name, value);
        }
    }
    let response = upstream.send().await.expect("upstream answers");
    proxy.seen.lock().unwrap().push(Forwarded {
        path_and_query,
        authorization: authorization.map(|value| value.trim_start_matches("Bearer ").to_string()),
        status: response.status().as_u16(),
    });
    let mut builder = axum::response::Response::builder().status(response.status());
    if let Some(value) = response.headers().get("content-type") {
        builder = builder.header("content-type", value);
    }
    builder
        .body(axum::body::Body::from_stream(response.bytes_stream()))
        .expect("response")
}

fn token_file(root: &TempRoot, token: &str) -> PathBuf {
    let path = root.path().join("access-token");
    fs::write(&path, format!("{token}\n")).expect("write the token file");
    path
}

#[tokio::test]
async fn a_network_client_rereads_its_token_file_after_a_401_and_only_then() {
    let key = IssuerKey::es256("k1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::start(
        &issuer,
        NetworkOptions {
            owner_subject: Some("alice".to_string()),
            leeway: Some(Duration::ZERO),
            ..NetworkOptions::default()
        },
    )
    .await;
    let proxy = ForwardingProxy::start(server.handle.addr()).await;
    let tokens = TempRoot::new();
    let expired = issuer.token_expiring_in(&key, "alice", -120);
    let path = token_file(&tokens, &expired);
    let client = api::ApiClient::remote(&proxy.url(), api::TokenSource::File(path.clone()))
        .expect("a loopback http URL is accepted");

    // The file still holds the expired token: re-reading it changes
    // nothing, so the 401 stands — and the request is not retried.
    assert!(matches!(
        client.whoami().await,
        Err(ClientError::Unauthorized)
    ));
    let first: Vec<_> = proxy.seen();
    assert_eq!(
        first.len(),
        1,
        "an unchanged token is not sent twice: {first:?}"
    );

    // The helper refreshed the file: the next 401 makes the client re-read
    // it and the same request goes through with the new token.
    let fresh = issuer.token(&key, "alice");
    fs::write(&path, &fresh).unwrap();
    let listing = client.control_tasks().await.expect("the fresh token works");
    assert!(listing.tasks.is_empty());
    let seen = proxy.seen();
    let tail: Vec<_> = seen[1..]
        .iter()
        .map(|f| (f.status, f.authorization.clone()))
        .collect();
    assert_eq!(
        tail,
        vec![(401, Some(expired.clone())), (200, Some(fresh.clone()))],
        "the cached token first, then the re-read one"
    );

    // A 403 is not a token problem: a stranger's valid token is not re-read.
    let stranger = issuer.token(&key, "mallory");
    let path = token_file(&tokens, &stranger);
    let other = api::ApiClient::remote(&proxy.url(), api::TokenSource::File(path)).unwrap();
    let before = proxy.seen().len();
    assert!(matches!(
        other.control_tasks().await,
        Err(ClientError::Forbidden)
    ));
    assert_eq!(proxy.seen().len(), before + 1);
}

#[tokio::test]
async fn a_remote_url_must_be_https_unless_it_is_loopback() {
    let source = || api::TokenSource::Static("t".to_string());
    assert!(api::ApiClient::remote("https://narsor.example.com/", source()).is_ok());
    assert!(api::ApiClient::remote("http://127.0.0.1:8080", source()).is_ok());
    assert!(api::ApiClient::remote("http://[::1]:8080", source()).is_ok());
    for refused in [
        "http://narsor.example.com",
        "https://user:pw@narsor.example.com",
        "https://narsor.example.com/?token=x",
        "ftp://narsor.example.com",
    ] {
        assert!(
            matches!(
                api::ApiClient::remote(refused, source()),
                Err(ClientError::Request { .. })
            ),
            "{refused} must be refused"
        );
    }
    // The prefix of a proxy mounting the API elsewhere is kept.
    let client = api::ApiClient::remote("https://example.com/narsor/", source()).unwrap();
    assert_eq!(client.base_url(), "https://example.com/narsor");
    assert!(
        !format!("{client:?}").contains("\"t\""),
        "the token is never printed"
    );
}

#[tokio::test]
async fn a_network_event_stream_reconnects_from_its_cursor_with_a_fresh_token() {
    let key = IssuerKey::es256("k1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::start(
        &issuer,
        NetworkOptions {
            owner_subject: Some("alice".to_string()),
            leeway: Some(Duration::ZERO),
            ..NetworkOptions::default()
        },
    )
    .await;
    let proxy = ForwardingProxy::start(server.handle.addr()).await;
    let tokens = TempRoot::new();
    let short = issuer.token_expiring_in(&key, "alice", 3);
    let path = token_file(&tokens, &short);
    let client =
        api::ApiClient::remote(&proxy.url(), api::TokenSource::File(path.clone())).unwrap();

    let cursor = client.control_tasks().await.expect("list").cursor;
    let mut stream = Box::pin(client.follow_control_events(Some(cursor), None));
    assert_eq!(
        server.create_task(&short, "task-a").await,
        StatusCode::CREATED
    );
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("an event before expiry")
        .expect("the stream is open")
        .expect("an event");
    assert_eq!(first.event.task_id.as_str(), "task-a");
    let resume_from = first.cursor.clone();

    // The helper refreshes the token; the stream ends at the old token's
    // expiry and reconnects on its own, from the cursor, with the new one.
    let fresh = issuer.token(&key, "alice");
    fs::write(&path, &fresh).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        server.create_task(&fresh, "task-b").await,
        StatusCode::CREATED
    );
    // Everything after the cursor arrives once — task-a's later events
    // (its queue verdict) and task-b — and nothing is repeated.
    let mut seen = vec![(first.event.task_id.to_string(), first.event.sequence)];
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(envelope)) => {
                    let key = (envelope.event.task_id.to_string(), envelope.event.sequence);
                    assert!(!seen.contains(&key), "{key:?} was delivered twice");
                    seen.push(key);
                    if envelope.event.task_id.as_str() == "task-b" {
                        break;
                    }
                }
                Some(Err(ClientError::Unavailable(_))) => continue,
                other => panic!("the stream should have reconnected: {other:?}"),
            }
        }
    })
    .await
    .expect("task-b's event after the reconnect");

    let streams: Vec<_> = proxy
        .seen()
        .into_iter()
        .filter(|f| f.path_and_query.starts_with("/control/v1/events/stream"))
        .collect();
    let reconnect = streams.last().expect("a reconnect");
    assert_eq!(reconnect.status, 200);
    assert_eq!(reconnect.authorization.as_deref(), Some(fresh.as_str()));
    let resumed = streams
        .iter()
        .filter(|f| f.status == 200)
        .nth(1)
        .expect("a second, successful connection");
    let encoded = url_encode(&resume_from);
    assert!(
        !resume_from.is_empty() && resumed.path_and_query.contains("cursor="),
        "resumed from a cursor: {} (first event's {encoded})",
        resumed.path_and_query
    );
    assert!(
        streams
            .iter()
            .any(|f| f.status == 401 && f.authorization.as_deref() == Some(short.as_str())),
        "the expired token drew a 401 before the re-read: {streams:?}"
    );
}

fn url_encode(text: &str) -> String {
    reqwest::Url::parse_with_params("http://x/", &[("c", text)])
        .unwrap()
        .query()
        .unwrap()
        .trim_start_matches("c=")
        .to_string()
}

#[tokio::test]
async fn a_network_server_serves_no_disk_paths_but_still_manages_remote_repositories() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::owned_by(&issuer, "alice").await;
    let alice = issuer.token(&key, "alice");

    let (status, body) = server
        .call(Method::GET, "/control/v1/capabilities", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let capabilities: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(capabilities["workflow_paths"], false);
    assert_eq!(capabilities["local_repository_paths"], false);
    assert_eq!(capabilities["legacy_executions"], false);
    assert_eq!(capabilities["connection_name"], "remote");
    assert!(capabilities["workflow_paths_reason"].is_string());

    // Reading a file of the server's disk is refused, whatever the file.
    let (status, body) = server
        .call(
            Method::POST,
            "/control/v1/workflows/validate",
            Some(&alice),
            Some(serde_json::json!({ "path": "/etc/hostname.yaml" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("workflow_paths_unavailable"), "{body}");

    // The catalogue is still served; the Historia has no old executions.
    let (status, _) = server
        .call(Method::GET, "/control/v1/workflows", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server
        .call(Method::GET, "/control/v1/executions", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"executions\":[]"), "{body}");
    let (status, _) = server
        .call(
            Method::POST,
            "/control/v1/executions/exec-1/recover",
            Some(&alice),
            Some(serde_json::json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Repositories: remote git URLs only.
    for (id, remote) in [
        ("path", "/srv/git/repo.git"),
        ("relative", "../repo"),
        ("home", "~/repo"),
        ("file", "file:///srv/git/repo.git"),
        ("file-upper", "FILE:///srv/git/repo.git"),
        ("bare", "repo.git"),
        ("ext", "ext::sh -c touch% /tmp/pwned"),
        ("ext-bare", "ext::sh"),
        ("fd", "fd::3"),
        ("file-scp", "file:/srv/git/repo.git"),
        ("padded", " https://example.com/acme.git"),
        ("spaced", "https://example.com/acme.git --upload-pack=x"),
        ("dash-host", "ssh://-oProxyCommand=x/acme.git"),
        ("dash-scp", "-oProxyCommand=x:acme.git"),
        ("backslash", "https://example.com\\@evil/acme.git"),
        ("bidi", "https://example.com/\u{202E}acme.git"),
    ] {
        let (status, body) = server
            .call(
                Method::POST,
                "/control/v1/repositories",
                Some(&alice),
                Some(serde_json::json!({ "id": id, "remote": remote })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{remote}: {body}");
        assert!(body.contains("invalid_repository"), "{remote}: {body}");
    }
    for (id, remote) in [
        ("https", "https://example.com/acme.git"),
        ("ssh", "ssh://git@example.com/acme.git"),
        ("scp", "git@example.com:team/acme.git"),
    ] {
        let (status, body) = server
            .call(
                Method::POST,
                "/control/v1/repositories",
                Some(&alice),
                Some(serde_json::json!({ "id": id, "remote": remote })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{remote}: {body}");
    }
    let (status, body) = server
        .call(Method::GET, "/control/v1/repositories", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"id\":\"scp\""), "{body}");
    let (status, _) = server
        .call(
            Method::DELETE,
            "/control/v1/repositories/https",
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .call(
            Method::DELETE,
            "/control/v1/repositories/https",
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The command line against a network server: only remote git URLs are
/// registered, and the files and old executions of the server's disk are not
/// offered — each refusal says so, instead of failing obscurely.
#[tokio::test(flavor = "multi_thread")]
async fn the_cli_gets_a_readable_refusal_from_a_network_server() {
    let key = IssuerKey::es256("key-1");
    let issuer = MockIssuer::start(&[&key]).await;
    let server = NetworkServer::owned_by(&issuer, "alice").await;
    let proxy = ForwardingProxy::start(server.handle.addr()).await;
    let alice = issuer.token(&key, "alice");
    let url = proxy.url();
    let root = server.root.path().to_path_buf();

    let run = move |args: Vec<&'static str>| {
        let (url, alice, root) = (url.clone(), alice.clone(), root.clone());
        async move {
            tokio::task::spawn_blocking(move || {
                let output = std::process::Command::new(env!("CARGO_BIN_EXE_narsor"))
                    .current_dir(&root)
                    .env("HOME", &root)
                    .env("XDG_CONFIG_HOME", root.join("config"))
                    .env_remove("NARSOR_TOKEN_FILE")
                    .env_remove("NARSOR_TOKEN_COMMAND")
                    .env("NARSOR_SERVER", &url)
                    .env("NARSOR_TOKEN", &alice)
                    .arg("--root")
                    .arg(&root)
                    .args(&args)
                    .output()
                    .expect("run the narsor binary");
                (
                    output.status.code().expect("exited normally"),
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                )
            })
            .await
            .expect("the binary ran")
        }
    };

    let (code, out, err) = run(vec!["task", "capabilities"]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Connection:             remote"), "{out}");
    assert!(out.contains("Workflow paths:         no ("), "{out}");
    assert!(out.contains("Local repository paths: no"), "{out}");

    let (code, _, err) = run(vec![
        "task",
        "repo",
        "add",
        "disk",
        "--remote",
        "/srv/git/repo.git",
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("invalid_repository"), "{err}");
    let (code, out, err) = run(vec![
        "task",
        "repo",
        "add",
        "api",
        "--remote",
        "git@example.com:team/api.git",
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Registered repository 'api'"), "{out}");
    let (code, out, _) = run(vec!["task", "repo", "list"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("git@example.com:team/api.git") && !out.contains("disk"),
        "{out}"
    );
    let (code, _, err) = run(vec!["task", "repo", "remove", "api"]).await;
    assert_eq!(code, 0, "{err}");

    for args in [
        vec!["task", "validate", "/srv/w.yaml"],
        vec!["task", "create", "--file", "/srv/w.yaml"],
        vec!["task", "history", "list"],
        vec!["task", "history", "recover", "exec-1"],
    ] {
        let (code, _, err) = run(args.clone()).await;
        assert_eq!(code, 1, "{args:?}: {err}");
        assert!(err.contains("needs the local daemon"), "{args:?}: {err}");
        assert!(err.contains("'remote' is not one"), "{args:?}: {err}");
    }

    let (code, out, err) = run(vec!["task", "workflows"]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("implement-feature"), "{out}");
}
