//! Integration tests of `narsor-cli observer …` and of the effect commands
//! of an observer's script (U7), against a real `ProtocolService` over a
//! seeded catalogue, without a server.
//!
//! The owner's commands run through [`operate`] on the [`ObserverApi`]
//! seam, implemented here on the service for one owner, as the owner
//! routes call it. A script's `task create --effect-key` and `task event`
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
use narsor::cli::observer::ObserverCommand;
use narsor::cli::observer::{operate, worker_token, ObserverApi, WorkerTokenApi};
use narsor::cli::observer_effect::{effect_call, perform, EffectApi};
use narsor::cli::observer_text::{dry_run_output, effect_output, observer_list_output};
use narsor::cli::observer_worker::{
    ScriptEnd, ScriptExit, ScriptRunner, ScriptSpec, Worker, WorkerApi, WorkerOptions,
};
use narsor::cli::{Cli, Command};
use narsor_engine::controller_protocol::{ProtocolError, ProtocolService, RunOutcome, RunReport};
use narsor_engine::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace,
};
use narsor_engine::observers::{effect_task_id, ObserverState, RunId};
use narsor_engine::orchestration::{Clock, ControlPlane, NewTask, OwnerRef, TaskEventKind};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::controller::{
    AddObserverWatchRequest, ApproveObserverRequest, CreateObserverRequest,
    CreateObserverWorkerRequest, ObserverCreateTaskRequest, ObserverDetailView,
    ObserverDryRunRequest, ObserverDryRunView, ObserverEffectOutcome, ObserverEffectResponse,
    ObserverEventRequest, ObserverListResponse, ObserverPlannedEffectView, ObserverRunReport,
    ObserverRunResultResponse, ObserverRunView, ObserverScriptView, ObserverView,
    ObserverWatchListResponse, ObserverWatchView, ObserverWorkerCreated,
    ObserverWorkerListResponse, ObserverWorkerRun, ObserverWorkerView, SetObserverScriptRequest,
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

    /// Runs `narsor-cli observer ARGS…` as the owner; a dry run looks at
    /// most a few times and runs `worker` between two looks.
    async fn cli(
        &self,
        args: &[&str],
        worker: Option<&Arc<TestWorker>>,
    ) -> Result<String, (i32, String)> {
        let mut argv = vec!["narsor-cli", "observer"];
        argv.extend_from_slice(args);
        let Command::Observer { command } =
            Cli::try_parse_from(argv).expect("arguments parse").command
        else {
            panic!("not an observer command");
        };
        let worker = worker.cloned();
        let clock = Arc::clone(&self.clock);
        operate(
            &self.api(),
            command,
            None,
            move || clock.now(),
            move |_| {
                let worker = worker.clone();
                async move {
                    if let Some(worker) = worker {
                        worker.run_once().await.expect("the worker claims");
                    }
                }
            },
        )
        .await
        .map_err(|error| (error.exit_code, error.message))
    }

    /// Runs `narsor-cli observer worker-token ARGS…` as the owner.
    async fn worker_token(&self, args: &[&str]) -> Result<String, (i32, String)> {
        let mut argv = vec!["narsor-cli", "observer", "worker-token"];
        argv.extend_from_slice(args);
        let Command::Observer {
            command: ObserverCommand::WorkerToken { command },
        } = Cli::try_parse_from(argv).expect("arguments parse").command
        else {
            panic!("not a worker-token command");
        };
        let clock = Arc::clone(&self.clock);
        worker_token(&self.api(), command, move || clock.now())
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

impl WorkerTokenApi for ServiceApi {
    fn create_observer_worker<'a>(
        &'a self,
        request: &'a CreateObserverWorkerRequest,
    ) -> BoxFuture<'a, Result<ObserverWorkerCreated, ClientError>> {
        ready(
            self.service
                .create_observer_worker(&self.owner, request.clone()),
        )
    }

    fn observer_workers(&self) -> BoxFuture<'_, Result<ObserverWorkerListResponse, ClientError>> {
        ready(self.service.observer_workers(&self.owner))
    }

    fn revoke_observer_worker<'a>(
        &'a self,
        worker_id: &'a str,
    ) -> BoxFuture<'a, Result<ObserverWorkerView, ClientError>> {
        ready(self.service.revoke_observer_worker(&self.owner, worker_id))
    }
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

    fn request_observer_dry_run<'a>(
        &'a self,
        observer: &'a str,
        request: &'a ObserverDryRunRequest,
    ) -> BoxFuture<'a, Result<ObserverDryRunView, ClientError>> {
        ready(
            self.service
                .request_observer_dry_run_for(&self.owner, observer, request.clone()),
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
                .claim_observer_run_for(&self.owner)
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
            .and_then(|run_id| {
                self.service
                    .finish_observer_run_for(&self.owner, &run_id, report.clone())
            })
            .map(|run| ObserverRunResultResponse {
                run_id: run.run_id().to_string(),
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
        "--effect-key".into(),
        key.into(),
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
    // F1 and F2 by the CLI: create, save a script, dry-run it with a worker
    // whose script creates tasks, approve, and the scheduled run creates
    // the task, once.
    let fx = Fixture::new();
    let created = fx.cli(CREATE_JIRA, None).await.unwrap();
    assert!(created.contains("Created observer 'jira-new'"), "{created}");
    assert!(created.contains("state draft"), "{created}");
    let file = fx.script_file("jira.sh", "narsor-cli task create --effect-key \"$KEY\"\n");
    let saved = fx
        .cli(&["script", "set", "jira-new", "--file", &file], None)
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
    let dry = fx
        .cli(&["dry-run", "jira-new", "--wait", "6"], Some(&worker))
        .await
        .unwrap();

    assert!(dry.contains("Dry run of version 1"), "{dry}");
    assert!(dry.contains("succeeded"), "{dry}");
    assert!(dry.contains("Nothing was created. It would have:"), "{dry}");
    assert!(dry.contains("created task"), "{dry}");
    assert!(dry.contains("(key ABC-1)"), "{dry}");
    let would = script.outputs();
    assert_eq!(would.len(), 1);
    assert!(
        would[0].as_ref().unwrap().contains("dry run: would create"),
        "{would:?}"
    );
    assert_eq!(fx.tasks(), 0);
    let record = fx
        .service
        .observers()
        .find_by_name("jira-new")
        .unwrap()
        .unwrap();
    assert_eq!(record.state, ObserverState::Draft);

    let approved = fx
        .cli(&["approve", "jira-new", "--version", "1"], None)
        .await
        .unwrap();
    assert!(approved.contains("state active"), "{approved}");
    fx.service.observer_pass();
    let handled = worker.run_once().await.unwrap().expect("a run is due");
    assert!(handled.accepted.is_some());

    let outputs = script.outputs();
    let created = outputs.last().unwrap().as_ref().unwrap();
    let task_id = effect_task_id(&record.observer_id, "ABC-1");
    assert!(
        created.contains(&format!("created task {task_id}")),
        "{created}"
    );
    assert_eq!(fx.tasks(), 1);

    // The next run finds the same issue: a duplicate, exit 0.
    fx.advance(Duration::seconds(301));
    fx.service.observer_pass();
    worker.run_once().await.unwrap().expect("a run is due");
    let again = script.outputs().last().unwrap().clone();
    assert!(
        again.as_ref().unwrap().starts_with("skipped: duplicate"),
        "{again:?}"
    );
    assert_eq!(fx.tasks(), 1);

    let listed = fx.cli(&["list"], None).await.unwrap();
    assert!(listed.contains("jira-new"), "{listed}");
    assert!(listed.contains("succeeded"), "{listed}");
}

#[tokio::test]
async fn observer_list_shows_state_last_run_and_its_error() {
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA, None).await.unwrap();
    let file = fx.script_file("jira.sh", "exit 3\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file], None)
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"], None)
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

    let listed = fx.cli(&["list"], None).await.unwrap();

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
async fn observer_list_and_show_warn_when_no_worker_claims_a_due_run() {
    // R18: a due run waiting longer than one interval.
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA, None).await.unwrap();
    let file = fx.script_file("jira.sh", "true\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file], None)
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"], None)
        .await
        .unwrap();
    fx.service.observer_pass();
    fx.advance(Duration::seconds(301));

    let listed = fx.cli(&["list"], None).await.unwrap();
    let shown = fx.cli(&["show", "jira-new"], None).await.unwrap();

    assert!(listed.contains("warning: no worker"), "{listed}");
    assert!(shown.contains("Warning: no worker"), "{shown}");
}

#[tokio::test]
async fn a_script_saved_on_an_active_observer_is_a_draft_beside_the_approved_version() {
    // Covers AE6.
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA, None).await.unwrap();
    let first = fx.script_file("v1.sh", "true\n");
    let second = fx.script_file("v2.sh", "echo changed\n");
    fx.cli(&["script", "set", "jira-new", "--file", &first], None)
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"], None)
        .await
        .unwrap();

    let saved = fx
        .cli(&["script", "set", "jira-new", "--file", &second], None)
        .await
        .unwrap();
    let shown = fx.cli(&["show", "jira-new"], None).await.unwrap();
    let listed = fx.cli(&["list"], None).await.unwrap();

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
async fn a_dry_run_without_a_worker_ends_with_the_no_worker_message() {
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA, None).await.unwrap();
    let file = fx.script_file("jira.sh", "true\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file], None)
        .await
        .unwrap();

    let (code, message) = fx
        .cli(&["dry-run", "jira-new", "--wait", "4"], None)
        .await
        .unwrap_err();

    assert_eq!(code, 1);
    assert!(message.starts_with("No worker"), "{message}");
    assert!(message.contains("within 4s"), "{message}");
    assert!(message.contains("narsor-cli observer worker"), "{message}");
    let shown = fx.cli(&["show", "jira-new"], None).await.unwrap();
    assert!(shown.contains("Pending dry run: version 1"), "{shown}");
}

#[tokio::test]
async fn an_observer_create_with_a_bad_condition_or_mapping_is_a_usage_error() {
    let fx = Fixture::new();

    let (code, message) = fx
        .cli(
            &[
                "create",
                "jira-new",
                "--source",
                "jira",
                "--workflow",
                "triage",
                "--condition",
                "[1]",
            ],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(code, 2);
    assert!(message.contains("--condition"), "{message}");

    let (code, message) = fx
        .cli(
            &[
                "create",
                "jira-new",
                "--source",
                "jira",
                "--workflow",
                "triage",
                "--map",
                "ticket",
            ],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(code, 2);
    assert!(message.contains("--map"), "{message}");
    assert!(fx.service.observers().list().unwrap().is_empty());
}

#[tokio::test]
async fn another_owners_observer_is_not_found() {
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA, None).await.unwrap();
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

    let error = operate(&stranger, command, None, Utc::now, |_| async {})
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
    fx.cli(CREATE_REVIEW, None).await.unwrap();
    let task = fx.task("task-1");
    let object = format!("{PREFIX}a/-/merge_requests/17");

    let added = fx
        .cli(
            &[
                "watch",
                "add",
                "mr-review",
                "--task",
                task.as_str(),
                "--object",
                &object,
                "--events",
                "approved,merged",
            ],
            None,
        )
        .await
        .unwrap();
    let listed = fx.cli(&["watch", "list", "mr-review"], None).await.unwrap();

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
        .cli(&["watch", "remove", "mr-review", entry], None)
        .await
        .unwrap();
    assert!(removed.starts_with("Removed watch entry"), "{removed}");
    let empty = fx.cli(&["watch", "list", "mr-review"], None).await.unwrap();
    assert_eq!(empty, "No watch entries.\n");
}

#[tokio::test]
async fn an_operator_runs_the_review_flow_through_the_cli_alone() {
    // F3 by the CLI: the event of a watched MR reaches the task's history,
    // once.
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW, None).await.unwrap();
    let task = fx.task("task-1");
    let object = format!("{PREFIX}a/-/merge_requests/17");
    fx.cli(
        &[
            "watch",
            "add",
            "mr-review",
            "--task",
            "task-1",
            "--object",
            &object,
            "--events",
            "approved",
        ],
        None,
    )
    .await
    .unwrap();
    let file = fx.script_file("mr.sh", "narsor-cli task event ...\n");
    fx.cli(&["script", "set", "mr-review", "--file", &file], None)
        .await
        .unwrap();
    fx.cli(&["approve", "mr-review", "--version", "1"], None)
        .await
        .unwrap();
    let event = [
        "task",
        "event",
        "task-1",
        "--type",
        "approved",
        "--key",
        "MR!17:approved",
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
    assert!(second.starts_with("skipped: duplicate"), "{second}");
    let external = fx
        .service
        .control_plane()
        .events_since(&task, 0)
        .unwrap()
        .iter()
        .filter(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. }))
        .count();
    assert_eq!(external, 1);
}

#[tokio::test]
async fn event_data_with_control_characters_is_printed_without_them() {
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW, None).await.unwrap();
    fx.task("task-1");
    let object = format!("{PREFIX}a/-/merge_requests/17");
    fx.cli(
        &[
            "watch",
            "add",
            "mr-review",
            "--task",
            "task-1",
            "--object",
            &object,
            "--events",
            "approved",
        ],
        None,
    )
    .await
    .unwrap();
    let file = fx.script_file("mr.sh", "true\n");
    fx.cli(&["script", "set", "mr-review", "--file", &file], None)
        .await
        .unwrap();
    let data =
        json!({"title": "fix\u{1b}[2J\u{1b}[31mred\u{202e}", "by\u{7}": "bob\r"}).to_string();
    let event = [
        "task",
        "event",
        "task-1",
        "--type",
        "approved",
        "--key",
        "MR!17:approved",
        "--data",
        &data,
    ];
    let (worker, _script) = fx.worker(&[&event]);

    let dry = fx
        .cli(&["dry-run", "mr-review", "--wait", "6"], Some(&worker))
        .await
        .unwrap();

    assert!(dry.contains("delivered an event to task task-1"), "{dry}");
    assert!(dry.contains("fix[2J[31mred"), "{dry}");
    for unsafe_char in ['\u{1b}', '\u{7}', '\r', '\u{202e}'] {
        assert!(!dry.contains(unsafe_char), "{dry:?}");
    }
    let detail = fx.service.observer_detail(&owner(), "mr-review").unwrap();
    let shown = fx.cli(&["show", "mr-review"], None).await.unwrap();
    assert!(!shown.contains('\u{1b}'), "{shown:?}");
    assert_eq!(detail.runs[0].planned_effects.len(), 1);
}

#[test]
fn the_text_of_untrusted_answers_has_no_control_characters() {
    let effect = ObserverEffectResponse {
        outcome: ObserverEffectOutcome::Duplicate,
        effect_key: "K\u{1b}[31m".into(),
        task_id: None,
        input_id: None,
        message: "skipped: duplicate\u{7}".into(),
    };
    assert_eq!(
        effect_output(&effect),
        "skipped: duplicate (effect key K[31m)\n"
    );

    let run = ObserverRunView {
        run_id: "obs-1.1".into(),
        sequence: 1,
        dry_run: true,
        script_version: 1,
        status: "failed".into(),
        claimed_at: Utc::now(),
        finished_at: Some(Utc::now()),
        exit_code: Some(1),
        error: Some("the script exited 1\u{1b}]0;title\u{7}".into()),
        tasks_created: 0,
        planned_effects: vec![ObserverPlannedEffectView {
            kind: "create_task".into(),
            effect_key: "ABC\u{0}-1".into(),
            task_id: Some("task-x".into()),
            data: json!({"k\u{1b}": "v\u{9b}"}),
        }],
        stderr: Some("line\u{1b}[1m one\nline two\r".into()),
    };
    let text = dry_run_output(&run);
    for unsafe_char in ['\u{1b}', '\u{7}', '\u{0}', '\u{9b}', '\r'] {
        assert!(!text.contains(unsafe_char), "{text:?}");
    }
    assert!(
        text.contains("stderr:\n  line[1m one\n  line two\n"),
        "{text}"
    );

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
    fx.cli(&create, None).await.unwrap();
    let file = fx.script_file("jira.sh", "true\n");
    fx.cli(&["script", "set", "jira-new", "--file", &file], None)
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"], None)
        .await
        .unwrap();
    fx.service.observer_pass();
    let run = fx.service.claim_observer_run().unwrap().unwrap();
    (run.run_id.to_string(), run.token.expose().to_string())
}

#[tokio::test]
async fn a_duplicate_key_exits_0_and_a_run_over_its_limit_exits_1() {
    let fx = Fixture::new();
    let (run, token) = claimed_jira_run(&fx, "1").await;

    let created = script_cli(&fx.service, &create_line("ABC-1"), Some(&run), &token).await;
    let duplicate = script_cli(&fx.service, &create_line("ABC-1"), Some(&run), &token).await;
    let over = script_cli(&fx.service, &create_line("ABC-2"), Some(&run), &token).await;

    assert!(created.unwrap().starts_with("created task"));
    assert!(duplicate.unwrap().starts_with("skipped: duplicate"));
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

// ---------------------------------------------------------------------------
// observer worker-token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_token_create_writes_a_new_file_of_mode_0600_and_never_overwrites() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let path = fx.root.join("credential");
    let file = path.to_str().unwrap();

    let printed = fx
        .worker_token(&["create", "--name", "host-1", "--output", file])
        .await
        .unwrap();

    let credential = fs::read_to_string(&path).unwrap();
    let credential = credential.strip_suffix('\n').expect("one line");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !printed.contains(credential),
        "the credential is not printed"
    );
    assert!(printed.contains("mode 0600"), "{printed}");
    let identity = fx
        .service
        .authenticate_observer_worker(credential)
        .expect("the file holds a working credential");
    assert_eq!(identity.owner, owner());

    let (code, message) = fx
        .worker_token(&["create", "--output", file])
        .await
        .unwrap_err();

    assert_eq!(code, 2);
    assert!(message.contains("already exists"), "{message}");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{credential}\n"),
        "the file is unchanged"
    );
    assert_eq!(
        fx.service.observer_workers(&owner()).unwrap().workers.len(),
        1,
        "no credential is created for a refused file"
    );
}

