//! Starting a task: one workflow definition plus its inputs, turned into an
//! execution that owns a directory to work in.
//!
//! This is the operation behind `narsor run` and behind the TUI's `n`,
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
//!   entry of `{catalog}/repositories/` provisions from that entry's `remote`,
//!   and anything else is handed to [`WorkspaceProvisioner`] as a raw git
//!   remote (whatever `git clone` accepts, including a local path);
//! * the definition declares `{ type: local, path: ... }` — the path is
//!   expanded the same way and its contents are copied into the workspace;
//! * the definition declares no workspace at all — an empty directory
//!   `{work}/tasks/{execution_id}/code/workspace` is created and used. A
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
//! `state.json` stores only the workflow *id*. So where the definition came
//! from is recorded in the execution's metadata, under two keys:
//!
//! * `metadata.cli.definition_path` — the absolute path of the file, when the
//!   definition came from one. The key is spelled `cli` for history: every
//!   execution already on disk carries it, and renaming it would make all of
//!   them unresumable;
//! * `metadata.cli.definition_name` — the workflow's *name* in the search path
//!   ([`crate::workflows`]). This is the key a built-in needs, because a
//!   built-in is compiled into the binary and has no path at all: without it an
//!   execution started as `narsor run template` could never
//!   be resumed, including executions paused at human gates.
//!
//! [`resolve_definition`] reads them back, **path first**: an execution written
//! before `definition_name` existed carries only the path and keeps resolving
//! exactly as it did, and an execution started from a file keeps running that
//! file even if a workflow of the same name later appears in the search path.
//! The name is the fallback — for a built-in, which has no path, and for a file
//! that has since moved but whose name is now reachable.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::domain::{
    Context, EventData, ExecutionStatus, RefOrigin, WorkflowDefinition, WorkflowExecution,
    Workspace, WorkspaceInfo, WorkspaceSpec,
};
use crate::engine::{Engine, EngineError};
use crate::persistence::{write_atomic, ExecutionStore, PersistenceError, StatePaths};
use crate::registry::{RegistryError, RepositoryRegistry};
use crate::workflow::{load_from_file, validate_path, LoadError, TemplateError, TemplateScope};
use crate::workflows::{ResolveError, ResolvedWorkflow, WorkflowOrigin, WorkflowResolver};
use crate::workspace::{
    materialise, Base, CheckpointError, MaterialiseRequest, RepositorySpec, WorkspaceError,
    WorkspaceProvisioner, WorkspaceRequest,
};

/// Key of the scratch area inside [`WorkflowExecution::metadata`] that holds
/// the definition path.
pub const METADATA_CLI_KEY: &str = "cli";

/// Key of the recorded definition path inside that scratch area.
pub const METADATA_DEFINITION_PATH_KEY: &str = "definition_path";

/// Key of the recorded workflow name inside that scratch area.
///
/// Written beside [`METADATA_DEFINITION_PATH_KEY`], never instead of it, so
/// that an execution written by an older binary keeps resolving.
pub const METADATA_DEFINITION_NAME_KEY: &str = "definition_name";

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

    /// The repository registry could not be read.
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

/// Validates a definition the resolver found and loads it.
///
/// The same two steps as [`load_definition`], against the YAML the resolver
/// carries rather than a file — which is the only form a built-in has.
pub fn load_resolved(resolved: &ResolvedWorkflow) -> Result<WorkflowDefinition> {
    let violations = resolved.validate()?;
    if !violations.is_empty() {
        return Err(TaskError::InvalidDefinition {
            violations: violations.iter().map(ToString::to_string).collect(),
        });
    }
    Ok(resolved.load()?)
}

/// Creates an execution, gives it a workspace and records where its definition
/// lives.
///
/// `inputs` are the already parsed and validated values — see the module
/// documentation for why. The returned execution is saved and ready to be
/// handed to [`Engine::run`](crate::engine::Engine::run) by the caller.
pub fn start_task(
    catalog: &Path,
    work: &Path,
    engine: &Engine,
    definition: &WorkflowDefinition,
    source: impl Into<DefinitionSource>,
    inputs: Map<String, Value>,
) -> Result<WorkflowExecution> {
    start_task_with(catalog, work, engine, definition, source, inputs, |_| {})
}

