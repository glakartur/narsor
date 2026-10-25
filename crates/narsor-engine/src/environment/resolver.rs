//! Resolving a create-task request into a backend and a sealed launch spec.
//!
//! [`CatalogLaunchResolver`] is the one place configuration, the repository
//! registry, the workflow and the owner's request meet. It only *reads*:
//! every check runs before the control plane is asked to store anything, so
//! a refused request leaves no task, no assignment and no command behind
//! (U6: "without persisting a partial assignment").
//!
//! # Precedence
//!
//! * **Backend** — the request's explicit `backend`, else the owner default
//!   (R10): the override the owner chose through the control plane, which
//!   fills in `backend` before resolution, else the configured
//!   `default_backend`. An existing task never re-resolves, so changing the
//!   default moves no task.
//! * **Base image** — a step's `image` wins over the workflow's
//!   `globals.image`, which wins over the repository's `base_image` (R15).
//!   The step overrides are recorded separately, each with its
//!   [`ImageSource`].
//! * **Harness** — the request, else the profile, else the live default
//!   harness selection. Since U7 this is only the task's *fallback* (R42
//!   reversed): a running task follows the installation's live default,
//!   which the control plane hands its runner with every answer
//!   ([`LaunchResolver::default_harness`]) as long as the task's backend
//!   runs it. Only a workflow's own `harness:` pins an action.
//! * **Profile** — the request, else `default_profile`.
//! * **Resources** — the request, else the backend's default, never above
//!   the backend's maximum.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

pub use narsor_protocol::controller::TaskDefaultHarness;

use crate::domain::{
    ActionConfig, BackendId, Context, DecisionConfig, GitIdentity, GitIdentityError, ImageError,
    ImageSource, ImageSpec, InvalidIdentifier, RefOrigin, ResolvedEnvironment, ResolvedImage,
    ResourceProfile, SecretReference, TaskLaunchSpec, TaskWorkspace, WorkflowDefinition,
    WorkspaceSpec,
};
use crate::persistence::{DefaultHarnessSelector, HarnessSelectionSnapshot};
use crate::registry::{RegistryError, RepositoryEntry, RepositoryRegistry};
use crate::workflow::TemplateScope;
use crate::workflows::WorkflowResolver;

