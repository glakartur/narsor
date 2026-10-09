//! Smoke tests for the command-line interface (U9).
//!
//! Every test drives the real binary through `std::process::Command` and
//! asserts on its exit code and its output, because the exit codes are the
//! contract the CLI publishes (0 success, 1 failure, 2 usage).
//!
//! # Nothing is written into the repository
//!
//! Each test gets its own temporary root, passed with `--root` and also used as
//! the child process's working directory. The store, the workspaces and the
//! fallback workspace directory therefore all live under `/tmp`, and the root
//! is removed when the test's [`TempRoot`] is dropped. The workflow fixture
//! deliberately declares no `globals.workspace` and reaches its script through
//! an absolute path handed in as an input, so not even the script has a reason
//! to touch the checkout.
//!
//! `XDG_CONFIG_HOME` and `HOME` are redirected into that same directory, so the
//! workflow search path (`src/workflows`) can never reach — or write into — the
//! developer's own `~/.config`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use narsor::domain::Workspace;
use narsor::engine::Engine;
use narsor::persistence::{ConversationSource, ExecutionStore};
use narsor::workflow::load_from_file;

/// The exit code of a successful command.
const EXIT_SUCCESS: i32 = 0;

/// The exit code of a validation or execution failure.
const EXIT_FAILURE: i32 = 1;

/// The exit code of a wrong invocation.
const EXIT_USAGE: i32 = 2;

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        engine_support::kill_processes_under(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// What one invocation of the binary produced.
#[derive(Debug)]
struct Invocation {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Invocation {
    /// Both streams together, for assertions that do not care which one carried
    /// the text.
    fn output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Runs the built binary against `root`, with `root` as its working directory.
fn cli(root: &TempRoot, args: &[&str]) -> Invocation {
    cli_at(root.path(), args)
}

/// [`cli`], taking a bare path instead of a [`TempRoot`] — for a test that
/// has to run two invocations concurrently (from separate threads) against
/// the same root, which cannot each borrow the same `&TempRoot` across a
/// `thread::spawn`'s `'static` bound.
fn cli_at(root: &Path, args: &[&str]) -> Invocation {
    cli_on_path(root, args, &engine_support::fake_claude_first_on_path())
}

/// [`cli_at`] with `path` as the `PATH` of the process — and of the daemon
/// it may spawn, whose conductor runs `claude` from it.
fn cli_on_path(root: &Path, args: &[&str], path: &str) -> Invocation {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root)
        // Local workspaces (the fixtures' source repositories) live under the
        // test root, which is the one directory they may be taken from.
        .env("NARSOR_LOCAL_WORKSPACE_ROOTS", root)
        // The daemon this spawns runs the conductor at every human gate: on
        // the fake harness, never a real `claude`.
        .env("PATH", path)
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run the narsor binary");

    Invocation {
        code: output.status.code().expect("the process exited normally"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The `tests/fixtures` directory.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The smoke-test workflow: one script state, one human state.
fn greet_workflow() -> String {
    fixtures().join("cli/greet.yaml").display().to_string()
}

/// The script that workflow runs, as an absolute path.
fn announce_script() -> String {
    format!(
        "script_path={}",
        fixtures().join("cli/announce.sh").display()
    )
}

/// Starts the smoke-test workflow and returns the invocation and the new id.
fn start_greet(root: &TempRoot) -> (Invocation, String) {
    let invocation = cli(
        root,
        &[
            "run",
            &greet_workflow(),
            "--input",
            "greeting=Good morning",
            "--input",
            &announce_script(),
            "--input",
            "is_experimental=true",
        ],
    );
    let id = execution_id(&invocation.stdout);
    (invocation, id)
}

/// Pulls the execution id out of what `run` printed.
fn execution_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Created execution: "))
        .unwrap_or_else(|| panic!("`run` did not print an execution id; stdout was:\n{stdout}"))
        .trim()
        .to_string()
}

/// Finds the state root that owns a runner-projected execution.
fn runner_state_dir(root: &TempRoot, execution_id: &str) -> PathBuf {
    let storage = root.path().join("local-controller/storage");
    fs::read_dir(&storage)
        .expect("local task storage")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("state"))
        .find(|state| state.join("executions").join(execution_id).is_dir())
        .unwrap_or_else(|| panic!("no runner storage contains execution {execution_id}"))
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[test]
fn validate_confirms_a_correct_definition() {
    let root = TempRoot::new();
    let file = fixtures().join("implement_feature.yaml");

    let invocation = cli(&root, &["validate", &file.display().to_string()]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation.stdout.contains("The definition is valid"),
        "stdout was:\n{}",
        invocation.stdout
    );
    assert!(invocation.stdout.contains("implement-feature"));
}

#[test]
fn run_creates_an_execution_and_prints_its_id() {
    let root = TempRoot::new();

    let (invocation, id) = start_greet(&root);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(!id.is_empty());
    // The workflow stops at its human state, so the run needs input.
    assert!(
        invocation.stdout.contains("Status: input_needed"),
        "stdout was:\n{}",
        invocation.stdout
    );
    assert!(runner_state_dir(&root, &id)
        .join("executions")
        .join(&id)
        .is_dir());
}

#[test]
fn list_shows_the_created_execution_with_its_status() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["list"]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    let line = invocation
        .stdout
        .lines()
        .find(|line| line.contains(&id))
        .unwrap_or_else(|| panic!("`list` did not mention {id}:\n{}", invocation.stdout));
    assert!(line.contains("input_needed"), "line was: {line}");
    assert!(line.contains("cli-greet"), "line was: {line}");
}

#[test]
fn show_prints_state_status_and_context_variables() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["show", &id]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    let stdout = invocation.stdout;
    assert!(
        stdout.contains("Status:        input_needed"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Current state: approve"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("Context variables:"), "stdout:\n{stdout}");
    // Set by the script through its JSON result, merged into the context by the
    // engine.
    assert!(
        stdout.contains(r#"announced = "Good morning""#),
        "stdout:\n{stdout}"
    );
}

#[test]
fn input_delivers_an_answer_and_resumes_the_execution() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(
        &root,
        &["input", &id, "--field", "approval_status=approved"],
    );

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation.stdout.contains("Status: completed"),
        "stdout:\n{}",
        invocation.stdout
    );

    let shown = cli(&root, &["show", &id]);
    assert!(shown.stdout.contains("Status:        completed"));
    assert!(shown.stdout.contains("Current state: completed"));
}

#[test]
fn events_lists_the_log_in_sequence_order() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["events", &id]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    let types: Vec<&str> = invocation
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .collect();
    assert_eq!(types.first(), Some(&"workflow_started"), "{types:?}");
    assert!(types.contains(&"human_input_requested"), "{types:?}");
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn list_without_any_executions_prints_an_empty_list() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["list"]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation.stdout.contains("No executions."),
        "stdout:\n{}",
        invocation.stdout
    );
}

#[test]
fn a_boolean_input_reaches_the_context_as_a_boolean() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["show", &id]);

    // `true`, not `"true"`: the value was parsed according to the declared type.
    assert!(
        invocation.stdout.contains("is_experimental = true"),
        "stdout:\n{}",
        invocation.stdout
    );
    assert!(!invocation.stdout.contains(r#"is_experimental = "true""#));

    let stored: serde_json::Value = serde_json::from_slice(
        &fs::read(
            runner_state_dir(&root, &id)
                .join("executions")
                .join(&id)
                .join("context.json"),
        )
        .expect("read context.json"),
    )
    .expect("parse context.json");
    assert_eq!(
        stored["variables"]["is_experimental"],
        serde_json::Value::Bool(true)
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn validate_reports_every_violation_of_a_broken_definition() {
    let root = TempRoot::new();
    let file = fixtures().join("invalid/five_violations.yaml");

    let invocation = cli(&root, &["validate", &file.display().to_string()]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    let listed = invocation
        .output()
        .lines()
        .filter(|line| line.trim_start().starts_with("- "))
        .count();
    assert_eq!(
        listed,
        5,
        "expected all five violations, got:\n{}",
        invocation.output()
    );
}

#[test]
fn run_without_a_required_input_is_a_usage_error() {
    let root = TempRoot::new();

    let invocation = cli(
        &root,
        &["run", &greet_workflow(), "--input", &announce_script()],
    );

    assert_eq!(invocation.code, EXIT_USAGE, "stdout: {}", invocation.stdout);
    assert!(
        invocation.output().contains("greeting"),
        "output:\n{}",
        invocation.output()
    );
    // Nothing was created.
    assert!(!root.path().join("executions").exists());
}

#[test]
fn an_unknown_input_is_a_usage_error() {
    let root = TempRoot::new();

    let invocation = cli(
        &root,
        &[
            "run",
            &greet_workflow(),
            "--input",
            "greeting=Hej",
            "--input",
            &announce_script(),
            "--input",
            "unknown_input=1",
        ],
    );

    assert_eq!(invocation.code, EXIT_USAGE);
    assert!(invocation.output().contains("unknown_input"));
}

#[test]
fn a_missing_command_line_argument_is_a_usage_error() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["show"]);

    assert_eq!(
        invocation.code, EXIT_USAGE,
        "clap's own code must match ours"
    );
}

#[test]
fn show_for_an_unknown_id_is_a_readable_failure() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["show", "no-such-execution"]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    assert!(
        invocation.output().contains("no-such-execution"),
        "output:\n{}",
        invocation.output()
    );
}

#[test]
fn input_for_an_execution_that_is_not_waiting_explains_the_status() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let answered = cli(
        &root,
        &["input", &id, "--field", "approval_status=approved"],
    );
    assert_eq!(answered.code, EXIT_SUCCESS, "stderr: {}", answered.stderr);

    let again = cli(
        &root,
        &["input", &id, "--field", "approval_status=approved"],
    );

    assert_eq!(again.code, EXIT_FAILURE);
    assert!(
        again.output().contains("completed"),
        "the message must name the status; output:\n{}",
        again.output()
    );
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[test]
fn validate_run_input_show_is_one_uninterrupted_path() {
    let root = TempRoot::new();

    let validated = cli(&root, &["validate", &greet_workflow()]);
    assert_eq!(validated.code, EXIT_SUCCESS, "stderr: {}", validated.stderr);

    let (started, id) = start_greet(&root);
    assert_eq!(started.code, EXIT_SUCCESS, "stderr: {}", started.stderr);
    assert!(started.stdout.contains("Human input expected"));

    let answered = cli(
        &root,
        &["input", &id, "--field", "approval_status=approved"],
    );
    assert_eq!(answered.code, EXIT_SUCCESS, "stderr: {}", answered.stderr);

    let shown = cli(&root, &["show", &id]);
    assert_eq!(shown.code, EXIT_SUCCESS, "stderr: {}", shown.stderr);
    assert!(shown.stdout.contains("Status:        completed"));

    // The whole run happened under the temporary root and nowhere else.
    assert!(runner_state_dir(&root, &id)
        .parent()
        .expect("runner state has task storage parent")
        .join("workspace")
        .is_dir());
    assert!(!Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".narsor")
        .exists());
}

// ---------------------------------------------------------------------------
// The repository registry
// ---------------------------------------------------------------------------

/// The registry fixture workflow: one script state, a templated git workspace.
fn clone_workflow() -> String {
    fixtures().join("cli/clone.yaml").display().to_string()
}

/// Runs `git` inside `path` with a deterministic identity and fails loudly.
///
/// The identity is passed per invocation with `-c` rather than read from the
/// machine's global config: CI has none, and `git commit` hard-fails without
/// one. `GIT_TERMINAL_PROMPT=0` keeps a misconfigured fixture from hanging.
fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.name=CLI Test"])
        .args(["-c", "user.email=cli@example.com"])
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
}

