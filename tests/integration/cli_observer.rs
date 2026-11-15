//! Integration tests of `narsor-cli observer …` and of the effect commands
//! of an observer's script (U7), against a real `ProtocolService` over a
//! seeded catalogue, without a server.
//!
//! The owner's commands run through [`operate`] on the [`ObserverApi`]
//! seam, implemented here on the service for one owner, as the owner
//! routes call it. A script's `task create --data` and `task event`
//! are parsed by the real command line and sent through [`perform`] on the
//! [`EffectApi`] seam, which authenticates the run token the way the run
//! gate does. Where a worker is needed, it is the real worker of U6 with a
//! script runner that plays the script: it runs `narsor-cli` command lines
//! with the environment the worker gave it.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use futures::future::BoxFuture;
use narsor::api::ClientError;
use narsor::cli::observer::{operate, ObserverApi};
use narsor::cli::observer_effect::{effect_call, perform, EffectApi};
use narsor::cli::observer_text::{effect_output, observer_list_output};
use narsor::cli::{Cli, Command};
use narsor_engine::controller_protocol::{ProtocolError, ProtocolService, RunOutcome, RunReport};
use narsor_engine::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace,
};
use narsor_engine::observers::{ObserverState, RunId};
use narsor_engine::orchestration::{Clock, ControlPlane, NewTask, OwnerRef, TaskEventKind};
use narsor_engine::persistence::StatePaths;
use narsor_observer_worker::worker::{
    ScriptEnd, ScriptExit, ScriptRunner, ScriptSpec, Worker, WorkerApi, WorkerOptions,
};
use narsor_protocol::controller::{
    AddObserverWatchRequest, ApproveObserverRequest, CreateObserverRequest,
    ObserverCreateTaskRequest, ObserverDetailView, ObserverEffectOutcome, ObserverEffectResponse,
    ObserverEventRequest, ObserverListResponse, ObserverRunReport, ObserverRunResultResponse,
    ObserverScriptView, ObserverView, ObserverWatchListResponse, ObserverWatchView,
    ObserverWorkerRun, SetObserverScriptRequest,
};
use serde_json::json;

