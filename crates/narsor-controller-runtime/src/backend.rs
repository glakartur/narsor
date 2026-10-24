//! Platform-neutral contract shared by execution backends and the reconciler.

use std::future::Future;

use std::fmt;

use narsor_protocol::identifier::TaskId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The deterministic names of one task's backend resources (R7).
///
/// Derived from the task alone, never from time or chance: whoever asks for
/// the same task gets the same names, which is what lets reconciliation find
/// a resource instead of duplicating it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskResources {
    /// The task.
    pub task_id: TaskId,
    /// The control plane's resource key the names were derived from.
    pub resource_key: String,
    /// Name of the compute resource (a Pod, a process slot).
    pub workload: String,
    /// Name of the read-only runner's compute resource (D63): deterministic
    /// like the others and never the workload's name, so a reader and a
    /// working runner are two resources the platform keeps apart, and one of
    /// them is never mistaken for the other.
    pub reader: String,
    /// Name of the task storage (a PVC, a directory).
    pub storage: String,
}

/// Where a workload stands, normalized across platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadPhase {
    /// Created, not yet running (an image pull, a scheduling wait).
    Starting,
    /// Running.
    Running,
    /// Terminated. `code` is the runner's exit code, `None` when it was
    /// killed by a signal or the platform cannot tell (see
    /// [`crate::runner::cli`] for the codes).
    Exited {
        /// Exit code, when known.
        code: Option<i32>,
    },
    /// Deletion was requested and is not finished.
    Deleting,
}

impl WorkloadPhase {
    /// Whether the workload is (about to be) running.
    pub fn is_alive(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

impl fmt::Display for WorkloadPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => f.write_str("starting"),
            Self::Running => f.write_str("running"),
            Self::Exited { code: Some(code) } => write!(f, "exited with code {code}"),
            Self::Exited { code: None } => f.write_str("terminated without an exit code"),
            Self::Deleting => f.write_str("being deleted"),
        }
    }
}

/// One observed workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadState {
    /// Platform-assigned id of this incarnation of the resource (a Pod UID);
    /// a replacement has a new one.
    pub uid: String,
    /// Monotonic version of this resource, so reports about it are ordered.
    pub resource_generation: u64,
    /// Runner incarnation the workload was created for — what its runner
    /// credential is bound to.
    pub incarnation: String,
    /// Desired generation it was launched at. Older than the task's current
    /// generation means a superseded launch that must go before a new one.
    pub launch_generation: u64,
    /// Where it stands.
    pub phase: WorkloadPhase,
}

/// Whether task storage exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageState {
    /// No storage under the task's name.
    Absent,
    /// Storage exists.
    Present,
    /// Deletion was requested and is not finished.
    Deleting,
}

impl StorageState {
    /// Whether anything of the storage remains.
    pub fn exists(self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// Everything the backend currently holds for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskObservation {
    /// The workload, if one exists under the task's name.
    pub workload: Option<WorkloadState>,
    /// The task storage.
    pub storage: StorageState,
}

/// A runner credential on its way into a workload.
///
/// A bearer secret: it is only ever handed to the platform's own secret
/// mechanism (a read-only file, a Secret volume) and never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct RunnerCredential(String);

impl RunnerCredential {
    /// Wraps a credential issued by the control plane.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for writing it where the runner reads it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RunnerCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RunnerCredential(<redacted>)")
    }
}

/// What a new workload needs to start its runner (see
/// [`crate::runner::cli`] for the invocation contract).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadLaunch {
    /// The task.
    pub task_id: TaskId,
    /// Desired generation it is launched for.
    pub generation: u64,
    /// Runner incarnation the credential is bound to.
    pub incarnation: String,
    /// Fencing epoch of the lease the launching controller held. Recorded
    /// on the workload (KTD2) so a platform-side reader — an operator, an
    /// admission policy, a later controller — can tell which lease holder
    /// created it; an adapter may also refuse to act over a workload a newer
    /// lease holder created.
    pub lease_epoch: u64,
    /// The immutable launch input, mounted read-only.
    pub launch_spec: Value,
    /// Base URL the runner reaches the control plane at.
    pub control_plane_url: String,
    /// The incarnation's credential, mounted read-only outside storage.
    pub credential: RunnerCredential,
}

