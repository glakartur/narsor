//! Integration tests of the observer worker (U6): `narsor-cli observer
//! worker`'s claim, script execution and result, against a real
//! `ProtocolService` over a temporary state root, without a server.
//!
//! The control plane is reached through the worker's [`WorkerApi`] seam,
//! implemented here on the service itself. Where the process is not the
//! subject the script runner is a fake that records what it was asked to
//! start; where it is (time limit, process group, environment), the tests
//! run real `/bin/sh` scripts with short limits, as `actions_script` does.
//! The test that needs two system users is `#[ignore]`d.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use narsor::api::ClientError;
use narsor::cli::observer_worker::{
    check_credential_file, check_secrets_dir, current_uid, kill_all_processes_of, processes_of,
    Masker, ProcessRunner, ScriptEnd, ScriptExit, ScriptRunner, ScriptSpec, ScriptUser, Worker,
    WorkerApi, WorkerOptions, WorkerStop, MASK, MAX_STDERR_BYTES, MAX_STDOUT_BYTES, SCRIPT_FILE,
};
use narsor_engine::controller_protocol::error::ProtocolError;
use narsor_engine::controller_protocol::service::MAX_STORED_LOG_BYTES;
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::domain::WorkflowDefinition;
use narsor_engine::observers::{
    CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverId, ObserverName, RunId, RunStatus,
    SecretRef, WatchMode, MAX_SCRIPT_OUTPUT_BYTES,
};
use narsor_engine::orchestration::{Clock, ControlPlane, OwnerRef};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::controller::{
    ObserverRunOutcome, ObserverRunReport, ObserverRunResultResponse, ObserverWorkerRun,
};
use serde_json::json;

const SENTINEL: &str = "s3cr3t-SENTINEL-4f1c9a";

/// A clock the test moves by hand, starting at the real time: the worker
/// cuts the script's limit by the run's deadline on the wall clock.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// A state root, a service on it, and a directory for the worker's files.
struct Fixture {
    root: PathBuf,
    service: Arc<ProtocolService>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-obs-worker-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(Utc::now())));
        let service = Arc::new(ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(root.join("state")), clock),
            crate::environment_support::seeded_resolver(&root.join("state")),
        ));
        Self { root, service }
    }

    fn state_root(&self) -> PathBuf {
        self.root.join("state")
    }

    /// A secrets directory (0700) holding `files`.
    fn secrets_dir(&self, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.root.join("secrets");
        fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o700);
        for (name, value) in files {
            fs::write(dir.join(name), format!("{value}\n")).unwrap();
        }
        dir
    }

    fn options(&self) -> WorkerOptions {
        let mut options = WorkerOptions::new("https://narsor.example.com");
        options.work_dir = self.root.join("work");
        options.path = "/usr/bin:/bin".to_string();
        options.kill_grace = StdDuration::from_millis(300);
        options
    }

    /// An active "create a task" observer running `script`, with the
    /// secret references `secrets`, its first run due.
    fn observer(&self, script: &str, secrets: &[(&str, &str)]) -> ObserverId {
        let mut config = ObserverConfig::new(
            ObserverName::parse("jira-new").unwrap(),
            "jira",
            ObserverEffect::CreateTask(CreateTaskEffect {
                workflow_id: "triage".into(),
                workflow_version: 1,
                workspaces: Default::default(),
                backend_id: None,
                profile: None,
                mapping: [("ticket".to_string(), "key".to_string())].into(),
            }),
        );
        config.watch_mode = WatchMode::All;
        config.secrets = secrets
            .iter()
            .map(|(name, env)| SecretRef {
                name: (*name).to_string(),
                env: (*env).to_string(),
            })
            .collect();
        let workflow: WorkflowDefinition = serde_json::from_value(json!({
            "id": "triage",
            "name": "triage",
            "version": 1,
            "inputs": [{"name": "ticket", "required": true}],
            "states": {},
            "initial_state": "start",
        }))
        .unwrap();
        let store = self.service.observers();
        let now = self.service.control_plane().now();
        let id = store
            .create(owner(), config, Some(&workflow), now)
            .unwrap()
            .observer_id;
        store.add_script(&id, script, now).unwrap();
        self.service.approve_observer_version(&id, 1).unwrap();
        self.service.observer_pass();
        id
    }

    fn api(&self) -> Arc<ServiceApi> {
        Arc::new(ServiceApi {
            service: Arc::clone(&self.service),
            owner: owner(),
        })
    }

    /// How the run `handled_run` ended, from its observer's record.
    fn status_of(&self, handled_run: &str) -> RunStatus {
        let run_id = RunId::parse(handled_run).unwrap();
        let last_run = self
            .service
            .observers()
            .load(&run_id.observer_id)
            .unwrap()
            .last_run
            .expect("a finished run");
        assert_eq!(last_run.sequence, run_id.sequence);
        last_run.status
    }

    /// The error file of the failed run `handled_run`.
    fn failure_of(&self, handled_run: &str) -> narsor_engine::observers::RunFailure {
        let run_id = RunId::parse(handled_run).unwrap();
        self.service
            .observers()
            .list_failures(&run_id.observer_id)
            .unwrap()
            .into_iter()
            .find(|failure| failure.sequence == run_id.sequence)
            .expect("the run's error file")
    }

    /// Every file under the state root, as text.
    fn stored_text(&self) -> String {
        let mut text = String::new();
        collect_text(&self.state_root(), &mut text);
        text
    }
}

