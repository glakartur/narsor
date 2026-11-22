//! Integration tests of `narsor-cli queue …` and of the effect commands of
//! an automation's script, against a real `ProtocolService` over a seeded
//! catalogue, without a server.
//!
//! Queues and automations are catalogue files: the tests write them, and
//! no command changes them (R21, R22). The queue commands run through
//! [`operate`] on the [`QueueApi`] seam, implemented here on the service for
//! one owner, as the owner routes call it. A script's `task create --data`
//! and `task decide` are parsed by the real command line and sent through
//! [`perform`] on the [`EffectApi`] seam, which authenticates the run token
//! the way the run gate does. Where a worker is needed, it is the real
//! worker with a script runner that plays the script: it runs `narsor-cli`
//! command lines with the environment the worker gave it.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use futures::future::BoxFuture;
use narsor::api::ClientError;
use narsor::cli::observer_effect::{effect_call, perform, EffectApi};
use narsor::cli::queue::{operate, QueueApi};
use narsor::cli::queue_text::effect_output;
use narsor::cli::{Cli, Command};
use narsor_engine::automations::HookPhase;
use narsor_engine::controller_protocol::{ProtocolError, ProtocolService};
use narsor_engine::observers::{AutomationKey, AutomationState, RunId};
use narsor_engine::orchestration::{Clock, ControlPlane, OwnerRef};
use narsor_engine::persistence::ServerStatePaths;
use narsor_observer_worker::worker::{
    ScriptEnd, ScriptExit, ScriptRunner, ScriptSpec, Worker, WorkerApi, WorkerOptions,
};
use narsor_protocol::controller::{
    ObserverCreateTaskRequest, ObserverDecideRequest, ObserverEffectOutcome,
    ObserverEffectResponse, ObserverQueueListResponse, ObserverQueueView, ObserverRunReport,
    ObserverRunResultResponse, ObserverWorkerRun,
};
use serde_json::json;

/// A clock the test moves by hand, starting at the real time: the worker
/// cuts a script's limit by the run's deadline on the wall clock.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

/// The `triage` workflow as the catalogue serves it.
fn triage_yaml() -> String {
    format!(
        "name: triage\nversion: 1\ninputs:\n  - name: ticket\n    type: string\n    \
         required: true\nglobals:\n  image: \"{}\"\nstates:\n  work:\n    action:\n      \
         type: script\n      command: \"true\"\n    choices: [success]\n    transitions:\n      \
         success: done\n  done:\n    description: finished\nterminal_states: [done]\n",
        crate::environment_support::workflow_image()
    )
}

/// A seeded catalogue with a `triage` workflow, a clock and a service.
struct Fixture {
    root: PathBuf,
    state: PathBuf,
    clock: Arc<ManualClock>,
    service: Arc<ProtocolService>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-cli-queue-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        let state = root.join("state");
        let resolver = crate::environment_support::seeded_resolver(&state);
        fs::write(state.join("workflows").join("triage.yaml"), triage_yaml()).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(Utc::now())));
        let service = Arc::new(ProtocolService::new(
            ControlPlane::with_clock(ServerStatePaths::new(&state), clock.clone()),
            resolver,
        ));
        Self {
            root,
            state,
            clock,
            service,
        }
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    fn api(&self) -> ServiceApi {
        ServiceApi {
            service: Arc::clone(&self.service),
            owner: owner(),
        }
    }

    /// The `always` queue `jira-intake` of the automation `jira-new`, which
    /// creates `triage` tasks mapping `ticket` from `key`, at most `limit`
    /// per run.
    fn jira_queue(&self, limit: u32) {
        crate::automation_support::automation(
            &self.state,
            "jira-new",
            "narsor-cli task create --data \"$JSON\"\n",
            &[],
        );
        crate::automation_support::always_queue(
            &self.state,
            "jira-intake",
            "jira-new",
            "{interval_secs: 300}",
            &format!(
                "  workflow_id: triage\n  mapping: {{ticket: key}}\n  task_limit_per_run: {limit}\n"
            ),
        );
    }

    /// Runs `narsor-cli queue ARGS…` as the owner.
    async fn cli(&self, args: &[&str]) -> Result<String, (i32, String)> {
        let mut argv = vec!["narsor-cli", "queue"];
        argv.extend_from_slice(args);
        let Command::Queue { command } =
            Cli::try_parse_from(argv).expect("arguments parse").command
        else {
            panic!("not a queue command");
        };
        let clock = Arc::clone(&self.clock);
        operate(&self.api(), command, move || clock.now())
            .await
            .map_err(|error| (error.exit_code, error.message))
    }

    /// `text` saved as a file of the test.
    fn script_file(&self, name: &str, text: &str) -> String {
        let path = self.root.join(name);
        fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_string()
    }

    /// A worker whose script runs the `narsor-cli` command lines `lines`
    /// and exits 0 with an empty state.
    fn worker(&self, lines: &[&[&str]]) -> (Arc<TestWorker>, Arc<CliScript>) {
        let script = Arc::new(CliScript {
            service: Arc::clone(&self.service),
            lines: lines
                .iter()
                .map(|line| line.iter().map(|arg| arg.to_string()).collect())
                .collect(),
            outputs: Mutex::new(Vec::new()),
        });
        let mut options = WorkerOptions::new("https://narsor.example.com");
        options.work_dir = self.root.join("work");
        options.path = "/usr/bin:/bin".to_string();
        let worker = Worker::new(self.api(), Arc::clone(&script), options);
        (Arc::new(worker), script)
    }

    fn tasks(&self) -> usize {
        self.service.control_plane().list_tasks().unwrap().len()
    }
}

