//! The objects the adapter creates, and the checks that run before any of
//! them reaches the API server (KTD2, KTD5, KTD6).
//!
//! # One task, three kinds of object
//!
//! * a **PVC** `narsor-…-storage` — the task storage (R9): Engine state,
//!   workspace and private HOME. No owner reference: it outlives every Pod
//!   and is deleted only by archive (R34);
//! * a **Pod** `narsor-…` — the compute of one launch, `restartPolicy:
//!   Never` (no automatic restart, R13). An init container per bundle
//!   copies the pinned runtime bundle (and each tool package) into an
//!   `emptyDir`; the main container runs the repository's own pinned base
//!   image with its command replaced by `narsor runner` (R20, R21);
//! * a **Secret** `narsor-…-launch-…` per incarnation — the runner
//!   credential and the launch spec, mounted read-only *outside* the task
//!   storage and owned by the Pod, so the garbage collector removes it with
//!   its Pod and the controller never needs permission to delete Secrets.
//!
//! Tool credentials are never copied: a projected volume maps each approved
//! `secret-name/key` of the backend's namespace to one file per secret
//! reference name, which is exactly the layout the runner's
//! `--secrets-dir` expects (R19). The runner turns them into environment
//! variables or HOME links itself; the Pod spec carries no value.
//!
//! # The runtime-bundle image contract
//!
//! A runtime bundle or tool package image carries its payload under
//! [`BUNDLE_PAYLOAD`] (`bin/` first of all) and a `cp` able to run
//! [`INSTALL_COMMAND`] — a static busybox is enough. Nothing else of it is
//! used, and nothing of the base image is changed: no derived image is ever
//! built (R20).
//!
//! # Validation before creation
//!
//! [`plan`] refuses, before a single API call, everything the adapter cannot
//! run faithfully: an image without a digest or from a registry outside the
//! allow list, a missing base image or runtime bundle, per-step images, an
//! unusable resource profile, a secret mapping the operator did not
//! approve. [`check_pod_shape`] then re-checks the *generated* Pod against
//! the very rules the cluster's admission policy enforces
//! (`deploy/kubernetes/admission-policy.yaml`), so a generator bug is a
//! clear refusal here rather than an opaque admission error there.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::names;
use crate::backends::{BackendError, BackendResult, TaskResources, WorkloadLaunch};
use crate::domain::{
    ImageReference, ResolvedImage, SecretReference, TaskLaunchSpec, TaskWorkspace,
};

/// Where the task storage is mounted in the main container.
pub const TASK_MOUNT: &str = "/task";
/// Where the runtime bundle is materialized (R20).
pub const RUNTIME_MOUNT: &str = "/opt/narsor";
/// Where each tool package is materialized: `{TOOLS_MOUNT}/{tool}`.
pub const TOOLS_MOUNT: &str = "/opt/narsor-tools";
/// The read-only launch material (credential, launch spec).
pub const LAUNCH_MOUNT: &str = "/var/run/narsor/launch";
/// The read-only tool secrets, one file per reference name.
pub const SECRETS_MOUNT: &str = "/var/run/narsor/secrets";
/// Where a bundle image carries its payload.
pub const BUNDLE_PAYLOAD: &str = "/narsor";
/// What every init container runs: copy the payload into its volume.
pub const INSTALL_COMMAND: [&str; 4] = ["cp", "-a", "/narsor/.", "/opt/narsor/"];
/// Name of the main container.
pub const RUNNER_CONTAINER: &str = "runner";
/// Key of the runner credential in the launch Secret.
pub const CREDENTIAL_KEY: &str = "credential";
/// Key of the launch spec in the launch Secret.
pub const LAUNCH_SPEC_KEY: &str = "launch-spec.json";
/// The `PATH` after the bundle directories: the conventional system
/// directories. Kubernetes cannot append to an image's own `PATH`, so the
/// main container's `PATH` is spelled out in full.
pub const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Label and annotation domain.
pub const DOMAIN: &str = "narsor.dev";
/// Label: every object the adapter creates.
pub const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "narsor");
/// Label: the hash of the task's resource key.
pub const TASK_LABEL: &str = "narsor.dev/task";
/// Label: `workload`, `storage` or `launch`.
pub const COMPONENT_LABEL: &str = "narsor.dev/component";
/// Label and annotation: the desired generation the Pod was launched at.
pub const GENERATION_KEY: &str = "narsor.dev/launch-generation";
/// Label and annotation: the lease epoch of the controller that launched it.
pub const EPOCH_KEY: &str = "narsor.dev/lease-epoch";
/// Annotation: the full task id (a label value cannot hold every task id).
pub const TASK_ID_ANNOTATION: &str = "narsor.dev/task-id";
/// Annotation: the resource key the names derive from.
pub const RESOURCE_KEY_ANNOTATION: &str = "narsor.dev/resource-key";
/// Annotation: the runner incarnation.
pub const INCARNATION_ANNOTATION: &str = "narsor.dev/incarnation";
/// Annotation: the pinned environment's identity (R21).
pub const ENVIRONMENT_ANNOTATION: &str = "narsor.dev/environment-identity";
/// Annotation: the launch spec's digest.
pub const SPEC_DIGEST_ANNOTATION: &str = "narsor.dev/launch-spec-digest";

