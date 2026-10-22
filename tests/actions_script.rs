//! Integration tests for the `ScriptAction` executor (U5).
//!
//! Everything here runs real processes in real temporary directories: the whole
//! point of this layer is what actually happens to a child process — its
//! working directory, its environment, its exit code and its death — so nothing
//! is mocked. The scripts live as executable files in `tests/fixtures/scripts/`
//! and are never generated at runtime.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use narsor_engine::domain::{
    ActionConfig, ActionError, ActionResult, Context, ScriptActionConfig, Workspace,
};
use narsor_engine::engine::actions::script::{MAX_CAPTURED_OUTPUT_BYTES, TRUNCATION_MARKER};
use narsor_engine::engine::actions::TIMEOUT_LABEL;
use narsor_engine::engine::{ActionExecutor, ActionOutcome, ActionRequest, ScriptExecutor};
use narsor_engine::workflow::TemplateScope;
use serde_json::{json, Map};

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
        let path = std::env::temp_dir().join(format!("narsor-script-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// A workspace whose primary repository is this directory.
    fn workspace(&self) -> Workspace {
        Workspace::local_folder(self.path.to_string_lossy().into_owned())
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Absolute path of a fixture script.
fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/scripts")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// A script configuration running a fixture script with the given arguments.
fn script(name: &str, args: &[&str]) -> ScriptActionConfig {
    ScriptActionConfig {
        command: fixture(name),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        working_dir: None,
        env: HashMap::new(),
        exit_codes: HashMap::new(),
        timeout_secs: None,
    }
}

/// Runs a script action in `workspace`, with an optional time limit.
async fn execute(
    config: ScriptActionConfig,
    workspace: &Workspace,
    timeout: Option<Duration>,
) -> Result<ActionOutcome, ActionError> {
    let action = ActionConfig::Script(config);

    ScriptExecutor::new()
        .execute(ActionRequest::new(&action, workspace, "action-exec-1").with_timeout(timeout))
        .await
}

/// Runs a script action expected to complete, and returns its result.
async fn complete(config: ScriptActionConfig, workspace: &Workspace) -> ActionResult {
    match execute(config, workspace, None).await {
        Ok(ActionOutcome::Completed(result)) => result,
        Ok(ActionOutcome::TimedOut(timed_out)) => panic!("unexpected timeout: {timed_out:?}"),
        Ok(ActionOutcome::Suspended(suspended)) => {
            panic!("a script action never suspends: {suspended:?}")
        }
        Err(error) => panic!("action failed: {error:?}"),
    }
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exit_code_zero_without_stdout_is_success() {
    let root = TempRoot::new();

    let result = complete(script("exit_with.sh", &["0"]), &root.workspace()).await;

    assert_eq!(result.output_label, "success");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout, "");
}

#[tokio::test]
async fn exit_code_is_mapped_through_the_exit_codes_table() {
    let root = TempRoot::new();

    let mut config = script("exit_with.sh", &["2"]);
    config.exit_codes.insert(2, "partial_failure".to_string());

    let result = complete(config, &root.workspace()).await;

    assert_eq!(result.output_label, "partial_failure");
    assert_eq!(result.exit_code, Some(2));
}

#[tokio::test]
async fn unmapped_non_zero_exit_code_falls_back_to_failure() {
    let root = TempRoot::new();

    let result = complete(script("exit_with.sh", &["7"]), &root.workspace()).await;

    assert_eq!(result.output_label, "failure");
    assert_eq!(result.exit_code, Some(7));
}

#[tokio::test]
async fn json_on_stdout_supplies_the_label_and_its_variables() {
    let root = TempRoot::new();

    let payload = r#"{"choice":"success","variables":{"pr_number":123}}"#;
    let result = complete(script("print_json.sh", &[payload]), &root.workspace()).await;

    assert_eq!(result.output_label, "success");
    assert_eq!(result.variables.get("pr_number"), Some(&json!(123)));
}

#[tokio::test]
async fn json_on_stdout_beats_the_exit_code_mapping() {
    let root = TempRoot::new();

    let payload = r#"{"choice":"needs_human","additional_data":{"findings":3}}"#;
    let mut config = script("print_json.sh", &[payload, "3"]);
    config.exit_codes.insert(3, "failure".to_string());

    let result = complete(config, &root.workspace()).await;

    assert_eq!(result.output_label, "needs_human");
    assert_eq!(result.exit_code, Some(3));
    assert_eq!(result.additional_data.get("findings"), Some(&json!(3)));
}

/// KTD3: a script answers with the agent's contract, `reason` included.
#[tokio::test]
async fn a_script_choice_brings_its_reason() {
    let root = TempRoot::new();

    let payload = r#"{"choice":"green","reason":"all checks"}"#;
    let result = complete(script("print_json.sh", &[payload]), &root.workspace()).await;

    assert_eq!(result.output_label, "green");
    assert_eq!(result.reason, "all checks");
}

#[tokio::test]
async fn a_script_names_its_artifacts_and_lists_additional_ones() {
    let root = TempRoot::new();

    let payload = r#"{"choice":"created","artifacts":{"pull_request":"https://example.invalid/pr/1"},
                      "additional_artifacts":["logs/build.txt"]}"#;
    let result = complete(script("print_json.sh", &[payload]), &root.workspace()).await;

    assert_eq!(
        result.artifacts.get("pull_request").map(String::as_str),
        Some("https://example.invalid/pr/1")
    );
    assert_eq!(result.additional_artifacts, vec!["logs/build.txt".to_string()]);
}

/// A choice whose other fields have the wrong shape is an error, not a
/// quiet fall-back that would lose part of the answer.
#[tokio::test]
async fn a_choice_with_malformed_artifacts_is_an_action_error() {
    let root = TempRoot::new();

    let payload = r#"{"choice":"created","artifacts":["logs/build.txt"]}"#;
    let error = execute(script("print_json.sh", &[payload]), &root.workspace(), None)
        .await
        .expect_err("a malformed answer is an error");

    assert_eq!(error.error_type.as_deref(), Some("invalid_script_output"));
    assert!(error.message.contains("'artifacts'"), "{}", error.message);
}

/// R22: the old `output` key is not the contract any more; the exit code
/// decides.
#[tokio::test]
async fn the_old_output_key_falls_back_to_the_exit_code() {
    let root = TempRoot::new();

    let result = complete(
        script("print_json.sh", &[r#"{"output":"needs_human"}"#]),
        &root.workspace(),
    )
    .await;

    assert_eq!(result.output_label, "success");
}

#[tokio::test]
async fn configured_environment_variables_reach_the_child() {
    let root = TempRoot::new();

    let mut config = script("print_env.sh", &["CI", "DB_HOST", "ORCHESTRATOR_ACTION_ID"]);
    config.env.insert("CI".to_string(), "true".to_string());
    config
        .env
        .insert("DB_HOST".to_string(), "localhost".to_string());

    let result = complete(config, &root.workspace()).await;

    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(lines, vec!["true", "localhost", "action-exec-1"]);
}

#[tokio::test]
async fn arguments_are_passed_separately_and_never_joined() {
    let root = TempRoot::new();

    let result = complete(
        script("print_args.sh", &["--message", "two words", "--verbose"]),
        &root.workspace(),
    )
    .await;

    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(lines, vec!["--message", "two words", "--verbose"]);
}

// ---------------------------------------------------------------------------
// Edge cases around parsing stdout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_json_stdout_falls_back_to_the_exit_code() {
    let root = TempRoot::new();

    let mut config = script("print_log_line.sh", &["4"]);
    config.exit_codes.insert(4, "tests_failed".to_string());

    let result = complete(config, &root.workspace()).await;

    assert_eq!(result.output_label, "tests_failed");
    assert!(result.stdout.contains("3 passed, 1 failed"));
}

#[tokio::test]
async fn json_without_a_choice_key_falls_back_to_the_exit_code() {
    let root = TempRoot::new();

    let mut config = script("print_json.sh", &[r#"{"status":"ok","count":2}"#, "5"]);
    config.exit_codes.insert(5, "needs_human".to_string());

    let result = complete(config, &root.workspace()).await;

    assert_eq!(result.output_label, "needs_human");
    assert!(result.additional_data.is_empty());
}

#[tokio::test]
async fn very_long_stdout_is_truncated_and_the_action_still_completes() {
    let root = TempRoot::new();

    // 100 lines of 1025 bytes — comfortably past the cap.
    let result = complete(script("print_long_line.sh", &["100"]), &root.workspace()).await;

    assert_eq!(result.output_label, "success");
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.ends_with(TRUNCATION_MARKER));
    assert_eq!(
        result.stdout.len(),
        MAX_CAPTURED_OUTPUT_BYTES + TRUNCATION_MARKER.len()
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_command_that_does_not_exist_is_an_action_error() {
    let root = TempRoot::new();

    let mut config = script("exit_with.sh", &[]);
    config.command = "/definitely/not/a/real/command".to_string();

    let error = execute(config, &root.workspace(), None)
        .await
        .expect_err("a missing command must not complete");

    assert_eq!(error.error_type.as_deref(), Some("spawn_failed"));
    assert!(
        error.message.contains("/definitely/not/a/real/command"),
        "message should name the command: {}",
        error.message
    );
    // The OS message, not a hand-written one.
    assert!(
        error.message.to_lowercase().contains("no such file"),
        "message should carry the OS error: {}",
        error.message
    );
}

#[tokio::test]
async fn exceeding_the_time_limit_kills_the_process() {
    let root = TempRoot::new();
    let pid_file = root.path().join("child.pid");

    let outcome = execute(
        script("sleep_forever.sh", &[&pid_file.to_string_lossy()]),
        &root.workspace(),
        Some(Duration::from_millis(300)),
    )
    .await
    .expect("a timeout is an outcome, not a failure");

    let ActionOutcome::TimedOut(timed_out) = outcome else {
        panic!("expected a timeout, got {outcome:?}");
    };
    assert!(timed_out.subject.contains("sleep_forever.sh"));

    // The process is really gone, not merely no longer awaited.
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("the script recorded its pid")
        .trim()
        .parse()
        .expect("pid is a number");
    assert!(!process_is_alive(pid), "process {pid} survived the timeout");

    // A state that declares `timeout` gets the label...
    let declared = vec![TIMEOUT_LABEL.to_string(), "success".to_string()];
    let result = timed_out
        .clone()
        .resolve(&declared)
        .expect("a declared timeout output resolves to a label");
    assert_eq!(result.output_label, TIMEOUT_LABEL);

    // ...and a state that does not gets an error naming the missing output.
    let error = timed_out
        .resolve(&["success".to_string()])
        .expect_err("an undeclared timeout output is an error");
    assert_eq!(error.error_type.as_deref(), Some("timeout"));
    assert!(error.message.contains(TIMEOUT_LABEL));
}

#[tokio::test]
async fn a_process_killed_by_a_signal_is_an_action_error() {
    let root = TempRoot::new();

    let error = execute(script("kill_self.sh", &[]), &root.workspace(), None)
        .await
        .expect_err("a signalled process must not be reported as a quiet failure");

    assert_eq!(error.error_type.as_deref(), Some("killed_by_signal"));
    assert!(
        error.message.contains("signal 9"),
        "message should name the signal: {}",
        error.message
    );
}

/// Whether a process id still exists. `/proc` is authoritative on Linux and the
/// child has already been reaped, so a surviving entry means a surviving
/// process.
fn process_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

// ---------------------------------------------------------------------------
// Integration with the workspace (U8) and the template scope (U4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_process_runs_in_the_workspace_not_in_the_narsor_directory() {
    let root = TempRoot::new();

    let result = complete(script("print_cwd.sh", &[]), &root.workspace()).await;

    let reported = PathBuf::from(result.stdout.trim());
    let expected = fs::canonicalize(root.path()).expect("canonical workspace path");
    assert_eq!(reported, expected);
    assert_ne!(reported, PathBuf::from(env!("CARGO_MANIFEST_DIR")));
}

#[tokio::test]
async fn a_relative_working_dir_is_resolved_against_the_workspace() {
    let root = TempRoot::new();
    fs::create_dir_all(root.path().join("services/api")).expect("create nested dir");

    let mut config = script("print_cwd.sh", &[]);
    config.working_dir = Some("services/api".to_string());

    let result = complete(config, &root.workspace()).await;

    let expected =
        fs::canonicalize(root.path().join("services/api")).expect("canonical nested path");
    assert_eq!(PathBuf::from(result.stdout.trim()), expected);
}

#[tokio::test]
async fn a_script_writing_a_file_creates_it_inside_the_workspace() {
    let root = TempRoot::new();

    let result = complete(
        script("write_file.sh", &["report.txt", "3 findings"]),
        &root.workspace(),
    )
    .await;
    assert_eq!(result.output_label, "success");

    let written = root.path().join("report.txt");
    assert_eq!(
        fs::read_to_string(&written).expect("the file lands in the workspace"),
        "3 findings"
    );
    assert!(
        !Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("report.txt")
            .exists(),
        "the file must not be written into the narsor repository"
    );
}

#[tokio::test]
async fn a_command_rendered_by_the_template_scope_runs_with_the_substituted_values() {
    let root = TempRoot::new();

    let mut inputs = Map::new();
    inputs.insert("branch".to_string(), json!("feature/login"));

    let mut context = Context::new();
    context
        .variables
        .insert("pr_number".to_string(), json!(123));

    let mut config = script(
        "print_args.sh",
        &["{{ inputs.branch }}", "{{ context.pr_number }}"],
    );
    config
        .env
        .insert("TARGET".to_string(), "{{ inputs.branch }}".to_string());

    let rendered = TemplateScope::new(&inputs, &context)
        .render_action(&ActionConfig::Script(config), "state 'run_tests'")
        .expect("the action renders");

    let ActionConfig::Script(rendered) = rendered else {
        panic!("rendering a script action must yield a script action");
    };
    assert_eq!(
        rendered.env.get("TARGET").map(String::as_str),
        Some("feature/login")
    );

    let result = complete(rendered, &root.workspace()).await;

    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(lines, vec!["feature/login", "123"]);
}

#[tokio::test]
async fn stdout_beyond_the_buffering_ceiling_does_not_grow_memory_without_bound() {
    let root = TempRoot::new();

    // 12 MiB of stdout — past MAX_BUFFERED_OUTPUT_BYTES (8 MiB), so older
    // bytes are dropped as newer diagnostics arrive. The script must still
    // run to completion: the pipe keeps being drained past the ceiling.
    let result = complete(script("print_long_line.sh", &["12288"]), &root.workspace()).await;

    assert_eq!(result.output_label, "success");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        result.stdout.len(),
        MAX_CAPTURED_OUTPUT_BYTES + TRUNCATION_MARKER.len()
    );
}