const PREFIX: &str = "https://git.example.com/";

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
        let root =
            std::env::temp_dir().join(format!("narsor-cli-observer-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        let state = root.join("state");
        let resolver = crate::environment_support::seeded_resolver(&state);
        fs::write(state.join("workflows").join("triage.yaml"), triage_yaml()).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(Utc::now())));
        let service = Arc::new(ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(&state), clock.clone()),
            resolver,
        ));
        Self {
            root,
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

    /// Runs `narsor-cli observer ARGS…` as the owner.
    async fn cli(&self, args: &[&str]) -> Result<String, (i32, String)> {
        let mut argv = vec!["narsor-cli", "observer"];
        argv.extend_from_slice(args);
        let Command::Observer { command } =
            Cli::try_parse_from(argv).expect("arguments parse").command
        else {
            panic!("not an observer command");
        };
        let clock = Arc::clone(&self.clock);
        operate(&self.api(), command, None, move || clock.now())
            .await
            .map_err(|error| (error.exit_code, error.message))
    }

    /// `script` saved as a file of the test.
    fn script_file(&self, name: &str, script: &str) -> String {
        let path = self.root.join(name);
        fs::write(&path, script).unwrap();
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

    /// A task of the owner.
    fn task(&self, id: &str) -> TaskId {
        let workspace = TaskWorkspace::single(TaskRepository::new(
            "narsor",
            TaskRepositorySource::Git(Box::new(GitIdentity {
                repository: Some("narsor".to_string()),
                remote_url: "https://example.com/narsor.git".to_string(),
                commit: "614c3a8".repeat(5) + "abcde",
                git_ref: None,
                ref_origin: None,
                credential: None,
            })),
        ));
        let spec = TaskLaunchSpec::new(
            "steps",
            1,
            json!({}),
            workspace,
            ResolvedEnvironment::harness_only("claude-code"),
            ResourceProfile::new(1000, 2048),
        );
        let task_id = TaskId::parse(id).unwrap();
        self.service
            .control_plane()
            .create_task(NewTask::new(
                task_id.clone(),
                owner(),
                BackendId::parse("k8s").unwrap(),
                spec,
            ))
            .unwrap();
        task_id
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

impl ObserverApi for ServiceApi {
    fn observers(&self) -> BoxFuture<'_, Result<ObserverListResponse, ClientError>> {
        ready(self.service.observers_of(&self.owner))
    }

    fn create_observer<'a>(
        &'a self,
        request: &'a CreateObserverRequest,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(self.service.create_observer(&self.owner, request.clone()))
    }

    fn observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, Result<ObserverDetailView, ClientError>> {
        ready(self.service.observer_detail(&self.owner, observer))
    }

    fn delete_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(self.service.delete_observer(&self.owner, observer))
    }

    fn set_observer_script<'a>(
        &'a self,
        observer: &'a str,
        request: &'a SetObserverScriptRequest,
    ) -> BoxFuture<'a, Result<ObserverScriptView, ClientError>> {
        ready(
            self.service
                .add_observer_script(&self.owner, observer, &request.script),
        )
    }

    fn approve_observer<'a>(
        &'a self,
        observer: &'a str,
        request: &'a ApproveObserverRequest,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(
            self.service
                .approve_observer_version_for(&self.owner, observer, request.version),
        )
    }

    fn pause_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(self.service.pause_observer_for(&self.owner, observer))
    }

    fn resume_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(self.service.resume_observer_for(&self.owner, observer))
    }

    fn observer_watches<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, Result<ObserverWatchListResponse, ClientError>> {
        ready(self.service.observer_watches(&self.owner, observer))
    }

    fn add_observer_watch<'a>(
        &'a self,
        observer: &'a str,
        request: &'a AddObserverWatchRequest,
    ) -> BoxFuture<'a, Result<ObserverWatchView, ClientError>> {
        ready(
            self.service
                .add_observer_watch(&self.owner, observer, request.clone())
                .map(|(entry, _)| entry),
        )
    }

    fn remove_observer_watch<'a>(
        &'a self,
        observer: &'a str,
        entry_id: &'a str,
    ) -> BoxFuture<'a, Result<ObserverWatchView, ClientError>> {
        ready(
            self.service
                .remove_observer_watch(&self.owner, observer, entry_id),
        )
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

    fn deliver_event<'a>(
        &'a self,
        run_id: &'a str,
        task_id: &'a str,
        request: &'a ObserverEventRequest,
    ) -> BoxFuture<'a, Result<ObserverEffectResponse, ClientError>> {
        ready(self.gate(run_id).and_then(|run| {
            self.service
                .observer_deliver_event(&run, task_id, request.clone())
        }))
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

/// `observer create jira-new` of `triage` mapping `ticket` from `key`.
const CREATE_JIRA: &[&str] = &[
    "create",
    "jira-new",
    "--source",
    "jira",
    "--workflow",
    "triage",
    "--map",
    "ticket=key",
];

// ---------------------------------------------------------------------------
// The owner's commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_operator_runs_the_jira_flow_through_the_cli_alone() {
    // F1 and F2 by the CLI: create, save a script, approve, and the
    // scheduled run's script creates the task.
    let fx = Fixture::new();
    let created = fx.cli(CREATE_JIRA).await.unwrap();
    assert!(created.contains("Created observer 'jira-new'"), "{created}");
    assert!(created.contains("state draft"), "{created}");
    let file = fx.script_file("jira.sh", "narsor-cli task create --data \"$JSON\"\n");
    let saved = fx
        .cli(&["script", "set", "jira-new", "--file", &file])
        .await
        .unwrap();
    assert!(saved.contains("Saved version 1"), "{saved}");
    let lines = [create_line("ABC-1")];
    let (worker, script) = fx.worker(
        &lines_of(&lines)
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>(),
    );
    // Nothing runs before the approval.
    fx.service.observer_pass();
    assert!(worker.run_once().await.unwrap().is_none());
    assert_eq!(fx.tasks(), 0);

    let approved = fx
        .cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();
    assert!(approved.contains("state active"), "{approved}");
    fx.service.observer_pass();
    let handled = worker.run_once().await.unwrap().expect("a run is due");
    assert!(handled.accepted.is_some());

    let outputs = script.outputs();
    let created = outputs.last().unwrap().as_ref().unwrap();
    assert!(created.starts_with("created task "), "{created}");
    assert_eq!(fx.tasks(), 1);

    let listed = fx.cli(&["list"]).await.unwrap();
    assert!(listed.contains("jira-new"), "{listed}");
    assert!(listed.contains("succeeded"), "{listed}");
}

