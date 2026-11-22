//! A thin, explicit client for the few Kubernetes REST endpoints the
//! adapter uses.
//!
//! Deliberately not `kube` + `k8s-openapi`: the adapter touches five
//! resource types through a dozen calls, and those crates would add the
//! whole generated API surface (and minutes of compile time) for that. The
//! existing `reqwest` (rustls) client speaks to the API server directly;
//! requests and answers are plain JSON, read through the minimal types in
//! [`super::objects`]. What is lost — informers, kubeconfig contexts, auth
//! plugins — the controller does not need: it observes by name, and it runs
//! either in-cluster (service-account token) or with an explicit API server,
//! token and CA bundle. Its one watch (the managed Pods, only to wake the
//! controller, R4) is a plain streaming GET read line by line
//! ([`KubeTransport::watch`], [`WatchEvent`]).
//!
//! # Authentication
//!
//! A bearer token, read from a file on *every* request when it comes from
//! one: the projected service-account token in a Pod is rotated by the
//! kubelet, and a cached copy would expire under a long-running controller.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub use reqwest::{Method, StatusCode};
use serde_json::Value;

use narsor_controller_runtime::backend::{BackendError, BackendResult};

/// Where the in-cluster service-account files are mounted.
pub const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// Longest a single API request may take before it counts as a transient
/// failure.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a connection to the API server may take to open; a watch has no
/// other bound than its own (see [`KubeTransport::watch`]).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a bearer token comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// No `Authorization` header (a test API server, a proxy that adds it).
    None,
    /// A fixed token.
    Static(String),
    /// A file re-read on every request.
    File(PathBuf),
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Static(_) => f.write_str("Static(<redacted>)"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
        }
    }
}

/// How to reach one namespace of one API server.
#[derive(Debug, Clone)]
pub struct ClusterAccess {
    /// Base URL of the API server, e.g. `https://10.0.0.1:443`.
    pub api_server: String,
    /// The namespace task resources live in.
    pub namespace: String,
    /// The bearer token.
    pub token: TokenSource,
    /// PEM CA bundle the API server's certificate must chain to; `None`
    /// uses the platform's web roots (rarely right for a cluster).
    pub ca_pem: Option<Vec<u8>>,
}

impl ClusterAccess {
    /// Explicit access, for tests and for a controller outside the cluster.
    pub fn explicit(api_server: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            api_server: api_server.into(),
            namespace: namespace.into(),
            token: TokenSource::None,
            ca_pem: None,
        }
    }

    /// The access a Pod has to its own cluster: `KUBERNETES_SERVICE_HOST` /
    /// `_PORT`, the mounted service-account token and CA bundle, and — unless
    /// `namespace` overrides it — the Pod's own namespace.
    pub fn in_cluster(namespace: Option<String>) -> Result<Self, String> {
        Self::in_cluster_from(Path::new(SERVICE_ACCOUNT_DIR), namespace, |name| {
            std::env::var(name).ok()
        })
    }

    /// [`Self::in_cluster`] with the directory and environment supplied.
    pub fn in_cluster_from(
        dir: &Path,
        namespace: Option<String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let host = env("KUBERNETES_SERVICE_HOST")
            .filter(|host| !host.is_empty())
            .ok_or("KUBERNETES_SERVICE_HOST is not set: not running inside a cluster")?;
        let port = env("KUBERNETES_SERVICE_PORT").unwrap_or_else(|| "443".to_string());
        // An IPv6 service address needs brackets in a URL.
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let ca_path = dir.join("ca.crt");
        let ca_pem = std::fs::read(&ca_path)
            .map_err(|error| format!("cannot read {}: {error}", ca_path.display()))?;
        let namespace = match namespace {
            Some(namespace) => namespace,
            None => {
                let path = dir.join("namespace");
                std::fs::read_to_string(&path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?
                    .trim()
                    .to_string()
            }
        };
        Ok(Self {
            api_server: format!("https://{host}:{port}"),
            namespace,
            token: TokenSource::File(dir.join("token")),
            ca_pem: Some(ca_pem),
        })
    }
}