/// Longest tool name; it becomes part of a volume and container name.
const MAX_TOOL_NAME: usize = 40;

/// What the operator configures for a Kubernetes backend.
///
/// ```yaml
/// storage_class: narsor-delete
/// storage_mib: 10240
/// workload_service_account: narsor-task
/// allowed_registries: [ghcr.io/acme/, ghcr.io/narsor/]
/// allowed_secrets: [narsor-github/token]
/// ```
///
/// The same values parameterize the admission policy
/// (`deploy/kubernetes/admission-policy.yaml`); keep them in step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubernetesBackendConfig {
    /// Namespace of the task objects; `None` uses the controller's own
    /// (in-cluster) namespace or the one given on the command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Storage class of every task PVC. It must delete the backing volume
    /// with the claim (`reclaimPolicy: Delete`): archive promises the
    /// storage is gone (R34, KTD2).
    pub storage_class: String,
    /// Size of a task PVC, in MiB.
    #[serde(default = "default_storage_mib")]
    pub storage_mib: u64,
    /// The zero-RBAC service account every task Pod runs as.
    #[serde(default = "default_service_account")]
    pub workload_service_account: String,
    /// UID the runner runs as (non-root).
    #[serde(default = "default_id")]
    pub run_as_user: u32,
    /// GID the runner runs as.
    #[serde(default = "default_id")]
    pub run_as_group: u32,
    /// Group owning the mounted volumes, so the non-root runner can write
    /// its storage and read its secrets.
    #[serde(default = "default_id")]
    pub fs_group: u32,
    /// Registry prefixes (`ghcr.io/acme/`) every image must come from.
    pub allowed_registries: Vec<String>,
    /// The `secret-name/key` pairs of this namespace a task may mount —
    /// the fixed secret mappings of KTD6.
    #[serde(default)]
    pub allowed_secrets: Vec<String>,
    /// How long a Pod may stay Pending (an image that never pulls, a
    /// launch Secret that never arrived) before it counts as dead.
    #[serde(default = "default_start_timeout")]
    pub start_timeout_secs: u64,
}

fn default_storage_mib() -> u64 {
    10 * 1024
}

fn default_service_account() -> String {
    "narsor-task".to_string()
}

fn default_id() -> u32 {
    1000
}

fn default_start_timeout() -> u64 {
    15 * 60
}

impl KubernetesBackendConfig {
    /// A configuration with the defaults and the two values that have none.
    pub fn new(storage_class: impl Into<String>, allowed_registries: Vec<String>) -> Self {
        Self {
            namespace: None,
            storage_class: storage_class.into(),
            storage_mib: default_storage_mib(),
            workload_service_account: default_service_account(),
            run_as_user: default_id(),
            run_as_group: default_id(),
            fs_group: default_id(),
            allowed_registries,
            allowed_secrets: Vec::new(),
            start_timeout_secs: default_start_timeout(),
        }
    }