type TestWorker = Worker<ServiceApi, Arc<CliScript>>;

/// The owner routes, the worker routes and their errors, without HTTP:
/// the service's own methods for one owner, as the routes call them.
#[derive(Clone)]
struct ServiceApi {
    service: Arc<ProtocolService>,
    owner: OwnerRef,
}

/// A refusal as the API client reports the route's answer.
fn client_error(error: ProtocolError) -> ClientError {
    match error {
        ProtocolError::Unauthorized => ClientError::Unauthorized,
        ProtocolError::Forbidden => ClientError::Forbidden,
        ProtocolError::NotFound => ClientError::NotFound,
        ProtocolError::BadRequest { code, message } => ClientError::Request {
            code: code.to_string(),
            message,
        },
        ProtocolError::Conflict { code, message } => ClientError::Refused {
            code: code.to_string(),
            message,
        },
        other => ClientError::Server(other.to_string()),
    }
}

fn ready<'a, T: Send + 'a>(
    result: Result<T, ProtocolError>,
) -> BoxFuture<'a, Result<T, ClientError>> {
    Box::pin(std::future::ready(result.map_err(client_error)))
}

impl QueueApi for ServiceApi {
    fn observer_queues(&self) -> BoxFuture<'_, Result<ObserverQueueListResponse, ClientError>> {
        ready(self.service.observer_queues_of(&self.owner))
    }

    fn observer_queue<'a>(
        &'a self,
        queue: &'a str,
    ) -> BoxFuture<'a, Result<ObserverQueueView, ClientError>> {
        ready(self.service.observer_queue(&self.owner, queue))
    }
}

impl WorkerApi for ServiceApi {
    fn claim(&self) -> BoxFuture<'_, Result<Option<ObserverWorkerRun>, ClientError>> {
        ready(
            self.service
                .claim_observer_run()
                .map(|claimed| claimed.map(|claimed| claimed.into_wire())),
        )
    }

    fn report<'a>(
        &'a self,
        run_id: &'a str,
        report: &'a ObserverRunReport,
    ) -> BoxFuture<'a, Result<ObserverRunResultResponse, ClientError>> {
        let result = RunId::parse(run_id)
            .map_err(|_| ProtocolError::NotFound)
            .and_then(|run_id| self.service.finish_observer_run(&run_id, report.clone()))
            .map(|run| ObserverRunResultResponse {
                run_id: run_id.to_string(),
                status: serde_json::to_value(run.status)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                error: run.error,
            });
        ready(result)
    }
}

