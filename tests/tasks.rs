//! Tests for the shared "start a task" module (`src/tasks`).
//!
//! This is the operation both `narsor run` and the TUI's new-task modal
//! perform, so it is tested once, here, directly — the two callers' own tests
//! then only have to prove that they call it.
//!
//! # Nothing is written into the repository
//!
//! Every test builds its own temporary root under `/tmp`: the store, the task
//! directories, the workflow definitions and the git repositories that are
//! cloned all live inside it, and the whole tree is removed when the test's
//! [`TempRoot`] drops. The repositories are real, built on the fly with the
//! `git` CLI the way `tests/workspace.rs` builds them, because what is being
//! asserted is what actually ends up on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

use narsor::domain::{EventData, ExecutionStatus, WorkflowDefinition};
use narsor::engine::Engine;
use narsor::persistence::ExecutionStore;
use narsor::registry::{RepositoryEntry, RepositoryRegistry};
use narsor::tasks::{self, TaskError};
use narsor::workflow::load_from_file;
use narsor::workspace::{RepositorySource, RepositorySpec};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-tasks-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An engine over a fresh store in `root`.
fn engine_for(root: &Path) -> Engine {
    Engine::new(ExecutionStore::new(root))
}

/// Writes a workflow definition into the temporary root and loads it.
///
/// The definition is the smallest one that validates: a single human state, so
/// nothing is ever executed — these tests stop where `start_task` stops.
fn definition_with(
    root: &TempRoot,
    name: &str,
    inputs: &str,
    globals: &str,
) -> (WorkflowDefinition, PathBuf) {
    let yaml = format!(
        r#"
name: {name}
version: 1
description: >
  Fixture for the shared task-starting module, written into the test's
  temporary root so that nothing is ever added to the repository.

{inputs}
{globals}
states:
  review:
    description: A person confirms the task that was just created.
    action:
      type: human
      message: "Confirm?"
      inputs:
        - name: approval_status
          type: select
          required: true
          options:
            - approved
            - rejected
    outputs:
      - approved
      - rejected
    transitions:
      approved: done
      rejected: done

  done:
    description: The task was confirmed.

terminal_states:
  done: completed
"#
    );

    let path = root.join(&format!("{name}.yaml"));
    fs::write(&path, yaml).expect("write the definition");
    let definition = load_from_file(&path).expect("load the definition");
    (definition, path)
}

/// One declared `repository` input, for the workspace templates to expand.
const REPOSITORY_INPUT: &str = r#"inputs:
  - name: repository
    type: string
    required: true
    description: The repository the workspace is provisioned from.
"#;

/// The inputs a definition is started with.
fn inputs(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), Value::String(value.to_string())))
        .collect()
}

// ---------------------------------------------------------------------------
// git fixtures
// ---------------------------------------------------------------------------