#[tokio::test]
async fn worker_token_create_without_output_prints_the_credential_once() {
    let fx = Fixture::new();

    let printed = fx.worker_token(&["create"]).await.unwrap();

    let credential = printed
        .lines()
        .find_map(|line| line.strip_prefix("Credential: "))
        .expect("the credential is printed");
    assert!(fx
        .service
        .authenticate_observer_worker(credential)
        .is_some());
}

#[tokio::test]
async fn worker_token_list_and_revoke() {
    let fx = Fixture::new();
    assert_eq!(
        fx.worker_token(&["list"]).await.unwrap(),
        "No worker credentials.\n"
    );
    let printed = fx
        .worker_token(&["create", "--name", "host-1"])
        .await
        .unwrap();
    let worker_id = printed
        .lines()
        .find_map(|line| line.strip_prefix("Worker: "))
        .expect("the worker id is printed")
        .to_string();
    let credential = printed
        .lines()
        .find_map(|line| line.strip_prefix("Credential: "))
        .unwrap()
        .to_string();

    let listed = fx.worker_token(&["list"]).await.unwrap();
    assert!(listed.contains(&worker_id), "{listed}");
    assert!(listed.contains("host-1"), "{listed}");
    assert!(listed.contains("active"), "{listed}");
    assert!(!listed.contains(&credential), "{listed}");

    let revoked = fx.worker_token(&["revoke", &worker_id]).await.unwrap();

    assert_eq!(revoked, format!("Revoked worker credential {worker_id}.\n"));
    let listed = fx.worker_token(&["list"]).await.unwrap();
    assert!(listed.contains("revoked"), "{listed}");
    assert!(fx
        .service
        .authenticate_observer_worker(&credential)
        .is_none());
    let (code, _) = fx
        .worker_token(&["revoke", "wkr-no-such-worker"])
        .await
        .unwrap_err();
    assert_eq!(code, 1);
}
