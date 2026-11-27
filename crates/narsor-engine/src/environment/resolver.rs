//! The launch resolver contract: what a create-task request asks for
//! ([`LaunchIntent`]), what it resolves to ([`ResolvedLaunch`]), why it can
//! be refused ([`LaunchError`]), and the [`LaunchResolver`] trait the control
//! plane calls.
//!
//! The implementation over the catalogue, the repository registry and the
//! workflow resolver is `crate::launch::CatalogLaunchResolver`; it lives
//! above this module so the configuration does not depend on the engine or
//! the workspace provisioner.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

pub use narsor_protocol::controller::TaskDefaultHarness;
pub use narsor_protocol::controller::WorkspaceRequest;

use crate::automations::{
    AutomationCatalogError, AutomationCatalogLoad, AutomationScript, HookPhase,
};
use crate::domain::{
    BackendId, GitIdentityError, ImageError, InvalidIdentifier, ResolvedEnvironment,
    ResourceProfile, TaskLaunchSpec, WorkflowDefinition,
};
use crate::registry::RegistryError;

use super::backend::{BackendRecord, SlotReleaseTimings};
use super::catalog::CatalogError;

/// What the owner asked for, before anything is resolved.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LaunchIntent {
    /// Explicit backend; the owner default when `None`.
    pub backend: Option<String>,
    /// Workflow to run.
    pub workflow_id: String,
    /// The workflow version the owner expects.
    pub workflow_version: u32,
    /// Workflow inputs.
    pub inputs: Value,
    /// What the owner chose for the workspaces of the workflow, by
    /// workspace name: a repository and branch for one the workflow leaves to
    /// be chosen, a pinned commit (R39) for any Git workspace.
    pub workspaces: BTreeMap<String, WorkspaceRequest>,
    /// Explicit profile.
    pub profile: Option<String>,
    /// Explicit harness.
    pub harness: Option<String>,
    /// Explicit resources.
    pub resources: Option<ResourceProfile>,
    /// Ticket, already normalized.
    pub ticket: Option<String>,
    /// Title, already normalized.
    pub title: Option<String>,
}

/// A request that resolved: where the task goes and what it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLaunch {
    /// The backend the task is pinned to.
    pub backend_id: BackendId,
    /// The sealed launch spec.
    pub spec: TaskLaunchSpec,
}