/// Runs `git` inside `path` with a deterministic identity and fails loudly.
///
/// The identity is passed per invocation with `-c` rather than read from the
/// machine's config: CI and fresh machines have none, and `git commit` hard
/// fails without one.
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.name=Tasks Test"])
        .args(["-c", "user.email=tasks@example.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository whose `main` branch holds `base.txt` and whose `feature` branch
/// additionally holds `feature.txt`, left checked out on `main`.
fn repo_with_feature_branch(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("create repo dir");
    git(path, &["-c", "init.defaultBranch=main", "init"]);

    fs::write(path.join("base.txt"), "base\n").expect("write base");
    git(path, &["add", "--", "base.txt"]);
    git(path, &["commit", "--message", "base commit"]);
    let default_branch = git(path, &["symbolic-ref", "--short", "HEAD"]);

    git(path, &["checkout", "-b", "feature"]);
    fs::write(path.join("feature.txt"), "feature\n").expect("write feature");
    git(path, &["add", "--", "feature.txt"]);
    git(path, &["commit", "--message", "feature commit"]);

    git(path, &["checkout", "--force", &default_branch]);
    path.to_path_buf()
}

// ---------------------------------------------------------------------------
// the three shapes of a workspace
// ---------------------------------------------------------------------------

/// A definition declaring no `globals.workspace` gets the empty fallback
/// directory, and the definition path is recorded where every caller looks.
#[test]
fn a_definition_without_a_workspace_gets_the_empty_working_directory() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let (definition, file) = definition_with(&root, "tasks-no-workspace", "", "");

    let execution = tasks::start_task(root.path(), root.path(), &engine, &definition, &file, Map::new())
        .expect("start the task");

    let workspace = PathBuf::from(&execution.workspace.path);
    assert_eq!(
        workspace,
        root.path()
            .join("tasks")
            .join(&execution.id)
            .join("code")
            .join("workspace"),
        "the fallback directory is named for the execution"
    );
    assert!(workspace.is_dir(), "and it exists");
    assert_eq!(
        fs::read_dir(&workspace).expect("read it").count(),
        0,
        "and it is empty"
    );

    // The stored record carries the same workspace, and the definition path is
    // under the key both the CLI and the interface resolve executions by.
    let stored = engine
        .store()
        .load_execution(&execution.id)
        .expect("reload the execution");
    assert_eq!(stored.workspace.path, execution.workspace.path);
    assert_eq!(
        stored
            .context
            .workspace
            .as_ref()
            .expect("a workspace in context")
            .path,
        execution.workspace.path
    );
    assert_eq!(
        tasks::stored_definition_path(&stored),
        Some(fs::canonicalize(&file).expect("canonicalize"))
    );
    assert!(
        stored.metadata["cli"]["definition_path"].is_string(),
        "recorded under metadata.cli.definition_path: {}",
        stored.metadata
    );
}

/// A `local` workspace is the source directory, copied.
#[test]
fn a_local_workspace_is_copied_into_the_task_directory() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let source = root.join("source");
    fs::create_dir_all(source.join("src")).expect("create the source tree");
    fs::write(source.join("README.md"), "hello\n").expect("write readme");
    fs::write(source.join("src/main.rs"), "fn main() {}\n").expect("write source");

    let (definition, file) = definition_with(
        &root,
        "tasks-local",
        REPOSITORY_INPUT,
        "globals:\n  workspace:\n    type: local\n    path: \"{{ inputs.repository }}\"\n",
    );

    let execution = tasks::start_task(
        root.path(),
        root.path(),
        &engine,
        &definition,
        &file,
        inputs(&[("repository", &source.display().to_string())]),
    )
    .expect("start the task");

    let workspace = PathBuf::from(&execution.workspace.path);
    assert_eq!(
        workspace,
        root.path()
            .join("tasks")
            .join(&execution.id)
            .join("code")
            .join("source"),
        "the checkout is named after the source directory"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("README.md")).expect("read the copy"),
        "hello\n"
    );
    assert!(
        workspace.join("src/main.rs").is_file(),
        "the tree is copied"
    );
    // The copy is a copy: the source is untouched by the execution.
    assert!(source.join("README.md").is_file());
}

/// A `git` workspace naming a **registered** repository clones the registered
/// remote and honours the entry's `default_branch`.
///
/// This is the registry-first rule, and it is the reason this module exists at
/// all: it decides what `git` is asked to clone.
#[test]
fn a_registered_repository_is_cloned_from_its_remote_at_its_default_branch() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let remote = repo_with_feature_branch(&root.join("remote"));

    RepositoryRegistry::new(root.path())
        .add(
            RepositoryEntry::new("acme-api", remote.display().to_string())
                .with_default_branch("feature"),
        )
        .expect("register the repository");

    let (definition, file) = definition_with(
        &root,
        "tasks-registered",
        REPOSITORY_INPUT,
        "globals:\n  workspace:\n    type: git\n    repository: \"{{ inputs.repository }}\"\n",
    );

    let execution = tasks::start_task(
        root.path(),
        root.path(),
        &engine,
        &definition,
        &file,
        inputs(&[("repository", "acme-api")]),
    )
    .expect("start the task");

    let workspace = PathBuf::from(&execution.workspace.path);
    assert_eq!(
        workspace,
        root.path()
            .join("tasks")
            .join(&execution.id)
            .join("code")
            .join("acme-api"),
        "the checkout is named after the registered id, not after the remote"
    );
    assert!(
        workspace.join("feature.txt").is_file(),
        "the entry's default_branch was checked out, not the remote's HEAD"
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feature"
    );
}