    /// Every problem with the configuration, empty when it is usable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(namespace) = &self.namespace {
            if !names::is_dns_label(namespace) {
                problems.push(format!("namespace '{namespace}' is not a DNS-1123 label"));
            }
        }
        if !names::is_dns_subdomain(&self.storage_class) {
            problems.push(format!(
                "storage_class '{}' is not a valid name",
                self.storage_class
            ));
        }
        if self.storage_mib == 0 {
            problems.push("storage_mib must be at least 1".to_string());
        }
        if !names::is_dns_subdomain(&self.workload_service_account) {
            problems.push(format!(
                "workload_service_account '{}' is not a valid name",
                self.workload_service_account
            ));
        }
        if self.run_as_user == 0 || self.run_as_group == 0 || self.fs_group == 0 {
            problems.push("run_as_user, run_as_group and fs_group must not be 0 (root)".into());
        }
        if self.allowed_registries.is_empty() {
            problems.push("allowed_registries must name at least one registry prefix".into());
        }
        for registry in &self.allowed_registries {
            // A prefix without its slash would let `ghcr.io/acme` admit
            // `ghcr.io/acme-evil/…`.
            if !registry.ends_with('/') || registry.len() < 2 || registry.contains('@') {
                problems.push(format!(
                    "allowed registry '{registry}' must be a prefix ending in '/'"
                ));
            }
        }
        for mapping in &self.allowed_secrets {
            if split_secret_key(mapping).is_none() {
                problems.push(format!(
                    "allowed secret '{mapping}' must be 'secret-name/key'"
                ));
            }
        }
        if self.start_timeout_secs == 0 {
            problems.push("start_timeout_secs must be at least 1".to_string());
        }
        problems
    }

    /// The admission rules the generated Pod must satisfy.
    pub fn shape_policy(&self) -> ShapePolicy {
        ShapePolicy {
            service_account: self.workload_service_account.clone(),
            allowed_registries: self.allowed_registries.clone(),
            allowed_secrets: self.allowed_secrets.iter().cloned().collect(),
        }
    }
}

/// `secret-name/key` → its parts, when both are valid.
pub fn split_secret_key(mapping: &str) -> Option<(&str, &str)> {
    let (secret, key) = mapping.split_once('/')?;
    (names::is_dns_subdomain(secret) && names::is_secret_key(key)).then_some((secret, key))
}

/// One tool package to materialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBundle {
    /// The tool's name (a DNS label).
    pub name: String,
    /// Its pinned image.
    pub image: String,
}

/// One file of the projected secrets volume.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretFile {
    /// The Secret in the backend's namespace.
    pub secret: String,
    /// Its key.
    pub key: String,
    /// The file name: the reference name the runner looks for.
    pub path: String,
}

/// A launch, validated: everything the Pod is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The repository's pinned base image (`repo@sha256:…`).
    pub base_image: String,
    /// The pinned runtime bundle.
    pub runtime_bundle: String,
    /// Tool packages, by name.
    pub tools: Vec<ToolBundle>,
    /// Tool and clone credentials, by file.
    pub secrets: Vec<SecretFile>,
    /// `{cpu}m`.
    pub cpu: String,
    /// `{memory}Mi`.
    pub memory: String,
}

fn rejected(message: impl Into<String>) -> BackendError {
    BackendError::rejected(message)
}