use super::backend::{BackendRecord, SlotReleaseTimings};
use super::catalog::{CatalogError, EnvironmentCatalog};
use super::image::{ImageResolver, NoTagResolution};
use super::profile::{ProfileRecord, RuntimeCatalog, DEFAULT_PROFILE};

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
    /// Registered repository; the workflow's `globals.workspace` when
    /// `None`.
    pub repository: Option<String>,
    /// The pinned commit to clone (R39).
    pub commit: Option<String>,
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
        if *from_default { " (it is the configured 'default_backend'; set default_backend to \
         another backend)" } else { "" }
    )]
    NoBuiltinLocal {
        /// Whether the task named no backend and `default_backend` chose it.
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
    /// A Git workspace without a pinned commit.
    #[error("repository '{0}' needs a pinned commit: pass the full commit id to clone")]
    MissingCommit(String),
    /// A commit that is not a full object id.
    #[error(transparent)]
    InvalidCommit(#[from] GitIdentityError),
    /// No base image anywhere, on a backend that runs images (R15, AE11).
    #[error(
        "no base image for backend '{backend}': register one on repository {} or set \
         'globals.image' in workflow '{workflow}'",
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
            LaunchError::MissingCommit(_) => "missing_commit",
            LaunchError::InvalidCommit(_) => "invalid_commit",
            LaunchError::MissingBaseImage { .. } => "missing_base_image",
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

    /// `max_concurrent_tasks` of a registered, usable backend, read fresh
    /// from configuration on every call, so the scheduler applies a changed
    /// limit to its next decision (U10).
    fn backend_limit(&self, backend: &BackendId) -> Result<u32, LaunchError>;

    /// The slot-release periods of a registered, usable backend, read fresh
    /// from configuration on every call, like [`Self::backend_limit`].
    fn slot_release_timings(&self, backend: &BackendId) -> Result<SlotReleaseTimings, LaunchError>;

    /// The configured `default_backend` — the fallback when the owner has
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
}

/// The harness registry and the live default harness.
pub trait HarnessDefaults: Send + Sync {
    /// Harnesses this build can run.
    fn supported(&self) -> Vec<String>;
    /// The default for agent steps that name none, right now.
    fn default_harness(&self) -> String;
    /// The default running tasks follow (U7), when it can change while they
    /// run; `None` for a fixed default, which running tasks need not hear.
    fn live_default(&self) -> Option<String> {
        None
    }
    /// A receiver woken by every change of [`Self::live_default`]; `None`
    /// for a fixed default.
    fn live_changes(&self) -> Option<watch::Receiver<u64>> {
        None
    }
}

/// Fixed harness defaults.
#[derive(Debug, Clone)]
pub struct StaticHarnessDefaults {
    supported: Vec<String>,
    default: String,
}

impl StaticHarnessDefaults {
    /// Fixed `supported` and `default`.
    pub fn new(supported: Vec<String>, default: impl Into<String>) -> Self {
        Self {
            supported,
            default: default.into(),
        }
    }

    /// This build's registry with `default` as the default.
    pub fn builtin(default: impl Into<String>) -> Self {
        let supported = crate::engine::HarnessRegistry::defaults()
            .supported()
            .into_iter()
            .map(str::to_string)
            .collect();
        Self::new(supported, default)
    }
}

impl HarnessDefaults for StaticHarnessDefaults {
    fn supported(&self) -> Vec<String> {
        self.supported.clone()
    }

    fn default_harness(&self) -> String {
        self.default.clone()
    }
}

impl HarnessDefaults for DefaultHarnessSelector {
    fn supported(&self) -> Vec<String> {
        self.current().supported
    }

    fn default_harness(&self) -> String {
        let HarnessSelectionSnapshot { effective, .. } = self.current();
        effective
    }

    fn live_default(&self) -> Option<String> {
        Some(self.current().effective)
    }

    fn live_changes(&self) -> Option<watch::Receiver<u64>> {
        Some(self.changes())
    }
}

/// The owner's configured defaults (R10, R16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerDefaults {
    /// Backend of a task that names none.
    pub default_backend: String,
    /// Profile of a task that names none.
    pub default_profile: String,
    /// Host directories a local task may take as its workspace. Empty
    /// refuses them all.
    pub local_workspace_roots: Vec<PathBuf>,
}

impl OwnerDefaults {
    /// The defaults from resolved configuration.
    pub fn from_config(config: &crate::config::Resolved) -> Self {
        Self {
            default_backend: config.default_backend().to_string(),
            default_profile: config.default_profile().to_string(),
            local_workspace_roots: config
                .list(crate::config::SettingKey::LocalWorkspaceRoots)
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        }
    }
}

/// The resolver over the on-disk catalogue.
pub struct CatalogLaunchResolver {
    catalog_root: PathBuf,
    catalog: EnvironmentCatalog,
    builtin_local: bool,
    registry: RepositoryRegistry,
    workflows: WorkflowResolver,
    owner: OwnerDefaults,
    harness: Arc<dyn HarnessDefaults>,
    images: Arc<dyn ImageResolver>,
}

impl fmt::Debug for CatalogLaunchResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatalogLaunchResolver")
            .field("catalog_root", &self.catalog_root)
            .field("owner", &self.owner)
            .field("images", &self.images)
            .finish_non_exhaustive()
    }
}

impl CatalogLaunchResolver {
    /// A resolver over `catalog_root`, finding workflows through
    /// `workflows`. Image tags are not resolved until
    /// [`Self::with_image_resolver`] says how.
    pub fn new(
        catalog_root: impl Into<PathBuf>,
        workflows: WorkflowResolver,
        owner: OwnerDefaults,
        harness: Arc<dyn HarnessDefaults>,
    ) -> Self {
        let catalog_root = catalog_root.into();
        Self {
            catalog: EnvironmentCatalog::new(catalog_root.clone()),
            builtin_local: true,
            registry: RepositoryRegistry::new(&catalog_root),
            catalog_root,
            workflows,
            owner,
            harness,
            images: Arc::new(NoTagResolution),
        }
    }