fn collect_text(dir: &Path, into: &mut String) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_text(&path, into);
        } else if let Ok(bytes) = fs::read(&path) {
            into.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The worker's control plane, without HTTP: the service's own claim and
/// result for the worker's owner, as the worker routes call them.
struct ServiceApi {
    service: Arc<ProtocolService>,
    owner: OwnerRef,
}

fn client_error(error: ProtocolError) -> ClientError {
    match error {
        ProtocolError::Unauthorized => ClientError::Unauthorized,
        ProtocolError::NotFound => ClientError::NotFound,
        ProtocolError::Conflict { .. } => ClientError::Conflict,
        other => ClientError::Server(other.to_string()),
    }
}

impl WorkerApi for ServiceApi {
    fn claim(&self) -> BoxFuture<'_, Result<Option<ObserverWorkerRun>, ClientError>> {
        let claimed = self
            .service
            .claim_observer_run_for(&self.owner)
            .map(|claimed| claimed.map(|claimed| claimed.into_wire()))
            .map_err(client_error);
        Box::pin(async move { claimed })
    }

    fn report<'a>(
        &'a self,
        run_id: &'a str,
        report: &'a ObserverRunReport,
    ) -> BoxFuture<'a, Result<ObserverRunResultResponse, ClientError>> {
        let result = self
            .service
            .finish_observer_run_for(&self.owner, &RunId::parse(run_id).unwrap(), report.clone())
            .map(|run| ObserverRunResultResponse {
                run_id: run_id.to_string(),
                status: serde_json::to_value(run.status)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                error: run.error,
            })
            .map_err(client_error);
        Box::pin(async move { result })
    }
}

/// A control plane that refuses the worker's credential.
struct RefusingApi;

impl WorkerApi for RefusingApi {
    fn claim(&self) -> BoxFuture<'_, Result<Option<ObserverWorkerRun>, ClientError>> {
        Box::pin(async { Err(ClientError::Unauthorized) })
    }

    fn report<'a>(
        &'a self,
        _run_id: &'a str,
        _report: &'a ObserverRunReport,
    ) -> BoxFuture<'a, Result<ObserverRunResultResponse, ClientError>> {
        Box::pin(async { Err(ClientError::Unauthorized) })
    }
}

type Reply = Box<dyn Fn(&ScriptSpec) -> io::Result<ScriptExit> + Send + Sync>;

/// Starts nothing: records each spec and answers with `reply`.
struct FakeRunner {
    specs: Mutex<Vec<ScriptSpec>>,
    reply: Reply,
}

impl FakeRunner {
    fn new(
        reply: impl Fn(&ScriptSpec) -> io::Result<ScriptExit> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            specs: Mutex::new(Vec::new()),
            reply: Box::new(reply),
        })
    }

    fn exiting(code: i32, stdout: &str, stderr: &str) -> Arc<Self> {
        let (stdout, stderr) = (stdout.to_string(), stderr.to_string());
        Self::new(move |_| Ok(exit(ScriptEnd::Exited(code), &stdout, &stderr)))
    }

    fn specs(&self) -> Vec<ScriptSpec> {
        self.specs.lock().unwrap().clone()
    }
}

impl ScriptRunner for FakeRunner {
    fn run(&self, spec: ScriptSpec) -> BoxFuture<'_, io::Result<ScriptExit>> {
        // The run directory exists while the script runs, with the script.
        assert!(spec.cwd.join(SCRIPT_FILE).is_file());
        let reply = (self.reply)(&spec);
        self.specs.lock().unwrap().push(spec);
        Box::pin(async move { reply })
    }
}

fn exit(end: ScriptEnd, stdout: &str, stderr: &str) -> ScriptExit {
    ScriptExit {
        end,
        stdout: stdout.as_bytes().to_vec(),
        stdout_total: stdout.len() as u64,
        stderr: stderr.as_bytes().to_vec(),
        stderr_total: stderr.len() as u64,
    }
}

fn env_names(spec: &ScriptSpec) -> Vec<&str> {
    let mut names: Vec<&str> = spec.env.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    names
}