/// Builds a real git repository under the temp root, holding one marker file.
///
/// Returns its absolute path: the value that is either registered as a remote
/// or handed to the workflow raw, depending on which path is under test.
fn source_repo(root: &TempRoot, name: &str, marker: &str) -> PathBuf {
    let path = root.path().join("sources").join(name);
    fs::create_dir_all(&path).expect("create the source repo dir");
    git(&path, &["-c", "init.defaultBranch=main", "init"]);
    fs::write(path.join("marker.txt"), marker).expect("write the marker file");
    git(&path, &["add", "--", "marker.txt"]);
    git(&path, &["commit", "--message", "add the marker"]);
    path
}

#[test]
fn repo_add_then_list_shows_the_registered_repository() {
    let root = TempRoot::new();

    let added = cli(
        &root,
        &[
            "repo",
            "add",
            "acme-api",
            "--remote",
            "https://github.com/acme/api.git",
            "--name",
            "ACME API",
            "--provider",
            "github",
            "--default-branch",
            "main",
            "--description",
            "The public API service.",
        ],
    );
    assert_eq!(added.code, EXIT_SUCCESS, "stderr: {}", added.stderr);

    let listed = cli(&root, &["repo", "list"]);

    assert_eq!(listed.code, EXIT_SUCCESS, "stderr: {}", listed.stderr);
    let line = listed
        .stdout
        .lines()
        .find(|line| line.starts_with("acme-api"))
        .unwrap_or_else(|| panic!("`repo list` did not mention it:\n{}", listed.stdout));
    assert!(line.contains("github"), "line was: {line}");
    assert!(line.contains("main"), "line was: {line}");
    assert!(
        line.contains("https://github.com/acme/api.git"),
        "line was: {line}"
    );
}

#[test]
fn repo_list_on_an_empty_registry_is_not_an_error() {
    let root = TempRoot::new();

    let listed = cli(&root, &["repo", "list"]);

    assert_eq!(listed.code, EXIT_SUCCESS, "stderr: {}", listed.stderr);
    assert!(
        listed.stdout.contains("No repositories registered."),
        "stdout:\n{}",
        listed.stdout
    );
}

#[test]
fn repo_show_prints_every_field() {
    let root = TempRoot::new();
    let added = cli(
        &root,
        &[
            "repo",
            "add",
            "acme-api",
            "--remote",
            "https://github.com/acme/api.git",
            "--name",
            "ACME API",
            "--provider",
            "github",
            "--default-branch",
            "main",
            "--description",
            "The public API service.",
        ],
    );
    assert_eq!(added.code, EXIT_SUCCESS, "stderr: {}", added.stderr);

    let shown = cli(&root, &["repo", "show", "acme-api"]);

    assert_eq!(shown.code, EXIT_SUCCESS, "stderr: {}", shown.stderr);
    let stdout = shown.stdout;
    assert!(stdout.contains("Repository:     acme-api"), "{stdout}");
    assert!(stdout.contains("Name:           ACME API"), "{stdout}");
    assert!(stdout.contains("Provider:       github"), "{stdout}");
    assert!(
        stdout.contains("Remote:         https://github.com/acme/api.git"),
        "{stdout}"
    );
    assert!(stdout.contains("Default branch: main"), "{stdout}");
    assert!(
        stdout.contains("Description:    The public API service."),
        "{stdout}"
    );
}

#[test]
fn repo_remove_drops_the_repository() {
    let root = TempRoot::new();
    let added = cli(
        &root,
        &["repo", "add", "acme-api", "--remote", "/srv/git/api"],
    );
    assert_eq!(added.code, EXIT_SUCCESS, "stderr: {}", added.stderr);

    let removed = cli(&root, &["repo", "remove", "acme-api"]);
    assert_eq!(removed.code, EXIT_SUCCESS, "stderr: {}", removed.stderr);

    let listed = cli(&root, &["repo", "list"]);
    assert!(
        listed.stdout.contains("No repositories registered."),
        "stdout:\n{}",
        listed.stdout
    );
    assert_eq!(cli(&root, &["repo", "show", "acme-api"]).code, EXIT_FAILURE);
}

#[test]
fn repo_show_for_an_unknown_id_is_a_readable_failure() {
    let root = TempRoot::new();

    let shown = cli(&root, &["repo", "show", "no-such-repository"]);

    assert_eq!(shown.code, EXIT_FAILURE);
    assert!(
        shown.output().contains("no-such-repository"),
        "output:\n{}",
        shown.output()
    );
}

#[test]
fn repo_remove_of_an_unknown_id_is_a_readable_failure() {
    let root = TempRoot::new();

    let removed = cli(&root, &["repo", "remove", "no-such-repository"]);

    assert_eq!(removed.code, EXIT_FAILURE);
    assert!(
        removed.output().contains("no-such-repository"),
        "output:\n{}",
        removed.output()
    );
}

#[test]
fn repo_add_with_a_duplicate_id_is_a_failure() {
    let root = TempRoot::new();
    let first = cli(
        &root,
        &["repo", "add", "acme-api", "--remote", "/srv/git/api"],
    );
    assert_eq!(first.code, EXIT_SUCCESS, "stderr: {}", first.stderr);

    let second = cli(
        &root,
        &["repo", "add", "acme-api", "--remote", "/srv/git/other"],
    );

    assert_eq!(second.code, EXIT_FAILURE);
    assert!(
        second.output().contains("acme-api"),
        "output:\n{}",
        second.output()
    );
    // The first registration survived the refusal.
    let shown = cli(&root, &["repo", "show", "acme-api"]);
    assert!(
        shown.stdout.contains("/srv/git/api"),
        "stdout:\n{}",
        shown.stdout
    );
}

#[test]
fn repo_add_without_the_required_remote_is_a_usage_error() {
    let root = TempRoot::new();

    let added = cli(&root, &["repo", "add", "acme-api"]);

    assert_eq!(added.code, EXIT_USAGE, "clap's own code must match ours");
}

#[test]
fn repo_add_refuses_a_remote_git_would_read_as_a_command() {
    let root = TempRoot::new();

    let added = cli(
        &root,
        &["repo", "add", "hostile", "--remote", "ext::sh -c whoami"],
    );

    assert_eq!(added.code, EXIT_FAILURE);
    assert!(
        added.output().contains("ext::"),
        "the message must say why; output:\n{}",
        added.output()
    );
}

#[test]
fn a_workflow_naming_a_registered_id_provisions_from_the_registered_remote() {
    let root = TempRoot::new();
    let registered = source_repo(&root, "registered", "from the registry");

    let added = cli(
        &root,
        &[
            "repo",
            "add",
            "acme-api",
            "--remote",
            &registered.display().to_string(),
            "--provider",
            "local",
            "--default-branch",
            "main",
        ],
    );
    assert_eq!(added.code, EXIT_SUCCESS, "stderr: {}", added.stderr);

    let invocation = cli(
        &root,
        &[
            "run",
            &clone_workflow(),
            "--input",
            "repository=acme-api",
            "--input",
            &announce_script(),
        ],
    );
    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stdout: {}\nstderr: {}",
        invocation.stdout, invocation.stderr
    );
    let _id = execution_id(&invocation.stdout);

    // Runner-owned task storage is authoritative after scheduler admission.
    // The marker proves the registered remote, not the raw input, was cloned.
    let checkout = task_workspace_with_marker(&root, "from the registry");
    assert!(checkout.is_dir(), "expected a checkout at {checkout:?}");
    assert_eq!(
        fs::read_to_string(checkout.join("marker.txt")).expect("read the marker"),
        "from the registry"
    );
}

#[test]
fn a_workflow_naming_an_unregistered_raw_path_still_provisions_as_before() {
    let root = TempRoot::new();
    let raw = source_repo(&root, "raw-repo", "from the raw path");

    let invocation = cli(
        &root,
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", raw.display()),
            "--input",
            &announce_script(),
        ],
    );
    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stdout: {}\nstderr: {}",
        invocation.stdout, invocation.stderr
    );
    let _id = execution_id(&invocation.stdout);

    // Nothing was registered, so the raw local remote was cloned by the
    // admitted runner into its own task storage.
    let checkout = task_workspace_with_marker(&root, "from the raw path");
    assert!(checkout.is_dir(), "expected a checkout at {checkout:?}");
    assert_eq!(
        fs::read_to_string(checkout.join("marker.txt")).expect("read the marker"),
        "from the raw path"
    );
}

/// AE1: a checkout slower than the API client's 800 ms request timeout is
/// no longer on the request path, so `run` acknowledges the queued task and
/// finishes normally instead of reporting a lost daemon.
#[test]
fn a_checkout_slower_than_the_request_timeout_is_not_a_lost_daemon() {
    let root = TempRoot::new();
    let raw = source_repo(&root, "slow-repo", "from the slow checkout");

    // A `git` that stalls only while cloning, then hands over to the real one.
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_string();
    let shim_dir = root.path().join("shim");
    fs::create_dir_all(&shim_dir).expect("create the shim dir");
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" clone \"*) sleep 2;; esac\nexec {real_git} \"$@\"\n"
        ),
    )
    .expect("write the git shim");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("make the shim runnable");
    let path = format!(
        "{}:{}",
        shim_dir.display(),
        engine_support::fake_claude_first_on_path()
    );

    let invocation = cli_on_path(
        root.path(),
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", raw.display()),
            "--input",
            &announce_script(),
        ],
        &path,
    );

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stdout: {}\nstderr: {}",
        invocation.stdout, invocation.stderr
    );
    assert!(
        !invocation.output().to_lowercase().contains("unreachable"),
        "a slow checkout is not a lost daemon; output:\n{}",
        invocation.output()
    );
    let _id = execution_id(&invocation.stdout);
}

/// An admitted task whose workspace cannot be provisioned fails before any
/// execution exists: `run` must end nonzero from the task-side failure
/// instead of waiting for an execution projection that cannot arrive.
#[test]
fn run_ends_nonzero_when_admitted_provisioning_fails_before_an_execution_exists() {
    let root = TempRoot::new();
    let missing = root.path().join("sources").join("does-not-exist");

    let invocation = cli(
        &root,
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", missing.display()),
            "--input",
            &announce_script(),
        ],
    );

    assert_eq!(
        invocation.code, EXIT_FAILURE,
        "stdout: {}\nstderr: {}",
        invocation.stdout, invocation.stderr
    );
    assert!(
        !invocation.stdout.contains("Created execution:"),
        "no execution may be invented; stdout:\n{}",
        invocation.stdout
    );
}

fn task_workspace_with_marker(root: &TempRoot, marker: &str) -> PathBuf {
    let storage = root.path().join("local-controller/storage");
    fs::read_dir(&storage)
        .expect("local task storage")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("workspace"))
        .find(|workspace| {
            fs::read_to_string(workspace.join("marker.txt")).is_ok_and(|value| value == marker)
        })
        .unwrap_or_else(|| panic!("no task workspace contains marker {marker:?}"))
}

// ---------------------------------------------------------------------------
// resolving a workflow by name (U12)
// ---------------------------------------------------------------------------

/// `validate` takes a built-in name, not only a path.
#[test]
fn validate_accepts_a_builtin_by_name() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["validate", "compound-engineering"]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation.stdout.contains("The definition is valid"),
        "stdout:\n{}",
        invocation.stdout
    );
}

/// A name in `{root}/workflows` is what `run` resolves, and the execution
/// records that name beside the path.
#[test]
fn run_resolves_a_name_from_the_root_workflow_directory() {
    let root = TempRoot::new();
    let directory = root.path().join("workflows");
    fs::create_dir_all(&directory).expect("create the workflow directory");
    fs::copy(
        fixtures().join("cli/greet.yaml"),
        directory.join("greet.yaml"),
    )
    .expect("install the fixture under its name");

    let invocation = cli(
        &root,
        &[
            "run",
            "greet",
            "--input",
            "greeting=Good morning",
            "--input",
            &announce_script(),
        ],
    );

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation.stderr.contains("root"),
        "it says the definition came from the data directory: {}",
        invocation.stderr
    );

    let id = execution_id(&invocation.stdout);
    let state = fs::read_to_string(
        runner_state_dir(&root, &id)
            .join("executions")
            .join(&id)
            .join("state.json"),
    )
    .expect("read the stored execution");
    assert!(
        !state.contains("definition_path"),
        "runner executions rely on the sealed task definition, not a mutable path: {state}"
    );
    let specs = runner_state_dir(&root, &id).join("launch-specs");
    let snapshot = fs::read_to_string(
        fs::read_dir(specs)
            .expect("runner launch specs")
            .find_map(Result::ok)
            .expect("one sealed spec")
            .path(),
    )
    .expect("read sealed spec");
    assert!(
        snapshot.contains("\"workflow_id\": \"cli-greet\""),
        "{snapshot}"
    );

    // And it can be answered from a second process, as always.
    let answered = cli(
        &root,
        &["input", &id, "--field", "approval_status=approved"],
    );
    assert_eq!(answered.code, EXIT_SUCCESS, "stderr: {}", answered.stderr);
}