/// [`start_task`], with a hook that observes the execution the moment it exists.
///
/// The CLI announces the new id before its workspace is provisioned, so that a
/// failing clone still tells the operator which execution to look at; that
/// announcement is the only reason this variant exists.
pub fn start_task_with(
    catalog: &Path,
    work: &Path,
    engine: &Engine,
    definition: &WorkflowDefinition,
    source: impl Into<DefinitionSource>,
    inputs: Map<String, Value>,
    on_created: impl FnOnce(&WorkflowExecution),
) -> Result<WorkflowExecution> {
    let source = source.into();
    // The two roots this needs, taken separately rather than as one: the
    // registry is read from the catalogue, which a deployment may mount
    // read-only, while the checkout lands in the work tree, which is local
    // scratch. Passing one root for both is what made them one volume.
    let provisioner = WorkspaceProvisioner::new(work);
    let request = workspace_request(catalog, definition, &inputs)?;
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
            Workspace::local_folder(work.display().to_string()),
        )
        .map_err(TaskError::Start)?;

    on_created(&execution);

    // The execution carries its own definition from here on (R2): resuming it
    // must not depend on a catalogue that may have changed, or on a worker that
    // may not have the catalogue at all. Only a file-sourced definition has raw
    // YAML to pin; a built-in resolves by name from the binary itself and is
    // never subject to catalogue drift.
    if let Some(path) = &source.path {
        let text = fs::read_to_string(path).map_err(|error| TaskError::Save {
            execution_id: execution.id.clone(),
            source: PersistenceError::io(path, error),
        })?;
        write_atomic(
            &engine.store().paths().definition_file(&execution.id),
            text.as_bytes(),
        )
        .map_err(|source| TaskError::Save {
            execution_id: execution.id.clone(),
            source,
        })?;
    }

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
    record_definition_source(&mut execution, &source);
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
    catalog: &Path,
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
            ..
        } => {
            let repository = render(repository, "repository")?;
            let git_ref = match git_ref {
                Some(git_ref) => Some(render(git_ref, "ref")?),
                None => None,
            };
            resolve_repository(catalog, &repository, git_ref.as_deref())?
        }
        WorkspaceSpec::Local { path, .. } => {
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
/// repository, so the value is looked up in `{catalog}/repositories/` first.
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
    catalog: &Path,
    repository: &str,
    git_ref: Option<&str>,
) -> Result<RepositorySpec> {
    let registered = RepositoryRegistry::new(catalog).get(repository)?;

    match registered {
        Some(entry) => {
            let (git_ref, ref_origin) = match entry.named_ref(git_ref) {
                Some((git_ref, origin)) => (Some(git_ref), Some(origin)),
                None => (None, None),
            };
            tracing::debug!(
                repository = %repository,
                remote = %entry.remote,
                git_ref = git_ref.as_deref().unwrap_or("(clone default)"),
                "resolved the workspace repository from the registry"
            );
            Ok(RepositorySpec::git(entry.id, entry.remote, git_ref).with_ref_origin(ref_origin))
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
            )
            .with_ref_origin(git_ref.map(|_| RefOrigin::Workflow)))
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

/// Records where the definition came from, so the interface, `input` and
/// `resume` can find it again.
///
/// Both keys are written when both are known: the path for an execution that
/// runs a file, the name for one that runs something reachable by name — a
/// built-in among them.
pub fn record_definition_source(execution: &mut WorkflowExecution, source: &DefinitionSource) {
    if !execution.metadata.is_object() {
        execution.metadata = json!({});
    }
    let mut recorded = Map::new();
    if let Some(path) = &source.path {
        recorded.insert(
            METADATA_DEFINITION_PATH_KEY.to_string(),
            Value::String(path.display().to_string()),
        );
    }
    if let Some(name) = &source.name {
        recorded.insert(
            METADATA_DEFINITION_NAME_KEY.to_string(),
            Value::String(name.clone()),
        );
    }
    if let Some(metadata) = execution.metadata.as_object_mut() {
        metadata.insert(METADATA_CLI_KEY.to_string(), Value::Object(recorded));
    }
}

/// Records the definition path, for a caller that only has one.
pub fn record_definition_path(execution: &mut WorkflowExecution, file: &Path) {
    record_definition_source(execution, &DefinitionSource::from(file));
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

/// The workflow name recorded when the execution was created.
pub fn stored_definition_name(execution: &WorkflowExecution) -> Option<String> {
    execution
        .metadata
        .get(METADATA_CLI_KEY)?
        .get(METADATA_DEFINITION_NAME_KEY)?
        .as_str()
        .map(ToString::to_string)
}

/// Where an execution's definition came from, as it is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DefinitionSource {
    /// The name it is reachable by in the search path, when it has one.
    pub name: Option<String>,
    /// The absolute path of the file it was read from, when it has one.
    pub path: Option<PathBuf>,
}

impl DefinitionSource {
    /// A definition read from a file the caller named itself.
    ///
    /// The stem is recorded as the name too: it costs nothing, and it lets a
    /// definition that has since moved still resolve if a copy of that name is
    /// reachable. The path keeps winning while it exists.
    pub fn from_path(file: &Path) -> Self {
        let path = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
            .filter(|stem| !stem.is_empty());
        Self {
            name,
            path: Some(path),
        }
    }

    /// A definition the resolver found — a built-in has a name and no path.
    pub fn from_resolved(resolved: &ResolvedWorkflow) -> Self {
        Self {
            name: Some(resolved.name.clone()),
            path: resolved.path().map(|path| path.to_path_buf()),
        }
    }
}

impl From<&Path> for DefinitionSource {
    fn from(path: &Path) -> Self {
        DefinitionSource::from_path(path)
    }
}

impl From<&PathBuf> for DefinitionSource {
    fn from(path: &PathBuf) -> Self {
        DefinitionSource::from_path(path)
    }
}

impl From<&ResolvedWorkflow> for DefinitionSource {
    fn from(resolved: &ResolvedWorkflow) -> Self {
        DefinitionSource::from_resolved(resolved)
    }
}

/// Why an execution's definition could not be found again.
#[derive(Debug, thiserror::Error)]
pub enum DefinitionLookupError {
    /// Neither key is present — an execution created outside these entry
    /// points.
    #[error(
        "Execution '{execution_id}' records no workflow definition \
         (neither metadata.cli.definition_path nor metadata.cli.definition_name) \
         — point at the definition with --file."
    )]
    NotRecorded {
        /// The execution that carries nothing.
        execution_id: String,
    },

    /// The recorded path is gone and the recorded name resolves to nothing.
    #[error("Could not find the definition of execution '{execution_id}': {source}")]
    Unresolved {
        /// The execution whose definition vanished.
        execution_id: String,
        /// Everywhere the resolver looked.
        #[source]
        source: ResolveError,
    },

    /// A file that is there but cannot be read.
    #[error("Could not read definition '{path}': {source}")]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}