/// The outcome of a create.
#[derive(Debug)]
pub enum Created {
    /// The object was created; the server's copy of it.
    Created(Value),
    /// An object of that name already exists (HTTP 409 AlreadyExists).
    AlreadyExists,
}

/// A boxed future, so [`KubeTransport`] stays object-safe.
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One open watch: the event objects the API server sends, one JSON object
/// per line, as they come.
pub trait WatchStream: Send {
    /// The next event object; `None` once the server ended the stream (its
    /// `timeoutSeconds` passed, the connection closed). `Err` is a stream
    /// cut off or unreadable.
    fn next_event(&mut self) -> TransportFuture<'_, BackendResult<Option<Value>>>;
}

/// The API server's answer to a watch request.
pub enum WatchAnswer<'a> {
    /// The watch is open.
    Events(Box<dyn WatchStream + 'a>),
    /// The server refused it: the status and its body.
    Refused(StatusCode, Value),
}

impl fmt::Debug for WatchAnswer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Events(_) => f.write_str("Events(..)"),
            Self::Refused(status, body) => {
                f.debug_tuple("Refused").field(status).field(body).finish()
            }
        }
    }
}

/// One event of a watch, as the adapter reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    /// `ADDED`, `MODIFIED` or `DELETED`: the object as it now is (as it
    /// last was, for `DELETED`).
    Changed {
        /// The object's `resourceVersion`: where to resume from.
        resource_version: Option<String>,
        /// The object.
        object: Value,
    },
    /// `BOOKMARK`: nothing changed, the watch is current up to this
    /// `resourceVersion`.
    Bookmark(Option<String>),
    /// An `ERROR` with code 410 Gone: the version the watch started from is
    /// too old, list again.
    Expired,
    /// Any other `ERROR`, or an event the adapter does not know.
    Failed(String),
}

impl WatchEvent {
    /// Reads one event object as the API server sends it,
    /// `{"type": …, "object": …}`.
    pub fn read(event: Value) -> Self {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let version = |object: &Value| {
            object
                .pointer("/metadata/resourceVersion")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match kind {
            "ADDED" | "MODIFIED" | "DELETED" => {
                let object = event.get("object").cloned().unwrap_or(Value::Null);
                Self::Changed {
                    resource_version: version(&object),
                    object,
                }
            }
            "BOOKMARK" => Self::Bookmark(event.get("object").and_then(version)),
            "ERROR" => {
                let status = event.get("object").cloned().unwrap_or(Value::Null);
                if status.get("code").and_then(Value::as_u64) == Some(410) {
                    Self::Expired
                } else {
                    let message = status
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message");
                    Self::Failed(format!("the watch failed: {message}"))
                }
            }
            other => Self::Failed(format!("unknown watch event type '{other}'")),
        }
    }
}

/// What a [`KubeClient::watch`] opened.
pub enum Watched<'a> {
    /// The events, as the server sends them; read each with
    /// [`WatchEvent::read`].
    Events(Box<dyn WatchStream + 'a>),
    /// The version to start from is too old (HTTP 410 Gone): list again.
    Expired,
}

/// One HTTP exchange with the API server: the boundary with the
/// infrastructure. Everything above it — paths, bodies, status and error
/// categorization — is the adapter's own and runs in tests unchanged;
/// tests supply an in-memory implementation instead of a server.
pub trait KubeTransport: fmt::Debug + Send + Sync {
    /// Sends the request; the status and the JSON body (`Null` when there is
    /// none). `Err` is a failure to get any answer.
    fn send<'a>(
        &'a self,
        method: Method,
        path: &'a str,
        body: Option<&'a Value>,
    ) -> TransportFuture<'a, BackendResult<(StatusCode, Value)>>;

    /// Opens a watch: a GET of `path` (whose query asks for `watch=true`
    /// and a `timeoutSeconds`) whose answer streams on. `timeout` bounds the
    /// whole exchange — the server's `timeoutSeconds` plus a margin — since
    /// a watch outlives any ordinary request. `Err` is a failure to get any
    /// answer.
    fn watch<'a>(
        &'a self,
        path: &'a str,
        timeout: Duration,
    ) -> TransportFuture<'a, BackendResult<WatchAnswer<'a>>>;
}