fn env_value<'a>(spec: &'a ScriptSpec, name: &str) -> Option<&'a str> {
    spec.env
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// Limits shared with the control plane
// ---------------------------------------------------------------------------

#[test]
fn the_workers_limits_are_the_control_planes() {
    assert_eq!(MAX_STDOUT_BYTES, MAX_SCRIPT_OUTPUT_BYTES);
    assert_eq!(MAX_STDERR_BYTES, MAX_STORED_LOG_BYTES);
}

// ---------------------------------------------------------------------------
// Happy path, environment, secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_script_reading_its_input_saves_the_state_it_prints() {
    let fx = Fixture::new();
    // Exits 3 unless its stdin is the claim's input document.
    let id = fx.observer(
        "input=$(cat)\n\
         case \"$input\" in *'\"name\":\"jira-new\"'*) ;; *) exit 3 ;; esac\n\
         printf '{\"state\": {\"cursor\": 5}}'\n",
        &[],
    );
    let worker = Worker::new(fx.api(), ProcessRunner, fx.options());

    let handled = worker.run_once().await.unwrap().expect("a run was due");

    assert_eq!(
        handled.report.outcome,
        ObserverRunOutcome::Exited { exit_code: 0 }
    );
    assert_eq!(handled.accepted.unwrap().status, "succeeded");
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.script_state, Some(json!({"cursor": 5})));
    assert_eq!(fx.status_of(&handled.run_id), RunStatus::Succeeded);
    // The run directory is gone.
    assert_eq!(fs::read_dir(fx.root.join("work")).unwrap().count(), 0);
    // Nothing more is due.
    assert!(worker.run_once().await.unwrap().is_none());
}

#[tokio::test]
async fn the_script_gets_the_explicit_list_and_nothing_else() {
    let fx = Fixture::new();
    let secrets = fx.secrets_dir(&[("jira-token", SENTINEL)]);
    fx.observer("true\n", &[("jira-token", "JIRA_TOKEN")]);
    let runner = FakeRunner::exiting(0, r#"{"state": {}}"#, "");
    let mut options = fx.options();
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), Arc::clone(&runner), options);

    let handled = worker.run_once().await.unwrap().unwrap();

    let specs = runner.specs();
    let spec = &specs[0];
    assert_eq!(
        env_names(spec),
        [
            "HOME",
            "JIRA_TOKEN",
            "NARSOR_OBSERVER_RUN",
            "NARSOR_SERVER",
            "NARSOR_TOKEN",
            "PATH"
        ]
    );
    assert_eq!(env_value(spec, "JIRA_TOKEN"), Some(SENTINEL));
    assert_eq!(
        env_value(spec, "NARSOR_OBSERVER_RUN"),
        Some(handled.run_id.as_str())
    );
    assert!(env_value(spec, "NARSOR_TOKEN").unwrap().starts_with("nso."));
    assert_eq!(
        env_value(spec, "NARSOR_SERVER"),
        Some("https://narsor.example.com")
    );
    assert_eq!(env_value(spec, "HOME"), spec.cwd.to_str());
    assert_eq!(spec.args, [SCRIPT_FILE]);
    let input: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
    assert_eq!(input["observer"]["name"], "jira-new");
    assert_eq!(spec.timeout, StdDuration::from_secs(300));
}

#[tokio::test]
async fn the_worker_environment_does_not_reach_a_real_script() {
    // The worker's own variables — a GH_TOKEN among them, in a real
    // deployment — are not passed on: the process starts from an empty
    // environment. Checked against every variable of this test process.
    let fx = Fixture::new();
    fx.observer("env >&2\nprintf '{\"state\": {}}'\n", &[]);
    let worker = Worker::new(fx.api(), ProcessRunner, fx.options());

    let handled = worker.run_once().await.unwrap().unwrap();

    let seen: Vec<&str> = handled
        .report
        .stderr
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    let allowed = [
        "PATH",
        "HOME",
        "NARSOR_SERVER",
        "NARSOR_TOKEN",
        "NARSOR_OBSERVER_RUN",
        // Set by the shell itself.
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ];
    for name in &seen {
        assert!(allowed.contains(name), "the script saw {name}: {seen:?}");
    }
    for (name, _) in std::env::vars() {
        if !allowed.contains(&name.as_str()) {
            assert!(!seen.contains(&name.as_str()), "{name} leaked");
        }
    }
    assert!(handled.report.stderr.contains("PATH=/usr/bin:/bin"));
}

