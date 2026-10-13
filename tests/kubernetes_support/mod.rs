//! An in-process mock of the Kubernetes API the adapter uses, with a
//! "kubelet" that runs a real `narsor-runner` for every Pod (U5, AE2).
//!
//! # The API
//!
//! Exactly the endpoints and verbs the controller's RBAC grants
//! (`deploy/kubernetes/controller-rbac.yaml`): Pods and PVCs get, create
//! and delete; Secrets create only (a read or delete of a Secret answers
//! 403, as RBAC would); StorageClasses get. Creates answer 409
//! `AlreadyExists` for a taken name, deletes honour a UID precondition
//! (409 on mismatch), a missing object is a 404 `Status`, and a request
//! without the expected bearer token is a 401. Every request and every
//! created object is recorded for the tests to inspect.
//!
//! # The kubelet
//!
//! A ticker that does what a node would, just faster:
//!
//! * a Pending Pod whose volumes all exist is "started": its volumes are
//!   materialized as directories (the PVC as a per-claim directory, the
//!   launch Secret and projected secrets as files, `emptyDir`s empty), the
//!   main container's command and arguments are taken *from the Pod spec*
//!   with every mount path rewritten to its directory, and the real
//!   `narsor` binary runs them. So the contract suite exercises the
//!   generated Pod, not a parallel description of it. Init containers are
//!   not run — the binary under test stands in for the runtime bundle;
//! * a runner that exits makes its Pod `Succeeded` or `Failed` with the
//!   exit code (128 + signal when killed, like a container runtime);
//! * a Pod with a `deletionTimestamp` has its runner killed and disappears,
//!   taking the Secrets it owns with it (garbage collection);
//! * a PVC with a `deletionTimestamp` disappears once no Pod mounts it
//!   (`kubernetes.io/pvc-protection`) — or not at all while a test holds
//!   it, to model a slow deprovisioner.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use narsor_controller::kubernetes::client::{ClusterAccess, TokenSource};
use narsor_controller::kubernetes::manifest::RUNNER_CONTAINER;

/// Namespace of the mock cluster.
pub const NAMESPACE: &str = "narsor-user";

/// The bearer token the mock expects.
pub const TOKEN: &str = "mock-service-account-token";

/// Storage class that deletes volumes with their claims.
pub const DELETE_CLASS: &str = "narsor-delete";

/// Storage class that keeps them.
pub const RETAIN_CLASS: &str = "narsor-retain";

/// One recorded API request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// HTTP method.
    pub method: String,
    /// Path.
    pub path: String,
    /// Response status.
    pub status: u16,
}

#[derive(Default)]
struct ClusterState {
    pods: BTreeMap<String, Value>,
    pvcs: BTreeMap<String, Value>,
    secrets: BTreeMap<String, Value>,
    storage_classes: BTreeMap<String, Value>,
    runners: HashMap<String, Runner>,
    requests: Vec<Request>,
    created: Vec<Value>,
    next_version: u64,
    hold_pvc_deletion: bool,
    kubelet: bool,
    fail_requests: u32,
}

struct Runner {
    uid: String,
    child: Child,
}