/// An unknown workflow is a usage error, with the built-ins named.
#[test]
fn run_with_an_unknown_workflow_name_is_a_usage_error() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["run", "nothing-of-that-name"]);

    assert_eq!(invocation.code, EXIT_USAGE, "stdout: {}", invocation.stdout);
    assert!(
        invocation.stderr.contains("compound-engineering")
            && invocation.stderr.contains("open-mercato-sdlc"),
        "the message names the built-ins: {}",
        invocation.stderr
    );
}

// ---------------------------------------------------------------------------
// steps / conversation (U8)
// ---------------------------------------------------------------------------

/// The `cli-flaky` fixture: a script action killed by signal on its first
/// attempt, succeeding on its second, under a fast (`backoff: 0s`) retry
/// policy — exactly the shape `narsor steps` needs to show more than one
/// attempt of the same state.
fn flaky_workflow() -> String {
    fixtures().join("cli/flaky.yaml").display().to_string()
}

/// Starts the flaky workflow and returns the invocation and the new id.
fn start_flaky(root: &TempRoot) -> (Invocation, String) {
    let script = fixtures().join("scripts/flaky_crash.sh");
    let counter = root.path().join("attempts.txt");
    let invocation = cli(
        root,
        &[
            "run",
            &flaky_workflow(),
            "--input",
            &format!("script_path={}", script.display()),
            "--input",
            &format!("counter_path={}", counter.display()),
        ],
    );
    let id = execution_id(&invocation.stdout);
    (invocation, id)
}

#[test]
fn steps_shows_one_line_per_attempt_with_its_status() {
    let root = TempRoot::new();
    let (invocation, id) = start_flaky(&root);
    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation.stdout.contains("Status: completed"),
        "stdout:\n{}",
        invocation.stdout
    );

    let steps = cli(&root, &["steps", &id]);

    assert_eq!(steps.code, EXIT_SUCCESS, "stderr: {}", steps.stderr);
    let first = steps
        .stdout
        .lines()
        .find(|line| line.starts_with("build #1"))
        .unwrap_or_else(|| panic!("no 'build #1' line:\n{}", steps.stdout));
    assert!(first.contains("failed"), "line was: {first}");
    let second = steps
        .stdout
        .lines()
        .find(|line| line.starts_with("build #2"))
        .unwrap_or_else(|| panic!("no 'build #2' line:\n{}", steps.stdout));
    assert!(second.contains("succeeded"), "line was: {second}");
}

#[test]
fn steps_for_an_unknown_execution_is_a_readable_failure() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["steps", "no-such-execution"]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    assert!(
        invocation.output().contains("no-such-execution"),
        "output:\n{}",
        invocation.output()
    );
}

/// `narsor conversation <id> <attempt>` reads through
/// [`narsor::persistence::ConversationStore`] the same way the embedded API's
/// handler does — directly against the store, without an HTTP round trip.
/// This test writes a conversation record by hand for an arbitrary attempt id
/// alongside a real execution (nothing ties a conversation file's name to any
/// event; it is looked up purely by the id given on the command line), then
/// checks every entry that was written comes back, in order, in full.
#[test]
fn conversation_prints_every_entry_in_order() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let store = ExecutionStore::new(runner_state_dir(&root, &id));
    let conversations = store.conversation_store(&id);
    let mut writer = conversations.writer("attempt-1").expect("open the writer");
    writer
        .append_narsor("prompt: do the thing")
        .expect("write the prompt entry");
    writer
        .append(
            ConversationSource::Stdout,
            br#"{"type":"result","subtype":"success","result":"done"}"#,
        )
        .expect("write the stdout entry");
    writer
        .append(ConversationSource::Stderr, b"a warning on stderr")
        .expect("write the stderr entry");
    writer.finish().expect("flush the writer");

    let invocation = cli(&root, &["conversation", &id, "attempt-1"]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    let lines: Vec<&str> = invocation.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "stdout:\n{}", invocation.stdout);
    assert!(
        lines[0].contains("prompt: do the thing"),
        "line was: {}",
        lines[0]
    );
    assert!(lines[1].contains("done"), "line was: {}", lines[1]);
    assert!(
        lines[2].contains("a warning on stderr"),
        "line was: {}",
        lines[2]
    );
}

#[test]
fn conversation_for_an_unknown_attempt_is_a_readable_failure() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["conversation", &id, "no-such-attempt"]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    assert!(
        invocation.output().contains("no-such-attempt"),
        "output:\n{}",
        invocation.output()
    );
}

// ---------------------------------------------------------------------------
// resume (U6): now a REST client of a daemon, not a direct `Engine` call
// ---------------------------------------------------------------------------

/// Writes a script-to-terminal workflow, starts it directly against the
/// store (mirroring `tests/api.rs`'s own `write_script_to_done_workflow` /
/// `post_resume_drives_a_paused_execution_to_a_terminal_state` fixtures —
/// there is no CLI-reachable way to pause an execution that is not waiting
/// on a human answer, since nothing exposes `Engine::pause_now` on the
/// command line), then parks it with `pause_now` and pins its definition
/// path the way `tasks::start_task` records it — exactly what `resume`
/// needs to find the definition again. No daemon exists yet at this point:
/// the CLI's own `resume` invocation is what discovers none running and
/// launches one (AE2), against the very same store this just wrote into.
///
/// The script sleeps briefly rather than exiting immediately, so that a test
/// racing two `resume` calls against this execution has a wide, reliable
/// window in which the execution is genuinely `Running` (and so the second
/// caller reliably observes `EngineError::AlreadyRunning`/`409`, not merely
/// "already completed").
fn pause_a_fresh_execution(root: &TempRoot) -> String {
    let script_path = root.path().join("succeed-slowly.sh");
    fs::write(&script_path, "#!/bin/sh\nsleep 0.3\nexit 0\n").expect("write script");
    let mut permissions = fs::metadata(&script_path)
        .expect("stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod script");

    let yaml = format!(
        r#"
name: cli-resume-fixture
version: 1
description: Minimal script-to-terminal fixture for the CLI's resume test.

states:
  start:
    description: Runs a script that always succeeds, slowly.
    action:
      type: script
      command: "{command}"
    outputs:
      - success
    transitions:
      success: done

  done:
    description: Terminal.

terminal_states:
  done: completed
"#,
        command = script_path.display()
    );
    let definition_path = root.path().join("cli-resume-fixture.yaml");
    fs::write(&definition_path, yaml).expect("write definition");
    let definition = load_from_file(&definition_path).expect("load definition");

    let store = ExecutionStore::new(root.path());
    let engine = Engine::new(store.clone());
    let workspace_path = root.path().join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    engine
        .pause_now(&execution.id, "paused for the CLI resume test")
        .expect("pause the execution");

    let mut reloaded = store
        .load_execution(&execution.id)
        .expect("reload execution");
    reloaded.metadata = serde_json::json!({
        "cli": { "definition_path": definition_path.display().to_string() }
    });
    store
        .save_execution(&reloaded)
        .expect("pin the definition path");

    execution.id
}

#[test]
fn resume_drives_a_paused_execution_to_a_terminal_state() {
    let root = TempRoot::new();
    let id = pause_a_fresh_execution(&root);

    let invocation = cli(&root, &["resume", &id]);

    assert_eq!(
        invocation.code, EXIT_SUCCESS,
        "stderr: {}",
        invocation.stderr
    );
    assert!(
        invocation
            .stdout
            .contains(&format!("Resumed execution {id}")),
        "stdout:\n{}",
        invocation.stdout
    );
    assert!(
        invocation.stdout.contains("Status: completed"),
        "stdout:\n{}",
        invocation.stdout
    );

    let shown = cli(&root, &["show", &id]);
    assert!(shown.stdout.contains("Status:        completed"));
}

/// `--file` on `resume` is refused, not silently ignored — the daemon has no
/// way to honour a local override path (see `resume`'s own doc comment).
#[test]
fn resume_with_file_override_is_a_usage_error() {
    let root = TempRoot::new();
    let id = pause_a_fresh_execution(&root);

    let invocation = cli(&root, &["resume", &id, "--file", "some-workflow"]);

    assert_eq!(invocation.code, EXIT_USAGE, "stdout: {}", invocation.stdout);
    assert!(
        invocation.output().contains("--file"),
        "output:\n{}",
        invocation.output()
    );
}

/// AE4-shaped: two drivers race to resume the same execution — here, two
/// `narsor resume` invocations from separate processes instead of `run`
/// from one and the TUI from another (the TUI itself is not migrated until
/// U7), which exercises exactly the same underlying guard
/// (`Engine::begin_run`/`active_runs`, R11) through the same REST path.
/// Exactly one must succeed; the other must fail readably — the daemon's
/// `409` must not reach the operator as a raw HTTP/JSON dump.
#[test]
fn two_concurrent_resumes_of_the_same_execution_exactly_one_succeeds() {
    let root = TempRoot::new();
    let id = pause_a_fresh_execution(&root);
    let root_path = root.path().to_path_buf();

    let path_a = root_path.clone();
    let id_a = id.clone();
    let thread_a = std::thread::spawn(move || cli_at(&path_a, &["resume", &id_a]));

    let path_b = root_path.clone();
    let id_b = id.clone();
    let thread_b = std::thread::spawn(move || cli_at(&path_b, &["resume", &id_b]));

    let result_a = thread_a.join().expect("resume thread a");
    let result_b = thread_b.join().expect("resume thread b");

    let successes = [&result_a, &result_b]
        .into_iter()
        .filter(|invocation| invocation.code == EXIT_SUCCESS)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one resume must succeed: a={result_a:?} b={result_b:?}"
    );

    let failure = if result_a.code != EXIT_SUCCESS {
        &result_a
    } else {
        &result_b
    };
    assert_eq!(failure.code, EXIT_FAILURE, "a={result_a:?} b={result_b:?}");
    assert!(
        !failure.output().contains('{'),
        "must be a readable sentence, not a raw JSON/HTTP body: {}",
        failure.output()
    );
    assert!(
        failure.output().to_lowercase().contains("already"),
        "must explain the conflict in words: {}",
        failure.output()
    );

    // Whichever one drove it, the execution still reaches its terminal
    // state — the race decided who drove it, not whether it finished.
    let shown = cli(&root, &["show", &id]);
    assert!(shown.stdout.contains("Status:        completed"));
}

// ---------------------------------------------------------------------------
// Conductor plan U5: resolving a problem from the command line
// ---------------------------------------------------------------------------

#[path = "engine_support/mod.rs"]
mod engine_support;
mod environment_support;
mod remote_support;