/// Checks one pinned image: a digest, the `pinned` form of it, an allowed
/// registry.
fn check_image(
    what: &str,
    image: &ResolvedImage,
    config: &KubernetesBackendConfig,
) -> BackendResult<String> {
    let parsed = ImageReference::parse(&image.pinned)
        .map_err(|error| rejected(format!("{what}: {error}")))?;
    if parsed.digest.as_ref() != Some(&image.digest) || parsed.tag.is_some() {
        return Err(rejected(format!(
            "{what} '{}' is not pinned as 'repository@{}'",
            image.pinned, image.digest
        )));
    }
    if !config
        .allowed_registries
        .iter()
        .any(|prefix| image.pinned.starts_with(prefix.as_str()))
    {
        return Err(rejected(format!(
            "{what} '{}' is not from an allowed registry ({})",
            image.pinned,
            config.allowed_registries.join(", ")
        )));
    }
    Ok(image.pinned.clone())
}

/// Validates a launch against the backend's configuration — before any API
/// call — and returns what the Pod is built from.
pub fn plan(
    config: &KubernetesBackendConfig,
    launch: &WorkloadLaunch,
) -> BackendResult<LaunchPlan> {
    let spec: &TaskLaunchSpec = &launch.launch_spec;
    let environment = &spec.environment;
    if !environment.identity_is_valid() {
        return Err(rejected(
            "the launch spec's environment does not match its identity",
        ));
    }
    let base_image = environment.base_image.as_ref().ok_or_else(|| {
        rejected("the task has no base image; a Kubernetes task runs in its repository's image")
    })?;
    let base_image = check_image("base image", base_image, config)?;
    if !environment.step_images.is_empty() {
        let states: Vec<&str> = environment.step_images.keys().map(String::as_str).collect();
        return Err(rejected(format!(
            "per-step images ({}) are not supported on Kubernetes: one task runs in one Pod \
             on one base image",
            states.join(", ")
        )));
    }
    let bundle = environment
        .runtime_bundle
        .as_ref()
        .ok_or_else(|| rejected("the task pins no Narsor runtime bundle"))?;
    let runtime_bundle = check_image("runtime bundle", bundle, config)?;
    let mut tools = Vec::new();
    for (name, image) in &environment.tools {
        if name.len() > MAX_TOOL_NAME || !names::is_dns_label(name) {
            return Err(rejected(format!(
                "tool name '{name}' must be a DNS label of at most {MAX_TOOL_NAME} characters"
            )));
        }
        tools.push(ToolBundle {
            name: name.clone(),
            image: check_image(&format!("tool '{name}'"), image, config)?,
        });
    }

    let problems = spec.resources.problems();
    if !problems.is_empty() {
        return Err(rejected(format!(
            "invalid resource profile: {}",
            problems.join("; ")
        )));
    }

    let mut references: Vec<&SecretReference> = environment.secrets.iter().collect();
    if let TaskWorkspace::Git(git) = &spec.workspace {
        references.extend(git.credential.as_ref());
    }
    let mut secrets: BTreeMap<String, SecretFile> = BTreeMap::new();
    for reference in references {
        if let Some(problem) = reference.delivery.problem() {
            return Err(rejected(format!("secret '{}': {problem}", reference.name)));
        }
        let plain = !reference.name.is_empty()
            && !reference.name.starts_with('.')
            && names::is_secret_key(&reference.name);
        if !plain {
            return Err(rejected(format!(
                "secret reference name '{}' cannot be a file name",
                reference.name
            )));
        }
        if !config.allowed_secrets.contains(&reference.key) {
            return Err(rejected(format!(
                "secret '{}' maps '{}', which this backend does not approve",
                reference.name, reference.key
            )));
        }
        let (secret, key) = split_secret_key(&reference.key).ok_or_else(|| {
            rejected(format!(
                "secret '{}' key '{}' is not 'secret-name/key'",
                reference.name, reference.key
            ))
        })?;
        let file = SecretFile {
            secret: secret.to_string(),
            key: key.to_string(),
            path: reference.name.clone(),
        };
        match secrets.get(&reference.name) {
            Some(existing) if *existing != file => {
                return Err(rejected(format!(
                    "secret reference '{}' names two different values",
                    reference.name
                )))
            }
            _ => {
                secrets.insert(reference.name.clone(), file);
            }
        }
    }

    let url = &launch.control_plane_url;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(rejected(format!(
            "control plane URL '{url}' is not http(s)"
        )));
    }

    Ok(LaunchPlan {
        base_image,
        runtime_bundle,
        tools,
        secrets: secrets.into_values().collect(),
        cpu: format!("{}m", spec.resources.cpu_millicores),
        memory: format!("{}Mi", spec.resources.memory_mib),
    })
}

