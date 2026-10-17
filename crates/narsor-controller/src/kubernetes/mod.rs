//! The Kubernetes adapter: one directly managed Pod and one independently
//! retained PVC per task (U5: R7-R9, R12-R22, R34, R39-R40, R42; KTD2,
//! KTD5, KTD6).
//!
//! # Mapping the backend contract
//!
//! | contract | Kubernetes |
//! |---|---|
//! | workload | Pod `narsor-{key}-{hash}`, `restartPolicy: Never` |
//! | task storage | PVC `narsor-{key}-{hash}-storage`, no owner reference |
//! | `uid` | the Pod's UID |
//! | `resource_generation` | launch generation × 8 + phase rank (below) |
//! | `incarnation`, `launch_generation` | the Pod's annotations |
//! | release compute | delete the Pod (UID precondition) |
//! | archive | delete the Pod, then — once it is gone — the PVC |
//!
//! Why not a Job or a StatefulSet: both would restart or replace a Pod on
//! their own, while Narsor replaces a lost workload only for a newer launch
//! generation recorded by the control plane — the owner's start/resume or
//! the resume policy's `Resume` (KTD2, D58).
//!
//! # Observation
//!
//! A Pod with a `deletionTimestamp` is `Deleting`; otherwise `Pending` is
//! `Starting` (until [`KubernetesBackendConfig::start_timeout_secs`], after
//! which a Pod that never started — an image that will not pull, a launch
//! Secret that never arrived — counts as terminated), `Running` is
//! `Running`, `Succeeded`/`Failed` are `Exited` with the runner container's
//! exit code. `Unknown` (the node stopped reporting) is an observation
//! failure: the controller then reports the task Unknown rather than
//! guessing it dead while a partitioned node may still run it.
//!
//! The same holds when the controller looks again to confirm an Unknown
//! task's loss: only a Pod that is gone or `Succeeded`/`Failed` (its node
//! reported the runner's end) confirms it. A Pod in phase `Unknown` stays
//! an observation failure, a `Running` one on a `NotReady` node still
//! counts as running, and an evicted one is `Deleting` until its node lets
//! go of it — all of which keep the task Unknown. A Pod still `Pending`
//! past the start timeout counts as terminated here too: a relaunch
//! deletes it first and, the Pod's name being the task's, cannot create a
//! second one while it exists. The ReadWriteOnce claim is the second guard
//! against two runners on one storage.
//!
//! `resourceVersion` is opaque and must not be compared, so the monotonic
//! `resource_generation` is derived deliberately: the phase only ever moves
//! forward for one Pod (Starting 1 → Running 2 → Exited 3 → Deleting 4), and
//! scaling by the launch generation keeps successive Pods of one task in
//! order too.
//!
//! # Race safety
//!
//! * One name per task: a second Pod of a task cannot exist; a create that
//!   finds one (HTTP 409) returns it as is (`created_workload: false`).
//! * Deletions carry the observed UID as a precondition, so a replacement
//!   another controller created meanwhile is never deleted by mistake.
//! * A relaunch needs the existing PVC (`StorageMissing` otherwise), and a
//!   PVC being deleted is never launched on: an archived task's storage is
//!   not quietly recreated.
//! * The Pod carries the lease epoch and launch generation as labels: an
//!   operator (or an admission rule) sees which lease holder made it.
//!
//! The controller decides everything else — when a Pod may go, when a new
//! one may come (KTD2); see [`narsor_engine::backends::Controller`].

pub mod client;
pub mod manifest;
pub mod names;
pub mod objects;

use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;

use self::client::{ClusterAccess, Created, KubeClient};
pub use self::manifest::KubernetesBackendConfig;
use self::manifest::{EPOCH_KEY, GENERATION_KEY, INCARNATION_ANNOTATION, RESOURCE_KEY_ANNOTATION};
use self::objects::{PersistentVolumeClaim, Pod, StorageClass};
use narsor_engine::backends::{
    ArchiveProgress, BackendError, BackendResult, ExecutionBackend, Provisioned, Removal,
    StorageState, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase, WorkloadState,
};
use narsor_engine::domain::TaskId;