/// The effect routes with a run's token, as the run gate checks it: the
/// token must be that of the run the path names.
struct TokenEffects {
    service: Arc<ProtocolService>,
    token: String,
}

impl TokenEffects {
    fn gate(&self, run_id: &str) -> Result<RunId, ProtocolError> {
        match self.service.authenticate_observer_run(&self.token) {
            Some(run) if run.to_string() == run_id => Ok(run),
            _ => Err(ProtocolError::Unauthorized),
        }
    }
}

impl EffectApi for TokenEffects {
    fn create_task<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverCreateTaskRequest,
    ) -> BoxFuture<'a, Result<ObserverEffectResponse, ClientError>> {
        ready(
            self.gate(run_id)
                .and_then(|run| self.service.observer_create_task(&run, request.clone())),
        )
    }

    fn decide<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverDecideRequest,
    ) -> BoxFuture<'a, Result<ObserverEffectResponse, ClientError>> {
        ready(
            self.gate(run_id)
                .and_then(|run| self.service.observer_decide(&run, request.clone())),
        )
    }
}

/// Plays a script that runs `narsor-cli` command lines with the
/// environment the worker set, and records what each printed (or its exit
/// code and message).
struct CliScript {
    service: Arc<ProtocolService>,
    lines: Vec<Vec<String>>,
    outputs: Mutex<Vec<Result<String, (i32, String)>>>,
}

impl CliScript {
    fn outputs(&self) -> Vec<Result<String, (i32, String)>> {
        self.outputs.lock().unwrap().clone()
    }
}

fn env_value(spec: &ScriptSpec, name: &str) -> Option<String> {
    spec.env
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// Runs `narsor-cli ARGV…` as a script would, with `observer_run` as its
/// `NARSOR_OBSERVER_RUN` and `token` as its `NARSOR_TOKEN`.
async fn script_cli(
    service: &Arc<ProtocolService>,
    argv: &[String],
    observer_run: Option<&str>,
    token: &str,
) -> Result<String, (i32, String)> {
    let mut full = vec!["narsor-cli".to_string()];
    full.extend(argv.iter().cloned());
    let cli = Cli::try_parse_from(full).map_err(|error| (2, error.to_string()))?;
    let Command::Task { command } = cli.command else {
        panic!("a script runs task commands");
    };
    let call = effect_call(&command, observer_run)
        .map_err(|error| (error.exit_code, error.message))?
        .expect("an effect command");
    let effects = TokenEffects {
        service: Arc::clone(service),
        token: token.to_string(),
    };
    perform(&effects, observer_run.unwrap_or_default(), &call)
        .await
        .map_err(|error| (error.exit_code, error.message))
}

impl ScriptRunner for CliScript {
    fn run(&self, spec: ScriptSpec) -> BoxFuture<'_, io::Result<ScriptExit>> {
        Box::pin(async move {
            let run = env_value(&spec, "NARSOR_OBSERVER_RUN");
            let token = env_value(&spec, "NARSOR_TOKEN").unwrap_or_default();
            for line in &self.lines {
                let output = script_cli(&self.service, line, run.as_deref(), &token).await;
                self.outputs.lock().unwrap().push(output);
            }
            let stdout = br#"{"state": {"cursor": 1}}"#.to_vec();
            Ok(ScriptExit {
                end: ScriptEnd::Exited(0),
                stdout_total: stdout.len() as u64,
                stdout,
                stderr: Vec::new(),
                stderr_total: 0,
            })
        })
    }
}

fn create_line(key: &str) -> Vec<String> {
    vec![
        "task".into(),
        "create".into(),
        "--data".into(),
        json!({"key": key}).to_string(),
    ]
}

