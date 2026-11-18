//! Integration tests of `narsor-cli observer …` and of the effect commands
//! of an observer's script (U7), against a real `ProtocolService` over a
//! seeded catalogue, without a server.
//!
//! The owner's commands run through [`operate`] on the [`ObserverApi`]
//! seam, implemented here on the service for one owner, as the owner
//! routes call it. A script's `task create --data` is parsed by the real
//! command line and sent through [`perform`] on the
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
use narsor::cli::observer::{operate, update_observer_request, ObserverApi, UpdateArgs};
use narsor::cli::observer_effect::{effect_call, perform, EffectApi};
use narsor::cli::observer_text::{effect_output, observer_list_output};
use narsor::cli::{Cli, Command};
use narsor_engine::controller_protocol::{ProtocolError, ProtocolService, RunOutcome, RunReport};
use narsor_engine::domain::TaskId;
use narsor_engine::observers::{ObserverState, QueueEntry, RunId};
use narsor_engine::orchestration::{Clock, ControlPlane, OwnerRef};
use narsor_engine::persistence::StatePaths;
use narsor_observer_worker::worker::{
    ScriptEnd, ScriptExit, ScriptRunner, ScriptSpec, Worker, WorkerApi, WorkerOptions,
};
use narsor_protocol::controller::{
    ApproveObserverRequest, CreateObserverRequest, GateWatch, GateWatchChoice,
    ObserverCreateTaskRequest, ObserverDecideRequest, ObserverDetailView, ObserverEffectOutcome,
    ObserverEffectResponse, ObserverListResponse, ObserverQueueListResponse, ObserverQueueView,
    ObserverRunReport, ObserverRunResultResponse, ObserverScriptTextView, ObserverScriptView,
    ObserverView, ObserverWorkerRun, SetObserverScriptRequest, UpdateObserverRequest,
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

    fn update_observer<'a>(
        &'a self,
        observer: &'a str,
        request: &'a UpdateObserverRequest,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(
            self.service
                .update_observer(&self.owner, observer, request.clone()),
        )
    }

    fn delete_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, Result<ObserverView, ClientError>> {
        ready(self.service.delete_observer(&self.owner, observer))
    }

    fn observer_queues(&self) -> BoxFuture<'_, Result<ObserverQueueListResponse, ClientError>> {
        ready(self.service.observer_queues_of(&self.owner))
    }

    fn observer_queue<'a>(
        &'a self,
        queue: &'a str,
    ) -> BoxFuture<'a, Result<ObserverQueueView, ClientError>> {
        ready(self.service.observer_queue(&self.owner, queue))
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

    fn observer_script<'a>(
        &'a self,
        observer: &'a str,
        version: u32,
    ) -> BoxFuture<'a, Result<ObserverScriptTextView, ClientError>> {
        ready(self.service.observer_script(&self.owner, observer, version))
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
        shown.contains("Script: approved version 1; version 2 not approved"),
        "{shown}"
    );
    assert!(shown.contains("State: active"), "{shown}");
    assert!(listed.contains("v1 (draft v2)"), "{listed}");
    assert!(
        listed.contains(
            "  script: version 2 not approved: approve it with `narsor-cli observer approve \
             jira-new --version 2`"
        ),
        "{listed}"
    );
    let record = fx
        .service
        .observers()
        .find_by_name("jira-new")
        .unwrap()
        .unwrap();
    assert_eq!(record.approved_version, Some(1));
}