fn container_security() -> Value {
    json!({
        "privileged": false,
        "allowPrivilegeEscalation": false,
        "runAsNonRoot": true,
        "capabilities": { "drop": ["ALL"] },
        "seccompProfile": { "type": "RuntimeDefault" },
    })
}

fn labels(task: &TaskResources, component: &str) -> serde_json::Map<String, Value> {
    let mut labels = serde_json::Map::new();
    labels.insert(MANAGED_BY.0.into(), MANAGED_BY.1.into());
    labels.insert(
        TASK_LABEL.into(),
        names::key_hash(&task.resource_key).into(),
    );
    labels.insert(COMPONENT_LABEL.into(), component.into());
    labels
}

fn task_annotations(task: &TaskResources) -> serde_json::Map<String, Value> {
    let mut annotations = serde_json::Map::new();
    annotations.insert(TASK_ID_ANNOTATION.into(), task.task_id.as_str().into());
    annotations.insert(
        RESOURCE_KEY_ANNOTATION.into(),
        task.resource_key.clone().into(),
    );
    annotations
}

/// The task storage claim.
pub fn pvc(config: &KubernetesBackendConfig, namespace: &str, task: &TaskResources) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": task.storage,
            "namespace": namespace,
            "labels": labels(task, "storage"),
            "annotations": task_annotations(task),
        },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": config.storage_class,
            "resources": { "requests": { "storage": format!("{}Mi", config.storage_mib) } },
        },
    })
}

/// The `PATH` of the main container: the runtime bundle first, then each
/// tool package, then the system directories.
pub fn runner_path(plan: &LaunchPlan) -> String {
    let mut path = format!("{RUNTIME_MOUNT}/bin");
    for tool in &plan.tools {
        path.push_str(&format!(":{TOOLS_MOUNT}/{}/bin", tool.name));
    }
    path.push(':');
    path.push_str(SYSTEM_PATH);
    path
}