fn lines_of(lines: &[Vec<String>]) -> Vec<Vec<&str>> {
    lines
        .iter()
        .map(|line| line.iter().map(String::as_str).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// The queue commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_catalogue_queue_runs_with_no_cli_step_and_queue_show_reports_it() {
    // Success criterion of the origin: the catalogue files are enough; the
    // scheduled run's script creates the task.
    let fx = Fixture::new();
    fx.jira_queue(10);
    let lines = [create_line("ABC-1")];
    let (worker, script) = fx.worker(
        &lines_of(&lines)
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>(),
    );

    fx.service.observer_pass();
    let handled = worker.run_once().await.unwrap().expect("a run is due");
    assert!(handled.accepted.is_some());

    let outputs = script.outputs();
    let created = outputs.last().unwrap().as_ref().unwrap();
    assert!(created.starts_with("created task "), "{created}");
    assert_eq!(fx.tasks(), 1);
    let shown = fx.cli(&["show", "jira-intake"]).await.unwrap();
    assert!(shown.contains("Mode: always"), "{shown}");
    assert!(shown.contains("Creates tasks of: triage"), "{shown}");
    assert!(shown.contains("  jira-new (check): active"), "{shown}");
    assert!(shown.contains("last run succeeded"), "{shown}");
}

/// Makes `task` of the owner wait at the gate on `queue` in `episode`.
fn add_entry(fx: &Fixture, queue: &str, task: &str, episode: &str) {
    let task = crate::gate_support::queued_task(&fx.service, task, &owner());
    crate::gate_support::wait_at(
        &fx.service,
        &task,
        episode,
        &crate::gate_support::review_gate(queue),
    );
}

#[tokio::test]
async fn queue_list_shows_each_queue_with_its_automations_and_a_bad_file_with_its_problem() {
    // U9, AE8: a bad queue file is listed with its problem; the other
    // queues are listed normally.
    let fx = Fixture::new();
    assert!(fx.cli(&["list"]).await.unwrap().starts_with("No queues."));
    crate::automation_support::mr_review(&fx.state);
    crate::automation_support::on_episodes_queue(
        &fx.state,
        "broken",
        &["missing-automation"],
        "{interval_secs: 300}",
    );
    add_entry(&fx, "mr-review", "task-1", "input-1");
    fx.advance(Duration::minutes(4));

    let listed = fx.cli(&["list"]).await.unwrap();
    let row = |queue: &str| {
        listed
            .lines()
            .find(|line| line.starts_with(&format!("{queue} ")))
            .unwrap_or_else(|| panic!("no {queue}: {listed}"))
            .to_string()
    };
    let review = row("mr-review");
    assert!(review.contains("on_episodes"), "{listed}");
    assert!(review.contains("every 300s"), "{listed}");
    assert!(
        review.trim_end().ends_with("1  gitlab-mr-label"),
        "{listed}"
    );
    let broken = row("broken");
    assert!(
        broken.contains("! automation 'missing-automation'"),
        "{listed}"
    );
    assert!(
        listed.contains("Catalogue files that cannot be used:\n  queues/broken.yaml: "),
        "{listed}"
    );

    let shown = fx.cli(&["show", "mr-review"]).await.unwrap();
    assert!(
        shown.contains("  gitlab-mr-label (check): active"),
        "{shown}"
    );
    assert!(shown.contains("Entries (oldest first):"), "{shown}");
    assert!(shown.contains("  task-1 "), "{shown}");
    assert!(shown.contains(" 4m\n"), "the entry waits 4m: {shown}");
    assert!(shown.contains("    gitlab-mr-label: no run yet"), "{shown}");
    // `--json` prints the server's whole answer, for a script or an agent.
    let listed: ObserverQueueListResponse =
        serde_json::from_str(&fx.cli(&["list", "--json"]).await.unwrap()).unwrap();
    assert!(listed.queues.iter().any(|queue| queue.queue == "mr-review"));
    assert_eq!(listed.problems[0].file, "queues/broken.yaml");
    let shown: ObserverQueueView =
        serde_json::from_str(&fx.cli(&["show", "mr-review", "--json"]).await.unwrap()).unwrap();
    assert_eq!(shown.entries.len(), 1);
    let error = fx.cli(&["show", "nothing"]).await.unwrap_err();
    assert_eq!(
        error,
        (1, "Could not read queue 'nothing': not found".to_string())
    );
}

#[test]
fn the_text_of_an_untrusted_answer_has_no_control_characters() {
    let effect = ObserverEffectResponse {
        outcome: ObserverEffectOutcome::Created,
        task_id: None,
        input_id: None,
        message: "created\u{7} task \u{1b}[31mtask-1".into(),
    };
    assert_eq!(effect_output(&effect), "created task [31mtask-1\n");
}

// ---------------------------------------------------------------------------
// The script's effect commands
// ---------------------------------------------------------------------------

/// The `jira-intake` queue with a run claimed; returns the run's id and
/// token.
async fn claimed_jira_run(fx: &Fixture, limit: u32) -> (String, String) {
    fx.jira_queue(limit);
    fx.service.observer_pass();
    let run = fx.service.claim_observer_run().unwrap().unwrap();
    (run.run_id.to_string(), run.token.expose().to_string())
}

#[tokio::test]
async fn a_created_task_exits_0_and_a_run_over_its_limit_exits_1() {
    let fx = Fixture::new();
    let (run, token) = claimed_jira_run(&fx, 1).await;

    let created = script_cli(&fx.service, &create_line("ABC-1"), Some(&run), &token).await;
    let over = script_cli(&fx.service, &create_line("ABC-1"), Some(&run), &token).await;

    assert!(created.unwrap().starts_with("created task"));
    let (code, message) = over.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("limit"), "{message}");
    assert!(message.contains("observer_task_limit"), "{message}");
    assert_eq!(fx.tasks(), 1);
    let record = fx
        .service
        .observers()
        .load(&AutomationKey::new("jira-intake", "jira-new").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.state, AutomationState::NeedsAttention);
}