/// Finds the definition an execution runs, from what it recorded.
///
/// Path first, name second — see the module documentation for why that order
/// is the one that keeps executions already on disk working.
pub fn resolve_definition(
    catalog: &Path,
    store_paths: &StatePaths,
    execution: &WorkflowExecution,
) -> std::result::Result<ResolvedWorkflow, DefinitionLookupError> {
    // The pinned copy wins over everything else: it is the definition this
    // execution actually started with.
    let pinned = store_paths.definition_file(&execution.id);
    if pinned.is_file() {
        let source = fs::read_to_string(&pinned).map_err(|source| DefinitionLookupError::Read {
            path: pinned.clone(),
            source,
        })?;
        return Ok(ResolvedWorkflow {
            name: stored_definition_name(execution)
                .unwrap_or_else(|| execution.workflow_id.clone()),
            origin: WorkflowOrigin::File(pinned),
            source,
        });
    }

    let path = stored_definition_path(execution);
    if let Some(path) = path.as_ref().filter(|path| path.is_file()) {
        let source = fs::read_to_string(path).map_err(|source| DefinitionLookupError::Read {
            path: path.clone(),
            source,
        })?;
        return Ok(ResolvedWorkflow {
            name: stored_definition_name(execution).unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default()
                    .to_string()
            }),
            origin: WorkflowOrigin::File(path.clone()),
            source,
        });
    }

    let Some(name) = stored_definition_name(execution) else {
        return Err(DefinitionLookupError::NotRecorded {
            execution_id: execution.id.clone(),
        });
    };

    WorkflowResolver::new(catalog)
        .resolve_name(&name)
        .map_err(|source| DefinitionLookupError::Unresolved {
            execution_id: execution.id.clone(),
            source,
        })
}