/// The task Pod of one launch.
pub fn pod(
    config: &KubernetesBackendConfig,
    namespace: &str,
    task: &TaskResources,
    launch: &WorkloadLaunch,
    plan: &LaunchPlan,
) -> Value {
    let launch_secret = names::launch_secret(&task.workload, &launch.incarnation);
    let resources = json!({
        "requests": { "cpu": plan.cpu, "memory": plan.memory },
        "limits": { "cpu": plan.cpu, "memory": plan.memory },
    });

    let mut labels = labels(task, "workload");
    labels.insert(GENERATION_KEY.into(), launch.generation.to_string().into());
    labels.insert(EPOCH_KEY.into(), launch.lease_epoch.to_string().into());
    let mut annotations = task_annotations(task);
    annotations.insert(
        INCARNATION_ANNOTATION.into(),
        launch.incarnation.clone().into(),
    );
    annotations.insert(GENERATION_KEY.into(), launch.generation.to_string().into());
    annotations.insert(EPOCH_KEY.into(), launch.lease_epoch.to_string().into());
    annotations.insert(
        ENVIRONMENT_ANNOTATION.into(),
        launch.launch_spec.environment.identity.clone().into(),
    );
    annotations.insert(
        SPEC_DIGEST_ANNOTATION.into(),
        launch.launch_spec.digest.clone().into(),
    );

    let mut init_containers = vec![json!({
        "name": "runtime-bundle",
        "image": plan.runtime_bundle,
        "command": INSTALL_COMMAND,
        "volumeMounts": [{ "name": "narsor-runtime", "mountPath": RUNTIME_MOUNT }],
        "resources": resources,
        "securityContext": container_security(),
    })];
    let mut volumes = vec![
        json!({ "name": "task-storage", "persistentVolumeClaim": { "claimName": task.storage } }),
        json!({ "name": "narsor-runtime", "emptyDir": {} }),
        json!({
            "name": "narsor-launch",
            "secret": { "secretName": launch_secret, "defaultMode": 0o440 },
        }),
    ];
    let mut mounts = vec![
        json!({ "name": "task-storage", "mountPath": TASK_MOUNT }),
        json!({ "name": "narsor-runtime", "mountPath": RUNTIME_MOUNT, "readOnly": true }),
        json!({ "name": "narsor-launch", "mountPath": LAUNCH_MOUNT, "readOnly": true }),
    ];
    for tool in &plan.tools {
        let volume = format!("tool-{}", tool.name);
        init_containers.push(json!({
            "name": volume,
            "image": tool.image,
            "command": INSTALL_COMMAND,
            "volumeMounts": [{ "name": volume, "mountPath": RUNTIME_MOUNT }],
            "resources": resources,
            "securityContext": container_security(),
        }));
        volumes.push(json!({ "name": volume, "emptyDir": {} }));
        mounts.push(json!({
            "name": volume,
            "mountPath": format!("{TOOLS_MOUNT}/{}", tool.name),
            "readOnly": true,
        }));
    }

    let mut args = vec![
        "runner".to_string(),
        "--task-id".into(),
        launch.task_id.to_string(),
        "--task-storage".into(),
        TASK_MOUNT.into(),
        "--incarnation".into(),
        launch.incarnation.clone(),
        "--control-plane".into(),
        launch.control_plane_url.clone(),
        "--credential-file".into(),
        format!("{LAUNCH_MOUNT}/{CREDENTIAL_KEY}"),
        "--launch-spec".into(),
        format!("{LAUNCH_MOUNT}/{LAUNCH_SPEC_KEY}"),
    ];
    if !plan.secrets.is_empty() {
        // One projected source per Secret, each listing only the approved
        // keys: nothing else of that Secret becomes visible.
        let mut by_secret: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for file in &plan.secrets {
            by_secret
                .entry(file.secret.as_str())
                .or_default()
                .push(json!({ "key": file.key, "path": file.path }));
        }
        let sources: Vec<Value> = by_secret
            .into_iter()
            .map(|(secret, items)| json!({ "secret": { "name": secret, "items": items } }))
            .collect();
        volumes.push(json!({
            "name": "tool-secrets",
            "projected": { "defaultMode": 0o440, "sources": sources },
        }));
        mounts
            .push(json!({ "name": "tool-secrets", "mountPath": SECRETS_MOUNT, "readOnly": true }));
        args.push("--secrets-dir".into());
        args.push(SECRETS_MOUNT.into());
    }

    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": task.workload,
            "namespace": namespace,
            "labels": labels,
            "annotations": annotations,
        },
        "spec": {
            "restartPolicy": "Never",
            "serviceAccountName": config.workload_service_account,
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "hostNetwork": false,
            "hostPID": false,
            "hostIPC": false,
            "securityContext": {
                "runAsNonRoot": true,
                "runAsUser": config.run_as_user,
                "runAsGroup": config.run_as_group,
                "fsGroup": config.fs_group,
                "fsGroupChangePolicy": "OnRootMismatch",
                "seccompProfile": { "type": "RuntimeDefault" },
            },
            "initContainers": init_containers,
            "containers": [{
                "name": RUNNER_CONTAINER,
                "image": plan.base_image,
                "command": [format!("{RUNTIME_MOUNT}/bin/narsor")],
                "args": args,
                // PATH only: HOME is the runner's to set (a private HOME on
                // the task storage, R17), and tool secrets reach their
                // tools through the runner, never through the Pod spec.
                "env": [{ "name": "PATH", "value": runner_path(plan) }],
                "volumeMounts": mounts,
                "resources": resources,
                "securityContext": container_security(),
                "terminationMessagePolicy": "FallbackToLogsOnError",
            }],
            "volumes": volumes,
        },
    })
}