/// The reqwest transport to a real API server.
#[derive(Debug)]
struct HttpTransport {
    http: reqwest::Client,
    /// For watches: no total timeout of its own, and a connection pool
    /// apart from the ordinary requests'.
    watch_http: reqwest::Client,
    base: String,
    token: TokenSource,
}

impl HttpTransport {
    async fn exchange(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> BackendResult<(StatusCode, Value)> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("Accept", "application/json");
        if let Some(token) = self.token()? {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            BackendError::transient(format!("the Kubernetes API is unreachable: {error}"))
        })?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| {
            BackendError::transient(format!("the Kubernetes API answer was cut off: {error}"))
        })?;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, body))
    }

    async fn open_watch(&self, path: &str, timeout: Duration) -> BackendResult<WatchAnswer<'_>> {
        let mut request = self
            .watch_http
            .get(format!("{}{path}", self.base))
            .header("Accept", "application/json")
            .timeout(timeout);
        if let Some(token) = self.token()? {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|error| {
            BackendError::transient(format!("the Kubernetes API is unreachable: {error}"))
        })?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.unwrap_or_default();
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Ok(WatchAnswer::Refused(status, body));
        }
        Ok(WatchAnswer::Events(Box::new(HttpWatch {
            response,
            lines: WatchLines::default(),
        })))
    }

    fn token(&self) -> BackendResult<Option<String>> {
        match &self.token {
            TokenSource::None => Ok(None),
            TokenSource::Static(token) => Ok(Some(token.clone())),
            TokenSource::File(path) => std::fs::read_to_string(path)
                .map(|token| Some(token.trim().to_string()))
                .map_err(|error| {
                    BackendError::transient(format!(
                        "cannot read the API token {}: {error}",
                        path.display()
                    ))
                }),
        }
    }
}

impl KubeTransport for HttpTransport {
    fn send<'a>(
        &'a self,
        method: Method,
        path: &'a str,
        body: Option<&'a Value>,
    ) -> TransportFuture<'a, BackendResult<(StatusCode, Value)>> {
        Box::pin(self.exchange(method, path, body))
    }

    fn watch<'a>(
        &'a self,
        path: &'a str,
        timeout: Duration,
    ) -> TransportFuture<'a, BackendResult<WatchAnswer<'a>>> {
        Box::pin(self.open_watch(path, timeout))
    }
}

/// A watch over HTTP: the response body, read chunk by chunk.
struct HttpWatch {
    response: reqwest::Response,
    lines: WatchLines,
}

impl HttpWatch {
    async fn next(&mut self) -> BackendResult<Option<Value>> {
        loop {
            if let Some(event) = self.lines.next_event()? {
                return Ok(Some(event));
            }
            let chunk = self.response.chunk().await.map_err(|error| {
                BackendError::transient(format!("the Pod watch was cut off: {error}"))
            })?;
            match chunk {
                Some(chunk) => self.lines.push(&chunk),
                None => return self.lines.finish(),
            }
        }
    }
}

impl WatchStream for HttpWatch {
    fn next_event(&mut self) -> TransportFuture<'_, BackendResult<Option<Value>>> {
        Box::pin(self.next())
    }
}

/// Splits a watch body into its event objects: one JSON object per line,
/// whatever the chunks it arrives in.
#[derive(Debug, Default)]
struct WatchLines {
    pending: Vec<u8>,
}

impl WatchLines {
    fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    /// The next complete line's object, skipping blank lines.
    fn next_event(&mut self) -> BackendResult<Option<Value>> {
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some(event) = parse_line(&line)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// The stream ended: what is left is a last line without its newline.
    fn finish(&mut self) -> BackendResult<Option<Value>> {
        let rest = std::mem::take(&mut self.pending);
        parse_line(&rest)
    }
}

fn parse_line(line: &[u8]) -> BackendResult<Option<Value>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(line)
        .map(Some)
        .map_err(|error| BackendError::transient(format!("unreadable watch event: {error}")))
}

/// The client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct KubeClient {
    transport: Arc<dyn KubeTransport>,
    namespace: String,
}

