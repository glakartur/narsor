//! Per-execution workspace lifecycle.
//!
//! Each execution works in its own directory tree under a configurable root,
//! laid out as `DOMAIN_MODEL.md` §2.8 describes:
//!
//! ```text
//! {root}/tasks/{execution_id}/code/
//!     {primary_repo}/     -- read-write
//!     {secondary_repo}/   -- read-only
//! ```
//!
//! [`provisioner`] creates that tree (git clone or a copy of a local folder),
//! clears the write bits on every secondary repository, and tears the tree down
//! when the execution finishes — unless the caller asked to preserve it, or the
//! read-write repository still holds work that was never committed or pushed.
//!
//! The domain types describing a workspace live in [`crate::domain::workspace`];
//! this module only provisions them.

pub mod checkpoint;
pub mod provisioner;

pub use checkpoint::{
    capture, materialise, Base, Capture, CaptureRequest, CheckpointError, MaterialiseRequest,
    CHECKPOINT_REF,
};
pub use provisioner::{
    CleanupOutcome, CleanupPolicy, ProvisionedRepository, ProvisionedWorkspace, RepositorySource,
    RepositorySpec, WorkspaceError, WorkspaceProvisioner, WorkspaceRequest, CODE_DIR, TASKS_DIR,
};