#[tokio::test]
async fn observer_list_shows_state_last_run_and_its_error() {
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA).await.unwrap();
    let file = fx.script_file("jira.sh", "exit 3\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file])
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();
    fx.service.observer_pass();
    let run = fx.service.claim_observer_run().unwrap().unwrap();
    fx.service
        .finish_observer_run(
            &run.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 3 },
                stdout: String::new(),
                stderr: "boom".into(),
            },
        )
        .unwrap();
    fx.advance(Duration::seconds(30));

    let listed = fx.cli(&["list"]).await.unwrap();

    let mut lines = listed.lines();
    let header = lines.next().unwrap();
    for column in ["NAME", "STATE", "SCRIPT", "LAST RUN", "NEXT RUN"] {
        assert!(header.contains(column), "{header}");
    }
    let row = lines.next().unwrap();
    assert!(row.starts_with("jira-new"), "{row}");
    assert!(row.contains("active"), "{row}");
    assert!(row.contains("v1"), "{row}");
    assert!(row.contains("failed 30s ago"), "{row}");
    let error = lines.next().unwrap();
    assert!(error.trim_start().starts_with("error:"), "{error}");
    assert!(!listed.contains("no worker"), "{listed}");
}

#[tokio::test]
async fn observer_show_lists_the_errors_of_failed_runs_only() {
    // D15: a successful run leaves no record to show.
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA).await.unwrap();
    let file = fx.script_file("jira.sh", "true\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file])
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();
    for (exit_code, stdout, stderr) in [
        (0, r#"{"state": {}}"#, ""),
        (3, "", "line 1\ncurl: \u{1b}[31mboom\n"),
    ] {
        fx.service.observer_pass();
        let run = fx.service.claim_observer_run().unwrap().unwrap();
        fx.service
            .finish_observer_run(
                &run.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code },
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                },
            )
            .unwrap();
        fx.advance(Duration::seconds(300));
    }

    let shown = fx.cli(&["show", "jira-new"]).await.unwrap();

    assert!(shown.contains("Last run: #2 failed"), "{shown}");
    let errors = shown
        .split_once("Errors (newest first):\n")
        .expect("an errors section")
        .1;
    let mut lines = errors.lines();
    let first = lines.next().unwrap();
    assert!(first.contains("#2 run of v1: failed"), "{first}");
    assert!(first.contains("exit 3"), "{first}");
    assert_eq!(lines.next().unwrap().trim(), "stderr:");
    assert_eq!(lines.next().unwrap().trim(), "line 1");
    assert_eq!(lines.next().unwrap().trim(), "curl: [31mboom");
    assert!(!shown.contains("#1 run"), "{shown}");
    assert!(!shown.contains("Runs"), "{shown}");
}

#[tokio::test]
async fn observer_list_and_show_warn_when_no_worker_claims_a_due_run() {
    // R18: a due run waiting longer than one interval.
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA).await.unwrap();
    let file = fx.script_file("jira.sh", "true\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file])
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();
    fx.service.observer_pass();
    fx.advance(Duration::seconds(301));

    let listed = fx.cli(&["list"]).await.unwrap();
    let shown = fx.cli(&["show", "jira-new"]).await.unwrap();

    assert!(listed.contains("warning: no worker"), "{listed}");
    assert!(shown.contains("Warning: no worker"), "{shown}");
}

