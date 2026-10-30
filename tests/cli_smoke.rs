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
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-cli"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root)
        .env_remove("NARSOR_SERVER")
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

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn a_missing_command_line_argument_is_a_usage_error() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["show"]);

    assert_eq!(
        invocation.code, EXIT_USAGE,
        "clap's own code must match ours"
    );
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The repository registry
// ---------------------------------------------------------------------------

#[test]
fn repo_add_without_the_required_remote_is_a_usage_error() {
    let root = TempRoot::new();

    let added = cli(&root, &["repo", "add", "acme-api"]);

    assert_eq!(added.code, EXIT_USAGE, "clap's own code must match ours");
}

// ---------------------------------------------------------------------------
// resolving a workflow by name (U12)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// steps / conversation (U8)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// resume (U6): now a REST client of a daemon, not a direct `Engine` call
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Conductor plan U5: resolving a problem from the command line
// ---------------------------------------------------------------------------

#[path = "engine_support/mod.rs"]
mod engine_support;
mod environment_support;
mod remote_support;

#[test]
fn input_with_a_malformed_artifact_pair_is_a_usage_error() {
    let root = TempRoot::new();
    let invocation = cli(
        &root,
        &[
            "input",
            "any-id",
            "--label",
            "implemented",
            "--artifact",
            "no-equals-sign",
        ],
    );
    assert_eq!(invocation.code, EXIT_USAGE, "{invocation:?}");
    assert!(invocation.output().contains("--artifact"), "{invocation:?}");
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
        let info = narsor_engine::persistence::DaemonInfo::read(
            &narsor_engine::persistence::StatePaths::new(root.path()).daemon_file(),
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
            let mut command = Command::new(env!("CARGO_BIN_EXE_narsor-cli"));
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
            &["task", "create", "implement-feature", "--task-id", "t-2"],
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
        assert!(show.stdout.contains("State      queued"), "{}", show.stdout);

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

        // A `local` backend on a workstation needs the owner's consent (R15).
        let refused = remote(&harness, token, &["controller", "enroll-code", "local"]).await;
        assert_ne!(refused.code, EXIT_SUCCESS, "{}", refused.output());
        assert!(
            refused.output().contains("workstation_risk_not_accepted"),
            "{}",
            refused.output()
        );
        let accepted = remote(
            &harness,
            token,
            &[
                "controller",
                "enroll-code",
                "local",
                "--accept-workstation-risk",
            ],
        )
        .await;
        assert_eq!(accepted.code, EXIT_SUCCESS, "{}", accepted.output());
        assert!(
            accepted.stdout.contains("Enrollment code: "),
            "{}",
            accepted.stdout
        );
        assert!(
            accepted.stderr.contains("WARNING") && accepted.stderr.contains("without a container"),
            "{}",
            accepted.stderr
        );
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
        asking.attention = vec![narsor_engine::orchestration::AttentionItem {
            kind: narsor_engine::orchestration::AttentionKind::Question,
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
        let malformed = remote(
            &harness,
            token,
            &["task", "input", "t-ask", "--artifact", "n"],
        )
        .await;
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
                "--artifact",
                "plan=docs/plan.md",
                "--artifact",
                "notes=docs/notes 1.md",
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
        assert_eq!(delivery.artifacts["plan"], "docs/plan.md");
        assert_eq!(delivery.artifacts["notes"], "docs/notes 1.md");
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
        assert_eq!(tasks[0]["runner_state"], "archived");
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
                    .contains("Supported: claude-code, codex, github-copilot, opencode"),
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
                && unknown
                    .stderr
                    .contains("claude-code, codex, github-copilot, opencode"),
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

        // `--json` prints the selection itself, for get and for a change.
        let set = remote(
            &harness,
            token,
            &["harness", "default", "set", "codex", "--json"],
        )
        .await;
        assert_eq!(set.code, EXIT_SUCCESS, "{}", set.output());
        let selection: serde_json::Value =
            serde_json::from_str(&set.stdout).expect("the selection as JSON");
        assert_eq!(selection["effective"], "codex", "{}", set.stdout);
        assert_eq!(selection["override"], "codex", "{}", set.stdout);
        let get = remote(&harness, token, &["harness", "default", "--json"]).await;
        assert_eq!(get.code, EXIT_SUCCESS, "{}", get.output());
        let selection: serde_json::Value =
            serde_json::from_str(&get.stdout).expect("the selection as JSON");
        assert_eq!(selection["effective"], "codex", "{}", get.stdout);
        assert_eq!(selection["configured"], "claude-code", "{}", get.stdout);
        assert_eq!(
            selection["supported"],
            serde_json::json!(["claude-code", "codex", "github-copilot", "opencode"]),
            "{}",
            get.stdout
        );
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
        use narsor_engine::controller_protocol::SubmitOperatorCommandRequest;
        use narsor_engine::orchestration::OperatorCommand;

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
        with_step.timeline = Some(narsor_engine::engine::Timeline {
            positions: vec![narsor_engine::engine::Position {
                label: "plan\u{1b}[2J#1".to_string(),
                state_id: "plan".to_string(),
                visit: 1,
                action_execution_id: None,
                action_type: Some("ag\u{1b}]8;;x\u{7}ent\u{202e}".to_string()),
                status: narsor_engine::engine::PositionStatus::Failed,
                started_at: None,
                ended_at: None,
                error: Some("bad\u{1b}[31m".to_string()),
                resumed_harness_session: None,
                harness_session_interrupted: false,
                retry_pending: false,
                conversation: narsor_engine::engine::ConversationState::NoRecord,
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
        // execution cannot be read once the workload is gone and nobody
        // knows whether its runner is (an unknown task is not read by a
        // reader: the old runner may still hold the storage).
        let steps = remote(&harness, token, &["task", "steps", "t-esc"]).await;
        assert_eq!(steps.code, EXIT_SUCCESS, "{}", steps.output());
        assert!(steps.stdout.contains("No steps."), "{}", steps.stdout);
        running
            .controller
            .report(&super::remote_support::report(
                "t-esc",
                running.epoch,
                1,
                narsor_engine::orchestration::Observation::WorkloadLost { state_known: false },
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
// `narsor`: the TUI, `--remote` an ignored alias (U14)
// ---------------------------------------------------------------------------

#[test]
fn tui_remote_alias_is_warned_about_and_plain_tui_is_not() {
    let warning = narsor::cli::tui_remote_warning(true).expect("the alias warns");
    assert!(warning.contains("--remote"), "{warning}");
    assert!(warning.contains("ignored"), "{warning}");
    assert_eq!(narsor::cli::tui_remote_warning(false), None);
}