/// A `git` workspace naming something the registry does not hold is still
/// cloned, as a raw remote — the behaviour that predates the registry.
#[test]
fn an_unregistered_repository_is_still_cloned_as_a_raw_remote() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let remote = repo_with_feature_branch(&root.join("unregistered"));

    let (definition, file) = definition_with(
        &root,
        "tasks-raw-remote",
        REPOSITORY_INPUT,
        "globals:\n  workspace:\n    type: git\n    repository: \"{{ inputs.repository }}\"\n",
    );

    let execution = tasks::start_task(
        root.path(),
        root.path(),
        &engine,
        &definition,
        &file,
        inputs(&[("repository", &remote.display().to_string())]),
    )
    .expect("start the task");

    let workspace = PathBuf::from(&execution.workspace.path);
    assert_eq!(
        workspace,
        root.path()
            .join("tasks")
            .join(&execution.id)
            .join("code")
            .join("unregistered"),
        "the checkout is named after the last segment of the remote"
    );
    assert!(
        workspace.join("base.txt").is_file(),
        "the remote's own default branch was checked out"
    );
    assert!(
        !workspace.join("feature.txt").exists(),
        "and nothing invented a branch for it"
    );
}

// ---------------------------------------------------------------------------
// what a refusal leaves behind
// ---------------------------------------------------------------------------

/// An input the workspace cannot be built from is refused before anything is
/// written: no execution, no task directory.
#[test]
fn a_rejected_input_leaves_nothing_on_disk() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let (definition, file) = definition_with(
        &root,
        "tasks-unknown-input",
        REPOSITORY_INPUT,
        "globals:\n  workspace:\n    type: local\n    path: \"{{ inputs.missing }}\"\n",
    );

    let error = tasks::start_task(
        root.path(),
        root.path(),
        &engine,
        &definition,
        &file,
        inputs(&[("repository", "unused")]),
    )
    .expect_err("the template cannot be expanded");

    assert!(
        matches!(error, TaskError::Template(_)),
        "the workspace template was rejected: {error}"
    );
    assert!(
        error
            .to_string()
            .starts_with("Could not expand the workspace template:"),
        "with the message the CLI prints: {error}"
    );
    assert_eq!(error.execution_id(), None, "nothing was created");
    assert_nothing_was_written(&root, &engine);
}

/// A workspace that cannot exist is refused the same way, by the pre-flight
/// validation that runs before the execution is created.
#[test]
fn a_workspace_that_cannot_be_created_is_refused_before_the_execution_exists() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let (definition, file) = definition_with(
        &root,
        "tasks-missing-source",
        REPOSITORY_INPUT,
        "globals:\n  workspace:\n    type: local\n    path: \"{{ inputs.repository }}\"\n",
    );

    let error = tasks::start_task(
        root.path(),
        root.path(),
        &engine,
        &definition,
        &file,
        inputs(&[("repository", &root.join("nowhere").display().to_string())]),
    )
    .expect_err("the source directory does not exist");

    assert!(
        matches!(error, TaskError::WorkspaceRejected(_)),
        "rejected by the pre-flight check: {error}"
    );
    assert_eq!(error.execution_id(), None, "nothing was created");
    assert_nothing_was_written(&root, &engine);
}