#[tokio::test]
async fn script_show_prints_the_newest_version_or_the_one_asked_with_its_approval() {
    // R32: the owner reads what they approve.
    let fx = Fixture::new();
    fx.cli(CREATE_JIRA).await.unwrap();
    let (code, message) = fx.cli(&["script", "show", "jira-new"]).await.unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("has no script version yet"), "{message}");

    let first = fx.script_file("v1.sh", "true\n");
    let second = fx.script_file("v2.sh", "echo changed\n\techo \u{1b}[31mred\n");
    fx.cli(&["script", "set", "jira-new", "--file", &first])
        .await
        .unwrap();
    fx.cli(&["approve", "jira-new", "--version", "1"])
        .await
        .unwrap();
    fx.cli(&["script", "set", "jira-new", "--file", &second])
        .await
        .unwrap();

    let newest = fx.cli(&["script", "show", "jira-new"]).await.unwrap();
    let approved = fx
        .cli(&["script", "show", "jira-new", "--version", "1"])
        .await
        .unwrap();

    let (header, text) = newest.split_once("\n---\n").expect("a header and the text");
    assert!(
        header.starts_with("Script version 2 of 'jira-new' (sha256:"),
        "{header}"
    );
    assert!(
        header.contains(
            "not approved: approve it with `narsor-cli observer approve jira-new --version 2`"
        ),
        "{header}"
    );
    assert_eq!(
        text, "echo changed\n echo [31mred\n",
        "escape removed, tab a space"
    );
    let (header, text) = approved.split_once("\n---\n").unwrap();
    assert!(
        header.starts_with("Script version 1 of 'jira-new'"),
        "{header}"
    );
    assert!(
        header.contains("approved: the schedule runs it"),
        "{header}"
    );
    assert_eq!(text, "true\n");

    let (code, message) = fx
        .cli(&["script", "show", "jira-new", "--version", "7"])
        .await
        .unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("Could not read version 7"), "{message}");
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
// Queues, run modes and schedules
// ---------------------------------------------------------------------------

/// `observer create gitlab-mr` for the gates of the `mr-review` queue, on
/// a cron schedule.
const CREATE_REVIEW: &[&str] = &[
    "create",
    "gitlab-mr",
    "--source",
    "gitlab",
    "--queue",
    "mr-review",
    "--cron",
    "*/5 * * * *",
];

#[tokio::test]
async fn an_observer_of_a_queue_shows_its_queue_mode_and_cron() {
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();

    let shown = fx.cli(&["show", "gitlab-mr"]).await.unwrap();

    assert!(shown.contains("Effect: creates no tasks"), "{shown}");
    assert!(shown.contains("Queue: mr-review"), "{shown}");
    assert!(shown.contains("Mode: on_episodes"), "{shown}");
    assert!(
        shown.contains("Schedule: cron '*/5 * * * *' (UTC)"),
        "{shown}"
    );
    let jira = fx.cli(CREATE_JIRA).await.unwrap();
    assert!(jira.contains("Created observer 'jira-new'"), "{jira}");
    let shown = fx.cli(&["show", "jira-new"]).await.unwrap();
    assert!(shown.contains("Queue: jira-new"), "{shown}");
    assert!(shown.contains("Mode: always"), "{shown}");
    assert!(shown.contains("Schedule: every 300s"), "{shown}");
}

#[tokio::test]
async fn an_observer_of_another_mode_in_a_queue_is_refused_with_the_queues_mode() {
    // Covers AE8 by the CLI.
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();

    let (code, message) = fx
        .cli(&[
            "create",
            "jira-new",
            "--source",
            "jira",
            "--queue",
            "mr-review",
            "--mode",
            "always",
        ])
        .await
        .unwrap_err();

    assert_eq!(code, 1);
    assert!(message.contains("'mr-review'"), "{message}");
    assert!(message.contains("on_episodes"), "{message}");
    assert_eq!(fx.service.observers().list().unwrap().len(), 1);
}

