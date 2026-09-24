//! Integration tests for the workspace lifecycle (U8).
//!
//! Everything here runs against a real temporary directory and real git
//! repositories built on the fly: the value of this layer is in actual file
//! permissions and actual checkouts, so nothing is mocked and nothing touches
//! the network.

use narsor::domain::{WorkflowExecution, WorkspaceType};
use narsor::workspace::{
    CleanupOutcome, CleanupPolicy, RepositorySpec, WorkspaceError, WorkspaceProvisioner,
    WorkspaceRequest,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-workspace-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn provisioner(&self) -> WorkspaceProvisioner {
        WorkspaceProvisioner::new(self.path.join("workspaces"))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        // Read-only repositories keep their directories unwritable, which would
        // defeat a plain recursive delete.
        restore_write_bits(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn restore_write_bits(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        let _ = fs::set_permissions(path, permissions);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                restore_write_bits(&entry.path());
            }
        }
    }
}

#[cfg(not(unix))]
fn restore_write_bits(_path: &Path) {}

/// Whether the filesystem actually refuses writes to an unwritable directory.
///
/// Running as root (or on a filesystem that ignores mode bits) makes the
/// read-only guard rail a no-op, and an assertion that a write fails would then
/// pass for the wrong reason. Probing is more honest than assuming.
#[cfg(unix)]
fn permission_bits_are_enforced(scratch: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let probe = scratch.join(format!("probe-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&probe).expect("create probe dir");
    let mut permissions = fs::metadata(&probe).expect("probe metadata").permissions();
    permissions.set_mode(0o555);
    fs::set_permissions(&probe, permissions).expect("chmod probe dir");
    let enforced = fs::write(probe.join("canary"), b"x").is_err();
    let mut permissions = fs::metadata(&probe).expect("probe metadata").permissions();
    permissions.set_mode(0o755);
    let _ = fs::set_permissions(&probe, permissions);
    let _ = fs::remove_dir_all(&probe);
    enforced
}

#[cfg(not(unix))]
fn permission_bits_are_enforced(_scratch: &Path) -> bool {
    false
}

/// Captures everything a scope logs, so that a warning can be asserted on.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }
}

impl io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `body` with a thread-local subscriber and returns what it logged.
fn capture_logs<T>(body: impl FnOnce() -> T) -> (T, String) {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let value = tracing::subscriber::with_default(subscriber, body);
    let logged = capture.contents();
    (value, logged)
}

// ---------------------------------------------------------------------------
// Git fixtures
// ---------------------------------------------------------------------------

/// Runs `git` inside `path` with a deterministic identity and fails loudly.
///
/// The identity is passed per invocation with `-c` instead of relying on the
/// machine's global git config: CI and fresh machines have none, and
/// `git commit` hard-fails without one. `GIT_TERMINAL_PROMPT=0` keeps a
/// misconfigured fixture from hanging on a prompt.
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.name=Workspace Test"])
        .args(["-c", "user.email=workspace@example.com"])
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

/// Creates a git repository with a deterministic identity and default branch.
fn init_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("create repo dir");
    git(path, &["-c", "init.defaultBranch=main", "init"]);
    path.to_path_buf()
}

/// Writes a file and commits it on the current HEAD.
fn commit_file(repo: &Path, name: &str, contents: &str, message: &str) {
    fs::write(repo.join(name), contents).expect("write file");
    git(repo, &["add", "--", name]);
    git(repo, &["commit", "--message", message]);
}

/// A repository whose default branch holds `base.txt` and whose `feature`
/// branch additionally holds `feature.txt`.
fn repo_with_feature_branch(path: &Path) -> PathBuf {
    let repo = init_repo(path);
    commit_file(&repo, "base.txt", "base\n", "base commit");
    let default_branch = git(&repo, &["symbolic-ref", "--short", "HEAD"]);

    git(&repo, &["checkout", "-b", "feature"]);
    commit_file(&repo, "feature.txt", "feature\n", "feature commit");

    git(&repo, &["checkout", "--force", &default_branch]);
    repo
}