/// Everything that can stop an execution from being taken over.
#[derive(Debug, thiserror::Error)]
pub enum TakeoverError {
    /// The store could not be read or written.
    #[error("{0}")]
    Persistence(#[from] PersistenceError),

    /// The workspace could not be provisioned.
    #[error("{0}")]
    Workspace(#[from] WorkspaceError),

    /// The checkpoint could not be restored.
    #[error("{0}")]
    Checkpoint(#[from] CheckpointError),

    /// The execution never wrote a checkpoint, so there is nothing to restore.
    #[error("execution {execution_id} has no checkpoint to take over from")]
    NoCheckpoint {
        /// The execution that was asked for.
        execution_id: String,
    },
}

/// Takes an execution over: reconcile, then rebuild the workspace from its
/// checkpoint.
///
/// This is the counterpart of [`start_task`] and the one path by which an
/// execution resumes in a process that did not start it — whether because the
/// previous one crashed, because its worker was killed, or because the work is
/// being handed to somebody else. There is deliberately no second path: the
/// work tree left behind by the previous process is **never** reused, because a
/// path that only works when the directory survived is a path a handover cannot
/// take (R26).
///
/// Running the execution on is the caller's business, as it is for
/// [`start_task`]: an execution the reconciliation found mid-action comes back
/// [`ExecutionStatus::Unknown`] and will not run until somebody looks at it.
pub fn takeover(
    store: &ExecutionStore,
    provisioner: &WorkspaceProvisioner,
    execution_id: &str,
) -> std::result::Result<WorkflowExecution, TakeoverError> {
    // D23, unchanged: an action in flight is not replayed, it is reported.
    crate::persistence::reconcile(store, execution_id)?;

    let mut execution = store.load_execution(execution_id)?;

    let bundle = store.paths().checkpoint_file(execution_id);
    if !bundle.is_file() {
        return Err(TakeoverError::NoCheckpoint {
            execution_id: execution_id.to_string(),
        });
    }

    // The work tree the previous process left behind is discarded, not reused
    // (R26). It is scratch: everything durable is in the checkpoint, and work
    // done after the last step boundary is the loss window this design already
    // accepts. Removing it is also what lets `provision` run at all, since it
    // refuses to write into a task directory that already exists.
    let task_dir = provisioner.paths().task_dir(execution_id);
    if task_dir.exists() {
        fs::remove_dir_all(&task_dir).map_err(|error| PersistenceError::io(&task_dir, error))?;
    }

    // Rebuild the checkout from the recorded descriptor, with this worker's own
    // credentials, then lay the delta over it.
    let request = takeover_request(&execution);
    let work_tree = match provision_workspace(provisioner, execution_id, request.as_ref()) {
        Ok(workspace) => PathBuf::from(workspace.path),
        Err(ProvisionFailure::Workspace(error)) => return Err(error.into()),
        Err(ProvisionFailure::Directory(error)) => {
            return Err(PersistenceError::io(provisioner.code_dir(execution_id), error).into())
        }
    };

    let base = match &execution.workspace.base_commit {
        Some(commit) => {
            checkout_commit(&work_tree, commit)?;
            Base::Commit(commit.clone())
        }
        None => Base::None,
    };

    materialise(MaterialiseRequest {
        work_tree: &work_tree,
        base,
        scratch: &store.paths().checkpoint_scratch(execution_id),
        bundle: &bundle,
    })?;

    // The record keeps its identity — id, repository, base commit — and only
    // learns where the work tree now lives.
    let path = work_tree.display().to_string();
    if let Some(info) = execution.context.workspace.as_mut() {
        info.path = path.clone();
    }
    execution.workspace.path = path;
    store.save_execution(&execution)?;

    Ok(execution)
}

/// The workspace to rebuild for a takeover, from the execution's own record.
///
/// A git checkout is cloned again from its recorded remote and ref. Anything
/// else — a copied local folder, or the fallback directory of a workflow with
/// no workspace — becomes the same empty fallback directory [`start_task`]
/// creates: there is no base to fetch for those, and the checkpoint
/// ([`Base::None`]) carries their whole content. Re-copying a local source
/// would not even be possible in general, since the folder it was copied from
/// lives on the machine that started the execution.
fn takeover_request(execution: &WorkflowExecution) -> Option<WorkspaceRequest> {
    let repository = execution.workspace.repository.as_ref()?;
    if repository.provider == "local" {
        return None;
    }
    Some(WorkspaceRequest::new(RepositorySpec::git(
        repository.name.clone(),
        repository.remote.clone(),
        execution.workspace.git_ref.clone(),
    )))
}

/// Checks the pinned base commit out in a fresh clone.
///
/// `provision` leaves the clone on a branch, which may have moved on since the
/// execution started, while the checkpoint delta is measured from the commit.
fn checkout_commit(work_tree: &Path, commit: &str) -> std::result::Result<(), TakeoverError> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(work_tree)
        .args(["checkout", "--quiet", "--detach", commit, "--"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(CheckpointError::Io)?;
    if !output.status.success() {
        return Err(CheckpointError::MissingBase {
            commit: commit.to_string(),
        }
        .into());
    }
    Ok(())
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