/// Engine-independent envelope checked before the controller asks the
/// control plane to issue a runner credential. Platform adapters still
/// validate the full nested spec before creating resources.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchInputEnvelope {
    version: u32,
    workflow_id: String,
    workflow_version: u32,
    inputs: Value,
    workspace: Value,
    environment: Value,
    resources: Value,
    #[serde(default)]
    definition: Option<Value>,
    #[serde(default)]
    ticket: Option<String>,
    #[serde(default)]
    title: Option<String>,
    digest: String,
}

/// Validates the versioned top-level launch contract without importing the
/// engine's persisted domain model.
pub fn validate_launch_input(value: &Value) -> Result<(), String> {
    let input: LaunchInputEnvelope = serde_json::from_value(value.clone())
        .map_err(|error| format!("control plane returned an invalid launch spec: {error}"))?;
    if input.version != 3 {
        return Err(format!(
            "task launch spec version {} is not supported by this build (expected 3)",
            input.version
        ));
    }
    if input.workflow_id.is_empty() || input.workflow_version == 0 || input.digest.is_empty() {
        return Err("task launch spec has an invalid workflow identity or digest".into());
    }
    let _ = (
        input.inputs,
        input.workspace,
        input.environment,
        input.resources,
        input.definition,
        input.ticket,
        input.title,
    );
    Ok(())
}

/// What [`ExecutionBackend::provision`] or [`ExecutionBackend::resume`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    /// The workload under the task's name afterwards.
    pub workload: WorkloadState,
    /// False when a workload of that name already existed and was returned
    /// as is (reconciliation, not a duplicate).
    pub created_workload: bool,
    /// Whether task storage was created by this call.
    pub created_storage: bool,
}

/// What [`ExecutionBackend::start_reader`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderStart {
    /// The reader under the task's reader name afterwards.
    pub workload: WorkloadState,
    /// False when a reader of that name already existed and was returned as
    /// is.
    pub created: bool,
}

/// What a removal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// The resource existed and is gone now.
    Removed,
    /// The resource existed; deletion started and is not finished.
    Initiated,
    /// Deletion had been started before and is still not finished.
    InProgress,
    /// Nothing under that name: already clean.
    AlreadyAbsent,
}

impl Removal {
    /// Whether the resource is gone.
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Removed | Self::AlreadyAbsent)
    }

    /// Whether this call issued the deletion (counted once per resource).
    pub fn issued(self) -> bool {
        matches!(self, Self::Removed | Self::Initiated)
    }
}

/// What [`ExecutionBackend::archive`] did to each resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveProgress {
    /// The workload.
    pub workload: Removal,
    /// The task storage.
    pub storage: Removal,
}

impl ArchiveProgress {
    /// Both resources are gone: archive may be reported complete.
    pub fn is_complete(&self) -> bool {
        self.workload.is_complete() && self.storage.is_complete()
    }
}

/// How a backend operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    /// Worth retrying as is (a timeout, an unreachable platform API).
    Transient,
    /// The platform refused for good (invalid shape, quota, missing image).
    Rejected,
    /// A relaunch found no task storage: the work it should continue is
    /// gone, and a fresh storage would silently restart it (R13).
    StorageMissing,
}

/// A failed backend operation. The message is for humans and must not hold
/// secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct BackendError {
    /// How it failed.
    pub kind: BackendErrorKind,
    /// What happened.
    pub message: String,
}

impl BackendError {
    /// A retryable failure.
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: BackendErrorKind::Transient,
            message: message.into(),
        }
    }

    /// A permanent refusal.
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            kind: BackendErrorKind::Rejected,
            message: message.into(),
        }
    }

    /// A relaunch without storage.
    pub fn storage_missing(message: impl Into<String>) -> Self {
        Self {
            kind: BackendErrorKind::StorageMissing,
            message: message.into(),
        }
    }
}

/// Result of a backend operation.
pub type BackendResult<T> = Result<T, BackendError>;