#[tokio::test]
async fn a_forged_token_or_the_path_of_another_run_is_refused() {
    let fx = Fixture::new();
    let (run, token) = claimed_jira_run(&fx, 10).await;

    let forged = script_cli(&fx.service, &create_line("ABC-1"), Some(&run), "nso.forged").await;
    let (code, message) = forged.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("rejected the token"), "{message}");
    let elsewhere = format!("{}~99", run.rsplit_once('~').unwrap().0);
    let wrong_run = script_cli(&fx.service, &create_line("ABC-1"), Some(&elsewhere), &token).await;
    assert_eq!(wrong_run.unwrap_err().0, 1);
    assert_eq!(fx.tasks(), 0);
}

// ---------------------------------------------------------------------------
// `task decide` (U6)
// ---------------------------------------------------------------------------

/// `task decide task-1 --episode input-1 ARGS…` as a script's line.
fn decide_line(args: &[&str]) -> Vec<String> {
    let mut line: Vec<String> = ["task", "decide", "task-1", "--episode", "input-1"]
        .into_iter()
        .map(String::from)
        .collect();
    line.extend(args.iter().map(|arg| arg.to_string()));
    line
}

/// The `mr-review` queue of the catalogue, a task waiting at its gate in
/// `input-1`, and the run claimed for the entry; returns the run's id and
/// token.
async fn claimed_review_run(fx: &Fixture) -> (String, String) {
    crate::automation_support::mr_review(&fx.state);
    let task = crate::gate_support::queued_task(&fx.service, "task-1", &owner());
    crate::gate_support::running(&fx.service, &task);
    crate::gate_support::wait_at(
        &fx.service,
        &task,
        "input-1",
        &crate::gate_support::review_gate("mr-review"),
    );
    fx.service.observer_pass();
    let run = fx.service.claim_observer_run().unwrap().unwrap();
    assert!(run.input.entry.is_some());
    (run.run_id.to_string(), run.token.expose().to_string())
}

#[tokio::test]
async fn task_decide_from_a_run_that_is_not_a_check_run_exits_1() {
    // Only `check` answers a gate; `in`/`out` runs come with U4/U5, so the
    // phase of the run in progress is set by hand.
    let fx = Fixture::new();
    let (run, token) = claimed_review_run(&fx).await;
    let key = AutomationKey::new("mr-review", "gitlab-mr-label").unwrap();
    let store = fx.service.observers();
    let lock = store.lock(&key).unwrap();
    let mut record = store.load(&key).unwrap().unwrap();
    record.active_run.as_mut().unwrap().phase = HookPhase::Out;
    store.save(&lock, &mut record, Utc::now()).unwrap();
    drop(lock);

    let refused = script_cli(
        &fx.service,
        &decide_line(&["--choice", "check_feedback"]),
        Some(&run),
        &token,
    )
    .await;

    let (code, message) = refused.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("not_a_check_run"), "{message}");
    let task = fx
        .service
        .control_plane()
        .task(&narsor_engine::domain::TaskId::parse("task-1").unwrap())
        .unwrap();
    assert!(task.inputs.is_empty());
}