#[tokio::test]
async fn gh_token_reaches_the_script_only_as_a_reference_of_the_observer() {
    let fx = Fixture::new();
    let secrets = fx.secrets_dir(&[("github", SENTINEL), ("other", "unused-value")]);
    fx.observer("true\n", &[("github", "GH_TOKEN")]);
    let runner = FakeRunner::exiting(0, r#"{"state": {}}"#, "");
    let mut options = fx.options();
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), Arc::clone(&runner), options);

    worker.run_once().await.unwrap().unwrap();

    let spec = &runner.specs()[0];
    assert_eq!(env_value(spec, "GH_TOKEN"), Some(SENTINEL));
    // A file of the secrets directory no reference names stays there.
    assert!(spec.env.iter().all(|(_, value)| value != "unused-value"));
}

#[tokio::test]
async fn a_missing_secret_file_fails_the_run_before_the_script_starts() {
    let fx = Fixture::new();
    let secrets = fx.secrets_dir(&[("present", SENTINEL)]);
    let id = fx.observer("true\n", &[("present", "PRESENT"), ("absent", "ABSENT")]);
    let runner = FakeRunner::exiting(0, r#"{"state": {}}"#, "");
    let mut options = fx.options();
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), Arc::clone(&runner), options);

    let handled = worker.run_once().await.unwrap().unwrap();

    assert!(runner.specs().is_empty(), "the script must not start");
    let ObserverRunOutcome::WorkerError { message } = &handled.report.outcome else {
        panic!("{:?}", handled.report.outcome);
    };
    assert!(message.contains("'absent'"), "{message}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert_eq!(fx.status_of(&handled.run_id), RunStatus::WorkerError);
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.script_state, None);
    assert_eq!(record.consecutive_failures, 1);
    assert_eq!(fs::read_dir(fx.root.join("work")).unwrap().count(), 0);
}

#[tokio::test]
async fn a_secret_without_a_secrets_directory_fails_the_run() {
    let fx = Fixture::new();
    fx.observer("true\n", &[("jira-token", "JIRA_TOKEN")]);
    let runner = FakeRunner::exiting(0, r#"{"state": {}}"#, "");
    let worker = Worker::new(fx.api(), Arc::clone(&runner), fx.options());

    let handled = worker.run_once().await.unwrap().unwrap();

    assert!(runner.specs().is_empty());
    assert!(matches!(
        &handled.report.outcome,
        ObserverRunOutcome::WorkerError { message } if message.contains("--secrets-dir")
    ));
}

// ---------------------------------------------------------------------------
// Masking (sentinel)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn secret_values_and_the_token_never_reach_the_server() {
    let fx = Fixture::new();
    let secrets = fx.secrets_dir(&[("jira-token", SENTINEL)]);
    let id = fx.observer(
        "echo \"out $JIRA_TOKEN $NARSOR_TOKEN\"\n\
         printf 'split %s\\001%s\\n' \"$(printf %s \"$JIRA_TOKEN\" | cut -c1-6)\" \"$(printf %s \"$JIRA_TOKEN\" | cut -c7-)\" >&2\n\
         echo \"err $JIRA_TOKEN $NARSOR_TOKEN\" >&2\n\
         exit 1\n",
        &[("jira-token", "JIRA_TOKEN")],
    );
    let mut options = fx.options();
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), ProcessRunner, options);

    let handled = worker.run_once().await.unwrap().unwrap();

    assert_eq!(
        handled.report.outcome,
        ObserverRunOutcome::Exited { exit_code: 1 }
    );
    let run = fx.failure_of(&handled.run_id);
    let stdout = run.stdout.clone().unwrap();
    let stderr = run.stderr.clone().unwrap();
    assert_eq!(stdout, format!("out {MASK} {MASK}\n"));
    assert!(stderr.contains(&format!("err {MASK} {MASK}")), "{stderr}");
    // Cut in two by a control character, it is still masked once the
    // character is gone.
    assert!(stderr.contains(&format!("split {MASK}")), "{stderr}");
    let stored = fx.stored_text();
    assert!(!stored.contains(SENTINEL));
    assert!(!stored.contains(&SENTINEL[..12]));
    // No record keeps a run token; one in the text would be the token
    // leaking through the script's output.
    assert!(!stored.contains("nso."));
    assert_eq!(fx.service.observers().load(&id).unwrap().script_state, None);
}

#[tokio::test]
async fn an_error_message_holding_a_secret_is_masked() {
    let fx = Fixture::new();
    let secrets = fx.secrets_dir(&[("jira-token", SENTINEL)]);
    fx.observer("true\n", &[("jira-token", "JIRA_TOKEN")]);
    let runner = FakeRunner::new(|spec| {
        let token = env_value(spec, "NARSOR_TOKEN").unwrap();
        Err(io::Error::other(format!(
            "exec failed near {SENTINEL} and {token}"
        )))
    });
    let mut options = fx.options();
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), Arc::clone(&runner), options);

    let handled = worker.run_once().await.unwrap().unwrap();

    let ObserverRunOutcome::WorkerError { message } = &handled.report.outcome else {
        panic!("{:?}", handled.report.outcome);
    };
    assert_eq!(
        message,
        &format!("cannot start the script: exec failed near {MASK} and {MASK}")
    );
    assert!(!fx.stored_text().contains(SENTINEL));
    assert!(!fx.stored_text().contains("nso."));
}

