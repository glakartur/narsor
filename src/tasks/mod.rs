//! Starting a task: one workflow definition plus its inputs, turned into an
//! execution that owns a directory to work in.
//!
//! This is the operation behind `orchestrator run` and behind the TUI's `n`,
//! and it lives here rather than in either of them because both need it and
//! because one of its steps — [`resolve_repository`] — decides what `git` is
//! asked to clone. A second, drifting copy of that rule is a
//! command-execution hole, so there is exactly one.
//!
//! # Where an execution gets its workspace
//!
//! Three cases, in this order:
//!
//! * the definition declares `globals.workspace: { type: git, ... }` — the
//!   `repository` field may still hold a `{{ inputs.* }}` template, so it is
//!   expanded against the inputs first and the result is then resolved
//!   **registry first** (see [`resolve_repository`]): a value that names an
//!   entry of `{root}/repositories.json` provisions from that entry's `remote`,
//!   and anything else is handed to [`WorkspaceProvisioner`] as a raw git
//!   remote (whatever `git clone` accepts, including a local path);
//! * the definition declares `{ type: local, path: ... }` — the path is
//!   expanded the same way and its contents are copied into the workspace;
//! * the definition declares no workspace at all — an empty directory
//!   `{root}/tasks/{execution_id}/code/workspace` is created and used. A
//!   workflow whose scripts live outside the repository (the usual shape of a
//!   smoke test) needs nothing more, and an empty directory is a better default
//!   than refusing to run.
//!
//! # The order of the steps is load-bearing
//!
//! [`start_task`] takes inputs that the caller has already parsed and
//! validated, because a rejected input must leave nothing on disk at all — the
//! CLI parses `--input name=value` pairs, the TUI reads a form, and neither
//! reaches this module until the values are good.
//!
//! Provisioning is then deliberately split around
//! [`Engine::start`](crate::engine::Engine::start): everything checkable
//! without touching the disk ([`WorkspaceProvisioner::validate`]) runs *before*
//! the execution is created, so a bad remote leaves nothing behind; the actual
//! clone runs *after*, because the task directory is named for the execution id
//! (`DOMAIN_MODEL.md` §2.8) and that id only exists once the execution does.
//! The execution record is then saved again with the real workspace. If the
//! clone fails, the freshly created execution is marked `Failed` with an
//! `Error` event rather than left `Running` with no directory to run in.
//!
//! # Running is the caller's business
//!
//! [`start_task`] stops once the execution exists, is workspaced and is saved.
//! It never calls [`Engine::run`](crate::engine::Engine::run), because the two
//! callers want different things from it: the CLI awaits the run to completion
//! and prints a summary, while the TUI starts it and keeps rendering.
//!
//! # Finding the definition again
//!
//! The engine needs the [`WorkflowDefinition`] to continue an execution, but
//! `state.json` stores only the workflow *id*. So the absolute path of the file
//! is recorded in `metadata.cli.definition_path`
//! ([`record_definition_path`]) and read back with [`stored_definition_path`].
//! The key is spelled `cli` for history: every execution already on disk
//! carries it, and renaming it would make all of them unresumable.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::domain::{
    Context, EventData, ExecutionStatus, WorkflowDefinition, WorkflowExecution, Workspace,
    WorkspaceInfo, WorkspaceSpec,
};
use crate::engine::{Engine, EngineError};
use crate::persistence::{ExecutionStore, PersistenceError};
use crate::registry::{RegistryError, RepositoryRegistry};
use crate::workflow::{load_from_file, validate_path, LoadError, TemplateError, TemplateScope};
use crate::workspace::{RepositorySpec, WorkspaceError, WorkspaceProvisioner, WorkspaceRequest};

/// Key of the scratch area inside [`WorkflowExecution::metadata`] that holds
/// the definition path.
pub const METADATA_CLI_KEY: &str = "cli";

/// Key of the recorded definition path inside that scratch area.
pub const METADATA_DEFINITION_PATH_KEY: &str = "definition_path";