/// Why a request did not resolve. Every message is free of secret values —
/// there are none to leak: only references are ever read.
#[derive(Debug, Error)]
pub enum LaunchError {
    /// A malformed backend id.
    #[error(transparent)]
    InvalidBackendId(#[from] InvalidIdentifier),
    /// No backend with that id is registered.
    #[error("backend '{0}' is not registered")]
    UnknownBackend(String),
    /// A network server has no built-in `local` backend (R13).
    #[error(
        "backend 'local' is not available: a network server has no built-in local backend{}; \
         register a backend of kind 'local' under another id",
        if *from_default { " (it is the configured 'server.defaults.backend'; set \
         server.defaults.backend to another backend)" } else { "" }
    )]
    NoBuiltinLocal {
        /// Whether the task named no backend and `server.defaults.backend` chose it.
        from_default: bool,
    },
    /// The backend record is unusable (limits, secrets, harnesses…).
    #[error("backend '{id}' is misconfigured: {}", problems.join("; "))]
    InvalidBackend {
        /// Backend id.
        id: String,
        /// Every problem.
        problems: Vec<String>,
    },
    /// The workflow cannot be found or loaded.
    #[error("workflow '{workflow}' cannot be loaded: {message}")]
    UnknownWorkflow {
        /// Workflow id.
        workflow: String,
        /// Why not.
        message: String,
    },
    /// The workflow does not validate.
    #[error("workflow '{workflow}' is invalid: {}", problems.join("; "))]
    InvalidWorkflow {
        /// Workflow id.
        workflow: String,
        /// Every validation error.
        problems: Vec<String>,
    },
    /// The catalogue holds another version of the workflow.
    #[error("workflow '{workflow}' is at version {available}, not the requested {requested}")]
    WorkflowVersionMismatch {
        /// Workflow id.
        workflow: String,
        /// Requested version.
        requested: u32,
        /// Version the catalogue has.
        available: u32,
    },
    /// The inputs are not an object, or a template over them fails.
    #[error("invalid inputs: {0}")]
    InvalidInputs(String),
    /// The workspace names a repository nobody registered.
    #[error("repository '{0}' is not registered; a remote task needs a registered repository")]
    UnknownRepository(String),
    /// The workspace cannot travel to the backend.
    #[error("unsupported workspace: {0}")]
    UnsupportedWorkspace(String),
    /// What the owner chose for the workspaces does not fit the workflow.
    #[error("invalid workspace choice: {0}")]
    InvalidWorkspaceChoice(String),
    /// A commit that is not a full object id.
    #[error(transparent)]
    InvalidCommit(#[from] GitIdentityError),
    /// No base image anywhere, on a backend that runs images (R15, AE11).
    #[error(
        "no base image for backend '{backend}': set 'globals.image' in workflow '{workflow}', \
         register one on repository {}, or define a global 'task_image' without components \
         in runtime.yaml",
        repository.as_deref().map(|id| format!("'{id}'")).unwrap_or_else(|| "(none)".to_string())
    )]
    MissingBaseImage {
        /// Backend.
        backend: String,
        /// Repository, when there is one.
        repository: Option<String>,
        /// Workflow.
        workflow: String,
    },
    /// The only image is the global task image, and it has components,
    /// which only the local Docker mode builds (KTD9).
    #[error(
        "backend '{backend}' runs container images, but the only image for workflow \
         '{workflow}' is the global task image in runtime.yaml, and its components are built \
         only by the local Docker mode; build an image with them, push it to a registry and \
         name it in 'globals.image' of workflow '{workflow}'"
    )]
    TaskImageNeedsBuild {
        /// Backend.
        backend: String,
        /// Workflow.
        workflow: String,
    },
    /// An image reference is malformed or breaks its digest policy.
    #[error("{field}: {error}")]
    InvalidImage {
        /// Where it is configured.
        field: String,
        /// What is wrong.
        error: ImageError,
    },
    /// A `resolve`-policy tag did not resolve.
    #[error("{field}: image '{reference}' could not be resolved to a digest: {reason}")]
    UnresolvedImage {
        /// Where it is configured.
        field: String,
        /// The reference.
        reference: String,
        /// Why not.
        reason: String,
    },
    /// A backend that runs images needs the runtime bundle (R20).
    #[error("backend '{0}' runs images, but no runtime bundle is configured in runtime.yaml")]
    MissingRuntimeBundle(String),
    /// The runtime catalogue is unusable.
    #[error("runtime.yaml is invalid: {}", .0.join("; "))]
    InvalidRuntime(Vec<String>),
    /// The profile does not exist.
    #[error("profile '{0}' is not defined")]
    UnknownProfile(String),
    /// The profile is unusable.
    #[error("profile '{id}' is invalid: {}", problems.join("; "))]
    InvalidProfile {
        /// Profile id.
        id: String,
        /// Every problem.
        problems: Vec<String>,
    },
    /// A profile names a tool the runtime catalogue does not have.
    #[error("profile '{profile}' needs tool '{tool}', which runtime.yaml does not provide")]
    UnknownTool {
        /// Profile id.
        profile: String,
        /// Tool name.
        tool: String,
    },
    /// The harness is unknown to this build or to the backend.
    #[error("unsupported harness '{harness}': {reason}")]
    UnsupportedHarness {
        /// Harness name.
        harness: String,
        /// Why.
        reason: String,
    },
    /// A secret reference not on the backend's allow list (R19).
    #[error(
        "{wanted_by} needs secret reference '{secret}', which backend '{backend}' does not \
         register"
    )]
    UnregisteredSecret {
        /// Reference name.
        secret: String,
        /// Backend.
        backend: String,
        /// Who asked for it.
        wanted_by: String,
    },
    /// The requested resources are unusable or above the backend's limit.
    #[error("invalid resources: {0}")]
    InvalidResources(String),
    /// A catalogue file is broken.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// A repository entry is broken.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