#[test]
fn the_masker_masks_longest_first_and_strips_control_characters() {
    let masker = Masker::new(["abc", "abcdef", "", "x\u{1}y"]);

    assert_eq!(
        masker.mask("1 abcdef 2 abc 3"),
        format!("1 {MASK} 2 {MASK} 3")
    );
    assert_eq!(masker.mask("ab\u{7}c\u{1b}[31m"), format!("{MASK}[31m"));
    assert_eq!(masker.mask("x\u{1}y and xy"), format!("{MASK} and {MASK}"));
    assert_eq!(masker.mask("line\nnext\ttab"), "line\nnext tab");
    assert_eq!(masker.longest(), 6);
}

#[tokio::test]
async fn a_secret_cut_by_the_stderr_limit_is_masked_whole() {
    let fx = Fixture::new();
    let secrets = fx.secrets_dir(&[("jira-token", SENTINEL)]);
    fx.observer("true\n", &[("jira-token", "JIRA_TOKEN")]);
    // The secret starts a few bytes before the limit.
    let runner = FakeRunner::new(|spec| {
        let mut stderr = "e".repeat(MAX_STDERR_BYTES - 4);
        stderr.push_str(SENTINEL);
        stderr.push_str(&"f".repeat(1000));
        let kept = &stderr.as_bytes()[..spec.stderr_limit];
        Ok(ScriptExit {
            end: ScriptEnd::Exited(0),
            stdout: br#"{"state": {}}"#.to_vec(),
            stdout_total: 13,
            stderr: kept.to_vec(),
            stderr_total: stderr.len() as u64,
        })
    });
    let mut options = fx.options();
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), runner, options);

    let handled = worker.run_once().await.unwrap().unwrap();

    let stderr = &handled.report.stderr;
    assert!(stderr.len() <= MAX_STDERR_BYTES);
    assert!(
        stderr.ends_with("[stderr truncated]"),
        "{}",
        &stderr[stderr.len() - 40..]
    );
    assert!(!stderr.contains(&SENTINEL[..4]));
}

// ---------------------------------------------------------------------------
// Limits and process ends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdout_over_the_limit_is_a_format_error_and_keeps_the_state() {
    let fx = Fixture::new();
    let id = fx.observer("true\n", &[]);
    let runner = FakeRunner::new(|spec| {
        let kept = vec![b'a'; spec.stdout_limit];
        Ok(ScriptExit {
            end: ScriptEnd::Exited(0),
            stdout: kept,
            stdout_total: (MAX_STDOUT_BYTES + 1) as u64,
            stderr: Vec::new(),
            stderr_total: 0,
        })
    });
    let worker = Worker::new(fx.api(), runner, fx.options());

    let handled = worker.run_once().await.unwrap().unwrap();

    assert_eq!(
        handled.report.outcome,
        ObserverRunOutcome::OutputTooLarge { exit_code: 0 }
    );
    assert!(handled.report.stdout.len() <= MAX_STDERR_BYTES);
    assert_eq!(fx.status_of(&handled.run_id), RunStatus::InvalidOutput);
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.script_state, None);
    assert_eq!(record.consecutive_failures, 1);
}

#[tokio::test]
async fn a_real_script_printing_too_much_is_a_format_error() {
    let fx = Fixture::new();
    fx.observer("head -c 1100000 /dev/zero | tr '\\000' a\n", &[]);
    let worker = Worker::new(fx.api(), ProcessRunner, fx.options());

    let handled = worker.run_once().await.unwrap().unwrap();

    assert_eq!(
        handled.report.outcome,
        ObserverRunOutcome::OutputTooLarge { exit_code: 0 }
    );
    assert_eq!(fx.status_of(&handled.run_id), RunStatus::InvalidOutput);
}