/// Provisioning that fails *after* the execution exists leaves it `Failed`
/// with an `Error` event — never `Running` with no directory to run in.
#[test]
fn a_failed_provisioning_leaves_the_execution_failed_with_an_error_event() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    // A remote that passes validation (it is neither an option nor `ext::`)
    // and that `git clone` cannot possibly resolve.
    let remote = root.join("no-such-repository");

    let (definition, file) = definition_with(
        &root,
        "tasks-clone-fails",
        REPOSITORY_INPUT,
        "globals:\n  workspace:\n    type: git\n    repository: \"{{ inputs.repository }}\"\n",
    );

    let error = tasks::start_task(
        root.path(),
        root.path(),
        &engine,
        &definition,
        &file,
        inputs(&[("repository", &remote.display().to_string())]),
    )
    .expect_err("the clone fails");

    let execution_id = error
        .execution_id()
        .expect("the failure names the execution it abandoned")
        .to_string();
    assert!(
        matches!(error, TaskError::Provision { .. }),
        "the clone failed: {error}"
    );

    let execution = engine
        .store()
        .load_execution(&execution_id)
        .expect("the execution is still in the store");
    assert_eq!(
        execution.status,
        ExecutionStatus::Failed,
        "an execution with no workspace is not left running"
    );
    assert!(execution.completed_at.is_some(), "and it is finished");

    let events = engine
        .store()
        .event_log(&execution_id)
        .read_all()
        .expect("read the event log");
    let error_event = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::Error { message, context } => Some((message.clone(), context.clone())),
            _ => None,
        })
        .expect("an Error event explains why");
    assert!(
        error_event.0.starts_with("Could not create the workspace:"),
        "the event carries the operator's message: {}",
        error_event.0
    );
    assert_eq!(error_event.1, json!({ "stage": "workspace_provisioning" }));

    // The half-provisioned tree was rolled back.
    assert!(
        !root.path().join("tasks").join(&execution_id).exists(),
        "no task directory is left behind"
    );
}

/// No execution and no task directory: the state of a root nothing reached.
fn assert_nothing_was_written(root: &TempRoot, engine: &Engine) {
    assert!(
        engine
            .store()
            .list_execution_ids()
            .expect("list the store")
            .is_empty(),
        "no execution was created"
    );
    assert!(
        !root.path().join("tasks").exists(),
        "no task directory was created"
    );
}

// ---------------------------------------------------------------------------
// resolve_repository, on its own
// ---------------------------------------------------------------------------

/// The registry wins over the raw-remote fallback, and a `ref` the workflow
/// declares wins over the entry's `default_branch`.
#[test]
fn resolve_repository_prefers_the_registry_and_lets_the_workflow_override_the_branch() {
    let root = TempRoot::new();
    RepositoryRegistry::new(root.path())
        .add(
            RepositoryEntry::new("acme-api", "https://example.test/acme/api.git")
                .with_default_branch("develop"),
        )
        .expect("register the repository");

    let registered =
        tasks::resolve_repository(root.path(), "acme-api", None).expect("resolve the id");
    assert_eq!(registered.name, "acme-api");
    assert_eq!(
        git_source(&registered),
        (
            "https://example.test/acme/api.git".to_string(),
            Some("develop".to_string())
        ),
        "the registered remote, at the entry's default branch"
    );

    let overridden = tasks::resolve_repository(root.path(), "acme-api", Some("release"))
        .expect("resolve the id");
    assert_eq!(git_source(&overridden).1, Some("release".to_string()));

    let raw = tasks::resolve_repository(root.path(), "https://example.test/other/thing.git", None)
        .expect("resolve the raw remote");
    assert_eq!(
        raw.name, "thing",
        "an unregistered value is a remote, named after its last segment"
    );
    assert_eq!(
        git_source(&raw),
        ("https://example.test/other/thing.git".to_string(), None),
        "and it is handed to git exactly as written"
    );
}

/// The remote and ref of a resolved git repository.
fn git_source(spec: &RepositorySpec) -> (String, Option<String>) {
    match &spec.source {
        RepositorySource::Git { remote, git_ref } => (remote.clone(), git_ref.clone()),
        other => panic!("expected a git repository, got {other:?}"),
    }
}