/// Builds a throwaway execution record carrying a provisioned workspace.
fn execution_with(workspace: narsor::domain::Workspace) -> WorkflowExecution {
    WorkflowExecution {
        id: "exec-1".to_string(),
        workflow_id: "implement-feature".to_string(),
        workflow_version: 1,
        status: narsor::domain::ExecutionStatus::Running,
        current_state: None,
        started_at: chrono::Utc::now(),
        completed_at: None,
        inputs: serde_json::Value::Null,
        context: narsor::domain::Context::new(),
        workspace,
        current_action_execution: None,
        metadata: serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[test]
fn local_workspace_holds_the_source_contents_and_is_recorded_on_the_execution() {
    let temp = TempRoot::new();
    let source = temp.join("source");
    fs::create_dir_all(source.join("src")).expect("create source tree");
    fs::write(source.join("README.md"), "hello\n").expect("write readme");
    fs::write(source.join("src/main.rs"), "fn main() {}\n").expect("write source file");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &source));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");

    let repo_dir = provisioner.repository_dir("exec-1", "app");
    assert_eq!(provisioned.primary().path, repo_dir);
    assert_eq!(
        fs::read_to_string(repo_dir.join("README.md")).expect("read readme"),
        "hello\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("src/main.rs")).expect("read source file"),
        "fn main() {}\n"
    );
    assert!(!provisioned.primary().read_only);

    let execution = execution_with(provisioned.workspace.clone());
    assert!(matches!(
        execution.workspace.workspace_type,
        WorkspaceType::LocalFolder
    ));
    assert_eq!(execution.workspace.path, repo_dir.display().to_string());

    // The source must not be touched: the workspace is a copy, not a link.
    assert!(!source.join("src").join("written-by-agent").exists());
    fs::write(repo_dir.join("written-by-agent"), "x").expect("workspace is writable");
    assert!(!source.join("written-by-agent").exists());
}

#[test]
fn git_workspace_is_cloned_at_the_requested_ref() {
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    repo_with_feature_branch(&origin);

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        origin.display().to_string(),
        Some("feature".to_string()),
    ));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");

    let repo_dir = provisioned.primary().path.clone();
    assert_eq!(
        fs::read_to_string(repo_dir.join("feature.txt")).expect("feature file checked out"),
        "feature\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("base.txt")).expect("base file checked out"),
        "base\n"
    );

    assert_eq!(
        git(&repo_dir, &["symbolic-ref", "--short", "HEAD"]),
        "feature"
    );

    assert!(matches!(
        provisioned.workspace.workspace_type,
        WorkspaceType::GitCheckout
    ));
    assert_eq!(provisioned.workspace.git_ref.as_deref(), Some("feature"));
    assert_eq!(
        provisioned
            .workspace
            .repository
            .as_ref()
            .expect("repository info")
            .name,
        "app"
    );
}

#[test]
fn secondary_repository_is_readable_but_not_writable() {
    let temp = TempRoot::new();
    let reference = temp.join("reference");
    fs::create_dir_all(&reference).expect("create reference dir");
    fs::write(reference.join("NOTES.md"), "reference\n").expect("write notes");
    let primary = temp.join("primary");
    fs::create_dir_all(&primary).expect("create primary dir");
    fs::write(primary.join("main.rs"), "fn main() {}\n").expect("write primary file");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &primary))
        .with_secondary(RepositorySpec::local("docs", &reference));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");

    let docs = provisioned.repository("docs").expect("secondary present");
    assert!(docs.read_only);
    assert_eq!(
        fs::read_to_string(docs.path.join("NOTES.md")).expect("secondary is readable"),
        "reference\n"
    );

    if !permission_bits_are_enforced(temp.path()) {
        eprintln!(
            "skipping the write-refusal assertion: this process can write into an \
             unwritable directory (running as root, or a filesystem that ignores mode bits)"
        );
        return;
    }

    // The actual verification bar for U8: a script running inside the workspace
    // cannot create or modify a file in a read-only repository.
    let script = format!(
        "echo agent > '{}' ; echo agent >> '{}'",
        docs.path.join("new-file.txt").display(),
        docs.path.join("NOTES.md").display()
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("run script");
    assert!(
        !output.status.success(),
        "script unexpectedly wrote into the read-only repository"
    );
    assert!(!docs.path.join("new-file.txt").exists());
    assert_eq!(
        fs::read_to_string(docs.path.join("NOTES.md")).expect("notes still readable"),
        "reference\n"
    );

    // The primary repository stays writable for the same script.
    let primary_repo = provisioned.primary();
    let script = format!(
        "echo agent > '{}'",
        primary_repo.path.join("out.txt").display()
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("run script");
    assert!(output.status.success());
}

#[test]
fn cleanup_removes_the_workspace_after_the_execution_finishes() {
    let temp = TempRoot::new();
    let source = temp.join("source");
    fs::create_dir_all(&source).expect("create source");
    fs::write(source.join("file.txt"), "x").expect("write file");
    let reference = temp.join("reference");
    fs::create_dir_all(&reference).expect("create reference");
    fs::write(reference.join("notes.txt"), "x").expect("write notes");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &source))
        .with_secondary(RepositorySpec::local("docs", &reference));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");
    assert!(provisioned.task_dir.exists());

    let outcome = provisioner
        .cleanup("exec-1", CleanupPolicy::Remove)
        .expect("cleanup");

    assert!(outcome.is_removed(), "unexpected outcome: {outcome:?}");
    assert!(!provisioned.task_dir.exists());
    // The read-only secondary repository must not have survived the delete.
    assert!(!provisioner.repository_dir("exec-1", "docs").exists());
}

