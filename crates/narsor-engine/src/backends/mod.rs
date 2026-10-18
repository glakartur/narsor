//! Engine-owned adapters implementing the shared backend contract.

pub mod fake;
pub mod local;

pub use fake::FakeBackend;
pub use local::{LocalBackend, LocalBackendConfig};
pub use narsor_controller_runtime::backend::{
    ArchiveProgress, BackendError, BackendErrorKind, BackendResult, ExecutionBackend, Provisioned,
    ReaderStart, Removal, RunnerCredential, StorageState, TaskObservation, TaskResources,
    WorkloadLaunch, WorkloadPhase, WorkloadState,
};
