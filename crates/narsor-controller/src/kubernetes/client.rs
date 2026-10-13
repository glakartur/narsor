//! A thin, explicit client for the few Kubernetes REST endpoints the
//! adapter uses.
//!
//! Deliberately not `kube` + `k8s-openapi`: the adapter touches five
//! resource types through a dozen calls, and those crates would add the
//! whole generated API surface (and minutes of compile time) for that. The
//! existing `reqwest` (rustls) client speaks to the API server directly;
//! requests and answers are plain JSON, read through the minimal types in
//! [`super::objects`]. What is lost — watches, kubeconfig contexts, auth
//! plugins — the controller does not need: it observes by name, and it runs
//! either in-cluster (service-account token) or with an explicit API server,
//! token and CA bundle.
//!
//! # Authentication
//!
//! A bearer token, read from a file on *every* request when it comes from
//! one: the projected service-account token in a Pod is rotated by the
//! kubelet, and a cached copy would expire under a long-running controller.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::Value;

use narsor_engine::backends::{BackendError, BackendResult};

/// Where the in-cluster service-account files are mounted.
pub const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// Longest a single API request may take before it counts as a transient
/// failure.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct KubeClient {
    http: reqwest::Client,
    base: String,
    namespace: String,
    token: TokenSource,
}

impl KubeClient {
    /// A client for `access`.
    pub fn new(access: &ClusterAccess) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
        if let Some(pem) = &access.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|error| format!("invalid CA bundle: {error}"))?;
            if certificates.is_empty() {
                return Err("the CA bundle holds no certificate".to_string());
            }
            // Only the cluster's CA: a public CA must not be able to stand
            // in for the API server.
            builder = builder.tls_certs_only(certificates);
        }
        let http = builder
            .build()
            .map_err(|error| format!("cannot build the HTTP client: {error}"))?;
        Ok(Self {
            http,
            base: access.api_server.trim_end_matches('/').to_string(),
            namespace: access.namespace.clone(),
            token: access.token.clone(),
        })
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

    async fn send(
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

    #[test]
    fn a_static_token_never_prints() {
        assert!(!format!("{:?}", TokenSource::Static("hunter2".into())).contains("hunter2"));
    }
}