#[test]
fn preserve_leaves_the_workspace_untouched() {
    let temp = TempRoot::new();
    let source = temp.join("source");
    fs::create_dir_all(&source).expect("create source");
    fs::write(source.join("file.txt"), "keep me\n").expect("write file");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &source));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");

    let outcome = provisioner
        .cleanup("exec-1", CleanupPolicy::Preserve)
        .expect("cleanup");

    assert!(matches!(outcome, CleanupOutcome::Preserved { .. }));
    assert!(provisioned.task_dir.exists());
    assert_eq!(
        fs::read_to_string(provisioned.primary().path.join("file.txt")).expect("file survived"),
        "keep me\n"
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn concurrent_executions_of_the_same_workflow_get_disjoint_directories() {
    let temp = TempRoot::new();
    let source = temp.join("source");
    fs::create_dir_all(&source).expect("create source");
    fs::write(source.join("file.txt"), "shared\n").expect("write file");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &source));
    let first = provisioner.provision("exec-1", &request).expect("first");
    let second = provisioner.provision("exec-2", &request).expect("second");

    assert_ne!(first.task_dir, second.task_dir);
    assert_ne!(first.primary().path, second.primary().path);

    fs::write(first.primary().path.join("file.txt"), "first\n").expect("write into first");
    assert_eq!(
        fs::read_to_string(second.primary().path.join("file.txt")).expect("second untouched"),
        "shared\n"
    );

    // Provisioning twice for the same execution never silently overwrites.
    let error = provisioner
        .provision("exec-1", &request)
        .expect_err("collision must be refused");
    assert!(matches!(error, WorkspaceError::WorkspaceExists { .. }));
    assert_eq!(
        fs::read_to_string(first.primary().path.join("file.txt")).expect("first untouched"),
        "first\n"
    );
}

#[test]
fn cleaning_up_an_execution_without_a_workspace_succeeds() {
    let temp = TempRoot::new();
    let provisioner = temp.provisioner();

    let outcome = provisioner
        .cleanup("never-provisioned", CleanupPolicy::Remove)
        .expect("cleanup of a missing workspace must not error");

    assert!(matches!(outcome, CleanupOutcome::NotPresent { .. }));
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn a_missing_git_ref_is_reported_before_the_first_action_starts() {
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    repo_with_feature_branch(&origin);

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        origin.display().to_string(),
        Some("does-not-exist".to_string()),
    ));

    let error = provisioner
        .provision("exec-1", &request)
        .expect_err("missing ref must fail provisioning");

    match &error {
        WorkspaceError::RefNotFound {
            git_ref,
            repository,
            ..
        } => {
            assert_eq!(git_ref, "does-not-exist");
            assert_eq!(repository, "app");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let message = error.to_string();
    assert!(message.contains("does-not-exist"), "message: {message}");
    assert!(message.contains("app"), "message: {message}");

    // Nothing is left behind, so the execution can be retried with a valid ref.
    assert!(!provisioner.task_dir("exec-1").exists());
}

#[test]
fn an_unreachable_local_path_fails_validation_not_the_first_action() {
    let temp = TempRoot::new();
    let missing = temp.join("nowhere");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &missing));

    // `validate` is what execution creation calls; it fails without touching
    // the filesystem at all.
    let error = provisioner
        .validate(&request)
        .expect_err("missing source must fail validation");
    assert!(matches!(error, WorkspaceError::SourceNotFound { .. }));
    assert!(error.to_string().contains("nowhere"));

    let error = provisioner
        .provision("exec-1", &request)
        .expect_err("missing source must fail provisioning");
    assert!(matches!(error, WorkspaceError::SourceNotFound { .. }));
    assert!(!provisioner.task_dir("exec-1").exists());

    // A file where a directory was expected is just as unusable.
    let file = temp.join("a-file");
    fs::write(&file, "not a directory").expect("write file");
    let error = provisioner
        .validate(&WorkspaceRequest::new(RepositorySpec::local("app", &file)))
        .expect_err("file source must fail validation");
    assert!(matches!(error, WorkspaceError::SourceNotADirectory { .. }));
}

// ---------------------------------------------------------------------------
// Integration
// ---------------------------------------------------------------------------