    /// The resolver of a network server: no built-in `local` backend, so a
    /// task on `local` is refused at creation (R13).
    pub fn without_builtin_local(mut self) -> Self {
        self.catalog = self.catalog.without_builtin_local();
        self.builtin_local = false;
        self
    }

    /// Resolves `resolve`-policy tags through `images`.
    pub fn with_image_resolver(mut self, images: Arc<dyn ImageResolver>) -> Self {
        self.images = images;
        self
    }

    fn backend(&self, intent: &LaunchIntent) -> Result<(BackendId, BackendRecord), LaunchError> {
        let requested = intent
            .backend
            .clone()
            .unwrap_or_else(|| self.owner.default_backend.clone());
        let backend_id = BackendId::parse(requested)?;
        let record = self
            .usable_backend(&backend_id)
            .map_err(|error| match error {
                LaunchError::UnknownBackend(id)
                    if !self.builtin_local && id == BackendRecord::LOCAL_ID =>
                {
                    LaunchError::NoBuiltinLocal {
                        from_default: intent.backend.is_none(),
                    }
                }
                error => error,
            })?;
        Ok((backend_id, record))
    }

    /// The registered record of `backend_id`, refused when it is unusable.
    fn usable_backend(&self, backend_id: &BackendId) -> Result<BackendRecord, LaunchError> {
        let record = self
            .catalog
            .backend(backend_id.as_str())?
            .ok_or_else(|| LaunchError::UnknownBackend(backend_id.to_string()))?;
        let problems = record.problems();
        if !problems.is_empty() {
            return Err(LaunchError::InvalidBackend {
                id: backend_id.to_string(),
                problems,
            });
        }
        Ok(record)
    }

