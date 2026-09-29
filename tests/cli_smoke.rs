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
    let output = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root)
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
    assert!(root.path().join("executions").join(&id).is_dir());
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
            root.path()
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
    assert!(root.path().join("tasks").join(&id).join("code").is_dir());
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
    let id = execution_id(&invocation.stdout);

    // The checkout is named after the registry id and holds the marker file of
    // the registered repository — so the registry, not the raw value, decided
    // which remote was cloned.
    let checkout = root
        .path()
        .join("tasks")
        .join(&id)
        .join("code")
        .join("acme-api");
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
    let id = execution_id(&invocation.stdout);

    // Nothing was registered, so the value was used as a raw remote and the
    // checkout is named after the last path segment, exactly as before.
    let checkout = root
        .path()
        .join("tasks")
        .join(&id)
        .join("code")
        .join("raw-repo");
    assert!(checkout.is_dir(), "expected a checkout at {checkout:?}");
    assert_eq!(
        fs::read_to_string(checkout.join("marker.txt")).expect("read the marker"),
        "from the raw path"
    );
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
    let state = fs::read_to_string(root.path().join("executions").join(&id).join("state.json"))
        .expect("read the stored execution");
    assert!(
        state.contains("\"definition_name\": \"greet\""),
        "the name is recorded beside the path: {state}"
    );
    assert!(
        state.contains("definition_path"),
        "and the path is still recorded, because there is one: {state}"
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

    let store = ExecutionStore::new(root.path());
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