impl KubeClient {
    /// A client for `access`.
    pub fn new(access: &ClusterAccess) -> Result<Self, String> {
        let http = http_client(access, reqwest::Client::builder().timeout(REQUEST_TIMEOUT))?;
        let watch_http = http_client(
            access,
            reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT),
        )?;
        let transport = HttpTransport {
            http,
            watch_http,
            base: access.api_server.trim_end_matches('/').to_string(),
            token: access.token.clone(),
        };
        Ok(Self::with_transport(
            Arc::new(transport),
            access.namespace.clone(),
        ))
    }

    /// A client over `transport`, working in `namespace`.
    pub fn with_transport(transport: Arc<dyn KubeTransport>, namespace: String) -> Self {
        Self {
            transport,
            namespace,
        }
    }

    /// The namespace this client works in.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// `/api/v1/namespaces/{ns}/{plural}[/{name}]`.
    pub fn core_path(&self, plural: &str, name: Option<&str>) -> String {
        let mut path = format!("/api/v1/namespaces/{}/{plural}", self.namespace);
        if let Some(name) = name {
            path.push('/');
            path.push_str(name);
        }
        path
    }

    /// GET; `None` on 404.
    pub async fn get(&self, path: &str) -> BackendResult<Option<Value>> {
        let (status, body) = self.send(Method::GET, path, None).await?;
        match status {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(body)),
            status => Err(failure("read", path, status, &body)),
        }
    }

    /// POST a new object.
    pub async fn create(&self, path: &str, object: &Value) -> BackendResult<Created> {
        let (status, body) = self.send(Method::POST, path, Some(object)).await?;
        match status {
            StatusCode::CONFLICT if reason(&body) == Some("AlreadyExists") => {
                Ok(Created::AlreadyExists)
            }
            status if status.is_success() => Ok(Created::Created(body)),
            status => Err(failure("create", path, status, &body)),
        }
    }

    /// PUT one object. With the object's `resourceVersion` the API server
    /// refuses a stale write; without it, the last write wins.
    pub async fn replace(&self, path: &str, object: &Value) -> BackendResult<Value> {
        let (status, body) = self.send(Method::PUT, path, Some(object)).await?;
        match status {
            StatusCode::CONFLICT => Err(BackendError::transient(format!(
                "{path} changed during the update; retry from a fresh version"
            ))),
            status if status.is_success() => Ok(body),
            status => Err(failure("update", path, status, &body)),
        }
    }

    /// DELETE, only if the object still has `uid` (a precondition, so a
    /// replacement created meanwhile under the same name is never deleted
    /// by mistake). `Ok(false)` when there was nothing to delete; a failed
    /// precondition is a transient error: re-observe and decide again.
    pub async fn delete(&self, path: &str, uid: &str) -> BackendResult<bool> {
        let options = serde_json::json!({
            "apiVersion": "v1",
            "kind": "DeleteOptions",
            "propagationPolicy": "Background",
            "preconditions": { "uid": uid },
        });
        let (status, body) = self.send(Method::DELETE, path, Some(&options)).await?;
        match status {
            StatusCode::NOT_FOUND => Ok(false),
            status if status.is_success() => Ok(true),
            StatusCode::CONFLICT => Err(BackendError::transient(format!(
                "{path} was replaced while it was being deleted"
            ))),
            status => Err(failure("delete", path, status, &body)),
        }
    }

    /// GET a collection (`path` with its query).
    pub async fn list(&self, path: &str) -> BackendResult<Value> {
        let (status, body) = self.send(Method::GET, path, None).await?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(failure("list", path, status, &body))
        }
    }

    /// Opens a watch of `path` (with its query, `timeoutSeconds` included),
    /// bounded by `timeout`.
    pub async fn watch<'a>(
        &'a self,
        path: &'a str,
        timeout: Duration,
    ) -> BackendResult<Watched<'a>> {
        match self.transport.watch(path, timeout).await? {
            WatchAnswer::Events(events) => Ok(Watched::Events(events)),
            WatchAnswer::Refused(StatusCode::GONE, _) => Ok(Watched::Expired),
            WatchAnswer::Refused(status, body) => Err(failure("watch", path, status, &body)),
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> BackendResult<(StatusCode, Value)> {
        self.transport.send(method, path, body).await
    }
}