/// Stops a fresh `agent_problem` execution on its problem — its agent answers
/// without a label and cannot be asked again — before any daemon runs, and
/// returns its id.
fn stop_on_a_problem(root: &TempRoot) -> String {
    let definition_path = engine_support::fixtures().join("engine/agent_problem.yaml");
    let definition = load_from_file(&definition_path).expect("load the fixture");
    let workspace_path = root.path().join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    fs::copy(
        engine_support::fixtures().join("scripts/print_json.sh"),
        workspace_path.join("print_json.sh"),
    )
    .expect("copy the fixture script");

    let store = ExecutionStore::new(root.path());
    let engine = Engine::new(store.clone()).with_executor(
        narsor::engine::ActionKind::Agent,
        std::sync::Arc::new(engine_support::LabellessAgentExecutor::answering(
            "Done: 3 units, all tests pass.",
        )),
    );
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");

    let mut pinned = store.load_execution(&execution.id).expect("reload");
    pinned.metadata = serde_json::json!({
        "cli": { "definition_path": definition_path.display().to_string() }
    });
    store
        .save_execution(&pinned)
        .expect("pin the definition path");

    let stopped = tokio::runtime::Runtime::new()
        .expect("a runtime")
        .block_on(engine.run(&definition, &execution.id))
        .expect("run to the problem");
    assert_eq!(stopped.status, narsor::domain::ExecutionStatus::InputNeeded);
    execution.id
}

#[test]
fn input_resolves_a_problem_with_a_label_data_and_variables() {
    let root = TempRoot::new();
    let id = stop_on_a_problem(&root);

    // The label leading to the gate needs the question it shows.
    let refused = cli(&root, &["input", &id, "--label", "needs_human_decision"]);
    assert_eq!(refused.code, EXIT_FAILURE, "{refused:?}");
    assert!(refused.output().contains("data.question"), "{refused:?}");

    let resolved = cli(
        &root,
        &[
            "input",
            &id,
            "--label",
            "implemented",
            "--data",
            "units_completed=3",
            "--variable",
            "summary=login works",
            "--comment",
            "read the answer",
        ],
    );
    assert_eq!(resolved.code, EXIT_SUCCESS, "{resolved:?}");

    let shown = cli(&root, &["show", &id]);
    assert!(
        shown.stdout.contains("Status:        completed"),
        "{shown:?}"
    );
    assert!(shown.stdout.contains("login works"), "{shown:?}");

    let events = cli(&root, &["events", &id]);
    assert!(
        events
            .stdout
            .contains("Decision 'implemented' made by 'human'"),
        "{events:?}"
    );
}

#[test]
fn input_with_a_malformed_data_pair_is_a_usage_error() {
    let root = TempRoot::new();
    let invocation = cli(
        &root,
        &[
            "input",
            "any-id",
            "--label",
            "implemented",
            "--data",
            "no-equals-sign",
        ],
    );
    assert_eq!(invocation.code, EXIT_USAGE, "{invocation:?}");
    assert!(invocation.output().contains("--data"), "{invocation:?}");
}

// ---------------------------------------------------------------------------
// The conductor's chat (U12)
// ---------------------------------------------------------------------------

/// A fake `claude` of this test's own, first on the `PATH` it returns,
/// answering every invocation of the conductor with `response`.
fn conductor_answering(root: &TempRoot, response: serde_json::Value) -> String {
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).expect("create the bin directory");
    let temp = bin.join(format!(".claude.{}.tmp", uuid::Uuid::new_v4()));
    fs::copy(fixtures().join("harness/fake_harness.sh"), &temp).expect("copy the fake harness");
    fs::rename(&temp, bin.join("claude")).expect("move the fake harness into place");
    let envelope = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "conductor-session",
        "result": response.to_string(),
    });
    fs::write(bin.join("stdout"), format!("{envelope}\n")).expect("write the answer");
    format!(
        "{}:{}",
        bin.display(),
        engine_support::fake_claude_first_on_path()
    )
}

/// F4, R28: the chat written from the CLI and from the API is one history;
/// a proposal in it is confirmed from the CLI, by its number, once.
#[test]
#[ignore = "conductor and problem recovery are not wired to scheduler-admitted (runner) executions yet; see unified task scheduler follow-up"]
fn the_cli_and_the_api_share_the_conductors_chat() {
    let root = TempRoot::new();
    let path = conductor_answering(
        &root,
        serde_json::json!({
            "decision": "reply",
            "message": "I would approve the announcement.",
            "confidence": "high",
            "rationale": "The script announced the greeting.",
            "proposals": [{
                "kind": "answer_input",
                "summary": "Approve the announcement",
                "values": {"approval_status": "approved"}
            }],
        }),
    );
    let started = cli_on_path(
        root.path(),
        &[
            "run",
            &greet_workflow(),
            "--input",
            "greeting=Hello",
            "--input",
            &announce_script(),
        ],
        &path,
    );
    assert_eq!(started.code, EXIT_SUCCESS, "stderr: {}", started.stderr);
    let id = execution_id(&started.stdout);

    let chat = cli_on_path(
        root.path(),
        &["conductor", "chat", &id, "should I approve it?"],
        &path,
    );
    assert_eq!(chat.code, EXIT_SUCCESS, "stderr: {}", chat.stderr);
    assert!(
        chat.stdout.contains("operator: should I approve it?"),
        "{}",
        chat.stdout
    );
    assert!(
        chat.stdout
            .contains("conductor: I would approve the announcement."),
        "{}",
        chat.stdout
    );
    let proposal_line = chat
        .stdout
        .lines()
        .find(|line| line.contains("conductor (proposal): Approve the announcement"))
        .unwrap_or_else(|| panic!("no proposal in:\n{}", chat.stdout));
    let proposal_number = proposal_line
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .expect("the proposal's number")
        .to_string();

    // The same history, written over the API.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    runtime.block_on(async {
        let info = narsor::persistence::DaemonInfo::read(
            &narsor::persistence::StatePaths::new(root.path()).daemon_file(),
        )
        .expect("the daemon is discoverable");
        narsor::api::ApiClient::new(info.addr, info.token)
            .post_chat(&id, "and from the API?")
            .await
            .expect("post over the API");
    });
    let history = cli_on_path(root.path(), &["conductor", "history", &id], &path);
    assert_eq!(history.code, EXIT_SUCCESS, "stderr: {}", history.stderr);
    let from_cli = history
        .stdout
        .find("operator: should I approve it?")
        .expect("the CLI's message");
    let from_api = history
        .stdout
        .find("operator: and from the API?")
        .unwrap_or_else(|| panic!("the API's message is missing:\n{}", history.stdout));
    assert!(from_cli < from_api, "{}", history.stdout);

    // Confirmed by its number: the gate is answered, once.
    let confirmed = cli_on_path(
        root.path(),
        &["conductor", "confirm", &id, &proposal_number],
        &path,
    );
    assert_eq!(confirmed.code, EXIT_SUCCESS, "stderr: {}", confirmed.stderr);
    assert!(
        confirmed.stdout.contains("Answered the pending input."),
        "{}",
        confirmed.stdout
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let shown = cli_on_path(root.path(), &["show", &id], &path);
        if shown.stdout.contains("Status:        completed") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the answer did not complete the execution:\n{}",
            shown.stdout
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let again = cli_on_path(
        root.path(),
        &["conductor", "confirm", &id, &proposal_number],
        &path,
    );
    assert_eq!(again.code, EXIT_SUCCESS, "stderr: {}", again.stderr);
    assert!(
        again.stdout.contains("already been settled"),
        "{}",
        again.stdout
    );

    // Only a choice of the conductor's can be undone.
    let undo = cli_on_path(root.path(), &["conductor", "undo", &id, "no-such"], &path);
    assert_eq!(undo.code, EXIT_FAILURE);
    assert!(undo.stderr.contains("has no decision"), "{}", undo.stderr);
}

// ---------------------------------------------------------------------------
// recover / conductor status: the CLI counterparts of the TUI's failure form
// and conductor pane
// ---------------------------------------------------------------------------