#[test]
fn uncommitted_changes_block_cleanup_and_leave_a_warning() {
    let temp = TempRoot::new();
    let source = temp.join("source");
    init_repo(&source);
    commit_file(&source, "base.txt", "base\n", "base commit");

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &source));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");

    // Simulate an agent that produced work but never committed it.
    fs::write(provisioned.primary().path.join("agent-output.md"), "work\n")
        .expect("write agent output");

    let (outcome, logs) = capture_logs(|| {
        provisioner
            .cleanup("exec-1", CleanupPolicy::Remove)
            .expect("cleanup")
    });

    match &outcome {
        CleanupOutcome::Blocked { reason, .. } => {
            assert!(reason.contains("app"), "reason: {reason}");
            assert!(reason.contains("uncommitted"), "reason: {reason}");
        }
        other => panic!("cleanup should have been blocked, got: {other:?}"),
    }
    assert!(logs.contains("WARN"), "logs: {logs}");
    assert!(logs.contains("cleanup blocked"), "logs: {logs}");

    // The work itself survived.
    assert!(provisioned.task_dir.exists());
    assert_eq!(
        fs::read_to_string(provisioned.primary().path.join("agent-output.md"))
            .expect("agent output survived"),
        "work\n"
    );

    // Once the work is committed and there is nowhere to push it, cleanup runs.
    commit_file(
        &provisioned.primary().path,
        "agent-output.md",
        "work\n",
        "agent work",
    );
    let outcome = provisioner
        .cleanup("exec-1", CleanupPolicy::Remove)
        .expect("cleanup");
    assert!(outcome.is_removed(), "unexpected outcome: {outcome:?}");
}

#[test]
fn unpushed_commits_block_cleanup_of_a_cloned_repository() {
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    repo_with_feature_branch(&origin);

    let provisioner = temp.provisioner();
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        origin.display().to_string(),
        Some("feature".to_string()),
    ));
    let provisioned = provisioner
        .provision("exec-1", &request)
        .expect("provision");

    // A clone that is exactly at its upstream can be thrown away; one commit
    // ahead of it cannot.
    commit_file(
        &provisioned.primary().path,
        "agent-output.md",
        "work\n",
        "agent work",
    );

    let (outcome, logs) = capture_logs(|| {
        provisioner
            .cleanup("exec-1", CleanupPolicy::Remove)
            .expect("cleanup")
    });

    match &outcome {
        CleanupOutcome::Blocked { reason, .. } => {
            assert!(reason.contains("not pushed"), "reason: {reason}");
        }
        other => panic!("cleanup should have been blocked, got: {other:?}"),
    }
    assert!(logs.contains("cleanup blocked"), "logs: {logs}");
    assert!(provisioned.task_dir.exists());
}

// ---------------------------------------------------------------------------
// Argument safety and credential redaction
// ---------------------------------------------------------------------------

#[test]
fn a_remote_using_the_ext_transport_is_rejected_before_anything_is_written() {
    let root = TempRoot::new();
    let provisioner = WorkspaceProvisioner::new(root.path());
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        "ext::sh -c 'touch /tmp/pwned'",
        None,
    ));

    let error = provisioner
        .provision("exec-ext", &request)
        .expect_err("the ext:: transport must be refused");

    assert!(
        matches!(error, WorkspaceError::UnsafeRemote { .. }),
        "expected UnsafeRemote, got {error:?}"
    );
    assert!(
        !provisioner.task_dir("exec-ext").exists(),
        "a refused request must not leave a task directory behind"
    );
}

#[test]
fn a_remote_that_git_would_read_as_an_option_is_rejected() {
    let root = TempRoot::new();
    let provisioner = WorkspaceProvisioner::new(root.path());
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        "--upload-pack=touch /tmp/x",
        None,
    ));

    assert!(matches!(
        provisioner.validate(&request),
        Err(WorkspaceError::UnsafeRemote { .. })
    ));
}

#[test]
fn a_ref_that_git_would_read_as_an_option_is_rejected() {
    let root = TempRoot::new();
    let provisioner = WorkspaceProvisioner::new(root.path());
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        "https://example.invalid/app.git",
        Some("--output=/tmp/x".to_string()),
    ));

    assert!(matches!(
        provisioner.validate(&request),
        Err(WorkspaceError::UnsafeRef { .. })
    ));
}

#[test]
fn a_password_embedded_in_a_remote_never_reaches_the_error_message() {
    let root = TempRoot::new();
    let provisioner = WorkspaceProvisioner::new(root.path());
    let request = WorkspaceRequest::new(RepositorySpec::git(
        "app",
        "https://oauth2:glpat-SUPERSECRET@gitlab.invalid/acme/app.git",
        None,
    ));

    let error = provisioner
        .provision("exec-secret", &request)
        .expect_err("cloning an unreachable host must fail");

    let rendered = format!("{error} {error:?}");
    assert!(
        !rendered.contains("glpat-SUPERSECRET"),
        "the token leaked into the error: {rendered}"
    );
    assert!(
        rendered.contains("***"),
        "the redaction marker is missing: {rendered}"
    );
}