/// A reqwest client trusting only the cluster's CA bundle, when it has one.
fn http_client(
    access: &ClusterAccess,
    mut builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, String> {
    if let Some(pem) = &access.ca_pem {
        let certificates = reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|error| format!("invalid CA bundle: {error}"))?;
        if certificates.is_empty() {
            return Err("the CA bundle holds no certificate".to_string());
        }
        // Only the cluster's CA: a public CA must not be able to stand in
        // for the API server.
        builder = builder.tls_certs_only(certificates);
    }
    builder
        .build()
        .map_err(|error| format!("cannot build the HTTP client: {error}"))
}

/// The `reason` of a Kubernetes `Status` answer.
fn reason(body: &Value) -> Option<&str> {
    body.get("reason").and_then(Value::as_str)
}

/// A failed call, categorized: what retrying can fix is transient, what the
/// API server refused for good (validation, admission, RBAC, quota) is a
/// rejection the owner must see.
fn failure(what: &str, path: &str, status: StatusCode, body: &Value) -> BackendError {
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message");
    let text = format!("cannot {what} {path}: HTTP {status}: {message}");
    let transient = status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::CONFLICT
        // A namespace being torn down, a quota recalculating: both pass.
        || reason(body) == Some("ServerTimeout");
    if transient {
        BackendError::transient(text)
    } else {
        BackendError::rejected(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_cluster_access_reads_the_mounted_service_account() {
        let dir = std::env::temp_dir().join(format!("narsor-sa-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ca.crt"), b"pem").unwrap();
        std::fs::write(dir.join("namespace"), b"narsor-tasks\n").unwrap();
        let env = |name: &str| match name {
            "KUBERNETES_SERVICE_HOST" => Some("fd00::1".to_string()),
            "KUBERNETES_SERVICE_PORT" => Some("6443".to_string()),
            _ => None,
        };
        let access = ClusterAccess::in_cluster_from(&dir, None, env).unwrap();
        assert_eq!(access.api_server, "https://[fd00::1]:6443");
        assert_eq!(access.namespace, "narsor-tasks");
        assert_eq!(access.token, TokenSource::File(dir.join("token")));
        let overridden = ClusterAccess::in_cluster_from(&dir, Some("other".into()), env).unwrap();
        assert_eq!(overridden.namespace, "other");
        assert!(ClusterAccess::in_cluster_from(&dir, None, |_| None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The watch body is read line by line whatever its chunks: events split
    /// across chunks or sharing one, a last line without its newline; a
    /// 410 ERROR asks for a new list, a bookmark only moves the version.
    #[test]
    fn a_watch_body_is_read_into_events_whatever_its_chunks() {
        let body = concat!(
            r#"{"type":"ADDED","object":{"metadata":{"name":"a","resourceVersion":"5"}}}"#,
            "\n\n",
            r#"{"type":"BOOKMARK","object":{"metadata":{"resourceVersion":"7"}}}"#,
            "\n",
            r#"{"type":"ERROR","object":{"kind":"Status","code":410,"reason":"Expired"}}"#,
        )
        .as_bytes();
        let mut lines = WatchLines::default();
        let mut events = Vec::new();
        for chunk in body.chunks(13) {
            lines.push(chunk);
            while let Some(event) = lines.next_event().unwrap() {
                events.push(WatchEvent::read(event));
            }
        }
        events.extend(lines.finish().unwrap().map(WatchEvent::read));

        assert_eq!(
            events,
            vec![
                WatchEvent::Changed {
                    resource_version: Some("5".into()),
                    object: serde_json::json!({"metadata":{"name":"a","resourceVersion":"5"}}),
                },
                WatchEvent::Bookmark(Some("7".into())),
                WatchEvent::Expired,
            ]
        );
        let mut garbled = WatchLines::default();
        garbled.push(b"{not json\n");
        assert!(garbled.next_event().is_err());
        assert!(matches!(
            WatchEvent::read(serde_json::json!({"type":"ERROR","object":{"code":500}})),
            WatchEvent::Failed(_)
        ));
    }

    #[test]
    fn a_static_token_never_prints() {
        assert!(!format!("{:?}", TokenSource::Static("hunter2".into())).contains("hunter2"));
    }
}