#[tokio::test]
async fn a_cron_with_seconds_is_refused_and_cron_with_interval_does_not_parse() {
    // Covers AE9 by the CLI.
    let fx = Fixture::new();
    let (code, message) = fx
        .cli(&[
            "create",
            "gitlab-mr",
            "--source",
            "gitlab",
            "--cron",
            "*/30 * * * * *",
        ])
        .await
        .unwrap_err();
    assert_eq!(code, 1);
    assert!(message.contains("seconds field"), "{message}");

    for argv in [
        &[
            "narsor-cli",
            "observer",
            "create",
            "x",
            "--source",
            "s",
            "--cron",
            "* * * * *",
            "--interval",
            "60",
        ][..],
        &[
            "narsor-cli",
            "observer",
            "create",
            "x",
            "--source",
            "s",
            "--mode",
            "sometimes",
        ][..],
    ] {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
    assert!(fx.service.observers().list().unwrap().is_empty());
}

#[tokio::test]
async fn a_removed_flag_of_observer_create_fails_with_a_hint_at_its_replacement() {
    let fx = Fixture::new();

    for (flag, value) in [
        ("--deliver-events", None),
        ("--watch-list", None),
        ("--allow-prefix", Some("https://gitlab.example.com/")),
        ("--max-object-len", Some("200")),
    ] {
        let mut args = vec!["create", "gitlab-mr", "--source", "gitlab", flag];
        args.extend(value);
        let (code, message) = fx.cli(&args).await.unwrap_err();
        assert_eq!(code, 2, "{flag}: {message}");
        assert!(message.contains(flag), "{message}");
        assert!(message.contains("removed"), "{message}");
        assert!(message.contains("--queue QUEUE"), "{message}");
    }
    assert!(fx.service.observers().list().unwrap().is_empty());
}

/// The entry of `episode` of `task` of the test's owner on `queue`, as a
/// runner's gate with a `watch` block adds it.
fn add_entry(fx: &Fixture, queue: &str, task: &str, episode: &str) {
    let gate = GateWatch {
        step: "review_work".into(),
        queue: queue.into(),
        params: [(
            "mr_url".to_string(),
            "https://gitlab/a/-/merge_requests/17".to_string(),
        )]
        .into(),
        choices: vec![GateWatchChoice {
            name: "check_feedback".into(),
            description: None,
            requires_variables: Vec::new(),
        }],
        variables: Vec::new(),
    };
    let entry = QueueEntry::from_gate(
        owner(),
        TaskId::parse(task).unwrap(),
        episode,
        &gate,
        fx.clock.now(),
    );
    let observers = fx.service.observers();
    let lock = observers.lock_queue_entries().unwrap();
    observers.add_queue_entry(&lock, &entry).unwrap();
}

#[tokio::test]
async fn an_observer_update_moves_it_to_another_queue_and_schedule() {
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();

    let updated = fx
        .cli(&[
            "update",
            "gitlab-mr",
            "--queue",
            "mr-merge",
            "--interval",
            "120",
        ])
        .await
        .unwrap();

    assert!(
        updated.contains(
            "Updated observer 'gitlab-mr': queue mr-merge, mode on_episodes, schedule every 120s; \
             state draft"
        ),
        "{updated}"
    );
    let shown = fx.cli(&["show", "gitlab-mr"]).await.unwrap();
    assert!(shown.contains("Queue: mr-merge"), "{shown}");
    assert!(shown.contains("Schedule: every 120s"), "{shown}");

    let updated = fx
        .cli(&["update", "gitlab-mr", "--cron", "0 * * * *"])
        .await
        .unwrap();
    assert!(
        updated.contains("schedule cron '0 * * * *' (UTC)"),
        "{updated}"
    );
    assert!(
        updated.contains("queue mr-merge"),
        "the queue stays: {updated}"
    );
}

#[tokio::test]
async fn an_observer_update_of_another_mode_in_a_queue_is_refused() {
    // Covers AE8 by `observer update`.
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();
    fx.cli(CREATE_JIRA).await.unwrap();

    let (code, message) = fx
        .cli(&["update", "jira-new", "--queue", "mr-review"])
        .await
        .unwrap_err();

    assert_eq!(code, 1);
    assert!(
        message.contains("Could not update the observer"),
        "{message}"
    );
    assert!(message.contains("on_episodes"), "{message}");
    let shown = fx.cli(&["show", "jira-new"]).await.unwrap();
    assert!(shown.contains("Queue: jira-new"), "{shown}");
}

#[tokio::test]
async fn an_observer_update_without_a_change_or_with_both_schedules_is_a_usage_error() {
    for argv in [
        &["narsor-cli", "observer", "update", "gitlab-mr"][..],
        &[
            "narsor-cli",
            "observer",
            "update",
            "gitlab-mr",
            "--cron",
            "* * * * *",
            "--interval",
            "60",
        ][..],
        &[
            "narsor-cli",
            "observer",
            "update",
            "gitlab-mr",
            "--mode",
            "sometimes",
        ][..],
    ] {
        let error = Cli::try_parse_from(argv).unwrap_err();
        assert_eq!(error.exit_code(), 2, "{argv:?}: {error}");
    }

    // The same checks without the parser, as `operate` makes them.
    let args = |interval: Option<u64>, cron: Option<&str>| UpdateArgs {
        observer: "gitlab-mr".into(),
        queue: None,
        mode: None,
        interval,
        cron: cron.map(str::to_string),
    };
    let empty = update_observer_request(&args(None, None)).unwrap_err();
    assert_eq!(empty.exit_code, 2);
    assert!(empty.message.contains("Nothing to change"), "{empty:?}");
    let both = update_observer_request(&args(Some(60), Some("* * * * *"))).unwrap_err();
    assert_eq!(both.exit_code, 2);
    assert!(both.message.contains("not both"), "{both:?}");
}

#[tokio::test]
async fn queue_list_and_show_give_the_observers_and_entries_of_a_queue() {
    let fx = Fixture::new();
    assert!(fx
        .cli(&["queue", "list"])
        .await
        .unwrap()
        .starts_with("No queues."));
    fx.cli(CREATE_REVIEW).await.unwrap();
    fx.cli(CREATE_JIRA).await.unwrap();
    add_entry(&fx, "mr-review", "task-1", "input-1");
    add_entry(&fx, "nobody-serves", "task-2", "input-1");
    fx.advance(Duration::minutes(4));

    let listed = fx.cli(&["queue", "list"]).await.unwrap();
    let lines: Vec<&str> = listed.lines().collect();
    assert!(lines[0].starts_with("QUEUE"), "{listed}");
    let row = |queue: &str| {
        lines
            .iter()
            .find(|line| line.starts_with(&format!("{queue} ")))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .unwrap_or_else(|| panic!("no {queue}: {listed}"))
    };
    assert_eq!(row("mr-review"), ["mr-review", "on_episodes", "1", "1"]);
    assert_eq!(row("jira-new"), ["jira-new", "always", "1", "0"]);
    assert_eq!(row("nobody-serves"), ["nobody-serves", "-", "0", "1"]);

    let shown = fx.cli(&["queue", "show", "mr-review"]).await.unwrap();
    assert!(shown.starts_with("Queue: mr-review\n"), "{shown}");
    assert!(shown.contains("Mode: on_episodes"), "{shown}");
    assert!(shown.contains("  gitlab-mr "), "{shown}");
    assert!(shown.contains("cron '*/5 * * * *' (UTC)"), "{shown}");
    assert!(shown.contains("Entries (oldest first):"), "{shown}");
    assert!(shown.contains("  task-1 "), "{shown}");
    assert!(shown.contains("review_work"), "{shown}");
    assert!(shown.contains(" 4m\n"), "the entry waits 4m: {shown}");
    assert!(
        shown.contains("    params: mr_url=https://gitlab/a/-/merge_requests/17"),
        "{shown}"
    );
    assert!(shown.contains("    gitlab-mr: no run yet"), "{shown}");

    let unserved = fx.cli(&["queue", "show", "nobody-serves"]).await.unwrap();
    assert!(
        unserved.contains("Mode: - (no observer serves it)"),
        "{unserved}"
    );
    assert!(
        unserved.contains("Observers: none; its entries wait for a person"),
        "{unserved}"
    );
    let empty = fx.cli(&["queue", "show", "jira-new"]).await.unwrap();
    assert!(empty.contains("Entries: none"), "{empty}");
}

#[tokio::test]
async fn queue_show_marks_a_newest_version_that_is_not_approved() {
    // Covers AE10 by the CLI.
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();
    let first = fx.script_file("v1.sh", "true\n");
    let second = fx.script_file("v2.sh", "echo changed\n");
    fx.cli(&["script", "set", "gitlab-mr", "--file", &first])
        .await
        .unwrap();
    let draft = fx.cli(&["queue", "show", "mr-review"]).await.unwrap();
    assert!(
        draft.contains("    script: version 1 not approved"),
        "{draft}"
    );

    fx.cli(&["approve", "gitlab-mr", "--version", "1"])
        .await
        .unwrap();
    let approved = fx.cli(&["queue", "show", "mr-review"]).await.unwrap();
    assert!(!approved.contains("not approved"), "{approved}");

    fx.cli(&["script", "set", "gitlab-mr", "--file", &second])
        .await
        .unwrap();
    let shown = fx.cli(&["queue", "show", "mr-review"]).await.unwrap();
    assert!(shown.contains("v1 (draft v2)"), "{shown}");
    assert!(
        shown.contains(
            "    script: version 2 not approved: approve it with `narsor-cli observer approve \
             gitlab-mr --version 2`"
        ),
        "{shown}"
    );
}

#[tokio::test]
async fn another_owners_queue_is_not_found() {
    let fx = Fixture::new();
    fx.cli(CREATE_REVIEW).await.unwrap();
    add_entry(&fx, "mr-review", "task-1", "input-1");
    let stranger = ServiceApi {
        service: Arc::clone(&fx.service),
        owner: OwnerRef::new("someone-else"),
    };
    let parse = |args: &[&str]| {
        let mut argv = vec!["narsor-cli", "observer"];
        argv.extend_from_slice(args);
        let Command::Observer { command } = Cli::try_parse_from(argv).unwrap().command else {
            panic!("not an observer command");
        };
        command
    };

    let error = operate(
        &stranger,
        parse(&["queue", "show", "mr-review"]),
        None,
        Utc::now,
    )
    .await
    .unwrap_err();
    assert_eq!(error.exit_code, 1);
    assert_eq!(error.message, "Could not read queue 'mr-review': not found");

    let listed = operate(&stranger, parse(&["queue", "list"]), None, Utc::now)
        .await
        .unwrap();
    assert!(listed.starts_with("No queues."), "{listed}");

    let error = operate(
        &stranger,
        parse(&["update", "gitlab-mr", "--queue", "mine"]),
        None,
        Utc::now,
    )
    .await
    .unwrap_err();
    assert_eq!(error.exit_code, 1);
    assert!(error.message.contains("not found"), "{}", error.message);
}

#[test]
fn the_text_of_untrusted_answers_has_no_control_characters() {
    let effect = ObserverEffectResponse {
        outcome: ObserverEffectOutcome::Created,
        task_id: None,
        input_id: None,
        message: "created\u{7} task \u{1b}[31mtask-1".into(),
    };
    assert_eq!(effect_output(&effect), "created task [31mtask-1\n");

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

/// An approved `on_episodes` observer of `mr-review` made through the CLI,
/// a task waiting at its gate in `input-1`, and the run claimed for the
/// entry; returns the run's id and token.
async fn claimed_review_run(fx: &Fixture) -> (String, String) {
    fx.cli(CREATE_REVIEW).await.unwrap();
    let file = fx.script_file("review.sh", "true\n");
    fx.cli(&["script", "set", "gitlab-mr", "--file", &file])
        .await
        .unwrap();
    fx.cli(&["approve", "gitlab-mr", "--version", "1"])
        .await
        .unwrap();
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