/// The launch Secret of one incarnation, owned by its Pod.
pub fn launch_secret(
    namespace: &str,
    task: &TaskResources,
    launch: &WorkloadLaunch,
    pod_uid: &str,
) -> BackendResult<Value> {
    let spec = serde_json::to_string(&launch.launch_spec)
        .map_err(|error| rejected(format!("the launch spec does not serialize: {error}")))?;
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "Opaque",
        "immutable": true,
        "metadata": {
            "name": names::launch_secret(&task.workload, &launch.incarnation),
            "namespace": namespace,
            "labels": labels(task, "launch"),
            "annotations": { INCARNATION_ANNOTATION: launch.incarnation },
            "ownerReferences": [{
                "apiVersion": "v1",
                "kind": "Pod",
                "name": task.workload,
                "uid": pod_uid,
                "blockOwnerDeletion": false,
            }],
        },
        "stringData": {
            CREDENTIAL_KEY: launch.credential.expose(),
            LAUNCH_SPEC_KEY: spec,
        },
    }))
}

/// The admission rules of KTD6, as the generated Pod must satisfy them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapePolicy {
    /// The one service account task Pods may use.
    pub service_account: String,
    /// Registry prefixes every image must start with.
    pub allowed_registries: Vec<String>,
    /// The `secret-name/key` pairs a projected volume may map.
    pub allowed_secrets: BTreeSet<String>,
}