/// Whether process `pid` is gone (or a zombie nobody waits for any more).
fn process_gone(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

fn wait_until_gone(pid: i32) -> bool {
    let started = Instant::now();
    while started.elapsed() < StdDuration::from_secs(5) {
        if process_gone(pid) {
            return true;
        }
        std::thread::sleep(StdDuration::from_millis(50));
    }
    false
}

fn pid_after(text: &str, label: &str) -> i32 {
    text.lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no '{label}' line in {text:?}"))
}

#[tokio::test]
async fn a_script_past_its_time_limit_is_killed_with_its_children() {
    let fx = Fixture::new();
    let id = fx.observer("sleep 30 &\necho \"child $!\" >&2\nsleep 30\n", &[]);
    let worker = Worker::new(fx.api(), ProcessRunner, fx.options());
    let mut run = fx.api().claim().await.unwrap().unwrap();
    run.timeout_secs = 1;

    let started = Instant::now();
    let report = worker.execute(&run).await;

    assert!(
        started.elapsed() < StdDuration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.outcome, ObserverRunOutcome::TimedOut);
    let child = pid_after(&report.stderr, "child ");
    assert!(
        wait_until_gone(child),
        "the script's child {child} survived"
    );
    let finished = fx
        .service
        .finish_observer_run(&RunId::parse(&run.run_id).unwrap(), report)
        .unwrap();
    assert_eq!(finished.status, RunStatus::TimedOut);
    assert_eq!(fx.service.observers().load(&id).unwrap().script_state, None);
}

#[tokio::test]
async fn a_background_process_holding_stdout_does_not_hang_the_worker() {
    let fx = Fixture::new();
    fx.observer(
        "sleep 30 &\necho \"child $!\" >&2\nprintf '{\"state\": {\"done\": true}}'\n",
        &[],
    );
    let worker = Worker::new(fx.api(), ProcessRunner, fx.options());

    let started = Instant::now();
    let handled = worker.run_once().await.unwrap().unwrap();

    assert!(
        started.elapsed() < StdDuration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        handled.report.outcome,
        ObserverRunOutcome::Exited { exit_code: 0 }
    );
    assert_eq!(handled.accepted.unwrap().status, "succeeded");
    let child = pid_after(&handled.report.stderr, "child ");
    assert!(
        wait_until_gone(child),
        "the background process {child} survived the run"
    );
}

#[tokio::test]
async fn a_background_process_holding_stdout_past_the_limit_ends_with_it() {
    let fx = Fixture::new();
    // The script's own shell waits for a child that never ends.
    fx.observer("( sleep 30; echo late ) &\nwait\n", &[]);
    let worker = Worker::new(fx.api(), ProcessRunner, fx.options());
    let mut run = fx.api().claim().await.unwrap().unwrap();
    run.timeout_secs = 1;

    let started = Instant::now();
    let report = worker.execute(&run).await;

    assert!(
        started.elapsed() < StdDuration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.outcome, ObserverRunOutcome::TimedOut);
}

// ---------------------------------------------------------------------------
// The credential
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_credential_stops_the_worker() {
    let fx = Fixture::new();
    let worker = Worker::new(RefusingApi, FakeRunner::exiting(0, "", ""), fx.options());

    let stopped = worker
        .serve(false, StdDuration::from_millis(10), std::future::pending())
        .await;

    assert!(matches!(stopped, Err(WorkerStop::CredentialRefused(_))));
}

#[test]
fn a_revoked_worker_credential_no_longer_authenticates() {
    let fx = Fixture::new();
    let created = fx
        .service
        .create_observer_worker(&owner(), Default::default())
        .unwrap();
    let identity = fx
        .service
        .authenticate_observer_worker(&created.credential)
        .unwrap();
    assert_eq!(identity.owner, owner());

    fx.service
        .revoke_observer_worker(&owner(), &created.worker.worker_id)
        .unwrap();

    assert!(fx
        .service
        .authenticate_observer_worker(&created.credential)
        .is_none());
    // Another owner cannot revoke or see it.
    assert!(fx
        .service
        .revoke_observer_worker(&OwnerRef::new("other"), &created.worker.worker_id)
        .is_err());
    assert!(fx
        .service
        .observer_workers(&OwnerRef::new("other"))
        .unwrap()
        .workers
        .is_empty());
}

#[test]
fn the_worker_refuses_a_credential_file_or_secrets_directory_open_to_others() {
    let fx = Fixture::new();
    let credential = fx.root.join("credential");
    fs::write(&credential, "nsw.wkr-1.secret\n").unwrap();
    set_mode(&credential, 0o644);
    let secrets = fx.secrets_dir(&[]);
    set_mode(&secrets, 0o755);

    assert!(check_credential_file(&credential).is_err());
    assert!(check_secrets_dir(&secrets).is_err());

    set_mode(&credential, 0o600);
    set_mode(&secrets, 0o700);
    assert!(check_credential_file(&credential).is_ok());
    assert!(check_secrets_dir(&secrets).is_ok());
}

#[test]
fn root_is_never_the_script_user() {
    assert!(ScriptUser::lookup("root").is_err());
    assert!(ScriptUser::lookup("no-such-user-narsor-test").is_err());
}

/// Needs root (or CAP_SETUID, CAP_SETGID, CAP_CHOWN and CAP_KILL) and an
/// unprivileged system user named by `NARSOR_TEST_SCRIPT_USER`, e.g.
/// `sudo NARSOR_TEST_SCRIPT_USER=nobody cargo test -- --ignored script_user`.
#[tokio::test]
#[ignore = "needs root and a second system user (NARSOR_TEST_SCRIPT_USER)"]
async fn a_script_user_cannot_read_the_worker_credential_or_secrets() {
    let name = std::env::var("NARSOR_TEST_SCRIPT_USER").expect("NARSOR_TEST_SCRIPT_USER");
    let user = ScriptUser::lookup(&name).unwrap();
    let fx = Fixture::new();
    set_mode(&fx.root, 0o711);
    set_mode(&fx.root.join("work"), 0o711);
    let secrets = fx.secrets_dir(&[("jira-token", SENTINEL)]);
    let credential = fx.root.join("credential");
    fs::write(&credential, "nsw.wkr-1.secret\n").unwrap();
    set_mode(&credential, 0o600);
    let script = format!(
        "id -u\n\
         cat {credential} && echo READ-CREDENTIAL\n\
         ls {secrets} && echo LISTED-SECRETS\n\
         cat {secrets}/jira-token && echo READ-SECRET\n\
         touch \"$HOME/left-behind\"\n\
         printf '{{\"state\": {{}}}}' >&2\n",
        credential = credential.display(),
        secrets = secrets.display()
    );
    fx.observer(&script, &[]);
    let mut options = fx.options();
    options.script_user = Some(user);
    options.secrets_dir = Some(secrets);
    let worker = Worker::new(fx.api(), ProcessRunner, options);

    let handled = worker.run_once().await.unwrap().unwrap();

    let output = format!("{}{}", handled.report.stdout, handled.report.stderr);
    assert!(output.starts_with(&user.uid.to_string()), "{output}");
    assert!(!output.contains("READ-CREDENTIAL"), "{output}");
    assert!(!output.contains("LISTED-SECRETS"), "{output}");
    assert!(!output.contains("READ-SECRET"), "{output}");
    assert!(!output.contains(SENTINEL));
    assert_eq!(fs::read_dir(fx.root.join("work")).unwrap().count(), 0);
}

#[test]
fn the_processes_of_a_user_are_found_in_proc() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();

    let found = processes_of(current_uid());

    child.kill().unwrap();
    child.wait().unwrap();
    assert!(found.contains(&pid), "{pid} not in {found:?}");
    assert!(!found.contains(&i32::try_from(std::process::id()).unwrap()));
    assert!(!processes_of(current_uid()).contains(&pid));
}