#[tokio::test]
async fn a_script_answers_its_gate_with_task_decide_and_a_refusal_exits_1() {
    let fx = Fixture::new();
    let (run, token) = claimed_review_run(&fx).await;
    let note = fx.script_file("note.txt", "split the change\n");

    let unknown = script_cli(
        &fx.service,
        &decide_line(&["--choice", "merge"]),
        Some(&run),
        &token,
    )
    .await;
    let decided = script_cli(
        &fx.service,
        &decide_line(&[
            "--choice",
            "revise_plan",
            "--reason",
            "the reviewer asked",
            "--var-file",
            &format!("note={note}"),
        ]),
        Some(&run),
        &token,
    )
    .await;
    let second = script_cli(
        &fx.service,
        &decide_line(&["--data", r#"{"choice": "check_feedback"}"#]),
        Some(&run),
        &token,
    )
    .await;

    let (code, message) = unknown.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.starts_with("The answer was refused"), "{message}");
    assert!(message.contains("invalid_answer"), "{message}");
    let decided = decided.unwrap();
    assert!(
        decided.starts_with("answered review_work of task task-1 with revise_plan"),
        "{decided}"
    );
    let (code, message) = second.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("idempotency_key_reused"), "{message}");
    let task = fx
        .service
        .control_plane()
        .task(&narsor_engine::domain::TaskId::parse("task-1").unwrap())
        .unwrap();
    let delivery = task.inputs[0].delivery.as_ref().unwrap();
    assert_eq!(delivery.values["note"], json!("split the change\n"));
    assert_eq!(delivery.comment.as_deref(), Some("the reviewer asked"));
}

#[tokio::test]
async fn task_decide_takes_the_answer_as_data() {
    let fx = Fixture::new();
    let (run, token) = claimed_review_run(&fx).await;

    let decided = script_cli(
        &fx.service,
        &decide_line(&[
            "--data",
            r#"{"choice": "revise_plan", "reason": "label removed", "variables": {"note": "x"}}"#,
        ]),
        Some(&run),
        &token,
    )
    .await;

    assert!(decided.unwrap().contains("with revise_plan"));
}

#[tokio::test]
async fn task_decide_usage_errors_exit_2_and_send_nothing() {
    let fx = Fixture::new();
    let (run, token) = claimed_review_run(&fx).await;

    let cases: Vec<(Vec<String>, Option<&str>, &str)> = vec![
        (
            decide_line(&["--choice", "check_feedback"]),
            None,
            "NARSOR_OBSERVER_RUN is not set",
        ),
        (
            decide_line(&["--choice", "check_feedback", "--data", "{}"]),
            Some(&run),
            "not both",
        ),
        (decide_line(&[]), Some(&run), "needs --choice"),
        (
            decide_line(&["--data", r#"{"choice": "x", "artifacts": {}}"#]),
            Some(&run),
            "Invalid --data",
        ),
        (
            decide_line(&["--choice", "revise_plan", "--var", "note"]),
            Some(&run),
            "name=value",
        ),
        (
            decide_line(&[
                "--choice",
                "revise_plan",
                "--var-file",
                "note=/no/such/file",
            ]),
            Some(&run),
            "Cannot read --var-file",
        ),
    ];
    for (line, observer_run, expected) in cases {
        let (code, message) = script_cli(&fx.service, &line, observer_run, &token)
            .await
            .unwrap_err();
        assert_eq!(code, 2, "{line:?}: {message}");
        assert!(message.contains(expected), "{line:?}: {message}");
    }
    let task = fx
        .service
        .control_plane()
        .task(&narsor_engine::domain::TaskId::parse("task-1").unwrap())
        .unwrap();
    assert!(task.inputs.is_empty());
}