/// Checks a Pod against the admission rules (mirrors
/// `deploy/kubernetes/admission-policy.yaml`); every violation at once.
pub fn check_pod_shape(pod: &Value, policy: &ShapePolicy) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    let name = pod
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let empty = serde_json::Map::new();
    let spec = pod.get("spec").and_then(Value::as_object).unwrap_or(&empty);
    let field = |key: &str| spec.get(key);

    if field("serviceAccountName").and_then(Value::as_str) != Some(&policy.service_account) {
        problems.push(format!(
            "the Pod must run as service account '{}'",
            policy.service_account
        ));
    }
    if field("automountServiceAccountToken") != Some(&Value::Bool(false)) {
        problems.push("automountServiceAccountToken must be false".to_string());
    }
    for host in ["hostNetwork", "hostPID", "hostIPC"] {
        if field(host) == Some(&Value::Bool(true)) {
            problems.push(format!("{host} is not allowed"));
        }
    }
    if field("restartPolicy").and_then(Value::as_str) != Some("Never") {
        problems.push("restartPolicy must be Never".to_string());
    }
    if field("ephemeralContainers").is_some() {
        problems.push("ephemeral containers are not allowed".to_string());
    }
    if pod.pointer("/spec/securityContext/runAsNonRoot") != Some(&Value::Bool(true)) {
        problems.push("securityContext.runAsNonRoot must be true".to_string());
    }

    let containers = ["initContainers", "containers"]
        .into_iter()
        .filter_map(|key| field(key).and_then(Value::as_array))
        .flatten();
    for container in containers {
        let cname = container.get("name").and_then(Value::as_str).unwrap_or("?");
        let image = container
            .get("image")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_pinned_image(image) {
            problems.push(format!(
                "container '{cname}': image '{image}' has no digest"
            ));
        }
        if !policy
            .allowed_registries
            .iter()
            .any(|prefix| image.starts_with(prefix.as_str()))
        {
            problems.push(format!(
                "container '{cname}': image '{image}' is not from an allowed registry"
            ));
        }
        let security = container.get("securityContext");
        let flag = |key: &str| security.and_then(|s| s.get(key));
        if flag("privileged") == Some(&Value::Bool(true)) {
            problems.push(format!("container '{cname}' must not be privileged"));
        }
        if flag("allowPrivilegeEscalation") != Some(&Value::Bool(false)) {
            problems.push(format!(
                "container '{cname}' must set allowPrivilegeEscalation: false"
            ));
        }
        let drops_all = security
            .and_then(|s| s.pointer("/capabilities/drop"))
            .and_then(Value::as_array)
            .is_some_and(|drop| drop.iter().any(|cap| cap == "ALL"));
        if !drops_all {
            problems.push(format!("container '{cname}' must drop ALL capabilities"));
        }
        if security
            .and_then(|s| s.pointer("/capabilities/add"))
            .and_then(Value::as_array)
            .is_some_and(|add| !add.is_empty())
        {
            problems.push(format!("container '{cname}' must not add capabilities"));
        }
        let host_port = container
            .get("ports")
            .and_then(Value::as_array)
            .is_some_and(|ports| ports.iter().any(|port| port.get("hostPort").is_some()));
        if host_port {
            problems.push(format!("container '{cname}' must not use a host port"));
        }
        // A `secretKeyRef` would hand any Secret of the namespace to the
        // task; secrets arrive only through the approved volume mappings.
        let env_reference = container.get("envFrom").is_some()
            || container
                .get("env")
                .and_then(Value::as_array)
                .is_some_and(|env| env.iter().any(|var| var.get("valueFrom").is_some()));
        if env_reference {
            problems.push(format!(
                "container '{cname}' must not take environment values from other objects"
            ));
        }
    }

    let volumes = field("volumes").and_then(Value::as_array);
    for volume in volumes.into_iter().flatten() {
        let vname = volume.get("name").and_then(Value::as_str).unwrap_or("?");
        let kinds: Vec<&str> = volume
            .as_object()
            .map(|object| {
                object
                    .keys()
                    .map(String::as_str)
                    .filter(|key| *key != "name")
                    .collect()
            })
            .unwrap_or_default();
        match kinds.as_slice() {
            ["emptyDir"] => {}
            ["persistentVolumeClaim"] => {
                let claim = volume
                    .pointer("/persistentVolumeClaim/claimName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if claim != format!("{name}{}", names::STORAGE_SUFFIX) {
                    problems.push(format!(
                        "volume '{vname}': claim '{claim}' is not this task's storage"
                    ));
                }
            }
            ["secret"] => {
                let secret = volume
                    .pointer("/secret/secretName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !secret.starts_with(&format!("{name}{}", names::LAUNCH_INFIX)) {
                    problems.push(format!(
                        "volume '{vname}': secret '{secret}' is not this Pod's launch secret"
                    ));
                }
            }
            ["projected"] => {
                let sources = volume
                    .pointer("/projected/sources")
                    .and_then(Value::as_array);
                for source in sources.into_iter().flatten() {
                    let Some(secret) = source.get("secret") else {
                        problems.push(format!(
                            "volume '{vname}': only secret sources may be projected"
                        ));
                        continue;
                    };
                    if source.as_object().is_some_and(|object| object.len() != 1) {
                        problems.push(format!(
                            "volume '{vname}': only secret sources may be projected"
                        ));
                    }
                    let secret_name = secret
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let items = secret.get("items").and_then(Value::as_array);
                    if items.is_none_or(|items| items.is_empty()) {
                        problems.push(format!(
                            "volume '{vname}': secret '{secret_name}' must list its approved keys"
                        ));
                    }
                    for item in items.into_iter().flatten() {
                        let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
                        let mapping = format!("{secret_name}/{key}");
                        if !policy.allowed_secrets.contains(&mapping) {
                            problems.push(format!(
                                "volume '{vname}': secret mapping '{mapping}' is not approved"
                            ));
                        }
                    }
                }
            }
            other => problems.push(format!(
                "volume '{vname}': volume type {other:?} is not allowed"
            )),
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// `…@sha256:<64 lowercase hex>`.
fn is_pinned_image(image: &str) -> bool {
    image
        .rsplit_once("@sha256:")
        .is_some_and(|(repository, hex)| {
            !repository.is_empty()
                && hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}
