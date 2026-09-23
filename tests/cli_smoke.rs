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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        let path = std::env::temp_dir().join(format!("orchestrator-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// What one invocation of the binary produced.
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
    let output = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .current_dir(root.path())
        .arg("--root")
        .arg(root.path())
        .args(args)
        .output()
        .expect("run the orchestrator binary");

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
    fixtures()
        .join("cli/greet.yaml")
        .display()
        .to_string()
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

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[test]
fn validate_confirms_a_correct_definition() {
    let root = TempRoot::new();
    let file = fixtures().join("implement_feature.yaml");

    let invocation = cli(&root, &["validate", &file.display().to_string()]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
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

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
    assert!(!id.is_empty());
    // The workflow stops at its human state, so the run ends paused.
    assert!(
        invocation.stdout.contains("Status: paused"),
        "stdout was:\n{}",
        invocation.stdout
    );
    assert!(root.path().join("executions").join(&id).is_dir());
}

#[test]
fn list_shows_the_created_execution_with_its_status() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["list"]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
    let line = invocation
        .stdout
        .lines()
        .find(|line| line.contains(&id))
        .unwrap_or_else(|| panic!("`list` did not mention {id}:\n{}", invocation.stdout));
    assert!(line.contains("paused"), "line was: {line}");
    assert!(line.contains("cli-greet"), "line was: {line}");
}

#[test]
fn show_prints_state_status_and_context_variables() {
    let root = TempRoot::new();
    let (_, id) = start_greet(&root);

    let invocation = cli(&root, &["show", &id]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
    let stdout = invocation.stdout;
    assert!(stdout.contains("Status:        paused"), "stdout:\n{stdout}");
    assert!(stdout.contains("Current state: approve"), "stdout:\n{stdout}");
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

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
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

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
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

    assert_eq!(invocation.code, EXIT_SUCCESS, "stderr: {}", invocation.stderr);
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
        &fs::read(root.path().join("executions").join(&id).join("context.json"))
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

    assert_eq!(invocation.code, EXIT_USAGE, "clap's own code must match ours");
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
    assert!(root.path().join("tasks").join(&id).join("code").is_dir());
    assert!(!Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".orchestrator")
        .exists());
}