/// Waits until `show` of `id` prints `needle`, and returns what it printed.
fn show_until(root: &TempRoot, id: &str, needle: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let shown = cli(root, &["show", id]);
        if shown.stdout.contains(needle) {
            return shown.stdout;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "'show' never printed '{needle}':\n{}",
            shown.stdout
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `show` says what a failed execution is a problem on and how to recover
/// it; `recover` starts the failed step afresh, bound to that failure.
#[test]
#[ignore = "conductor and problem recovery are not wired to scheduler-admitted (runner) executions yet; see unified task scheduler follow-up"]
fn recover_restarts_a_failed_step_shown_as_a_problem() {
    let root = TempRoot::new();
    let workflow = engine_support::fixtures()
        .join("engine/missing_script.yaml")
        .display()
        .to_string();
    let started = cli(&root, &["run", &workflow]);
    assert_eq!(started.code, EXIT_FAILURE, "{started:?}");
    let id = execution_id(&started.stdout);

    // The conductor's diagnosis fails on the fake harness: the problem is
    // the raw error, under the failure's key.
    let shown = show_until(&root, &id, "Problem (failure):");
    assert!(shown.contains("  Failure: "), "{shown}");
    assert!(
        shown.contains(&format!("Recover with: narsor recover {id}")),
        "{shown}"
    );

    let recovered = cli(&root, &["recover", &id, "--comment", "the script is back"]);
    assert_eq!(recovered.code, EXIT_FAILURE, "still missing: {recovered:?}");
    assert!(
        recovered
            .stdout
            .contains(&format!("Recovered execution {id}.")),
        "{recovered:?}"
    );
    let steps = cli(&root, &["steps", &id]);
    assert!(
        steps
            .stdout
            .lines()
            .any(|line| line.starts_with("build #3")),
        "the recovery ran the step again:\n{}",
        steps.stdout
    );
}

/// `recover` of an execution that has no failed step says why, without
/// asking the daemon to recover anything.
#[test]
fn recover_of_an_execution_that_did_not_fail_explains_the_status() {
    let root = TempRoot::new();
    let (started, id) = start_greet(&root);
    assert_eq!(started.code, EXIT_SUCCESS, "{started:?}");

    let refused = cli(&root, &["recover", &id]);
    assert_eq!(refused.code, EXIT_FAILURE, "{refused:?}");
    assert!(
        refused
            .stderr
            .contains("has no failed step to recover (status: input_needed)"),
        "{refused:?}"
    );
}

/// `conductor status` prints what the conductor of an execution is doing
/// and what it kept.
#[test]
#[ignore = "conductor and problem recovery are not wired to scheduler-admitted (runner) executions yet; see unified task scheduler follow-up"]
fn conductor_status_prints_what_the_conductor_is_doing() {
    let root = TempRoot::new();
    let (started, id) = start_greet(&root);
    assert_eq!(started.code, EXIT_SUCCESS, "{started:?}");

    let status = cli(&root, &["conductor", "status", &id]);
    assert_eq!(status.code, EXIT_SUCCESS, "{status:?}");
    assert!(status.stdout.contains("Working on:"), "{status:?}");
    assert!(status.stdout.contains("Queued:"), "{status:?}");
    assert!(status.stdout.contains("Unanswered:    0"), "{status:?}");

    let unknown = cli(&root, &["conductor", "status", "no-such-execution"]);
    assert_eq!(unknown.code, EXIT_FAILURE, "{unknown:?}");
}

/// R30: `show` prints the conductor's summary of the gate an execution
/// waits on, with what it recommends.
#[test]
#[ignore = "conductor and problem recovery are not wired to scheduler-admitted (runner) executions yet; see unified task scheduler follow-up"]
fn show_prints_the_conductors_summary_of_a_gate() {
    let root = TempRoot::new();
    let path = conductor_answering(
        &root,
        serde_json::json!({
            "decision": "annotate",
            "summary": "The greeting was announced.",
            "recommendation": "approved",
            "confidence": "high",
        }),
    );
    let started = cli_on_path(
        root.path(),
        &[
            "run",
            &greet_workflow(),
            "--input",
            "greeting=Hello",
            "--input",
            &announce_script(),
        ],
        &path,
    );
    assert_eq!(started.code, EXIT_SUCCESS, "stderr: {}", started.stderr);
    let id = execution_id(&started.stdout);

    let shown = show_until(&root, &id, "Conductor's summary:");
    assert!(
        shown.contains("Conductor's summary: The greeting was announced."),
        "{shown}"
    );
    assert!(shown.contains("  Recommends: approved"), "{shown}");
}

// ---------------------------------------------------------------------------
// Operator commands on a local task (U3)
// ---------------------------------------------------------------------------

/// Stops whatever a test left running under its root — the daemon and the
/// runner of a task that outlives it by design — when the test ends.
struct LocalProcesses {
    root: PathBuf,
}

impl Drop for LocalProcesses {
    fn drop(&mut self) {
        let root = self.root.display().to_string();
        let Ok(entries) = fs::read_dir("/proc") else {
            return;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let command = fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&command).replace('\0', " ");
            if pid != std::process::id() && command.contains("narsor") && command.contains(&root) {
                // SAFETY: killing a process of this test's own root; the
                // runner leads its own process group, and so takes its
                // harness with it.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}

/// A one-step workflow that starts a long script, and allows overrides.
const SLOW_STEP_WORKFLOW: &str = r#"
name: slow
version: 1
states:
  slow:
    allow_manual_override: true
    action:
      type: script
      command: sh
      args: [-c, "echo started >> \"$HOME/started\"; sleep 60; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// Polls until `check` holds, for up to thirty seconds.
fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn task_interrupt_restart_and_override_steer_the_runner_of_a_local_task() {
    let root = TempRoot::new();
    let _processes = LocalProcesses {
        root: root.path().to_path_buf(),
    };
    fs::create_dir_all(root.path().join("workflows")).unwrap();
    fs::write(root.path().join("workflows/slow.yaml"), SLOW_STEP_WORKFLOW).unwrap();
    let created = cli(&root, &["task", "create", "slow", "--task-id", "t-slow"]);
    assert_eq!(created.code, EXIT_SUCCESS, "{}", created.output());
    let started = root
        .path()
        .join("local-controller/storage/t-slow/home/started");
    let started_runs = || {
        fs::read_to_string(&started)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    wait_for("the step to start", || started_runs() == 1);
    wait_for("the task to be running", || {
        cli(&root, &["task", "show", "t-slow"])
            .stdout
            .contains("Lifecycle  running")
    });

    // The CLI waits for the runner's verdict: success means it was carried out.
    let interrupted = cli(&root, &["task", "interrupt", "t-slow"]);
    assert_eq!(interrupted.code, EXIT_SUCCESS, "{}", interrupted.output());
    assert!(
        interrupted.stdout.contains("carried out"),
        "{}",
        interrupted.stdout
    );

    // Nothing runs now, and the runner says so as a failure.
    let again = cli(&root, &["task", "interrupt", "t-slow"]);
    assert_eq!(again.code, EXIT_FAILURE, "{}", again.output());
    assert!(
        again.output().contains("nothing to interrupt"),
        "{}",
        again.output()
    );

    let restarted = cli(
        &root,
        &["task", "restart-step", "t-slow", "--note", "be careful"],
    );
    assert_eq!(restarted.code, EXIT_SUCCESS, "{}", restarted.output());
    wait_for("the step to run again", || started_runs() == 2);

    // An override needs its reason, and the audit trail keeps it.
    let interrupted = cli(&root, &["task", "interrupt", "t-slow"]);
    assert_eq!(interrupted.code, EXIT_SUCCESS, "{}", interrupted.output());
    let unexplained = cli(
        &root,
        &["task", "transition", "t-slow", "completed", "--override"],
    );
    assert_eq!(unexplained.code, EXIT_FAILURE, "{}", unexplained.output());
    assert!(
        unexplained.output().contains("reason"),
        "{}",
        unexplained.output()
    );
    let overridden = cli(
        &root,
        &[
            "task",
            "transition",
            "t-slow",
            "completed",
            "--override",
            "--reason",
            "ticket closed upstream",
        ],
    );
    assert_eq!(overridden.code, EXIT_SUCCESS, "{}", overridden.output());

    let events = cli(&root, &["task", "events", "t-slow"]);
    assert_eq!(events.code, EXIT_SUCCESS, "{}", events.output());
    assert!(
        events
            .stdout
            .contains("override transition to 'completed' (reason: ticket closed upstream)"),
        "{}",
        events.stdout
    );
    assert!(
        events.stdout.contains("queued by local"),
        "{}",
        events.stdout
    );
    assert!(
        events.stdout.contains("command-5 Applied"),
        "{}",
        events.stdout
    );

    // The execution's own events and its step timeline, read for a task
    // whose runner is done: from the task's storage.
    wait_for("the task's runner to finish", || {
        !cli(&root, &["task", "steps", "t-slow"])
            .stdout
            .contains("No steps.")
    });
    let steps = cli(&root, &["task", "steps", "t-slow"]);
    assert_eq!(steps.code, EXIT_SUCCESS, "{}", steps.output());
    let execution = cli(&root, &["task", "events", "t-slow", "--execution"]);
    assert_eq!(execution.code, EXIT_SUCCESS, "{}", execution.output());
    assert!(execution.stdout.lines().count() > 1, "{}", execution.stdout);
}

/// The CLI half of the conductor of a local task (U5): chat, read what it
/// said and proposed, reject a proposal, and what is refused and why.
#[test]
fn task_conductor_commands_talk_to_the_conductor_in_the_tasks_runner() {
    let root = TempRoot::new();
    let _processes = LocalProcesses {
        root: root.path().to_path_buf(),
    };
    let path = conductor_answering(
        &root,
        serde_json::json!({
            "decision": "reply",
            "message": "Still sleeping.",
            "confidence": "high",
            "rationale": "The step has not ended.",
            "proposals": [{"kind": "abort", "summary": "Cancel the task"}],
        }),
    );
    let run = |args: &[&str]| cli_on_path(root.path(), args, &path);
    fs::create_dir_all(root.path().join("workflows")).unwrap();
    fs::write(root.path().join("workflows/slow.yaml"), SLOW_STEP_WORKFLOW).unwrap();
    let created = run(&["task", "create", "slow", "--task-id", "t-slow"]);
    assert_eq!(created.code, EXIT_SUCCESS, "{}", created.output());
    wait_for("the task to be running", || {
        run(&["task", "show", "t-slow"])
            .stdout
            .contains("Lifecycle  running")
    });

    let sent = run(&["task", "conductor", "chat", "t-slow", "how is it going?"]);
    assert_eq!(sent.code, EXIT_SUCCESS, "{}", sent.output());
    assert!(sent.stdout.contains("carried out"), "{}", sent.stdout);
    wait_for("the conductor's answer", || {
        run(&["task", "conductor", "history", "t-slow"])
            .stdout
            .contains("Still sleeping.")
    });
    let history = run(&["task", "conductor", "history", "t-slow"]);
    assert_eq!(history.code, EXIT_SUCCESS, "{}", history.output());
    assert!(
        history.stdout.contains("Conductor: running on claude-code"),
        "{}",
        history.stdout
    );
    assert!(
        history.stdout.contains("you: how is it going?"),
        "{}",
        history.stdout
    );
    let proposal = history
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("Proposal "))
        .expect("the proposal is listed");
    assert!(
        proposal.contains("[Pending]  Cancel the task"),
        "{proposal}"
    );
    let proposal_id = proposal.split_whitespace().next().unwrap().to_string();

    // The proposal is carried out or rejected once, whoever asks.
    let rejected = run(&["task", "conductor", "reject", "t-slow", &proposal_id]);
    assert_eq!(rejected.code, EXIT_SUCCESS, "{}", rejected.output());
    let history = run(&["task", "conductor", "history", "t-slow"]);
    assert!(history.stdout.contains("[Rejected]"), "{}", history.stdout);
    let confirmed = run(&["task", "conductor", "confirm", "t-slow", &proposal_id]);
    assert_eq!(confirmed.code, EXIT_FAILURE, "{}", confirmed.output());
    assert!(
        confirmed.output().contains("was rejected"),
        "{}",
        confirmed.output()
    );

    // What cannot be done is said so, by the control plane or the runner.
    let undone = run(&["task", "conductor", "undo", "t-slow", "no-such-decision"]);
    assert_eq!(undone.code, EXIT_FAILURE, "{}", undone.output());
    assert!(
        undone.output().contains("no decision"),
        "{}",
        undone.output()
    );
    // The conductor's harness is no longer chosen per task.
    let harness = run(&["task", "conductor", "harness", "t-slow", "codex"]);
    assert_ne!(harness.code, EXIT_SUCCESS, "{}", harness.output());
    assert!(
        harness
            .output()
            .contains("unrecognized subcommand 'harness'"),
        "{}",
        harness.output()
    );
    let empty = run(&["task", "conductor", "chat", "t-slow", "  "]);
    assert_eq!(empty.code, EXIT_FAILURE, "{}", empty.output());
    assert!(
        empty.output().contains("cannot be empty"),
        "{}",
        empty.output()
    );
    let unknown = run(&["task", "conductor", "history", "no-such-task"]);
    assert_eq!(unknown.code, EXIT_FAILURE, "{}", unknown.output());
}

// ---------------------------------------------------------------------------
// The rest of what the TUI does, in the CLI (R8): create from a file, the
// attention of a task, the archive, the history and the registry
// ---------------------------------------------------------------------------

/// A workflow file with two violations at once.
const BROKEN_WORKFLOW: &str = "name: broken\nversion: 1\nstates:\n  a:\n    description: x\n    transitions:\n      go: nowhere\nterminal_states: [missing]\n";

#[test]
fn a_task_from_a_file_is_listed_shown_answered_and_archived_from_the_cli() {
    let root = TempRoot::new();
    let _processes = LocalProcesses {
        root: root.path().to_path_buf(),
    };
    let greet = greet_workflow();
    let script = announce_script();

    // The file is checked first: every violation is listed, nothing is queued.
    let broken = root.path().join("broken.yaml");
    fs::write(&broken, BROKEN_WORKFLOW).unwrap();
    let broken_path = broken.display().to_string();
    let refused = cli(&root, &["task", "create", "--file", &broken_path]);
    assert_eq!(refused.code, EXIT_FAILURE, "{}", refused.output());
    assert!(refused.stderr.contains("is invalid"), "{}", refused.stderr);
    let verdict = cli(&root, &["task", "validate", "broken.yaml"]);
    assert_eq!(verdict.code, EXIT_FAILURE, "{}", verdict.output());
    assert!(
        verdict.stdout.contains("Invalid — ") && verdict.stdout.contains("\n  - "),
        "{}",
        verdict.stdout
    );
    let valid = cli(&root, &["task", "validate", &greet]);
    assert_eq!(valid.code, EXIT_SUCCESS, "{}", valid.output());
    assert!(
        valid.stdout.contains("Valid: cli-greet v1"),
        "{}",
        valid.stdout
    );
    assert!(
        valid
            .stdout
            .contains("input is_experimental: Boolean optional, default false"),
        "{}",
        valid.stdout
    );
    let not_a_file = cli(&root, &["task", "validate", "does-not-exist.yaml"]);
    assert_eq!(not_a_file.code, EXIT_FAILURE, "{}", not_a_file.output());

    // A missing or mistyped input is a usage error, before anything is queued.
    let missing = cli(&root, &["task", "create", "--file", &greet]);
    assert_eq!(missing.code, EXIT_USAGE, "{}", missing.output());
    assert!(missing.stderr.contains("greeting"), "{}", missing.stderr);
    let mistyped = cli(
        &root,
        &[
            "task",
            "create",
            "--file",
            &greet,
            "--input",
            "greeting=Hi",
            "--input",
            &script,
            "--input",
            "is_experimental=maybe",
        ],
    );
    assert_eq!(mistyped.code, EXIT_USAGE, "{}", mistyped.output());
    // A file replaces the catalogue's workflow and its options.
    let both = cli(&root, &["task", "create", "greet", "--file", &greet]);
    assert_eq!(both.code, EXIT_USAGE, "{}", both.output());
    let neither = cli(&root, &["task", "create"]);
    assert_eq!(neither.code, EXIT_USAGE, "{}", neither.output());

    let created = cli(
        &root,
        &[
            "task",
            "create",
            "--file",
            &greet,
            "--input",
            "greeting=Hi",
            "--input",
            &script,
            "--task-id",
            "t-greet",
        ],
    );
    assert_eq!(created.code, EXIT_SUCCESS, "{}", created.output());
    assert!(
        created.stdout.contains("Created task: t-greet"),
        "{}",
        created.stdout
    );

    // It runs until the human step asks: the list marks it, the details say
    // what it waits on and by which id it is answered.
    wait_for("the task to ask", || {
        cli(&root, &["task", "list"])
            .stdout
            .lines()
            .any(|line| line.starts_with("t-greet") && line.contains(" ? "))
    });
    let shown = cli(&root, &["task", "show", "t-greet"]);
    assert_eq!(shown.code, EXIT_SUCCESS, "{}", shown.output());
    let waiting = shown
        .stdout
        .lines()
        .find(|line| line.starts_with("Waiting    ? question in 'approve'"))
        .unwrap_or_else(|| panic!("no waiting line:\n{}", shown.stdout));
    assert!(waiting.contains("(id "), "{waiting}");

    // The same summary, machine readable.
    let json = cli(&root, &["task", "show", "t-greet", "--json"]);
    assert_eq!(json.code, EXIT_SUCCESS, "{}", json.output());
    let summary: serde_json::Value = serde_json::from_str(&json.stdout).expect("JSON");
    assert_eq!(summary["task_id"], "t-greet");
    let waiting_id = summary["attention"][0]["waiting_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(waiting.contains(&waiting_id), "{waiting}");
    let listing = cli(&root, &["task", "list", "--json"]);
    let listed: serde_json::Value = serde_json::from_str(&listing.stdout).expect("JSON");
    assert_eq!(listed[0]["task_id"], "t-greet");

    // An answer names the wait it is for; another one is refused.
    let wrong = cli(
        &root,
        &[
            "task",
            "input",
            "t-greet",
            "--field",
            "approval_status=approved",
            "--waiting-id",
            "other",
        ],
    );
    assert_eq!(wrong.code, EXIT_FAILURE, "{}", wrong.output());
    assert!(wrong.stderr.contains(&waiting_id), "{}", wrong.stderr);
    let answered = cli(
        &root,
        &[
            "task",
            "input",
            "t-greet",
            "--field",
            "approval_status=approved",
            "--waiting-id",
            &waiting_id,
        ],
    );
    assert_eq!(answered.code, EXIT_SUCCESS, "{}", answered.output());
    wait_for("the task to finish", || {
        cli(&root, &["task", "show", "t-greet"])
            .stdout
            .contains("Runner     completed")
    });

    // Archived, it leaves nothing to answer and shows in the history list only.
    assert!(cli(&root, &["task", "list", "--archived"])
        .stdout
        .contains("No archived tasks."));
    let archived = cli(&root, &["task", "archive", "t-greet"]);
    assert_eq!(archived.code, EXIT_SUCCESS, "{}", archived.output());
    wait_for("the task to be archived", || {
        cli(&root, &["task", "list", "--archived"])
            .stdout
            .lines()
            .any(|line| line.starts_with("t-greet") && line.contains("archived"))
    });
    let all = cli(&root, &["task", "list"]);
    assert!(all.stdout.contains("t-greet"), "{}", all.stdout);
}

#[test]
fn history_reads_an_old_execution_and_refuses_to_recover_one_that_did_not_fail() {
    let root = TempRoot::new();
    let _processes = LocalProcesses {
        root: root.path().to_path_buf(),
    };
    let id = stop_on_a_problem(&root);

    let listed = cli(&root, &["task", "history", "list"]);
    assert_eq!(listed.code, EXIT_SUCCESS, "{}", listed.output());
    assert!(
        listed.stdout.contains(&id) && listed.stdout.contains("input_needed"),
        "{}",
        listed.stdout
    );
    let shown = cli(&root, &["task", "history", "show", &id]);
    assert_eq!(shown.code, EXIT_SUCCESS, "{}", shown.output());
    assert!(
        shown.stdout.contains(&format!("Execution  {id}")),
        "{}",
        shown.stdout
    );
    assert!(
        shown.stdout.contains("Recovery   not possible"),
        "{}",
        shown.stdout
    );
    let events = cli(&root, &["task", "history", "events", &id]);
    assert_eq!(events.code, EXIT_SUCCESS, "{}", events.output());
    assert!(events.stdout.lines().count() > 1, "{}", events.stdout);
    let after = cli(
        &root,
        &["task", "history", "events", &id, "--after", "1000"],
    );
    assert_eq!(after.code, EXIT_SUCCESS, "{}", after.output());
    assert!(after.stdout.is_empty(), "{}", after.stdout);
    let steps = cli(&root, &["task", "history", "steps", &id]);
    assert_eq!(steps.code, EXIT_SUCCESS, "{}", steps.output());
    assert!(steps.stdout.contains("work"), "{}", steps.stdout);
    let conductor = cli(&root, &["task", "history", "conductor", &id]);
    assert_eq!(conductor.code, EXIT_SUCCESS, "{}", conductor.output());
    assert!(
        conductor.stdout.contains("Working on:"),
        "{}",
        conductor.stdout
    );
    let no_record = cli(
        &root,
        &["task", "history", "conversation", &id, "no-such-attempt"],
    );
    assert_eq!(no_record.code, EXIT_FAILURE, "{}", no_record.output());

    let recovered = cli(
        &root,
        &["task", "history", "recover", &id, "--comment", "try again"],
    );
    assert_eq!(recovered.code, EXIT_FAILURE, "{}", recovered.output());
    assert!(
        recovered.stderr.contains("not_recoverable"),
        "{}",
        recovered.stderr
    );

    let unknown = cli(&root, &["task", "history", "show", "no-such-execution"]);
    assert_eq!(unknown.code, EXIT_FAILURE, "{}", unknown.output());
    assert!(unknown.stderr.contains("not found"), "{}", unknown.stderr);
}

#[test]
fn the_registry_and_the_catalogue_are_read_and_changed_from_the_cli() {
    let root = TempRoot::new();
    let _processes = LocalProcesses {
        root: root.path().to_path_buf(),
    };
    let empty = cli(&root, &["task", "repo", "list"]);
    assert_eq!(empty.code, EXIT_SUCCESS, "{}", empty.output());
    assert!(
        empty.stdout.contains("No repositories registered."),
        "{}",
        empty.stdout
    );

    let added = cli(
        &root,
        &[
            "task",
            "repo",
            "add",
            "api",
            "--remote",
            "https://example.com/api.git",
            "--default-branch",
            "trunk",
        ],
    );
    assert_eq!(added.code, EXIT_SUCCESS, "{}", added.output());
    // A local daemon takes a path of its own disk too.
    let local = cli(
        &root,
        &["task", "repo", "add", "disk", "--remote", "/srv/disk"],
    );
    assert_eq!(local.code, EXIT_SUCCESS, "{}", local.output());
    let duplicate = cli(
        &root,
        &[
            "task",
            "repo",
            "add",
            "api",
            "--remote",
            "https://example.com/other.git",
        ],
    );
    assert_eq!(duplicate.code, EXIT_FAILURE, "{}", duplicate.output());
    let listed = cli(&root, &["task", "repo", "list"]);
    assert!(
        listed.stdout.lines().any(|line| line.starts_with("api ")
            && line.contains("trunk")
            && line.ends_with("https://example.com/api.git")),
        "{}",
        listed.stdout
    );
    // The registry is the one `narsor repo` reads.
    assert!(cli(&root, &["repo", "list"]).stdout.contains("disk"));
    let removed = cli(&root, &["task", "repo", "remove", "api"]);
    assert_eq!(removed.code, EXIT_SUCCESS, "{}", removed.output());
    let again = cli(&root, &["task", "repo", "remove", "api"]);
    assert_eq!(again.code, EXIT_FAILURE, "{}", again.output());
    assert!(again.stderr.contains("not found"), "{}", again.stderr);

    let capabilities = cli(&root, &["task", "capabilities"]);
    assert_eq!(capabilities.code, EXIT_SUCCESS, "{}", capabilities.output());
    assert!(
        capabilities
            .stdout
            .contains("Connection:             local"),
        "{}",
        capabilities.stdout
    );
    assert!(
        capabilities.stdout.contains("Workflow paths:         yes"),
        "{}",
        capabilities.stdout
    );
    let workflows = cli(&root, &["task", "workflows"]);
    assert_eq!(workflows.code, EXIT_SUCCESS, "{}", workflows.output());
    assert!(
        workflows.stdout.contains("[built-in]") && workflows.stdout.contains("input "),
        "{}",
        workflows.stdout
    );
}

// ---------------------------------------------------------------------------
// Remote tasks through the control plane (U8)
// ---------------------------------------------------------------------------

mod remote_cli {
    use std::process::Command;

    use super::remote_support::RemoteHarness;
    use super::{Invocation, EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE};

    /// Runs the binary against the control plane at `server`, with `token`
    /// in `NARSOR_TOKEN` when given — never on the command line.
    async fn remote(harness: &RemoteHarness, token: Option<&str>, args: &[&str]) -> Invocation {
        let root = harness.root.path().to_path_buf();
        let server = harness.base_url();
        let token = token.map(str::to_string);
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_narsor"));
            command
                .current_dir(&root)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("HOME", &root)
                .env_remove("NARSOR_TOKEN")
                .env_remove("NARSOR_TOKEN_FILE")
                .env_remove("NARSOR_TOKEN_COMMAND")
                .env("NARSOR_SERVER", &server)
                .arg("--root")
                .arg(&root)
                .args(&args);
            if let Some(token) = token {
                command.env("NARSOR_TOKEN", token);
            }
            let output = command.output().expect("run the narsor binary");
            Invocation {
                code: output.status.code().expect("exited normally"),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        })
        .await
        .expect("the binary ran")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tasks_are_created_listed_and_steered_through_a_remote_control_plane() {
        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());

        let created = remote(
            &harness,
            token,
            &[
                "task",
                "create",
                "implement-feature",
                "--workflow-version",
                "1",
                "--backend",
                "k8s",
                "--input",
                "ticket=NAR-1",
                "--task-id",
                "t-1",
            ],
        )
        .await;
        assert_eq!(created.code, EXIT_SUCCESS, "{}", created.output());
        assert!(
            created.stdout.contains("Created task: t-1"),
            "{}",
            created.stdout
        );
        assert!(created.stdout.contains("Backend: k8s"));

        // The default backend changes where new tasks go, not t-1.
        let set = remote(&harness, token, &["backend", "default", "set", "backend-a"]).await;
        assert_eq!(set.code, EXIT_SUCCESS, "{}", set.output());
        assert!(
            set.stdout.contains("Effective: backend-a"),
            "{}",
            set.stdout
        );
        let second = remote(
            &harness,
            token,
            &[
                "task",
                "create",
                "implement-feature",
                "--workflow-version",
                "1",
                "--task-id",
                "t-2",
            ],
        )
        .await;
        assert_eq!(second.code, EXIT_SUCCESS, "{}", second.output());
        assert!(
            second.stdout.contains("Backend: backend-a"),
            "{}",
            second.stdout
        );

        let list = remote(&harness, token, &["task", "list"]).await;
        assert_eq!(list.code, EXIT_SUCCESS, "{}", list.output());
        let row = |id: &str| {
            list.stdout
                .lines()
                .find(|line| line.starts_with(id))
                .unwrap_or_default()
                .to_string()
        };
        assert!(
            row("t-1").contains("k8s") && row("t-1").contains("queued"),
            "{}",
            list.stdout
        );
        assert!(row("t-2").contains("backend-a"), "{}", list.stdout);

        let show = remote(&harness, token, &["task", "show", "t-1"]).await;
        assert_eq!(show.code, EXIT_SUCCESS, "{}", show.output());
        assert!(show.stdout.contains("Lifecycle  queued"), "{}", show.stdout);

        // A pause of a queued task is refused readably; archive is accepted.
        let pause = remote(&harness, token, &["task", "pause", "t-1"]).await;
        assert_eq!(pause.code, EXIT_FAILURE, "{}", pause.output());
        assert!(pause.stderr.contains("pause refused"), "{}", pause.stderr);
        let archive = remote(&harness, token, &["task", "archive", "t-2"]).await;
        assert_eq!(archive.code, EXIT_SUCCESS, "{}", archive.output());
        assert!(
            archive.stdout.contains("archive accepted"),
            "{}",
            archive.stdout
        );

        // A second queued task on k8s, moved ahead of t-1: `move` and the
        // owner API are one operation, and `list` shows the queue position.
        let third = remote(
            &harness,
            token,
            &[
                "task",
                "create",
                "implement-feature",
                "--workflow-version",
                "1",
                "--backend",
                "k8s",
                "--task-id",
                "t-3",
            ],
        )
        .await;
        assert_eq!(third.code, EXIT_SUCCESS, "{}", third.output());
        let moved = remote(&harness, token, &["task", "move", "t-3", "--before", "t-1"]).await;
        assert_eq!(moved.code, EXIT_SUCCESS, "{}", moved.output());
        assert!(moved.stdout.contains("t-3 is now #1"), "{}", moved.stdout);
        let list = remote(&harness, token, &["task", "list"]).await;
        let position = |id: &str| list.stdout.lines().position(|line| line.starts_with(id));
        assert!(position("t-3") < position("t-1"), "{}", list.stdout);
        assert!(
            list.stdout
                .lines()
                .any(|line| line.starts_with("t-3") && line.contains("#1")),
            "{}",
            list.stdout
        );
        let bottom = remote(&harness, token, &["task", "move", "t-3", "--bottom"]).await;
        assert_eq!(bottom.code, EXIT_SUCCESS, "{}", bottom.output());
        assert!(bottom.stdout.contains("t-3 is now #2"), "{}", bottom.stdout);

        // Exactly one target is required; an unknown anchor is refused.
        let conflicting = remote(
            &harness,
            token,
            &["task", "move", "t-3", "--top", "--after", "t-1"],
        )
        .await;
        assert_eq!(conflicting.code, EXIT_USAGE, "{}", conflicting.output());
        let nothing = remote(&harness, token, &["task", "move", "t-3"]).await;
        assert_eq!(nothing.code, EXIT_USAGE, "{}", nothing.output());
        let unknown = remote(
            &harness,
            token,
            &["task", "move", "t-3", "--before", "nope"],
        )
        .await;
        assert_eq!(unknown.code, EXIT_FAILURE, "{}", unknown.output());
        // Priority is gone from the CLI.
        let priority = remote(&harness, token, &["task", "priority", "t-1", "5"]).await;
        assert_eq!(priority.code, EXIT_USAGE, "{}", priority.output());
        let flag = remote(
            &harness,
            token,
            &["task", "create", "implement-feature", "--priority", "5"],
        )
        .await;
        assert_eq!(flag.code, EXIT_USAGE, "{}", flag.output());

        let events = remote(&harness, token, &["task", "events", "t-3"]).await;
        assert_eq!(events.code, EXIT_SUCCESS, "{}", events.output());
        assert!(events.stdout.contains("created"), "{}", events.stdout);
        assert!(events.stdout.contains("moved to #1"), "{}", events.stdout);

        let who = remote(&harness, token, &["auth", "whoami"]).await;
        assert_eq!(who.code, EXIT_SUCCESS, "{}", who.output());
        assert!(who.stdout.contains("Mode: trusted"), "{}", who.stdout);

        let code = remote(&harness, token, &["controller", "enroll-code", "k8s"]).await;
        assert_eq!(code.code, EXIT_SUCCESS, "{}", code.output());
        assert!(code.stdout.contains("Enrollment code: "), "{}", code.stdout);
        let controllers = remote(&harness, token, &["controller", "list"]).await;
        assert_eq!(controllers.code, EXIT_SUCCESS, "{}", controllers.output());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn materials_are_uploaded_listed_downloaded_and_deleted_from_the_cli() {
        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());
        let created = remote(
            &harness,
            token,
            &[
                "task",
                "create",
                "implement-feature",
                "--workflow-version",
                "1",
                "--backend",
                "k8s",
                "--task-id",
                "t-1",
            ],
        )
        .await;
        assert_eq!(created.code, EXIT_SUCCESS, "{}", created.output());

        let local = harness.root.path().join("brief");
        std::fs::create_dir_all(local.join("docs")).unwrap();
        std::fs::write(local.join("docs/spec.md"), "the spec").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", local.join("link")).unwrap();
        let brief = local.display().to_string();
        let upload = remote(
            &harness,
            token,
            &[
                "task",
                "materials",
                "upload",
                "t-1",
                &brief,
                "--id",
                "brief",
            ],
        )
        .await;
        assert_eq!(upload.code, EXIT_SUCCESS, "{}", upload.output());
        assert!(
            upload.stdout.contains("Uploaded material brief: 1 file(s)"),
            "{}",
            upload.stdout
        );
        assert!(
            upload.stderr.contains("skipped") && upload.stderr.contains("link"),
            "{}",
            upload.stderr
        );

        let list = remote(&harness, token, &["task", "materials", "list", "t-1"]).await;
        assert_eq!(list.code, EXIT_SUCCESS, "{}", list.output());
        assert!(
            list.stdout.contains("waiting for the runner"),
            "{}",
            list.stdout
        );
        let show = remote(
            &harness,
            token,
            &["task", "materials", "show", "t-1", "brief"],
        )
        .await;
        assert!(
            show.stdout.contains("brief/docs/spec.md"),
            "{}",
            show.output()
        );

        let dest = harness.root.path().join("download");
        let dest_arg = dest.display().to_string();
        let download = remote(
            &harness,
            token,
            &["task", "materials", "download", "t-1", "brief", &dest_arg],
        )
        .await;
        assert_eq!(download.code, EXIT_SUCCESS, "{}", download.output());
        assert_eq!(
            std::fs::read_to_string(dest.join("brief/docs/spec.md")).unwrap(),
            "the spec"
        );
        let again = remote(
            &harness,
            token,
            &["task", "materials", "download", "t-1", "brief", &dest_arg],
        )
        .await;
        assert_eq!(again.code, EXIT_FAILURE, "{}", again.output());
        assert!(again.stderr.contains("already exists"), "{}", again.stderr);
        let forced = remote(
            &harness,
            token,
            &[
                "task",
                "materials",
                "download",
                "t-1",
                "brief",
                &dest_arg,
                "--force",
            ],
        )
        .await;
        assert_eq!(forced.code, EXIT_SUCCESS, "{}", forced.output());

        let deleted = remote(
            &harness,
            token,
            &["task", "materials", "delete", "t-1", "brief"],
        )
        .await;
        assert_eq!(deleted.code, EXIT_SUCCESS, "{}", deleted.output());
        let events = remote(&harness, token, &["task", "events", "t-1"]).await;
        assert!(
            events.stdout.contains("material brief Uploaded"),
            "{}",
            events.stdout
        );
        assert!(
            events.stdout.contains("material brief Deleted"),
            "{}",
            events.stdout
        );
        assert!(!events.stdout.contains("spec.md"), "{}", events.stdout);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_reaches_the_runner_with_fields_data_and_variables() {
        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());
        let mut asking = super::remote_support::snapshot(1, Some("ask-1"));
        asking.attention = vec![narsor::orchestration::AttentionItem {
            kind: narsor::orchestration::AttentionKind::Question,
            state_id: Some("review".to_string()),
            since: chrono::Utc::now(),
            waiting_id: "ask-1".to_string(),
        }];
        let running = harness.running("t-ask", "k8s", asking).await;

        // What the task waits on is listed, with the id an answer names.
        let shown = remote(&harness, token, &["task", "show", "t-ask"]).await;
        assert_eq!(shown.code, EXIT_SUCCESS, "{}", shown.output());
        assert!(
            shown.stdout.contains("Waiting    ? question in 'review'")
                && shown.stdout.contains("(id ask-1)"),
            "{}",
            shown.stdout
        );
        let listed = remote(&harness, token, &["task", "list"]).await;
        assert!(
            listed
                .stdout
                .lines()
                .any(|line| line.starts_with("t-ask") && line.contains(" ? ")),
            "{}",
            listed.stdout
        );

        let refused = remote(
            &harness,
            token,
            &[
                "task",
                "input",
                "t-ask",
                "--field",
                "reason=x",
                "--waiting-id",
                "other",
            ],
        )
        .await;
        assert_eq!(refused.code, EXIT_FAILURE, "{}", refused.output());
        assert!(
            refused.stderr.contains("question ask-1"),
            "{}",
            refused.stderr
        );
        let malformed = remote(&harness, token, &["task", "input", "t-ask", "--data", "n"]).await;
        assert_eq!(malformed.code, EXIT_USAGE, "{}", malformed.output());

        let answered = remote(
            &harness,
            token,
            &[
                "task",
                "input",
                "t-ask",
                "--field",
                "reason=because",
                "--field",
                "numbers=3",
                "--field",
                r#"extra={"a":[1,2]}"#,
                "--data",
                "n=3",
                "--data",
                "s=plain text",
                "--data",
                r#"list=["a","b"]"#,
                "--variable",
                "v=true",
                "--label",
                "approved",
                "--comment",
                "looks fine",
                "--waiting-id",
                "ask-1",
            ],
        )
        .await;
        assert_eq!(answered.code, EXIT_SUCCESS, "{}", answered.output());

        let polled = running.runner.poll(Some("ask-1"), 0).await.expect("poll");
        let delivery = &polled.inputs[0].delivery;
        assert_eq!(delivery.values["reason"], serde_json::json!("because"));
        assert_eq!(
            delivery.values["numbers"],
            serde_json::json!("3"),
            "a form field is text unless it is an object or an array"
        );
        assert_eq!(delivery.values["extra"], serde_json::json!({"a": [1, 2]}));
        assert_eq!(delivery.data["n"], serde_json::json!(3));
        assert_eq!(delivery.data["s"], serde_json::json!("plain text"));
        assert_eq!(delivery.data["list"], serde_json::json!(["a", "b"]));
        assert_eq!(delivery.variables["v"], serde_json::json!(true));
        assert_eq!(delivery.chosen_label.as_deref(), Some("approved"));
        assert_eq!(delivery.comment.as_deref(), Some("looks fine"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archived_tasks_are_listed_apart_and_every_listing_has_a_json_form() {
        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());
        for id in ["t-keep", "t-old", "t-older"] {
            harness.create(id, Some("k8s")).await;
        }
        for id in ["t-older", "t-old"] {
            let archived = remote(&harness, token, &["task", "archive", id]).await;
            assert_eq!(archived.code, EXIT_SUCCESS, "{}", archived.output());
            harness.clock.advance(chrono::Duration::seconds(1));
        }

        let history = remote(&harness, token, &["task", "list", "--archived"]).await;
        assert_eq!(history.code, EXIT_SUCCESS, "{}", history.output());
        let ids: Vec<&str> = history
            .stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(
            ids,
            ["t-old", "t-older"],
            "newest archive first: {}",
            history.stdout
        );
        assert!(history.stdout.contains("archived"), "{}", history.stdout);

        let all = remote(&harness, token, &["task", "list"]).await;
        assert!(
            all.stdout.contains("t-keep") && all.stdout.contains("t-old"),
            "{}",
            all.stdout
        );
        let json = remote(&harness, token, &["task", "list", "--json"]).await;
        assert_eq!(json.code, EXIT_SUCCESS, "{}", json.output());
        let tasks: serde_json::Value = serde_json::from_str(&json.stdout).expect("JSON");
        assert_eq!(tasks.as_array().unwrap().len(), 3);
        let archived_json =
            remote(&harness, token, &["task", "list", "--archived", "--json"]).await;
        let tasks: serde_json::Value = serde_json::from_str(&archived_json.stdout).expect("JSON");
        assert_eq!(tasks.as_array().unwrap().len(), 2);
        assert_eq!(tasks[0]["lifecycle"], "archived");
        let one = remote(&harness, token, &["task", "show", "t-keep", "--json"]).await;
        let task: serde_json::Value = serde_json::from_str(&one.stdout).expect("JSON");
        assert_eq!(task["task_id"], "t-keep");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_catalogue_the_registry_and_the_history_are_read_through_the_control_plane() {
        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());

        let capabilities = remote(&harness, token, &["task", "capabilities"]).await;
        assert_eq!(capabilities.code, EXIT_SUCCESS, "{}", capabilities.output());
        assert!(
            capabilities.stdout.contains("Workflow paths:         yes")
                && capabilities.stdout.contains("Old executions:         no"),
            "{}",
            capabilities.stdout
        );
        let workflows = remote(&harness, token, &["task", "workflows"]).await;
        assert_eq!(workflows.code, EXIT_SUCCESS, "{}", workflows.output());
        assert!(
            workflows.stdout.contains("implement-feature")
                && workflows.stdout.contains("[built-in]"),
            "{}",
            workflows.stdout
        );

        let added = remote(
            &harness,
            token,
            &[
                "task",
                "repo",
                "add",
                "api",
                "--remote",
                "https://example.com/api.git",
            ],
        )
        .await;
        assert_eq!(added.code, EXIT_SUCCESS, "{}", added.output());
        let hostile = remote(
            &harness,
            token,
            &[
                "task",
                "repo",
                "add",
                "evil",
                "--remote",
                "ext::sh -c touch% /tmp/pwned",
            ],
        )
        .await;
        assert_eq!(hostile.code, EXIT_FAILURE, "{}", hostile.output());
        assert!(
            hostile.stderr.contains("invalid_repository"),
            "{}",
            hostile.stderr
        );
        let listed = remote(&harness, token, &["task", "repo", "list"]).await;
        assert!(
            listed.stdout.contains("api") && !listed.stdout.contains("evil"),
            "{}",
            listed.stdout
        );
        let removed = remote(&harness, token, &["task", "repo", "remove", "api"]).await;
        assert_eq!(removed.code, EXIT_SUCCESS, "{}", removed.output());

        let history = remote(&harness, token, &["task", "history", "list"]).await;
        assert_eq!(history.code, EXIT_SUCCESS, "{}", history.output());
        assert!(
            history.stdout.contains("No executions from before"),
            "{}",
            history.stdout
        );
        let unknown = remote(&harness, token, &["task", "history", "show", "exec-1"]).await;
        assert_eq!(unknown.code, EXIT_FAILURE, "{}", unknown.output());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_default_harness_is_read_set_and_reset_through_the_control_plane() {
        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());

        // Without an override the configured harness applies, marked so;
        // `default` alone is `get`.
        for args in [&["harness", "default"][..], &["harness", "default", "get"]] {
            let get = remote(&harness, token, args).await;
            assert_eq!(get.code, EXIT_SUCCESS, "{}", get.output());
            assert!(
                get.stdout
                    .contains("default harness: claude-code (configured)"),
                "{}",
                get.stdout
            );
            assert!(get.stdout.contains("Override: -"), "{}", get.stdout);
            assert!(
                get.stdout
                    .contains("Supported: claude-code, codex, opencode"),
                "{}",
                get.stdout
            );
        }

        let set = remote(&harness, token, &["harness", "default", "set", "opencode"]).await;
        assert_eq!(set.code, EXIT_SUCCESS, "{}", set.output());
        assert!(
            set.stdout
                .contains("default harness: opencode (override; configured claude-code)"),
            "{}",
            set.stdout
        );
        assert!(set.stdout.contains("Effective: opencode"), "{}", set.stdout);
        let get = remote(&harness, token, &["harness", "default", "get"]).await;
        assert!(get.stdout.contains("Effective: opencode"), "{}", get.stdout);
        assert!(get.stdout.contains("Override: opencode"), "{}", get.stdout);

        // An unknown harness is refused with the supported ones; the
        // override stays.
        let unknown = remote(&harness, token, &["harness", "default", "set", "nieznany"]).await;
        assert_eq!(unknown.code, EXIT_FAILURE, "{}", unknown.output());
        assert!(
            unknown.stderr.contains("unsupported_harness")
                && unknown.stderr.contains("claude-code, codex, opencode"),
            "{}",
            unknown.stderr
        );
        let get = remote(&harness, token, &["harness", "default", "get"]).await;
        assert!(get.stdout.contains("Effective: opencode"), "{}", get.stdout);

        let reset = remote(&harness, token, &["harness", "default", "reset"]).await;
        assert_eq!(reset.code, EXIT_SUCCESS, "{}", reset.output());
        assert!(
            reset
                .stdout
                .contains("default harness: claude-code (configured)"),
            "{}",
            reset.stdout
        );
        assert!(reset.stdout.contains("Override: -"), "{}", reset.stdout);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_server_needs_a_token_and_a_good_one() {
        let harness = RemoteHarness::start().await;
        let missing = remote(&harness, None, &["task", "list"]).await;
        assert_eq!(missing.code, EXIT_FAILURE);
        assert!(
            missing.stderr.contains("NARSOR_TOKEN"),
            "{}",
            missing.stderr
        );

        let wrong = remote(&harness, Some("not-the-token"), &["task", "list"]).await;
        assert_eq!(wrong.code, EXIT_FAILURE);
        assert!(
            wrong.stderr.contains("rejected the token"),
            "{}",
            wrong.stderr
        );

        // The default harness sits behind the same owner gate.
        let missing = remote(&harness, None, &["harness", "default", "get"]).await;
        assert_eq!(missing.code, EXIT_FAILURE);
        assert!(
            missing.stderr.contains("NARSOR_TOKEN"),
            "{}",
            missing.stderr
        );
        let wrong = remote(
            &harness,
            Some("not-the-token"),
            &["harness", "default", "set", "codex"],
        )
        .await;
        assert_eq!(wrong.code, EXIT_FAILURE);
        assert!(
            wrong.stderr.contains("rejected the token"),
            "{}",
            wrong.stderr
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn text_a_runner_sends_never_reaches_the_terminal_raw() {
        use narsor::controller_protocol::SubmitOperatorCommandRequest;
        use narsor::orchestration::OperatorCommand;

        let harness = RemoteHarness::start().await;
        let token = harness.handle.token().to_string();
        let token = Some(token.as_str());
        let running = harness
            .running("t-esc", "k8s", super::remote_support::snapshot(1, None))
            .await;
        harness
            .owner
            .control_operator_command(
                "t-esc",
                &SubmitOperatorCommandRequest {
                    command: OperatorCommand::Interrupt,
                    idempotency_key: "k-1".to_string(),
                    expected_generation: None,
                },
            )
            .await
            .expect("queued");
        let polled = running.runner.poll(None, 0).await.expect("poll");
        running
            .runner
            .ack_command(
                &polled.commands[0].command_id,
                false,
                Some("boom\u{1b}[2J\u{1b}]8;;http://evil\u{7}x\u{202e}".to_string()),
            )
            .await
            .expect("ack");

        let events = remote(&harness, token, &["task", "events", "t-esc"]).await;
        assert_eq!(events.code, EXIT_SUCCESS, "{}", events.output());
        assert!(
            events.stdout.contains("boom[2J]8;;http://evilx"),
            "{}",
            events.stdout
        );
        assert!(
            !events.stdout.contains('\u{1b}') && !events.stdout.contains('\u{202e}'),
            "{:?}",
            events.stdout
        );

        // A timeline whose texts carry escapes prints without them.
        let mut with_step = super::remote_support::snapshot(2, None);
        with_step.timeline = Some(narsor::engine::Timeline {
            positions: vec![narsor::engine::Position {
                label: "plan\u{1b}[2J#1".to_string(),
                state_id: "plan".to_string(),
                visit: 1,
                action_execution_id: None,
                action_type: Some("ag\u{1b}]8;;x\u{7}ent\u{202e}".to_string()),
                status: narsor::engine::PositionStatus::Failed,
                started_at: None,
                ended_at: None,
                error: Some("bad\u{1b}[31m".to_string()),
                resumed_harness_session: None,
                harness_session_interrupted: false,
                retry_pending: false,
                conversation: narsor::engine::ConversationState::NoRecord,
            }],
            ..Default::default()
        });
        running.runner.snapshot(&with_step).await.expect("snapshot");
        let printed = remote(&harness, token, &["task", "steps", "t-esc"]).await;
        assert_eq!(printed.code, EXIT_SUCCESS, "{}", printed.output());
        assert!(printed.stdout.contains("plan[2J#1"), "{}", printed.stdout);
        assert!(
            !printed.stdout.contains('\u{1b}') && !printed.stdout.contains('\u{202e}'),
            "{:?}",
            printed.stdout
        );
        let mut empty = super::remote_support::snapshot(3, None);
        empty.timeline = None;
        running.runner.snapshot(&empty).await.expect("snapshot");

        // The timeline of a task with no steps yet, and the reason an
        // execution cannot be read once the workload is gone.
        let steps = remote(&harness, token, &["task", "steps", "t-esc"]).await;
        assert_eq!(steps.code, EXIT_SUCCESS, "{}", steps.output());
        assert!(steps.stdout.contains("No steps."), "{}", steps.stdout);
        running
            .controller
            .report(&super::remote_support::report(
                "t-esc",
                running.epoch,
                1,
                narsor::orchestration::Observation::WorkloadLost { state_known: true },
                Some(1),
            ))
            .await
            .expect("lost");
        let execution = remote(&harness, token, &["task", "events", "t-esc", "--execution"]).await;
        assert_eq!(execution.code, EXIT_FAILURE, "{}", execution.output());
        assert!(
            execution.stderr.contains("runner is not running"),
            "{}",
            execution.stderr
        );
    }
}

// ---------------------------------------------------------------------------
// `narsor tui`: one interface, `--remote` an ignored alias (U14)
// ---------------------------------------------------------------------------

#[test]
fn tui_remote_alias_is_warned_about_and_plain_tui_is_not() {
    let warning = narsor::cli::tui_remote_warning(true).expect("the alias warns");
    assert!(warning.contains("--remote"), "{warning}");
    assert!(warning.contains("ignored"), "{warning}");
    assert_eq!(narsor::cli::tui_remote_warning(false), None);
}

/// Without a terminal the interface cannot take the screen, so both spellings
/// stop at the same place — after connecting — which is how to see, without a
/// terminal, that they run the same thing: the alias differs by its warning
/// and nothing else, and the local daemon was launched and answers on
/// `/control/v1`.
#[test]
fn tui_without_a_server_launches_the_local_daemon_and_remote_runs_the_same_interface() {
    let root = TempRoot::new();
    let _processes = LocalProcesses {
        root: root.path().to_path_buf(),
    };

    let plain = cli(&root, &["tui"]);
    assert_eq!(plain.code, EXIT_FAILURE, "{}", plain.output());
    assert!(
        plain
            .stderr
            .contains("The terminal interface stopped with an error"),
        "it got as far as the terminal; stderr: {}",
        plain.stderr
    );
    assert!(!plain.stderr.contains("--remote"), "{}", plain.stderr);

    let info = narsor::persistence::DaemonInfo::read(
        &narsor::persistence::StatePaths::new(root.path()).daemon_file(),
    )
    .expect("`narsor tui` launched the local daemon");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let capabilities = runtime
        .block_on(narsor::api::ApiClient::new(info.addr, info.token).control_capabilities())
        .expect("the daemon answers on /control/v1");
    assert!(!capabilities.connection_name.is_empty());

    let alias = cli(&root, &["tui", "--remote"]);
    assert_eq!(alias.code, plain.code, "{}", alias.output());
    assert!(
        alias.stderr.contains("`narsor tui --remote` is ignored"),
        "{}",
        alias.stderr
    );
    assert!(
        alias
            .stderr
            .contains("The terminal interface stopped with an error"),
        "{}",
        alias.stderr
    );
}