impl LaunchError {
    /// Stable machine-readable reason code for the API.
    pub fn code(&self) -> &'static str {
        match self {
            LaunchError::InvalidBackendId(_) => "invalid_identifier",
            LaunchError::UnknownBackend(_) | LaunchError::NoBuiltinLocal { .. } => {
                "unknown_backend"
            }
            LaunchError::InvalidBackend { .. } => "invalid_backend",
            LaunchError::UnknownWorkflow { .. } => "unknown_workflow",
            LaunchError::InvalidWorkflow { .. } => "invalid_workflow",
            LaunchError::WorkflowVersionMismatch { .. } => "workflow_version_mismatch",
            LaunchError::InvalidInputs(_) => "invalid_inputs",
            LaunchError::UnknownRepository(_) => "unknown_repository",
            LaunchError::UnsupportedWorkspace(_) => "unsupported_workspace",
            LaunchError::InvalidWorkspaceChoice(_) => "invalid_workspace_choice",
            LaunchError::InvalidCommit(_) => "invalid_commit",
            LaunchError::MissingBaseImage { .. } => "missing_base_image",
            LaunchError::TaskImageNeedsBuild { .. } => "task_image_needs_build",
            LaunchError::InvalidImage { .. } => "invalid_image",
            LaunchError::UnresolvedImage { .. } => "unresolved_image",
            LaunchError::MissingRuntimeBundle(_) => "missing_runtime_bundle",
            LaunchError::InvalidRuntime(_) => "invalid_runtime",
            LaunchError::UnknownProfile(_) => "unknown_profile",
            LaunchError::InvalidProfile { .. } => "invalid_profile",
            LaunchError::UnknownTool { .. } => "unknown_tool",
            LaunchError::UnsupportedHarness { .. } => "unsupported_harness",
            LaunchError::UnregisteredSecret { .. } => "unregistered_secret",
            LaunchError::InvalidResources(_) => "invalid_resources",
            LaunchError::Catalog(_) | LaunchError::Registry(_) => "invalid_configuration",
        }
    }

    /// Whether this is a server-side storage failure rather than something
    /// the owner or operator can fix by changing a request or a file.
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            LaunchError::Catalog(CatalogError::Storage { .. })
                | LaunchError::Registry(RegistryError::Storage { .. })
        )
    }
}

/// Turns a create-task request into a backend and a launch spec.
pub trait LaunchResolver: Send + Sync {
    /// Resolves and validates `intent`, reading but never writing.
    fn resolve(&self, intent: &LaunchIntent) -> Result<ResolvedLaunch, LaunchError>;

    /// Re-resolves only the catalogue-derived environment of the sealed
    /// `spec` of an existing task on `backend`, with the rules of creation:
    /// base and step images, the task image (sealed only on a backend that
    /// runs no images), the runtime bundle, tools, the profile by its stored
    /// id and its secret references, and the workspace's git credentials.
    /// The workspace, inputs, definition, resources, labels and fallback
    /// harness stay as sealed, so an unchanged catalogue yields the stored
    /// identity. Refused with the same errors as a create request.
    fn resolve_environment(
        &self,
        backend: &BackendId,
        spec: &TaskLaunchSpec,
    ) -> Result<ResolvedEnvironment, LaunchError>;

    /// Whether `backend` is registered and usable, read fresh from
    /// configuration on every call.
    fn check_backend(&self, backend: &BackendId) -> Result<(), LaunchError>;

    /// The slot-release periods of a registered, usable backend, read fresh
    /// from configuration on every call, like [`Self::check_backend`].
    fn slot_release_timings(&self, backend: &BackendId) -> Result<SlotReleaseTimings, LaunchError>;

    /// The configured `server.defaults.backend` — the fallback when the owner has
    /// not chosen one through the control plane (R10).
    fn configured_default_backend(&self) -> String;

    /// The record of `backend` as the catalogue declares it — the built-in
    /// `local` included where it exists — usable or not; `None` when there
    /// is no such backend.
    fn backend_record(&self, backend: &BackendId) -> Result<Option<BackendRecord>, LaunchError>;

    /// The installation's live default harness as it applies to a task on
    /// `backend` (U7): the name when the backend runs it, why not when it
    /// does not. `None` when there is no live default to follow (no
    /// selector) or the backend's record cannot be read — never an error,
    /// so a runner's poll is never refused for it.
    fn default_harness(&self, backend: &BackendId) -> Option<TaskDefaultHarness> {
        let _ = backend;
        None
    }

    /// A receiver woken whenever the live default harness changes, so a
    /// waiting runner hears of it at once; `None` when there is none.
    fn default_harness_changes(&self) -> Option<watch::Receiver<u64>> {
        None
    }

    /// The catalogue's valid definition of workflow `workflow_id`, as a
    /// task create request would run it: an automation's task create
    /// without a version takes its version. A resolver without a catalogue
    /// knows no workflow.
    fn workflow_definition(&self, workflow_id: &str) -> Result<WorkflowDefinition, LaunchError> {
        Err(LaunchError::UnknownWorkflow {
            workflow: workflow_id.to_string(),
            message: "this server has no workflow catalogue".to_string(),
        })
    }

    /// The catalogue's automations and queues, read now (no cache), with a
    /// problem for each file that cannot be used. A resolver without a
    /// catalogue has none.
    fn automation_catalog(&self) -> AutomationCatalogLoad {
        AutomationCatalogLoad::default()
    }

    /// The text of `automation`'s `phase` script as the catalogue holds it
    /// now, read when a run is claimed.
    fn automation_script(
        &self,
        automation: &str,
        phase: HookPhase,
    ) -> Result<AutomationScript, AutomationCatalogError> {
        let _ = (automation, phase);
        Err(AutomationCatalogError::NoCatalogue)
    }
}