fn validate_profile_settings(settings: &Value) -> BackendResult<()> {
    let root = settings
        .as_object()
        .ok_or_else(|| BackendError::rejected("profile settings must be a JSON object"))?;
    if root.len() != 1 || !root.contains_key("permissions") {
        return Err(BackendError::rejected(
            "profile settings may contain only permissions.allow and permissions.deny",
        ));
    }
    let permissions = root["permissions"]
        .as_object()
        .ok_or_else(|| BackendError::rejected("profile permissions must be an object"))?;
    if permissions
        .keys()
        .any(|key| !matches!(key.as_str(), "allow" | "deny"))
    {
        return Err(BackendError::rejected(
            "profile settings may contain only permissions.allow and permissions.deny",
        ));
    }
    for key in ["allow", "deny"] {
        if let Some(value) = permissions.get(key) {
            let entries = value.as_array().ok_or_else(|| {
                BackendError::rejected(format!("permissions.{key} must be an array"))
            })?;
            if entries
                .iter()
                .any(|entry| !entry.as_str().is_some_and(|value| !value.trim().is_empty()))
            {
                return Err(BackendError::rejected(format!(
                    "permissions.{key} entries must be non-empty strings"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod profile_update_tests {
    use super::*;

    #[test]
    fn profile_proposals_allow_only_permission_lists() {
        for value in [
            serde_json::json!({"permissions":{"allow":["Read"],"deny":[]}}),
            serde_json::json!({"permissions":{"allow":[]}}),
        ] {
            assert!(validate_profile_settings(&value).is_ok(), "{value}");
        }
        for value in [
            serde_json::json!({"permissions":{"allow":[],"hooks":{}}}),
            serde_json::json!({"apiKey":"secret","permissions":{"allow":[]}}),
            serde_json::json!({"permissions":{"allow":"Read"}}),
            serde_json::json!({"permissions":{"allow":[""]}}),
            serde_json::json!({"permissions":{"allow":[],"deny":[],"other":[]}}),
        ] {
            assert!(validate_profile_settings(&value).is_err(), "{value}");
        }
    }
}
use narsor_engine::environment::{BackendCapabilities, BackendKind};

/// The Kubernetes adapter. Cloning shares the HTTP client.
#[derive(Debug, Clone)]
pub struct KubernetesBackend {
    client: KubeClient,
    config: Arc<KubernetesBackendConfig>,
}

impl KubernetesBackend {
    /// An adapter working in `access`'s namespace.
    pub fn new(access: &ClusterAccess, config: KubernetesBackendConfig) -> Result<Self, String> {
        let problems = config.problems();
        if !problems.is_empty() {
            return Err(format!(
                "invalid Kubernetes backend configuration: {}",
                problems.join("; ")
            ));
        }
        if !names::is_dns_label(&access.namespace) {
            return Err(format!(
                "namespace '{}' is not a DNS-1123 label",
                access.namespace
            ));
        }
        Ok(Self {
            client: KubeClient::new(access)?,
            config: Arc::new(config),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &KubernetesBackendConfig {
        &self.config
    }

    /// The namespace task objects live in.
    pub fn namespace(&self) -> &str {
        self.client.namespace()
    }

    /// Applies an owner-approved profile update with a digest check and
    /// Kubernetes resourceVersion compare-and-swap. Replays are harmless.
    pub async fn apply_claude_profile_update(
        &self,
        update: &narsor_protocol::controller::ClaudeProfileUpdate,
    ) -> BackendResult<()> {
        if self.config.claude_profile_secret.as_deref() != Some(update.secret_name.as_str()) {
            return Err(BackendError::rejected(
                "profile update names a Secret outside this backend's configured profile",
            ));
        }
        validate_profile_settings(&update.settings)?;
        if narsor_engine::domain::canonical_sha256(&update.settings) != update.proposed_digest {
            return Err(BackendError::rejected(
                "profile update digest does not match its settings",
            ));
        }
        let path = format!(
            "/api/v1/namespaces/{}/secrets/{}",
            self.namespace(),
            update.secret_name
        );
        let mut secret = self
            .client
            .get(&path)
            .await
            .map_err(|error| BackendError {
                kind: error.kind,
                message: "could not read the configured profile Secret".into(),
            })?
            .ok_or_else(|| BackendError::rejected("configured profile Secret does not exist"))?;
        let metadata = secret
            .get("metadata")
            .and_then(Value::as_object)
            .ok_or_else(|| BackendError::rejected("profile Secret metadata is malformed"))?;
        let annotations = metadata.get("annotations").and_then(Value::as_object);
        if annotations
            .and_then(|a| a.get("narsor.dev/profile-proposal-id"))
            .and_then(Value::as_str)
            == Some(update.proposal_id.as_str())
            && annotations
                .and_then(|a| a.get("narsor.dev/profile-digest"))
                .and_then(Value::as_str)
                == Some(update.proposed_digest.as_str())
        {
            return Ok(());
        }
        let version = metadata
            .get("resourceVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if version != update.expected_resource_version {
            return Err(BackendError::transient(
                "profile Secret changed after proposal review; refresh the proposal",
            ));
        }
        let key = &self.config.claude_profile_settings_key;
        let encoded = secret
            .pointer(&format!("/data/{key}"))
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError::rejected("configured profile key is missing"))?;
        let current_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).map_err(
                |_| BackendError::rejected("configured profile key is not valid base64"),
            )?;
        let current: Value = serde_json::from_slice(&current_bytes)
            .map_err(|_| BackendError::rejected("configured profile settings are invalid JSON"))?;
        if narsor_engine::domain::canonical_sha256(&current) != update.expected_digest {
            return Err(BackendError::transient(
                "profile Secret content changed after proposal review; refresh the proposal",
            ));
        }
        let serialized = serde_json::to_vec(&update.settings)
            .map_err(|error| BackendError::rejected(error.to_string()))?;
        secret["data"][key] = Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serialized,
        ));
        secret["metadata"]["annotations"]["narsor.dev/profile-proposal-id"] =
            Value::String(update.proposal_id.clone());
        secret["metadata"]["annotations"]["narsor.dev/profile-digest"] =
            Value::String(update.proposed_digest.clone());
        self.client
            .replace(&path, &secret)
            .await
            .map_err(|error| BackendError {
                kind: error.kind,
                message: if error.kind == narsor_engine::backends::BackendErrorKind::Transient {
                    "profile Secret changed during the update; refresh the proposal".into()
                } else {
                    "Kubernetes refused the configured profile update".into()
                },
            })?;
        Ok(())
    }

    /// Checks, at controller start, what would otherwise fail a task later:
    /// the API server answers and the storage class destroys data with its
    /// claim (KTD2's archive-as-destruction).
    pub async fn preflight(&self) -> BackendResult<()> {
        self.check_storage_class(&self.config.storage_class).await
    }

    /// Refuses a storage class that keeps the backing volume after its
    /// claim is deleted: archive would then claim a removal it did not do.
    async fn check_storage_class(&self, name: &str) -> BackendResult<()> {
        let path = format!("/apis/storage.k8s.io/v1/storageclasses/{name}");
        let class: StorageClass = match self.client.get(&path).await? {
            Some(value) => parse("storage class", value)?,
            None => {
                return Err(BackendError::rejected(format!(
                    "storage class '{name}' does not exist"
                )))
            }
        };
        if class.deletes_volumes() {
            Ok(())
        } else {
            Err(BackendError::rejected(format!(
                "storage class '{name}' has reclaimPolicy {}: deleting a task's claim would \
                 keep its data, so archive could not destroy it; use a class with \
                 reclaimPolicy Delete",
                class.reclaim_policy.as_deref().unwrap_or("?")
            )))
        }
    }

    async fn get_pod(&self, task: &TaskResources) -> BackendResult<Option<Pod>> {
        let path = self.client.core_path("pods", Some(&task.workload));
        let Some(value) = self.client.get(&path).await? else {
            return Ok(None);
        };
        let pod: Pod = parse("Pod", value)?;
        check_owner(task, &pod.metadata.annotations, &task.workload)?;
        Ok(Some(pod))
    }

    async fn get_pvc(&self, task: &TaskResources) -> BackendResult<Option<PersistentVolumeClaim>> {
        let path = self
            .client
            .core_path("persistentvolumeclaims", Some(&task.storage));
        let Some(value) = self.client.get(&path).await? else {
            return Ok(None);
        };
        let pvc: PersistentVolumeClaim = parse("PVC", value)?;
        check_owner(task, &pvc.metadata.annotations, &task.storage)?;
        Ok(Some(pvc))
    }

    /// The normalized state of one of this adapter's Pods.
    fn workload_state(&self, pod: &Pod) -> BackendResult<WorkloadState> {
        let annotations = &pod.metadata.annotations;
        let incarnation = annotations
            .get(INCARNATION_ANNOTATION)
            .cloned()
            .ok_or_else(|| {
                BackendError::rejected(format!(
                    "Pod '{}' carries no runner incarnation",
                    pod.metadata.name
                ))
            })?;
        let launch_generation = annotations
            .get(GENERATION_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                BackendError::rejected(format!(
                    "Pod '{}' carries no launch generation",
                    pod.metadata.name
                ))
            })?;
        let phase = self.phase(pod)?;
        let rank = match phase {
            WorkloadPhase::Starting => 1,
            WorkloadPhase::Running => 2,
            WorkloadPhase::Exited { .. } => 3,
            WorkloadPhase::Deleting => 4,
        };
        Ok(WorkloadState {
            uid: pod.metadata.uid.clone(),
            resource_generation: launch_generation.saturating_mul(8).saturating_add(rank),
            incarnation,
            launch_generation,
            phase,
        })
    }

    fn phase(&self, pod: &Pod) -> BackendResult<WorkloadPhase> {
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(WorkloadPhase::Deleting);
        }
        match pod.status.phase.as_deref() {
            None | Some("Pending") => {
                let started_long_ago = pod.metadata.creation_timestamp.is_some_and(|created| {
                    let age = Utc::now().signed_duration_since(created);
                    age.num_seconds() >= self.config.start_timeout_secs as i64
                });
                Ok(if started_long_ago {
                    WorkloadPhase::Exited { code: None }
                } else {
                    WorkloadPhase::Starting
                })
            }
            Some("Running") => Ok(WorkloadPhase::Running),
            Some("Succeeded") | Some("Failed") => {
                let code = pod
                    .status
                    .container_statuses
                    .iter()
                    .find(|status| status.name == manifest::RUNNER_CONTAINER)
                    .and_then(|status| status.state.terminated.as_ref())
                    .and_then(|terminated| terminated.exit_code);
                Ok(WorkloadPhase::Exited { code })
            }
            Some(other) => Err(BackendError::transient(format!(
                "Pod '{}' is in phase {other}: its node does not report",
                pod.metadata.name
            ))),
        }
    }

    async fn create(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        create_storage: bool,
    ) -> BackendResult<Provisioned> {
        // Everything that can be refused is refused before the first call.
        let plan = manifest::plan(&self.config, launch)?;
        let namespace = self.client.namespace();
        let pod = manifest::pod(&self.config, namespace, task, launch, &plan);
        if let Err(problems) = manifest::check_pod_shape(&pod, &self.config.shape_policy()) {
            return Err(BackendError::rejected(format!(
                "the generated Pod violates the admission rules: {}",
                problems.join("; ")
            )));
        }

        let created_storage = match self.get_pvc(task).await? {
            Some(pvc) if pvc.metadata.deletion_timestamp.is_some() => {
                return Err(BackendError::transient(format!(
                    "task storage '{}' is being deleted",
                    task.storage
                )))
            }
            Some(_) => false,
            None if !create_storage => {
                return Err(BackendError::storage_missing(format!(
                    "task storage '{}' does not exist",
                    task.storage
                )))
            }
            None => {
                self.check_storage_class(&self.config.storage_class).await?;
                let claim = manifest::pvc(&self.config, namespace, task);
                let path = self.client.core_path("persistentvolumeclaims", None);
                matches!(
                    self.client.create(&path, &claim).await?,
                    Created::Created(_)
                )
            }
        };

        let pods = self.client.core_path("pods", None);
        let created = match self.client.create(&pods, &pod).await? {
            Created::Created(value) => parse::<Pod>("Pod", value)?,
            Created::AlreadyExists => {
                let existing = self.get_pod(task).await?.ok_or_else(|| {
                    BackendError::transient(format!(
                        "Pod '{}' existed and vanished; observe again",
                        task.workload
                    ))
                })?;
                return Ok(Provisioned {
                    workload: self.workload_state(&existing)?,
                    created_workload: false,
                    created_storage,
                });
            }
        };

        // The launch Secret is owned by the Pod, so it can only be made
        // after it. Should that fail, the Pod — which could never start —
        // goes again, and the controller retries with a new incarnation.
        let secret = manifest::launch_secret(namespace, task, launch, &created.metadata.uid)?;
        let secrets = self.client.core_path("secrets", None);
        let made = match self.client.create(&secrets, &secret).await {
            Ok(Created::Created(_)) => Ok(()),
            Ok(Created::AlreadyExists) => Err(BackendError::transient(format!(
                "the launch Secret of incarnation '{}' already exists",
                launch.incarnation
            ))),
            Err(error) => Err(error),
        };
        if let Err(error) = made {
            let path = self.client.core_path("pods", Some(&task.workload));
            if let Err(cleanup) = self.client.delete(&path, &created.metadata.uid).await {
                tracing::warn!(pod = %task.workload, %cleanup, "kubernetes: could not remove a Pod without its launch Secret");
            }
            return Err(error);
        }

        Ok(Provisioned {
            workload: self.workload_state(&created)?,
            created_workload: true,
            created_storage,
        })
    }

    async fn remove_pod(&self, task: &TaskResources) -> BackendResult<Removal> {
        let Some(pod) = self.get_pod(task).await? else {
            return Ok(Removal::AlreadyAbsent);
        };
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(Removal::InProgress);
        }
        let path = self.client.core_path("pods", Some(&task.workload));
        if !self.client.delete(&path, &pod.metadata.uid).await? {
            return Ok(Removal::AlreadyAbsent);
        }
        Ok(match self.get_pod(task).await? {
            Some(_) => Removal::Initiated,
            None => Removal::Removed,
        })
    }

    async fn remove_pvc(&self, task: &TaskResources) -> BackendResult<Removal> {
        let Some(pvc) = self.get_pvc(task).await? else {
            return Ok(Removal::AlreadyAbsent);
        };
        if pvc.metadata.deletion_timestamp.is_some() {
            return Ok(Removal::InProgress);
        }
        let class = pvc.spec.storage_class_name.as_deref().ok_or_else(|| {
            BackendError::rejected(format!(
                "task storage '{}' names no storage class; whether deleting it destroys its \
                 data cannot be verified",
                task.storage
            ))
        })?;
        self.check_storage_class(class).await?;
        let path = self
            .client
            .core_path("persistentvolumeclaims", Some(&task.storage));
        if !self.client.delete(&path, &pvc.metadata.uid).await? {
            return Ok(Removal::AlreadyAbsent);
        }
        Ok(match self.get_pvc(task).await? {
            Some(_) => Removal::Initiated,
            None => Removal::Removed,
        })
    }
}

impl ExecutionBackend for KubernetesBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Kubernetes
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            pause_resume: true,
            persistent_storage: true,
            archive: true,
        }
    }

    fn apply_claude_profile_update(
        &self,
        update: &narsor_protocol::controller::ClaudeProfileUpdate,
    ) -> impl std::future::Future<Output = BackendResult<()>> + Send {
        KubernetesBackend::apply_claude_profile_update(self, update)
    }

    fn resources(&self, task_id: &TaskId, resource_key: &str) -> BackendResult<TaskResources> {
        // The key must be a task id; names are derived from it, never from
        // anything that could be steered into another task's names.
        TaskId::parse(resource_key).map_err(|error| BackendError::rejected(error.to_string()))?;
        Ok(TaskResources {
            task_id: task_id.clone(),
            resource_key: resource_key.to_string(),
            workload: names::workload(resource_key),
            storage: names::storage(resource_key),
        })
    }

    async fn observe(&self, task: &TaskResources) -> BackendResult<TaskObservation> {
        let workload = match self.get_pod(task).await? {
            Some(pod) => Some(self.workload_state(&pod)?),
            None => None,
        };
        let storage = match self.get_pvc(task).await? {
            None => StorageState::Absent,
            Some(pvc) if pvc.metadata.deletion_timestamp.is_some() => StorageState::Deleting,
            Some(_) => StorageState::Present,
        };
        Ok(TaskObservation { workload, storage })
    }

    async fn provision(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.create(task, launch, true).await
    }

    async fn resume(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.create(task, launch, false).await
    }

    async fn release_compute(&self, task: &TaskResources) -> BackendResult<Removal> {
        self.remove_pod(task).await
    }

    async fn archive(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
        let workload = self.remove_pod(task).await?;
        if !workload.is_complete() {
            // The claim goes only once no Pod can write it any more.
            return Ok(ArchiveProgress {
                workload,
                storage: Removal::InProgress,
            });
        }
        let storage = self.remove_pvc(task).await?;
        Ok(ArchiveProgress { workload, storage })
    }
}

/// Refuses an object under a task's name that another task (or someone
/// else entirely) created: its annotations must name this resource key.
fn check_owner(
    task: &TaskResources,
    annotations: &std::collections::BTreeMap<String, String>,
    name: &str,
) -> BackendResult<()> {
    match annotations.get(RESOURCE_KEY_ANNOTATION) {
        Some(key) if *key == task.resource_key => Ok(()),
        _ => Err(BackendError::rejected(format!(
            "'{name}' exists but was not created for task '{}'; refusing to touch it",
            task.task_id
        ))),
    }
}

fn parse<T: serde::de::DeserializeOwned>(what: &str, value: Value) -> BackendResult<T> {
    serde_json::from_value(value)
        .map_err(|error| BackendError::transient(format!("unreadable {what}: {error}")))
}

/// The lease epoch a Pod was launched under, as its label records it.
pub fn launched_under_epoch(pod: &Value) -> Option<u64> {
    pod.pointer(&format!(
        "/metadata/labels/{}",
        EPOCH_KEY.replace('/', "~1")
    ))
    .and_then(Value::as_str)
    .and_then(|value| value.parse().ok())
}