#[tokio::test]
async fn a_script_saved_on_an_active_observer_is_a_draft_beside_the_approved_version() {
    // Covers AE6.
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA).await.unwrap();
    let first = fx.script_file("v1.sh", "true\n");
    let second = fx.script_file("v2.sh", "echo changed\n");
    fx.cli(&["script", "set", "jira-new", "--file", &first])
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();

    let saved = fx
        .cli(&["script", "set", "jira-new", "--file", &second])
        .await
        .unwrap();
    let shown = fx.cli(&["show", "jira-new"]).await.unwrap();
    let listed = fx.cli(&["list"]).await.unwrap();

    assert!(saved.contains("Saved version 2"), "{saved}");
    assert!(
        shown.contains("Script: approved version 1; draft version 2 (not approved)"),
        "{shown}"
    );
    assert!(shown.contains("State: active"), "{shown}");
    assert!(listed.contains("v1 (draft v2)"), "{listed}");
    let record = fx
        .service
        .observers()
        .find_by_name("jira-new")
        .unwrap()
        .unwrap();
    assert_eq!(record.approved_version, Some(1));
}

#[tokio::test]
async fn an_observer_create_with_a_bad_condition_or_mapping_is_a_usage_error() {
    let fx = Fixture::new();

    let (code, message) = fx
        .cli(&[
            "create",
            "jira-new",
            "--source",
            "jira",
            "--workflow",
            "triage",
            "--condition",
            "[1]",
        ])
        .await
        .unwrap_err();
    assert_eq!(code, 2);
    assert!(message.contains("--condition"), "{message}");

    let (code, message) = fx
        .cli(&[
            "create",
            "jira-new",
            "--source",
            "jira",
            "--workflow",
            "triage",
            "--map",
            "ticket",
        ])
        .await
        .unwrap_err();
    assert_eq!(code, 2);
    assert!(message.contains("--map"), "{message}");
    assert!(fx.service.observers().list().unwrap().is_empty());
}

#[tokio::test]
async fn another_owners_observer_is_not_found() {
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA).await.unwrap();
    let stranger = ServiceApi {
        service: Arc::clone(&fx.service),
        owner: OwnerRef::new("someone-else"),
    };
    let Command::Observer { command } =
        Cli::try_parse_from(["narsor-cli", "observer", "pause", "jira-new"])
            .unwrap()
            .command
    else {
        panic!("not an observer command");
    };

    let error = operate(&stranger, command, None, Utc::now)
        .await
        .unwrap_err();

    assert_eq!(error.exit_code, 1);
    assert!(error.message.contains("not found"), "{}", error.message);
}

// ---------------------------------------------------------------------------
// Watch entries and events
// ---------------------------------------------------------------------------

/// `observer create mr-review` delivering events, watching under PREFIX.
const CREATE_REVIEW: &[&str] = &[
    "create",
    "mr-review",
    "--source",
    "gitlab",
    "--deliver-events",
    "--allow-prefix",
    PREFIX,
];

#[tokio::test]
async fn a_watch_entry_added_by_the_owner_is_in_the_watch_list() {
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();
    let task = fx.task("task-1");
    let object = format!("{PREFIX}a/-/merge_requests/17");

    let added = fx
        .cli(&[
            "watch",
            "add",
            "mr-review",
            "--task",
            task.as_str(),
            "--object",
            &object,
            "--events",
            "approved,merged",
        ])
        .await
        .unwrap();
    let listed = fx.cli(&["watch", "list", "mr-review"]).await.unwrap();

    assert!(
        added.contains(&format!("Watching {object} for task task-1")),
        "{added}"
    );
    assert!(added.contains("approved,merged"), "{added}");
    let row = listed.lines().nth(1).unwrap();
    assert!(row.contains("task-1"), "{row}");
    assert!(row.contains("approved,merged"), "{row}");
    assert!(row.contains("operator"), "{row}");
    assert!(row.contains(&object), "{row}");

    let entry = row.split_whitespace().next().unwrap();
    let removed = fx
        .cli(&["watch", "remove", "mr-review", entry])
        .await
        .unwrap();
    assert!(removed.starts_with("Removed watch entry"), "{removed}");
    let empty = fx.cli(&["watch", "list", "mr-review"]).await.unwrap();
    assert_eq!(empty, "No watch entries.\n");
}