/// Directory name of the fallback workspace, used when the definition declares
/// none.
pub const FALLBACK_WORKSPACE_NAME: &str = "workspace";

/// Everything that can stop a task from starting.
///
/// The messages are the ones the CLI has always printed, so a caller with no
/// better wording of its own can simply render the error.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The definition file could not be read, parsed or compiled.
    #[error("Could not load the definition: {0}")]
    Load(#[from] LoadError),

    /// The definition does not pass static validation.
    ///
    /// The violations are carried whole, because each caller prints them in its
    /// own layout.
    #[error("The definition is invalid:\n  {}", violations.join("\n  "))]
    InvalidDefinition {
        /// Every violation, so the author sees them all at once.
        violations: Vec<String>,
    },

    /// A `{{ ... }}` inside `globals.workspace` could not be expanded.
    #[error("Could not expand the workspace template: {0}")]
    Template(#[from] TemplateError),

    /// `{root}/repositories.json` could not be read.
    #[error("Could not read the registry: {0}")]
    Registry(#[from] RegistryError),

    /// The workspace was rejected before the execution was created.
    #[error("The workspace cannot be created: {0}")]
    WorkspaceRejected(#[source] WorkspaceError),

    /// The engine refused to create the execution.
    #[error("Could not create the execution: {0}")]
    Start(#[source] EngineError),

    /// Cloning or copying the workspace failed, after the execution existed.
    #[error("Could not create the workspace: {source}")]
    Provision {
        /// The execution that was abandoned as a result.
        execution_id: String,
        /// What the provisioner said.
        #[source]
        source: WorkspaceError,
    },

    /// The fallback workspace directory could not be created.
    #[error("Could not create the workspace directory: {source}")]
    WorkspaceDirectory {
        /// The execution that was abandoned as a result.
        execution_id: String,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },

    /// The workspaced execution could not be written back to the store.
    #[error("Could not save the execution: {source}")]
    Save {
        /// The execution that was created but never saved with its workspace.
        execution_id: String,
        /// What the store said.
        #[source]
        source: PersistenceError,
    },
}

impl TaskError {
    /// The execution this error left behind, when it left one behind at all.
    ///
    /// An error carrying an id happened *after*
    /// [`Engine::start`](crate::engine::Engine::start), so the store holds a
    /// record an operator can look at; the others happened before anything was
    /// written.
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            TaskError::Provision { execution_id, .. }
            | TaskError::WorkspaceDirectory { execution_id, .. }
            | TaskError::Save { execution_id, .. } => Some(execution_id),
            _ => None,
        }
    }
}

/// Result of an operation in this module.
pub type Result<T> = std::result::Result<T, TaskError>;

/// Validates a definition file in full and loads it.
///
/// Both steps in one call, in the order that matters: every violation is
/// collected before anything is compiled, so a caller can show the author all
/// of them at once.
pub fn load_definition(path: &Path) -> Result<WorkflowDefinition> {
    let violations = validate_path(path)?;
    if !violations.is_empty() {
        return Err(TaskError::InvalidDefinition {
            violations: violations.iter().map(ToString::to_string).collect(),
        });
    }
    Ok(load_from_file(path)?)
}

/// Creates an execution, gives it a workspace and records where its definition
/// lives.
///
/// `inputs` are the already parsed and validated values — see the module
/// documentation for why. The returned execution is saved and ready to be
/// handed to [`Engine::run`](crate::engine::Engine::run) by the caller.
pub fn start_task(
    root: &Path,
    engine: &Engine,
    definition: &WorkflowDefinition,
    definition_path: &Path,
    inputs: Map<String, Value>,
) -> Result<WorkflowExecution> {
    start_task_with(root, engine, definition, definition_path, inputs, |_| {})
}

/// [`start_task`], with a hook that observes the execution the moment it exists.
///
/// The CLI announces the new id before its workspace is provisioned, so that a
/// failing clone still tells the operator which execution to look at; that
/// announcement is the only reason this variant exists.
pub fn start_task_with(
    root: &Path,
    engine: &Engine,
    definition: &WorkflowDefinition,
    definition_path: &Path,
    inputs: Map<String, Value>,
    on_created: impl FnOnce(&WorkflowExecution),
) -> Result<WorkflowExecution> {
    let provisioner = WorkspaceProvisioner::new(root);
    let request = workspace_request(root, definition, &inputs)?;
    if let Some(request) = &request {
        provisioner
            .validate(request)
            .map_err(TaskError::WorkspaceRejected)?;
    }

    let mut execution = engine
        .start(
            definition,
            Value::Object(inputs),
            // Replaced below, once the execution id names the task directory.
            Workspace::local_folder(root.display().to_string()),
        )
        .map_err(TaskError::Start)?;

    on_created(&execution);

    let workspace = match provision_workspace(&provisioner, &execution.id, request.as_ref()) {
        Ok(workspace) => workspace,
        Err(failure) => {
            let error = failure.into_task_error(&execution.id);
            abandon_execution(engine.store(), &mut execution, &error.to_string());
            return Err(error);
        }
    };

    execution.context.workspace = Some(WorkspaceInfo {
        id: workspace.id.clone(),
        path: workspace.path.clone(),
        workspace_type: format!("{:?}", workspace.workspace_type).to_lowercase(),
    });
    execution.workspace = workspace;
    record_definition_path(&mut execution, definition_path);
    engine
        .store()
        .save_execution(&execution)
        .map_err(|source| TaskError::Save {
            execution_id: execution.id.clone(),
            source,
        })?;

    Ok(execution)
}

/// Builds the workspace request from the definition, expanding templates in it.
///
/// `None` means the definition declares no workspace and the fallback directory
/// is used.
pub fn workspace_request(
    root: &Path,
    definition: &WorkflowDefinition,
    inputs: &Map<String, Value>,
) -> Result<Option<WorkspaceRequest>> {
    let Some(spec) = &definition.workspace else {
        return Ok(None);
    };

    let context = Context::new();
    let scope = TemplateScope::new(inputs, &context);
    let render = |template: &str, field: &str| -> Result<String> {
        Ok(scope.render(template, &format!("globals.workspace.{field}"))?)
    };

    let spec = match spec {
        WorkspaceSpec::Git {
            repository,
            git_ref,
        } => {
            let repository = render(repository, "repository")?;
            let git_ref = match git_ref {
                Some(git_ref) => Some(render(git_ref, "ref")?),
                None => None,
            };
            resolve_repository(root, &repository, git_ref.as_deref())?
        }
        WorkspaceSpec::Local { path } => {
            let path = render(path, "path")?;
            RepositorySpec::local(repository_name(&path), PathBuf::from(path))
        }
    };

    Ok(Some(WorkspaceRequest::new(spec)))
}

/// Turns the `repository` of a `git` workspace into a repository to provision.
///
/// # Precedence: registry id first, raw remote as fallback
///
/// `WORKFLOW_SCHEMA.md` §3.2 defines `repository` as the id of a registered
/// repository, so the value is looked up in `{root}/repositories.json` first.
/// A hit provisions from the entry's `remote`, checks out its `default_branch`
/// when the workflow declares no `ref` of its own, and names the checkout
/// directory after the entry's id — an id the registry already validated as a
/// usable directory name.
///
/// A miss keeps the behaviour the CLI had before a registry existed: the value
/// is handed to `git clone` as a raw remote, which is what makes a workflow
/// carrying a URL or a local path — `tests/fixtures/implement_feature.yaml`
/// among them — keep working unchanged.
///
/// Which branch was taken is logged at `debug`, because "it cloned something
/// other than what I expected" is otherwise a silent difference.
pub fn resolve_repository(
    root: &Path,
    repository: &str,
    git_ref: Option<&str>,
) -> Result<RepositorySpec> {
    let registered = RepositoryRegistry::new(root).get(repository)?;

    match registered {
        Some(entry) => {
            let git_ref = git_ref
                .map(ToString::to_string)
                .or_else(|| entry.default_branch.clone());
            tracing::debug!(
                repository = %repository,
                remote = %entry.remote,
                git_ref = git_ref.as_deref().unwrap_or("(clone default)"),
                "resolved the workspace repository from the registry"
            );
            Ok(RepositorySpec::git(entry.id, entry.remote, git_ref))
        }
        None => {
            tracing::debug!(
                repository = %repository,
                "no registry entry for the workspace repository — using it as a raw git remote"
            );
            Ok(RepositorySpec::git(
                repository_name(repository),
                repository,
                git_ref.map(ToString::to_string),
            ))
        }
    }
}

/// The directory name a remote or path is checked out into.
pub fn repository_name(remote: &str) -> String {
    let trimmed = remote.trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() || name == "." || name == ".." {
        FALLBACK_WORKSPACE_NAME.to_string()
    } else {
        name.to_string()
    }
}

/// Records the definition path so the interface and `input`/`resume` can find
/// it again.
pub fn record_definition_path(execution: &mut WorkflowExecution, file: &Path) {
    let path = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if !execution.metadata.is_object() {
        execution.metadata = json!({});
    }
    if let Some(metadata) = execution.metadata.as_object_mut() {
        metadata.insert(
            METADATA_CLI_KEY.to_string(),
            json!({ METADATA_DEFINITION_PATH_KEY: path.display().to_string() }),
        );
    }
}

/// The definition path recorded when the execution was created.
pub fn stored_definition_path(execution: &WorkflowExecution) -> Option<PathBuf> {
    execution
        .metadata
        .get(METADATA_CLI_KEY)?
        .get(METADATA_DEFINITION_PATH_KEY)?
        .as_str()
        .map(PathBuf::from)
}

/// What provisioning can fail with, before it knows which execution it belongs
/// to.
enum ProvisionFailure {
    /// The provisioner refused or could not finish.
    Workspace(WorkspaceError),
    /// The fallback directory could not be created.
    Directory(std::io::Error),
}

impl ProvisionFailure {
    /// Names the execution the failure abandoned.
    fn into_task_error(self, execution_id: &str) -> TaskError {
        match self {
            ProvisionFailure::Workspace(source) => TaskError::Provision {
                execution_id: execution_id.to_string(),
                source,
            },
            ProvisionFailure::Directory(source) => TaskError::WorkspaceDirectory {
                execution_id: execution_id.to_string(),
                source,
            },
        }
    }
}

/// Materialises the workspace of an execution, or the fallback directory.
fn provision_workspace(
    provisioner: &WorkspaceProvisioner,
    execution_id: &str,
    request: Option<&WorkspaceRequest>,
) -> std::result::Result<Workspace, ProvisionFailure> {
    match request {
        Some(request) => provisioner
            .provision(execution_id, request)
            .map(|provisioned| provisioned.workspace)
            .map_err(ProvisionFailure::Workspace),
        None => {
            let path = provisioner
                .code_dir(execution_id)
                .join(FALLBACK_WORKSPACE_NAME);
            fs::create_dir_all(&path).map_err(ProvisionFailure::Directory)?;
            Ok(Workspace::local_folder(path.display().to_string()))
        }
    }
}

/// Marks an execution that never got a workspace as failed.
///
/// Leaving it `Running` would make it look resumable when nothing can ever
/// step it: it has no directory to run in.
fn abandon_execution(store: &ExecutionStore, execution: &mut WorkflowExecution, message: &str) {
    execution.status = ExecutionStatus::Failed;
    execution.completed_at = Some(chrono::Utc::now());
    let _ = store.event_log(&execution.id).append_next(
        &execution.id,
        EventData::Error {
            message: message.to_string(),
            context: json!({ "stage": "workspace_provisioning" }),
        },
    );
    let _ = store.save_execution(execution);
}