/// The mock cluster: API server plus kubelet.
pub struct MockCluster {
    state: Mutex<ClusterState>,
    scratch: PathBuf,
    program: PathBuf,
    runner_args: Vec<String>,
    /// Base URL of the API server.
    pub url: String,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl MockCluster {
    /// Serves a mock cluster on loopback. With `kubelet`, Pods run
    /// `program …`; without, they stay Pending.
    pub fn start(scratch: &Path, program: impl Into<PathBuf>, kubelet: bool) -> Arc<Self> {
        fs::create_dir_all(scratch).expect("scratch");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the mock API");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let mut state = ClusterState {
            kubelet,
            ..ClusterState::default()
        };
        for (name, policy) in [(DELETE_CLASS, "Delete"), (RETAIN_CLASS, "Retain")] {
            state.storage_classes.insert(
                name.to_string(),
                json!({
                    "apiVersion": "storage.k8s.io/v1",
                    "kind": "StorageClass",
                    "metadata": { "name": name },
                    "provisioner": "mock.narsor.dev",
                    "reclaimPolicy": policy,
                }),
            );
        }
        let cluster = Arc::new(Self {
            state: Mutex::new(state),
            scratch: scratch.to_path_buf(),
            program: program.into(),
            runner_args: [
                "--poll-wait-secs",
                "1",
                "--retry-min-ms",
                "50",
                "--retry-max-ms",
                "300",
                "--report-interval-ms",
                "50",
                "--problem-report-secs",
                "10",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            url,
            tasks: Mutex::new(Vec::new()),
        });
        let router = Router::new()
            .route(
                "/api/v1/namespaces/:namespace/:plural",
                get(collection).post(collection),
            )
            .route(
                "/api/v1/namespaces/:namespace/:plural/:name",
                get(object).delete(object),
            )
            .route(
                "/apis/storage.k8s.io/v1/storageclasses/:name",
                get(storage_class),
            )
            .with_state(Arc::clone(&cluster));
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        let weak = Arc::downgrade(&cluster);
        let kubelet = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let Some(cluster) = weak.upgrade() else {
                    return;
                };
                cluster.tick();
            }
        });
        cluster.tasks.lock().unwrap().extend([server, kubelet]);
        cluster
    }

    /// How the adapter reaches it.
    pub fn access(&self) -> ClusterAccess {
        let mut access = ClusterAccess::explicit(&self.url, NAMESPACE);
        access.token = TokenSource::Static(TOKEN.to_string());
        access
    }

    fn lock(&self) -> MutexGuard<'_, ClusterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // -- inspection and control ---------------------------------------------

    /// Every request so far.
    pub fn requests(&self) -> Vec<Request> {
        self.lock().requests.clone()
    }

    /// Requests that would change the cluster.
    pub fn mutations(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.method != "GET")
            .collect()
    }

    /// Every object a create was accepted for, as sent.
    pub fn created(&self) -> Vec<Value> {
        self.lock().created.clone()
    }

    /// Created objects of one kind.
    pub fn created_of(&self, kind: &str) -> Vec<Value> {
        self.created()
            .into_iter()
            .filter(|object| object["kind"] == kind)
            .collect()
    }

    /// The current Pod of that name.
    pub fn pod(&self, name: &str) -> Option<Value> {
        self.lock().pods.get(name).cloned()
    }

    /// The current PVC of that name.
    pub fn pvc(&self, name: &str) -> Option<Value> {
        self.lock().pvcs.get(name).cloned()
    }

    /// Names of the current Secrets.
    pub fn secret_names(&self) -> Vec<String> {
        self.lock().secrets.keys().cloned().collect()
    }

    /// Names of the current Pods.
    pub fn pod_names(&self) -> Vec<String> {
        self.lock().pods.keys().cloned().collect()
    }

    /// Adds a Secret of the trust namespace (a tool credential).
    pub fn seed_secret(&self, name: &str, data: &[(&str, &str)]) {
        let data: serde_json::Map<String, Value> = data
            .iter()
            .map(|(key, value)| (key.to_string(), Value::String(value.to_string())))
            .collect();
        self.lock().secrets.insert(
            name.to_string(),
            json!({ "metadata": { "name": name, "uid": uuid::Uuid::new_v4().to_string() }, "stringData": data }),
        );
    }

    /// Changes a storage class's reclaim policy (an operator mistake).
    pub fn set_reclaim_policy(&self, class: &str, policy: &str) {
        if let Some(object) = self.lock().storage_classes.get_mut(class) {
            object["reclaimPolicy"] = Value::String(policy.to_string());
        }
    }

    /// While held, deleted PVCs stay Terminating.
    pub fn hold_pvc_deletion(&self, hold: bool) {
        self.lock().hold_pvc_deletion = hold;
    }

    /// The next `count` API requests fail with HTTP 503.
    pub fn fail_requests(&self, count: u32) {
        self.lock().fail_requests = count;
    }

    /// Deletes a Pod behind the controller's back (an eviction, an operator
    /// with kubectl).
    pub fn evict(&self, name: &str) -> bool {
        let mut state = self.lock();
        let Some(pod) = state.pods.get_mut(name) else {
            return false;
        };
        pod["metadata"]["deletionTimestamp"] = Value::String(now());
        true
    }

    /// The Pod's node stops reporting (a partition): the Pod's phase turns
    /// `Unknown` while its runner keeps running. The runner's end, should
    /// it come, is still reported.
    pub fn partition(&self, name: &str) -> bool {
        let mut state = self.lock();
        let Some(pod) = state.pods.get_mut(name) else {
            return false;
        };
        pod["status"]["phase"] = Value::String("Unknown".into());
        true
    }

    /// Kills the runner of a Pod (an OOM kill, a crash).
    pub fn kill_runner(&self, name: &str) -> bool {
        let state = self.lock();
        let Some(runner) = state.runners.get(name) else {
            return false;
        };
        // SAFETY: signalling the process group the kubelet created.
        unsafe {
            libc::kill(-(runner.child.id() as libc::pid_t), libc::SIGKILL);
        }
        true
    }

    /// Kills every runner and stops serving. The server holds the cluster,
    /// so this — not a drop — is what ends it.
    pub fn shutdown(&self) {
        let mut state = self.lock();
        for runner in state.runners.values_mut() {
            // SAFETY: as above.
            unsafe {
                libc::kill(-(runner.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = runner.child.wait();
        }
        state.runners.clear();
        drop(state);
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    /// The local directory standing in for a PVC.
    pub fn volume_dir(&self, claim: &str) -> Option<PathBuf> {
        let uid = self.pvc(claim)?["metadata"]["uid"].as_str()?.to_string();
        Some(self.scratch.join("volumes").join(uid))
    }

    // -- the API -------------------------------------------------------------

    fn record(&self, state: &mut ClusterState, method: &Method, path: &str, status: StatusCode) {
        state.requests.push(Request {
            method: method.to_string(),
            path: path.to_string(),
            status: status.as_u16(),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn handle(
        &self,
        method: Method,
        path: String,
        headers: &HeaderMap,
        namespace: Option<&str>,
        plural: &str,
        name: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut state = self.lock();
        let (status, answer) =
            self.answer(&mut state, &method, headers, namespace, plural, name, body);
        self.record(&mut state, &method, &path, status);
        (status, Json(answer)).into_response()
    }

    #[allow(clippy::too_many_arguments)]
    fn answer(
        &self,
        state: &mut ClusterState,
        method: &Method,
        headers: &HeaderMap,
        namespace: Option<&str>,
        plural: &str,
        name: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let authorized = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(&format!("Bearer {TOKEN}"));
        if !authorized {
            return status(StatusCode::UNAUTHORIZED, "Unauthorized", "no valid token");
        }
        if state.fail_requests > 0 {
            state.fail_requests -= 1;
            return status(
                StatusCode::SERVICE_UNAVAILABLE,
                "ServiceUnavailable",
                "injected",
            );
        }
        if namespace.is_some_and(|namespace| namespace != NAMESPACE) {
            return status(StatusCode::FORBIDDEN, "Forbidden", "outside the namespace");
        }
        let forbidden = || status(StatusCode::FORBIDDEN, "Forbidden", "RBAC: not granted");
        match (plural, method.clone(), name) {
            ("storageclasses", Method::GET, Some(name)) => match state.storage_classes.get(name) {
                Some(object) => (StatusCode::OK, object.clone()),
                None => not_found(name),
            },
            ("secrets", Method::POST, None) => self.create(state, "secrets", body),
            ("secrets", _, _) => forbidden(),
            ("pods" | "persistentvolumeclaims", Method::POST, None) => {
                self.create(state, plural, body)
            }
            ("pods" | "persistentvolumeclaims", Method::GET, Some(name)) => {
                match objects(state, plural).get(name) {
                    Some(object) => (StatusCode::OK, object.clone()),
                    None => not_found(name),
                }
            }
            ("pods" | "persistentvolumeclaims", Method::DELETE, Some(name)) => {
                let precondition = body
                    .as_ref()
                    .and_then(|body| body.pointer("/preconditions/uid"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let objects = objects(state, plural);
                let Some(object) = objects.get_mut(name) else {
                    return not_found(name);
                };
                if precondition.is_some_and(|uid| object["metadata"]["uid"] != uid.as_str()) {
                    return status(StatusCode::CONFLICT, "Conflict", "uid precondition failed");
                }
                if object["metadata"]["deletionTimestamp"].is_null() {
                    object["metadata"]["deletionTimestamp"] = Value::String(now());
                }
                (StatusCode::OK, object.clone())
            }
            _ => forbidden(),
        }
    }

    fn create(
        &self,
        state: &mut ClusterState,
        plural: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let Some(mut object) = body else {
            return status(StatusCode::BAD_REQUEST, "BadRequest", "no body");
        };
        let Some(name) = object["metadata"]["name"].as_str().map(str::to_string) else {
            return status(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", "no name");
        };
        if objects(state, plural).contains_key(&name) {
            return status(StatusCode::CONFLICT, "AlreadyExists", "already exists");
        }
        state.created.push(object.clone());
        state.next_version += 1;
        let uid = uuid::Uuid::new_v4().to_string();
        object["metadata"]["uid"] = Value::String(uid.clone());
        object["metadata"]["resourceVersion"] = Value::String(state.next_version.to_string());
        object["metadata"]["creationTimestamp"] = Value::String(now());
        match plural {
            "pods" => object["status"] = json!({ "phase": "Pending" }),
            "persistentvolumeclaims" => {
                object["status"] = json!({ "phase": "Bound" });
                let _ = fs::create_dir_all(self.scratch.join("volumes").join(&uid));
            }
            _ => {}
        }
        objects(state, plural).insert(name, object.clone());
        (StatusCode::CREATED, object)
    }

    // -- the kubelet ---------------------------------------------------------

    fn tick(&self) {
        let mut state = self.lock();
        let state = &mut *state;

        // Exits.
        let mut exited = Vec::new();
        for (name, runner) in state.runners.iter_mut() {
            if let Ok(Some(status)) = runner.child.try_wait() {
                use std::os::unix::process::ExitStatusExt;
                let code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(9));
                exited.push((name.clone(), runner.uid.clone(), code));
            }
        }
        for (name, uid, code) in exited {
            state.runners.remove(&name);
            if let Some(pod) = state.pods.get_mut(&name) {
                if pod["metadata"]["uid"] == uid.as_str() {
                    pod["status"] = json!({
                        "phase": if code == 0 { "Succeeded" } else { "Failed" },
                        "containerStatuses": [{
                            "name": RUNNER_CONTAINER,
                            "state": { "terminated": { "exitCode": code } },
                        }],
                    });
                }
            }
        }

        // Deletions.
        let deleting: Vec<(String, String)> = state
            .pods
            .iter()
            .filter(|(_, pod)| !pod["metadata"]["deletionTimestamp"].is_null())
            .map(|(name, pod)| {
                (
                    name.clone(),
                    pod["metadata"]["uid"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect();
        for (name, uid) in deleting {
            if let Some(runner) = state.runners.get_mut(&name) {
                // SAFETY: the process group the kubelet created.
                unsafe {
                    libc::kill(-(runner.child.id() as libc::pid_t), libc::SIGKILL);
                }
                if !matches!(runner.child.try_wait(), Ok(Some(_))) {
                    continue;
                }
                state.runners.remove(&name);
            }
            state.pods.remove(&name);
            state.secrets.retain(|_, secret| {
                !secret["metadata"]["ownerReferences"]
                    .as_array()
                    .is_some_and(|owners| owners.iter().any(|owner| owner["uid"] == uid.as_str()))
            });
        }
        if !state.hold_pvc_deletion {
            let mounted: Vec<String> = state.pods.values().flat_map(claims).collect();
            let gone: Vec<String> = state
                .pvcs
                .iter()
                .filter(|(name, pvc)| {
                    !pvc["metadata"]["deletionTimestamp"].is_null() && !mounted.contains(name)
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in gone {
                if let Some(pvc) = state.pvcs.remove(&name) {
                    if let Some(uid) = pvc["metadata"]["uid"].as_str() {
                        let _ = fs::remove_dir_all(self.scratch.join("volumes").join(uid));
                    }
                }
            }
        }

        // Starts.
        if !state.kubelet {
            return;
        }
        let pending: Vec<String> = state
            .pods
            .iter()
            .filter(|(name, pod)| {
                pod["status"]["phase"] == "Pending"
                    && pod["metadata"]["deletionTimestamp"].is_null()
                    && !state.runners.contains_key(*name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in pending {
            let pod = state.pods[&name].clone();
            match self.start_runner(state, &pod) {
                Ok(Some(child)) => {
                    let uid = pod["metadata"]["uid"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    state.runners.insert(name.clone(), Runner { uid, child });
                    state.pods.get_mut(&name).unwrap()["status"] = json!({
                        "phase": "Running",
                        "containerStatuses": [{ "name": RUNNER_CONTAINER, "state": { "running": {} } }],
                    });
                }
                // A volume is not there yet: try again next tick.
                Ok(None) => {}
                Err(error) => {
                    eprintln!("mock kubelet: cannot start {name}: {error}");
                    state.pods.get_mut(&name).unwrap()["status"] = json!({
                        "phase": "Failed",
                        "containerStatuses": [{
                            "name": RUNNER_CONTAINER,
                            "state": { "terminated": { "exitCode": 128 } },
                        }],
                    });
                }
            }
        }
    }

    /// Materializes the Pod's volumes and runs its main container's command
    /// with the real binary; `None` while a volume source is missing.
    fn start_runner(&self, state: &ClusterState, pod: &Value) -> Result<Option<Child>, String> {
        use std::os::unix::process::CommandExt;

        let uid = pod["metadata"]["uid"].as_str().unwrap_or_default();
        let dir = self.scratch.join("pods").join(uid);
        let mut volumes: HashMap<String, PathBuf> = HashMap::new();
        for volume in pod["spec"]["volumes"].as_array().into_iter().flatten() {
            let name = volume["name"].as_str().unwrap_or_default().to_string();
            let path = dir.join(&name);
            if let Some(claim) = volume.pointer("/persistentVolumeClaim/claimName") {
                let Some(pvc) = state.pvcs.get(claim.as_str().unwrap_or_default()) else {
                    return Ok(None);
                };
                let claim_uid = pvc["metadata"]["uid"].as_str().unwrap_or_default();
                volumes.insert(name, self.scratch.join("volumes").join(claim_uid));
                continue;
            }
            let mut files: Vec<(String, String)> = Vec::new();
            if let Some(secret) = volume.pointer("/secret/secretName") {
                let Some(secret) = state.secrets.get(secret.as_str().unwrap_or_default()) else {
                    return Ok(None);
                };
                for (key, value) in secret["stringData"].as_object().into_iter().flatten() {
                    files.push((key.clone(), value.as_str().unwrap_or_default().to_string()));
                }
            }
            for source in volume
                .pointer("/projected/sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let secret_name = source["secret"]["name"].as_str().unwrap_or_default();
                let Some(secret) = state.secrets.get(secret_name) else {
                    return Ok(None);
                };
                for item in source["secret"]["items"].as_array().into_iter().flatten() {
                    let key = item["key"].as_str().unwrap_or_default();
                    let Some(value) = secret["stringData"][key].as_str() else {
                        return Ok(None);
                    };
                    files.push((
                        item["path"].as_str().unwrap_or_default().to_string(),
                        value.to_string(),
                    ));
                }
            }
            fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            for (file, value) in files {
                fs::write(path.join(file), value).map_err(|e| e.to_string())?;
            }
            volumes.insert(name, path);
        }

        let container = pod["spec"]["containers"]
            .as_array()
            .and_then(|containers| containers.iter().find(|c| c["name"] == RUNNER_CONTAINER))
            .ok_or("no runner container")?;
        let mut mounts: Vec<(String, PathBuf)> = Vec::new();
        for mount in container["volumeMounts"].as_array().into_iter().flatten() {
            let volume = mount["name"].as_str().unwrap_or_default();
            let local = volumes
                .get(volume)
                .ok_or_else(|| format!("mount of unknown volume {volume}"))?;
            mounts.push((
                mount["mountPath"].as_str().unwrap_or_default().to_string(),
                local.clone(),
            ));
        }
        // Longest mount path first, so nested mounts win.
        mounts.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        let rewrite = |arg: &str| -> String {
            for (mount, local) in &mounts {
                if arg == mount {
                    return local.display().to_string();
                }
                if let Some(rest) = arg.strip_prefix(&format!("{mount}/")) {
                    return local.join(rest).display().to_string();
                }
            }
            arg.to_string()
        };
        let command = container["command"][0].as_str().unwrap_or_default();
        if command != "/opt/narsor/bin/narsor-runner" {
            return Err(format!("unexpected command {command}"));
        }
        let args: Vec<String> = container["args"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|arg| rewrite(arg.as_str().unwrap_or_default()))
            .chain(self.runner_args.iter().cloned())
            .collect();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let log = fs::File::create(dir.join("runner.log")).map_err(|e| e.to_string())?;
        let log_err = log.try_clone().map_err(|e| e.to_string())?;
        Command::new(&self.program)
            .args(&args)
            .env("RUST_LOG", "narsor=info")
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .process_group(0)
            .spawn()
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

fn objects<'a>(state: &'a mut ClusterState, plural: &str) -> &'a mut BTreeMap<String, Value> {
    match plural {
        "pods" => &mut state.pods,
        "persistentvolumeclaims" => &mut state.pvcs,
        _ => &mut state.secrets,
    }
}

fn claims(pod: &Value) -> Vec<String> {
    pod["spec"]["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|volume| volume.pointer("/persistentVolumeClaim/claimName"))
        .filter_map(|claim| claim.as_str().map(str::to_string))
        .collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn status(code: StatusCode, reason: &str, message: &str) -> (StatusCode, Value) {
    (
        code,
        json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "reason": reason,
            "message": message,
            "code": code.as_u16(),
        }),
    )
}

fn not_found(name: &str) -> (StatusCode, Value) {
    status(
        StatusCode::NOT_FOUND,
        "NotFound",
        &format!("{name} not found"),
    )
}

async fn collection(
    State(cluster): State<Arc<MockCluster>>,
    method: Method,
    UrlPath((namespace, plural)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let path = format!("/api/v1/namespaces/{namespace}/{plural}");
    cluster.handle(
        method,
        path,
        &headers,
        Some(&namespace),
        &plural,
        None,
        body.map(|Json(body)| body),
    )
}

async fn object(
    State(cluster): State<Arc<MockCluster>>,
    method: Method,
    UrlPath((namespace, plural, name)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let path = format!("/api/v1/namespaces/{namespace}/{plural}/{name}");
    cluster.handle(
        method,
        path,
        &headers,
        Some(&namespace),
        &plural,
        Some(&name),
        body.map(|Json(body)| body),
    )
}

async fn storage_class(
    State(cluster): State<Arc<MockCluster>>,
    method: Method,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/apis/storage.k8s.io/v1/storageclasses/{name}");
    cluster.handle(
        method,
        path,
        &headers,
        None,
        "storageclasses",
        Some(&name),
        None,
    )
}