#[tokio::test]
async fn an_operator_runs_the_review_flow_through_the_cli_alone() {
    // F3 by the CLI: the event of a watched MR reaches the task's history.
    // The server does not deduplicate: two calls are two events.
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();
    let task = fx.task("task-1");
    let object = format!("{PREFIX}a/-/merge_requests/17");
    fx.cli(&[
        "watch",
        "add",
        "mr-review",
        "--task",
        "task-1",
        "--object",
        &object,
        "--events",
        "approved",
    ])
    .await
    .unwrap();
    let file = fx.script_file("mr.sh", "narsor-cli task event ...\n");
    fx.cli(&["script", "set", "mr-review", "--file", &file])
        .await
        .unwrap();
    fx.cli(&["approve", "mr-review", "--version", "1"])
        .await
        .unwrap();
    let event = [
        "task",
        "event",
        "task-1",
        "--type",
        "approved",
        "--data",
        r#"{"mr": 17}"#,
    ];
    let (worker, script) = fx.worker(&[&event, &event]);

    fx.service.observer_pass();
    worker.run_once().await.unwrap().expect("a run is due");

    let outputs = script.outputs();
    let first = outputs[0].as_ref().unwrap();
    assert!(
        first.contains("recorded in the history of task task-1"),
        "{first}"
    );
    let second = outputs[1].as_ref().unwrap();
    assert!(
        second.contains("recorded in the history of task task-1"),
        "{second}"
    );
    let external = fx
        .service
        .control_plane()
        .events_since(&task, 0)
        .unwrap()
        .iter()
        .filter(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. }))
        .count();
    assert_eq!(external, 2);
}

#[test]
fn the_text_of_untrusted_answers_has_no_control_characters() {
    let effect = ObserverEffectResponse {
        outcome: ObserverEffectOutcome::Recorded,
        task_id: None,
        input_id: None,
        message: "recorded\u{7} in \u{1b}[31mtask-1".into(),
    };
    assert_eq!(effect_output(&effect), "recorded in [31mtask-1\n");

    let listing = ObserverListResponse {
        observers: Vec::new(),
    };
    assert!(observer_list_output(&listing, Utc::now()).starts_with("No observers"));
}

// ---------------------------------------------------------------------------
// The script's effect commands
// ---------------------------------------------------------------------------

/// An approved "create a task" observer of the CLI with a run claimed;
/// returns the run's id and token.
async fn claimed_jira_run(fx: &Fixture, limit: &str) -> (String, String) {
    let mut create = CREATE_JIRA.to_vec();
    create.extend_from_slice(&["--task-limit", limit]);
    fx.cli(&create).await.unwrap();
    let file = fx.script_file("jira.sh", "true\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file])
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();
    fx.service.observer_pass();
    let run = fx.service.claim_observer_run().unwrap().unwrap();
    (run.run_id.to_string(), run.token.expose().to_string())
}

#[tokio::test]
async fn a_created_task_exits_0_and_a_run_over_its_limit_exits_1() {
    let fx = Fixture::new();
    let (run, token) = claimed_jira_run(&fx, "1").await;

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
        .find_by_name("jira-new")
        .unwrap()
        .unwrap();
    assert_eq!(record.state, ObserverState::NeedsAttention);
}

#[tokio::test]
async fn a_forged_token_or_the_path_of_another_run_is_refused() {
    let fx = Fixture::new();
    let (run, token) = claimed_jira_run(&fx, "10").await;

    let forged = script_cli(&fx.service, &create_line("ABC-1"), Some(&run), "nso.forged").await;
    let (code, message) = forged.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("rejected the token"), "{message}");
    let elsewhere = format!("{}.99", run.rsplit_once('.').unwrap().0);
    let wrong_run = script_cli(&fx.service, &create_line("ABC-1"), Some(&elsewhere), &token).await;
    assert_eq!(wrong_run.unwrap_err().0, 1);
    assert_eq!(fx.tasks(), 0);
}