/// One execution platform, as the generic controller drives it.
///
/// Every operation is idempotent and safe to repeat after a crash or a lost
/// response: the controller observes before it acts and retries what it
/// could not confirm. An adapter never decides runner state — it never
/// recreates a lost workload, never removes compute on its own and never
/// deletes storage outside [`Self::archive`].
pub trait ExecutionBackend: Send + Sync + 'static {
    /// The platform kind (matches the backend's catalogue record).
    fn kind(&self) -> BackendKind;

    /// The lifecycle operations this platform supports (R3).
    fn capabilities(&self) -> BackendCapabilities;

    /// The task's resource names, derived from its resource key alone.
    ///
    /// Must be deterministic and injective over keys: two tasks never share
    /// a name (AE1). Refuses a key the platform cannot name safely.
    fn resources(&self, task_id: &TaskId, resource_key: &str) -> BackendResult<TaskResources>;

    /// What exists for the task right now.
    fn observe(
        &self,
        task: &TaskResources,
    ) -> impl Future<Output = BackendResult<TaskObservation>> + Send;

    /// The first start: creates the task storage when absent, then the
    /// workload. A workload that already exists under the task's name is
    /// returned unchanged (`created_workload: false`), never duplicated.
    fn provision(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> impl Future<Output = BackendResult<Provisioned>> + Send;

    /// A relaunch after pause or an owner's start/resume: creates the
    /// workload on the *existing* storage, failing with
    /// [`BackendErrorKind::StorageMissing`] when there is none.
    fn resume(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> impl Future<Output = BackendResult<Provisioned>> + Send;

    /// Removes the workload and keeps the storage — the stop-at-safe-point
    /// primitive, also used to clear a dead or superseded workload before a
    /// relaunch. The controller calls it for a live runner only after the
    /// runner acknowledged a safe point.
    fn release_compute(
        &self,
        task: &TaskResources,
    ) -> impl Future<Output = BackendResult<Removal>> + Send;

    /// Removes workload and storage (R34). Absent resources count as
    /// removed; complete only once both are gone.
    fn archive(
        &self,
        task: &TaskResources,
    ) -> impl Future<Output = BackendResult<ArchiveProgress>> + Send;

    /// The task's read-only runner, if one exists under its reader name
    /// (D63). The reader is a workload of its own: it is never the task's
    /// [`TaskObservation::workload`], and it neither takes a slot nor counts
    /// as the task's compute.
    fn observe_reader(
        &self,
        task: &TaskResources,
    ) -> impl Future<Output = BackendResult<Option<WorkloadState>>> + Send;

    /// Starts a read-only runner on the task's *existing* storage: the
    /// launch's runner invocation plus `--read-only` (see
    /// [`crate::runner::cli`]), with storage mounted read-only where the
    /// platform can. Fails with [`BackendErrorKind::StorageMissing`] when
    /// there is no storage — a reader never creates one. A reader that
    /// already exists is returned unchanged. The controller never calls it
    /// while a working runner of the task lives (R15).
    fn start_reader(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> impl Future<Output = BackendResult<ReaderStart>> + Send;

    /// Removes the task's reader and nothing else: the workload and the
    /// storage stay. Idempotent; complete once the reader is gone.
    fn stop_reader(
        &self,
        task: &TaskResources,
    ) -> impl Future<Output = BackendResult<Removal>> + Send;
}
/// What platform a backend runs task workloads on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Kubernetes,
    Fake,
    Local,
}

impl BackendKind {
    pub fn runs_container_images(self) -> bool {
        !matches!(self, Self::Local)
    }
}

/// Lifecycle operations a backend supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendCapabilities {
    pub pause_resume: bool,
    pub persistent_storage: bool,
    pub archive: bool,
}

impl Default for BackendCapabilities {
    fn default() -> Self {
        Self {
            pause_resume: true,
            persistent_storage: true,
            archive: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate_launch_input;
    use serde_json::json;

    #[test]
    fn launch_input_with_ticket_and_title_is_accepted() {
        let spec = json!({
            "version": 3,
            "workflow_id": "compound-engineering",
            "workflow_version": 1,
            "inputs": {},
            "workspace": {},
            "environment": {},
            "resources": {},
            "definition": null,
            "ticket": "NAR-001",
            "title": "Poprawa formularza",
            "digest": "sha256:abc",
        });
        assert_eq!(validate_launch_input(&spec), Ok(()));
    }
}