/// A script that leaves its process group (`setsid`) still runs as the
/// script user; after the run nothing of that user is left, so it cannot
/// read the environment of the next run. Needs root, like the test above.
#[tokio::test]
#[ignore = "needs root and a second system user (NARSOR_TEST_SCRIPT_USER)"]
async fn a_script_user_process_that_left_its_group_does_not_outlive_the_run() {
    let name = std::env::var("NARSOR_TEST_SCRIPT_USER").expect("NARSOR_TEST_SCRIPT_USER");
    let user = ScriptUser::lookup(&name).unwrap();
    let fx = Fixture::new();
    set_mode(&fx.root, 0o711);
    set_mode(&fx.root.join("work"), 0o711);
    fx.observer(
        "setsid sleep 30 </dev/null >/dev/null 2>&1 &\n\
         echo \"child $!\" >&2\n\
         printf '{\"state\": {}}'\n",
        &[],
    );
    let mut options = fx.options();
    options.script_user = Some(user);
    let worker = Worker::new(fx.api(), ProcessRunner, options);

    let handled = worker.run_once().await.unwrap().unwrap();

    let child = pid_after(&handled.report.stderr, "child ");
    assert!(wait_until_gone(child), "the daemonized {child} survived");
    assert!(processes_of(user.uid).is_empty());
}

/// A process of the script user that runs before a claim — left by an
/// earlier worker, say — is killed before a run is claimed.
#[tokio::test]
#[ignore = "needs root and a second system user (NARSOR_TEST_SCRIPT_USER)"]
async fn a_leftover_script_user_process_is_killed_before_the_claim() {
    use std::os::unix::process::CommandExt as _;
    let name = std::env::var("NARSOR_TEST_SCRIPT_USER").expect("NARSOR_TEST_SCRIPT_USER");
    let user = ScriptUser::lookup(&name).unwrap();
    let fx = Fixture::new();
    set_mode(&fx.root, 0o711);
    set_mode(&fx.root.join("work"), 0o711);
    let mut leftover = std::process::Command::new("sleep")
        .arg("30")
        .uid(user.uid)
        .gid(user.gid)
        .process_group(0)
        .spawn()
        .unwrap();
    fx.observer("printf '{\"state\": {}}'\n", &[]);
    let mut options = fx.options();
    options.script_user = Some(user);
    let worker = Worker::new(fx.api(), ProcessRunner, options);

    let handled = worker.run_once().await.unwrap();

    assert!(handled.is_some());
    let _ = leftover.wait();
    assert!(processes_of(user.uid).is_empty());
}