    /// The canonical form of a host directory a local task may work on.
    ///
    /// A local workspace is copied into the task's storage, so a path chosen
    /// by a workflow definition (or built from its inputs) must not be able
    /// to name any directory of the host: it has to be absolute and, once
    /// symlinks and `..` are resolved, lie inside one of the owner's
    /// `local_workspace_roots`. The sealed spec keeps the canonical path, so
    /// nothing is re-resolved later.
    fn allowed_local_directory(&self, path: &str) -> Result<String, LaunchError> {
        let refuse = |reason: String| LaunchError::UnsupportedWorkspace(reason);
        let requested = std::path::Path::new(path);
        if !requested.is_absolute() {
            return Err(refuse(format!(
                "local workspace '{path}' is not an absolute path"
            )));
        }
        let canonical = requested
            .canonicalize()
            .map_err(|error| refuse(format!("local workspace '{path}' cannot be used: {error}")))?;
        if !canonical.is_dir() {
            return Err(refuse(format!(
                "local workspace '{path}' is not a directory"
            )));
        }
        let allowed = self.owner.local_workspace_roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|root| canonical.starts_with(root))
        });
        if !allowed {
            return Err(refuse(format!(
                "local workspace '{}' is outside every directory listed in \
                 'local_workspace_roots'",
                canonical.display()
            )));
        }
        canonical
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| refuse("local workspace path is not valid UTF-8".to_string()))
    }

    fn workflow(&self, intent: &LaunchIntent) -> Result<WorkflowDefinition, LaunchError> {
        let unknown = |message: String| LaunchError::UnknownWorkflow {
            workflow: intent.workflow_id.clone(),
            message,
        };
        let resolved = self
            .workflows
            .resolve_name(&intent.workflow_id)
            .map_err(|error| unknown(error.to_string()))?;
        let problems = resolved
            .validate()
            .map_err(|error| unknown(error.to_string()))?;
        if !problems.is_empty() {
            return Err(LaunchError::InvalidWorkflow {
                workflow: intent.workflow_id.clone(),
                problems: problems.iter().map(ToString::to_string).collect(),
            });
        }
        let definition = resolved
            .load()
            .map_err(|error| unknown(error.to_string()))?;
        if definition.version != intent.workflow_version {
            return Err(LaunchError::WorkflowVersionMismatch {
                workflow: intent.workflow_id.clone(),
                requested: intent.workflow_version,
                available: definition.version,
            });
        }
        Ok(definition)
    }

    fn profile(&self, intent: &LaunchIntent) -> Result<ProfileRecord, LaunchError> {
        let id = intent
            .profile
            .clone()
            .unwrap_or_else(|| self.owner.default_profile.clone());
        let record = match self.catalog.profile(&id)? {
            Some(record) => record,
            // Only the implicit default may be absent: a fresh installation
            // runs without any profile file, but a profile someone *named*
            // and nobody defined is a mistake worth reporting.
            None if intent.profile.is_none() && id == DEFAULT_PROFILE => {
                ProfileRecord::empty(id.clone())
            }
            None => return Err(LaunchError::UnknownProfile(id)),
        };
        let problems = record.problems();
        if !problems.is_empty() {
            return Err(LaunchError::InvalidProfile { id, problems });
        }
        Ok(record)
    }

    fn runtime(&self) -> Result<Option<RuntimeCatalog>, LaunchError> {
        let runtime = self.catalog.runtime()?;
        if let Some(runtime) = &runtime {
            let problems = runtime.problems();
            if !problems.is_empty() {
                return Err(LaunchError::InvalidRuntime(problems));
            }
        }
        Ok(runtime)
    }

    fn resolve_image(
        &self,
        field: &str,
        spec: &ImageSpec,
        source: ImageSource,
    ) -> Result<ResolvedImage, LaunchError> {
        let reference = spec.check().map_err(|error| LaunchError::InvalidImage {
            field: field.to_string(),
            error,
        })?;
        let (digest, tag_resolved) = match &reference.digest {
            Some(digest) => (digest.clone(), false),
            None => {
                let digest = self.images.resolve_tag(&reference).map_err(|reason| {
                    LaunchError::UnresolvedImage {
                        field: field.to_string(),
                        reference: spec.reference.clone(),
                        reason,
                    }
                })?;
                (digest, true)
            }
        };
        Ok(ResolvedImage {
            reference: spec.reference.clone(),
            pinned: reference.pinned(&digest),
            digest,
            tag_resolved,
            source,
        })
    }

    /// The repository the task works on and the workspace it clones.
    fn workspace(
        &self,
        intent: &LaunchIntent,
        definition: &WorkflowDefinition,
        backend_id: &BackendId,
        backend: &BackendRecord,
        secrets: &mut SecretSet,
    ) -> Result<(Option<RepositoryEntry>, TaskWorkspace), LaunchError> {
        let inputs = match &intent.inputs {
            Value::Null => Value::Object(Default::default()),
            other => other.clone(),
        };
        let context = Context::new();
        let scope = TemplateScope::from_execution(&inputs, &context)
            .map_err(|error| LaunchError::InvalidInputs(error.to_string()))?
            .with_task_fields(intent.ticket.as_deref(), intent.title.as_deref());
        let render = |template: &str, field: &str| {
            scope
                .render(template, field)
                .map_err(|error| LaunchError::InvalidInputs(error.to_string()))
        };

        let (repository, workflow_ref) = match (&intent.repository, &definition.workspace) {
            (Some(repository), _) => (Some(repository.clone()), None),
            (
                None,
                Some(WorkspaceSpec::Git {
                    repository,
                    git_ref,
                    ..
                }),
            ) => {
                let git_ref = match git_ref {
                    Some(git_ref) => Some(render(git_ref, "globals.workspace.ref")?),
                    None => None,
                };
                (
                    Some(render(repository, "globals.workspace.repository")?),
                    git_ref,
                )
            }
            (None, Some(WorkspaceSpec::Local { path, .. })) => {
                if backend.kind.runs_container_images() {
                    return Err(LaunchError::UnsupportedWorkspace(format!(
                        "workflow '{}' works on a local folder, which cannot travel to backend \
                         '{backend_id}'",
                        definition.id
                    )));
                }
                let path =
                    self.allowed_local_directory(&render(path, "globals.workspace.path")?)?;
                return Ok((None, TaskWorkspace::Local { path }));
            }
            (None, None) => (None, None),
        };

        let Some(repository) = repository else {
            if intent.commit.is_some() {
                return Err(LaunchError::UnsupportedWorkspace(
                    "a commit was given but no repository to clone it from".to_string(),
                ));
            }
            return Ok((None, TaskWorkspace::None));
        };

        let entry = match self.registry.get(&repository)? {
            Some(entry) => entry,
            None if !backend.kind.runs_container_images()
                && std::path::Path::new(&repository).is_dir() =>
            {
                let repository = self.allowed_local_directory(&repository)?;
                let output = std::process::Command::new("git")
                    .args(["-C", &repository, "rev-parse", "HEAD"])
                    .output()
                    .map_err(|error| {
                        LaunchError::UnsupportedWorkspace(format!(
                            "could not read local Git workspace '{repository}': {error}"
                        ))
                    })?;
                if !output.status.success() {
                    return Err(LaunchError::UnsupportedWorkspace(format!(
                        "local workspace '{repository}' is not a Git repository"
                    )));
                }
                let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
                GitIdentity::check_commit(&commit)?;
                return Ok((
                    None,
                    TaskWorkspace::Git(GitIdentity {
                        repository: None,
                        remote_url: repository,
                        commit,
                        git_ref: None,
                        ref_origin: None,
                        credential: None,
                    }),
                ));
            }
            None => return Err(LaunchError::UnknownRepository(repository.clone())),
        };
        // A commit — explicit, or a workflow `ref` that already is one —
        // pins the workspace now. Otherwise a backend that clones for itself
        // pins it at admission: resolving a branch, a tag or `HEAD` needs the
        // network, which creation must not wait for (R6). A backend running
        // images always needs the commit up front.
        // Without a workflow `ref`, the branch of the repository configuration
        // is used; with neither, the clone takes the remote's `HEAD`.
        let named_ref = entry.named_ref(workflow_ref.as_deref());
        let (commit, git_ref, ref_origin) = match (&intent.commit, named_ref) {
            (Some(commit), _) => (commit.clone(), None, None),
            (None, Some((git_ref, RefOrigin::Workflow)))
                if GitIdentity::check_commit(&git_ref).is_ok() =>
            {
                (git_ref, None, None)
            }
            (None, named) if !backend.kind.runs_container_images() => match named {
                Some((git_ref, origin)) => (String::new(), Some(git_ref), Some(origin)),
                None => (String::new(), None, None),
            },
            (None, _) => return Err(LaunchError::MissingCommit(repository)),
        };
        if !commit.is_empty() {
            GitIdentity::check_commit(&commit)?;
        }
        let credential = match &entry.git_credential {
            Some(name) => Some(secrets.require(
                backend_id,
                backend,
                name,
                &format!("repository '{}'", entry.id),
            )?),
            None => None,
        };
        let workspace = TaskWorkspace::Git(GitIdentity {
            repository: Some(entry.id.clone()),
            remote_url: entry.remote.clone(),
            commit,
            git_ref,
            ref_origin,
            credential,
        });
        Ok((Some(entry), workspace))
    }

    fn harness(
        &self,
        intent: &LaunchIntent,
        profile: &ProfileRecord,
        definition: &WorkflowDefinition,
        backend_id: &BackendId,
        backend: &BackendRecord,
    ) -> Result<String, LaunchError> {
        let supported = self.harness.supported();
        let selected = intent
            .harness
            .clone()
            .or_else(|| profile.harness.clone())
            .unwrap_or_else(|| self.harness.default_harness());
        // Every harness the task may run: the fallback harness (the live
        // default is followed at run time, D56) plus each one a step names
        // literally (a templated one is only known at run time).
        let mut needed = BTreeSet::from([selected.clone()]);
        for state in definition.states.values() {
            if let Some(ActionConfig::Agent(agent)) = &state.action {
                needed.extend(agent.harness.iter().filter(|h| !h.contains("{{")).cloned());
            }
            if let Some(DecisionConfig::Agent(agent)) = &state.decision {
                needed.extend(agent.harness.iter().filter(|h| !h.contains("{{")).cloned());
            }
        }
        for harness in needed {
            if !supported.contains(&harness) {
                return Err(LaunchError::UnsupportedHarness {
                    reason: format!("this build supports {}", supported.join(", ")),
                    harness,
                });
            }
            if let Some(reason) = backend.harness_refusal(backend_id, &harness) {
                return Err(LaunchError::UnsupportedHarness { reason, harness });
            }
        }
        Ok(selected)
    }

    fn resources(
        &self,
        intent: &LaunchIntent,
        backend_id: &BackendId,
        backend: &BackendRecord,
    ) -> Result<ResourceProfile, LaunchError> {
        let resources = intent.resources.unwrap_or(backend.limits.default_resources);
        let problems = resources.problems();
        if !problems.is_empty() {
            return Err(LaunchError::InvalidResources(problems.join("; ")));
        }
        let max = backend.limits.max_resources();
        if !resources.fits_within(&max) {
            return Err(LaunchError::InvalidResources(format!(
                "{resources} exceed backend '{backend_id}''s limit of {max}"
            )));
        }
        Ok(resources)
    }

    /// Resolves an already-loaded workflow definition into a sealed launch.
    ///
    /// The local daemon uses this path after resolving a workflow reference
    /// (which may be a local file rather than a catalogue name).  It keeps
    /// that compatibility at the API boundary while applying exactly the
    /// same backend, environment, workspace, and snapshot rules as a
    /// control-plane create request.
    pub fn resolve_definition(
        &self,
        intent: &LaunchIntent,
        definition: WorkflowDefinition,
    ) -> Result<ResolvedLaunch, LaunchError> {
        let (backend_id, backend) = self.backend(intent)?;
        let mut secrets = SecretSet::default();
        let (repository, workspace) =
            self.workspace(intent, &definition, &backend_id, &backend, &mut secrets)?;

        let base_image = match (&definition.globals.image, &repository) {
            (Some(image), _) => Some(self.resolve_image(
                "globals.image",
                image,
                ImageSource::Workflow {
                    workflow: definition.id.clone(),
                },
            )?),
            (None, Some(entry)) => match &entry.base_image {
                Some(image) => Some(self.resolve_image(
                    &format!("repository '{}' base_image", entry.id),
                    image,
                    ImageSource::Repository {
                        repository: entry.id.clone(),
                    },
                )?),
                None => None,
            },
            (None, None) => None,
        };
        if base_image.is_none() && backend.kind.runs_container_images() {
            return Err(LaunchError::MissingBaseImage {
                backend: backend_id.to_string(),
                repository: repository.as_ref().map(|entry| entry.id.clone()),
                workflow: definition.id.clone(),
            });
        }

        let mut state_ids: Vec<_> = definition.states.keys().collect();
        state_ids.sort();
        let mut step_images = BTreeMap::new();
        for state_id in state_ids {
            if let Some(image) = &definition.states[state_id].image {
                let resolved = self.resolve_image(
                    &format!("states.{state_id}.image"),
                    image,
                    ImageSource::Step {
                        workflow: definition.id.clone(),
                        state: state_id.clone(),
                    },
                )?;
                step_images.insert(state_id.clone(), resolved);
            }
        }

        let profile = self.profile(intent)?;
        let runtime = self.runtime()?;
        let runtime_bundle = match (&runtime, backend.kind.runs_container_images()) {
            (Some(runtime), true) => Some(self.resolve_image(
                "runtime.yaml bundle",
                &runtime.bundle,
                ImageSource::RuntimeBundle,
            )?),
            (None, true) => return Err(LaunchError::MissingRuntimeBundle(backend_id.to_string())),
            (_, false) => None,
        };
        let mut tools = BTreeMap::new();
        for tool in &profile.tools {
            let spec = runtime
                .as_ref()
                .and_then(|runtime| runtime.tools.get(tool))
                .ok_or_else(|| LaunchError::UnknownTool {
                    profile: profile.id.clone(),
                    tool: tool.clone(),
                })?;
            let resolved = self.resolve_image(
                &format!("runtime.yaml tools.{tool}"),
                spec,
                ImageSource::Tool { tool: tool.clone() },
            )?;
            tools.insert(tool.clone(), resolved);
        }
        for name in &profile.secrets {
            secrets.require(
                &backend_id,
                &backend,
                name,
                &format!("profile '{}'", profile.id),
            )?;
        }

        let harness = self.harness(intent, &profile, &definition, &backend_id, &backend)?;
        let resources = self.resources(intent, &backend_id, &backend)?;
        let environment = ResolvedEnvironment {
            base_image,
            step_images,
            runtime_bundle,
            tools,
            profile: profile.resolve(),
            harness,
            secrets: secrets.0.into_values().collect(),
            identity: String::new(),
        };
        let inputs = match &intent.inputs {
            Value::Null => Value::Object(Default::default()),
            other => other.clone(),
        };
        let spec = TaskLaunchSpec::new(
            definition.id.clone(),
            definition.version,
            inputs,
            workspace,
            environment,
            resources,
        )
        .with_definition(&definition)
        .map_err(|error| LaunchError::InvalidWorkflow {
            workflow: definition.id.clone(),
            problems: vec![format!("the definition cannot be snapshotted: {error}")],
        })?
        .with_labels(intent.ticket.clone(), intent.title.clone());
        Ok(ResolvedLaunch { backend_id, spec })
    }
}

