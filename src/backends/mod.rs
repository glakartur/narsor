//! Backend-neutral execution adapters and the controller that drives them
//! (U4: R3-R5, R7-R14, R34, R37; KTD1-KTD3).
//!
//! The control plane decides *what* should be true of a task (its desired
//! lifecycle); an [`ExecutionBackend`] knows *how* to make it true on one
//! platform. Between them sits the generic [`controller`]: it talks to the
//! control plane only through the controller protocol, so the same loop runs
//! in-process next to the daemon or as a separate process next to a remote
//! platform (R5), and it is the only place that knows the lifecycle rules.
//! An adapter therefore implements a handful of idempotent primitives and
//! nothing else:
//!
//! * **deterministic identity** ([`ExecutionBackend::resources`]) — every
//!   resource name is a pure function of the task's `resource_key`, so a
//!   retried create, a restarted controller or a new lease holder finds the
//!   existing resource instead of creating a second one (R7);
//! * **observe** — what exists right now, including which runner
//!   incarnation and launch generation a workload was created for;
//! * **provision / resume** — create the workload (and, on the first start
//!   only, the task storage); never a duplicate of an existing name;
//! * **release compute** — the stop-at-safe-point primitive: remove the
//!   workload, keep the storage (R9, R12);
//! * **archive** — remove workload and storage; absent resources count as
//!   removed (R34).
//!
//! Nothing here names a platform type: observations are normalized into
//! [`TaskObservation`] before they leave an adapter, and the control plane,
//! clients and wire DTOs only ever see [`crate::orchestration`] records
//! (R8, AE2).
//!
//! * [`controller`] — the reconciliation loop over the controller protocol;
//! * [`fake`] — the in-memory adapter with a simulated runner (contract
//!   tests);
//! * [`local`] — `narsor-runner` as a local child process over a local
//!   directory, the supported local adapter;
//! * [`kubernetes`] — one Pod and one PVC per task (KTD2), the first
//!   production remote adapter;
//! * [`testkit`] — counters, fault injection and a severable control-plane
//!   link usable with *any* adapter, so every adapter runs one contract
//!   suite.

pub mod controller;
pub mod fake;
pub mod kubernetes;
pub mod local;
pub mod testkit;

use std::fmt;
use std::future::Future;

use crate::domain::{TaskId, TaskLaunchSpec};
use crate::environment::{BackendCapabilities, BackendKind};

pub use controller::{
    supervise, Controller, ControllerConfig, ControllerLink, ControllerStatus, StepOutcome,
    Supervision,
};
pub use fake::FakeBackend;
pub use kubernetes::{KubernetesBackend, KubernetesBackendConfig};
pub use local::{LocalBackend, LocalBackendConfig};

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
    pub launch_spec: TaskLaunchSpec,
    /// Base URL the runner reaches the control plane at.
    pub control_plane_url: String,
    /// The incarnation's credential, mounted read-only outside storage.
    pub credential: RunnerCredential,
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
/// could not confirm. An adapter never decides lifecycle — it never
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
}