/// Set by `a_non_root_worker_with_cap_kill_ends_only_the_script_users_processes`
/// for the helper it runs as the worker user: `<uid>:<gid>` of the script user.
const KILL_HELPER_ENV: &str = "NARSOR_TEST_KILL_HELPER_SCRIPT_USER";

/// The test filter that runs exactly `name` in this module.
fn this_module_test(name: &str) -> String {
    let path = module_path!();
    let module = path.split_once("::").map_or(path, |(_, rest)| rest);
    format!("{module}::{name}")
}

/// Whether the effective capability set of this process holds `CAP_KILL`.
fn holds_cap_kill() -> bool {
    const CAP_KILL: u32 = 5;
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
        .unwrap();
    u64::from_str_radix(effective, 16).unwrap() & (1 << CAP_KILL) != 0
}

/// The worker side of the test below: runs as a non-root worker user that
/// holds `CAP_KILL` (and the switch of user) only as ambient capabilities,
/// like the shipped systemd unit. Does nothing unless that test runs it.
#[test]
#[ignore = "run by a_non_root_worker_with_cap_kill_ends_only_the_script_users_processes"]
fn kill_helper_as_a_non_root_worker() {
    let Ok(ids) = std::env::var(KILL_HELPER_ENV) else {
        return;
    };
    let (uid, gid) = ids.split_once(':').unwrap();
    let user = ScriptUser {
        uid: uid.parse().unwrap(),
        gid: gid.parse().unwrap(),
    };
    assert_ne!(current_uid(), 0, "the helper must not run as root");
    assert!(
        holds_cap_kill(),
        "the helper must hold CAP_KILL, or the test proves nothing"
    );
    kill_all_processes_of(user).unwrap();
}

/// A worker that is not root keeps its capabilities across the switch to
/// the script user; the helper process that kills the script user's
/// processes must give up `CAP_KILL` before `kill(-1)`, or it kills every
/// process on the host. Needs root, `setpriv`, and three distinct
/// unprivileged users: `NARSOR_TEST_SCRIPT_USER`, `NARSOR_TEST_WORKER_USER`
/// (runs the helper; must be able to execute this test binary) and
/// `NARSOR_TEST_THIRD_USER` (whose process must survive), e.g.
/// `sudo NARSOR_TEST_SCRIPT_USER=nobody NARSOR_TEST_WORKER_USER=… \
///  NARSOR_TEST_THIRD_USER=… cargo test -- --ignored cap_kill`.
#[test]
#[ignore = "needs root, setpriv and three system users (NARSOR_TEST_SCRIPT_USER, NARSOR_TEST_WORKER_USER, NARSOR_TEST_THIRD_USER)"]
fn a_non_root_worker_with_cap_kill_ends_only_the_script_users_processes() {
    use std::os::unix::process::CommandExt as _;
    let lookup = |variable: &str| {
        let name = std::env::var(variable).expect(variable);
        ScriptUser::lookup(&name).unwrap()
    };
    let script = lookup("NARSOR_TEST_SCRIPT_USER");
    let worker = lookup("NARSOR_TEST_WORKER_USER");
    let third = lookup("NARSOR_TEST_THIRD_USER");
    assert_eq!(current_uid(), 0, "needs root to start the users' processes");
    let spawn_sleep_as = |user: ScriptUser| {
        std::process::Command::new("sleep")
            .arg("30")
            .uid(user.uid)
            .gid(user.gid)
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let mut bystander = spawn_sleep_as(third);
    let mut leftover = spawn_sleep_as(script);
    let bystander_pid = i32::try_from(bystander.id()).unwrap();
    let leftover_pid = i32::try_from(leftover.id()).unwrap();

    let status = std::process::Command::new("setpriv")
        .arg(format!("--reuid={}", worker.uid))
        .arg(format!("--regid={}", worker.gid))
        .arg("--clear-groups")
        .arg("--inh-caps=+kill,+setuid,+setgid")
        .arg("--ambient-caps=+kill,+setuid,+setgid")
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &this_module_test("kill_helper_as_a_non_root_worker"),
        ])
        .args(["--ignored", "--test-threads=1"])
        .env(KILL_HELPER_ENV, format!("{}:{}", script.uid, script.gid))
        .status()
        .unwrap();

    let bystander_alive = !process_gone(bystander_pid);
    let leftover_ended = wait_until_gone(leftover_pid);
    for child in [&mut bystander, &mut leftover] {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(status.success(), "the helper run failed: {status}");
    assert!(
        bystander_alive,
        "the third user's process {bystander_pid} was killed"
    );
    assert!(
        leftover_ended,
        "the script user's process {leftover_pid} survived"
    );
}