/// The secret references a task collects, deduplicated by name.
#[derive(Default)]
struct SecretSet(BTreeMap<String, SecretReference>);

impl SecretSet {
    fn require(
        &mut self,
        backend_id: &BackendId,
        backend: &BackendRecord,
        name: &str,
        wanted_by: &str,
    ) -> Result<SecretReference, LaunchError> {
        let reference = backend.secret_reference(backend_id, name).ok_or_else(|| {
            LaunchError::UnregisteredSecret {
                secret: name.to_string(),
                backend: backend_id.to_string(),
                wanted_by: wanted_by.to_string(),
            }
        })?;
        self.0.insert(reference.name.clone(), reference.clone());
        Ok(reference)
    }
}

impl LaunchResolver for CatalogLaunchResolver {
    fn backend_limit(&self, backend: &BackendId) -> Result<u32, LaunchError> {
        Ok(self.usable_backend(backend)?.limits.max_concurrent_tasks)
    }

    fn slot_release_timings(&self, backend: &BackendId) -> Result<SlotReleaseTimings, LaunchError> {
        Ok(self.usable_backend(backend)?.limits.slot_release_timings())
    }

    fn configured_default_backend(&self) -> String {
        self.owner.default_backend.clone()
    }

    fn backend_record(&self, backend: &BackendId) -> Result<Option<BackendRecord>, LaunchError> {
        Ok(self.catalog.backend(backend.as_str())?)
    }

    fn default_harness(&self, backend: &BackendId) -> Option<TaskDefaultHarness> {
        let harness = self.harness.live_default()?;
        let record = match self.catalog.backend(backend.as_str()) {
            Ok(Some(record)) => record,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%backend, %error, "cannot read the backend to check the default harness");
                return None;
            }
        };
        Some(match record.harness_refusal(backend, &harness) {
            None => TaskDefaultHarness::Available { harness },
            Some(reason) => TaskDefaultHarness::Unavailable { reason, harness },
        })
    }

    fn default_harness_changes(&self) -> Option<watch::Receiver<u64>> {
        self.harness.live_changes()
    }

    fn resolve(&self, intent: &LaunchIntent) -> Result<ResolvedLaunch, LaunchError> {
        let definition = self.workflow(intent)?;
        self.resolve_definition(intent, definition)
    }
}
