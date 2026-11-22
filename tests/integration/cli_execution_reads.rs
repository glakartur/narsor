//! What `narsor-cli list|show [--details]|events|steps|conversation` print
//! for a task's
//! execution, read from a served control plane (U8).
//!
//! The execution lives on the task's storage, which only its runner reads:
//! a fake runner here serves the relayed reads from it the way
//! `narsor-runner` does, and a fake controller starts a read-only runner
//! when a stopped task is read (D63). The server is given no path to that
//! storage. The lines are compared with timestamps replaced by `<T>`; they
//! are those the CLI printed when the server still read local storage
//! itself.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use narsor::cli::commands::{
    execution_conversation_lines, execution_event_lines, execution_list_lines,
    execution_show_details_lines, execution_show_lines, execution_steps_lines, task_state_lines,
};
use narsor_api_client::api::token::TokenSource;
use narsor_api_client::api::{ApiClient, ClientError};
use narsor_engine::api::{conversation_response_from_snapshot, execution_details, execution_state};
use narsor_engine::conductor::{
    Annotation, ConductorState, ConductorStore, FailureOutcome, FailureReport, PendingInvocation,
    QueuedRequest,
};
use narsor_engine::controller_protocol::{
    ControllerIdentity, ExecutionEventsPage, ExecutionStateResponse, ProtocolConfig,
    ProtocolService, RunnerCredentialRequest, RunnerDirective, RunnerIdentity, RunnerPollQuery,
    LOCAL_OWNER,
};
use narsor_engine::domain::{
    BackendId, ConductorPurpose, ConductorTrigger, Context, Event, EventData, ExecutionStatus,
    GitIdentity, RepositoryInfo, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace, WorkflowExecution, Workspace,
};
use narsor_engine::engine::{current_timeline, PendingHumanInput};
use narsor_engine::orchestration::{
    BackendAvailability, BackendLease, CommandRequest, ControlPlane, ControllerReport,
    ConversationAttemptSummary, ConversationAvailability, ExecutionCursor, NewTask, Observation,
    ObservedSummary, OwnerCommand, OwnerRef, RunnerSnapshot, SchedulingPolicy,
};
use narsor_engine::persistence::conversation::ConversationSource;
use narsor_engine::persistence::{ExecutionStore, ServerStatePaths};
use narsor_server::api::{start_with_services, ApiHandle, ApiServices};
use serde_json::json;
use tokio::task::JoinHandle;

const EXECUTION: &str = "exec-1";
const ATTEMPT: &str = "attempt-1";

/// The execution the runner of task `id` runs: [`EXECUTION`] for `task-1`.
fn execution_of(id: &str) -> String {
    match id {
        "task-1" => EXECUTION.to_string(),
        other => format!("exec-{other}"),
    }
}
const CONTROLLER: &str = "controller-a";
const RUNNER: &str = "pod-1";

/// How long the CLI waits for a stopped task's read-only runner here.
const READER_WAIT: Duration = Duration::from_secs(5);

/// A served control plane, a client of its owner, and the runners and
/// controller standing in for a backend's.
struct Fixture {
    root: PathBuf,
    service: Arc<ProtocolService>,
    /// Serves while the fixture lives.
    _server: ApiHandle,
    client: ApiClient,
    leases: Mutex<HashMap<String, BackendLease>>,
    runners: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-cli-reads-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        fs::create_dir_all(&state).unwrap();
        let service = Arc::new(ProtocolService::with_config(
            ControlPlane::new(ServerStatePaths::new(&state)),
            crate::environment_support::seeded_resolver(&state),
            ProtocolConfig {
                reader_connect_wait: Duration::from_millis(100),
                ..ProtocolConfig::default()
            },
        ));
        let server = start_with_services(
            root.join("catalog"),
            ApiServices {
                control: Some(Arc::clone(&service)),
                ..ApiServices::default()
            },
        )
        .await
        .expect("start the server");
        let client = ApiClient::remote(
            &format!("http://{}", server.addr()),
            TokenSource::Static(server.token().to_string()),
        )
        .expect("a loopback client");
        Self {
            root,
            service,
            _server: server,
            client,
            leases: Mutex::new(HashMap::new()),
            runners: Mutex::new(HashMap::new()),
        }
    }

    /// The backend's storage of task `id`, holding the execution
    /// [`execution_of`] it; nothing of the server's points at it.
    fn storage(&self, id: &str) -> ExecutionStore {
        let store = ExecutionStore::new(self.root.join("backend").join(id));
        if !store.exists(&execution_of(id)) {
            write_execution(&store, &execution_of(id));
        }
        store
    }

    /// The controller's lease of `backend`.
    fn lease(&self, backend: &BackendId) -> BackendLease {
        let mut leases = self.leases.lock().unwrap();
        leases
            .entry(backend.to_string())
            .or_insert_with(|| {
                self.service
                    .control_plane()
                    .acquire_lease(backend, CONTROLLER, chrono::Duration::days(1))
                    .unwrap()
            })
            .clone()
    }

    fn report(&self, task_id: &TaskId, lease: &BackendLease, observation: Observation) {
        let control = self.service.control_plane();
        control
            .report(ControllerReport {
                task_id: task_id.clone(),
                lease: lease.token(),
                desired_generation: control.task(task_id).unwrap().generation,
                observation,
                resource: None,
                summary: ObservedSummary {
                    workload_present: true,
                    storage_present: true,
                    runner_status: None,
                    detail: None,
                },
                observed_at: Utc::now(),
            })
            .expect("controller report");
    }

    /// A runner credential of `role` for task `id`, as its controller gets
    /// it.
    fn credential(&self, id: &str, backend: &BackendId, reader: bool) -> RunnerIdentity {
        let lease = self.lease(backend);
        let controller = ControllerIdentity {
            controller_id: CONTROLLER.to_string(),
            backend_id: backend.clone(),
        };
        let request = RunnerCredentialRequest {
            lease_epoch: lease.epoch,
            generation: self
                .service
                .control_plane()
                .task(&TaskId::parse(id).unwrap())
                .unwrap()
                .generation,
            incarnation: if reader { "reader-1" } else { RUNNER }.to_string(),
        };
        let issued = if reader {
            self.service
                .issue_reader_credential(&controller, id, request)
        } else {
            self.service
                .issue_runner_credential(&controller, id, request)
        }
        .expect("a runner credential");
        self.service
            .authenticate_runner(&issued.credential)
            .expect("the credential authenticates")
    }

    /// The task `id` of `owner` on `backend`, running: its runner reported
    /// the execution in its storage and answers the reads of it.
    fn running_task(&self, id: &str, owner: &str, backend: &str) {
        let task_id = TaskId::parse(id).unwrap();
        let backend_id = BackendId::parse(backend).unwrap();
        let control = self.service.control_plane();
        control
            .create_task(NewTask::new(
                task_id.clone(),
                OwnerRef::new(owner),
                backend_id.clone(),
                launch_spec(),
            ))
            .unwrap();
        let lease = self.lease(&backend_id);
        control
            .schedule(
                &backend_id,
                &SchedulingPolicy {
                    backend: BackendAvailability::Available,
                    max_concurrent_tasks: Some(16),
                },
            )
            .unwrap();
        self.report(&task_id, &lease, Observation::StartAccepted);
        self.report(&task_id, &lease, Observation::RunnerReady);
        let runner = self.credential(id, &backend_id, false);
        let store = self.storage(id);
        self.service
            .record_runner_snapshot(
                &task_id,
                RUNNER,
                runner.launch_generation,
                runner_snapshot(&store, &execution_of(id), 1, false),
                chrono::Duration::hours(1),
            )
            .expect("runner snapshot");
        let handle = tokio::spawn(serve_reads(
            Arc::clone(&self.service),
            runner,
            store,
            execution_of(id),
        ));
        self.runners.lock().unwrap().insert(id.to_string(), handle);
    }

    /// Pauses the running task `id`: its runner acknowledges a safe point
    /// and goes away, and the task is `Stopped`.
    fn stop(&self, id: &str, backend: &str) {
        let task_id = TaskId::parse(id).unwrap();
        let backend_id = BackendId::parse(backend).unwrap();
        let control = self.service.control_plane();
        let launch_generation = control.task(&task_id).unwrap().generation;
        control
            .command(
                &task_id,
                CommandRequest::new(OwnerCommand::Pause, "pause-1"),
            )
            .expect("pause");
        self.service
            .record_runner_snapshot(
                &task_id,
                RUNNER,
                launch_generation,
                runner_snapshot(&self.storage(id), &execution_of(id), 2, true),
                chrono::Duration::hours(1),
            )
            .expect("safe-point snapshot");
        self.report(
            &task_id,
            &self.lease(&backend_id),
            Observation::PausedAtSafePoint,
        );
        if let Some(runner) = self.runners.lock().unwrap().remove(id) {
            runner.abort();
        }
    }

    /// Stands in for the controller of `backend`: once a read of the stopped
    /// task `id` leases a reader, starts a read-only runner on its storage.
    fn start_readers(&self, id: &str, backend: &str) -> JoinHandle<()> {
        let service = Arc::clone(&self.service);
        let task_id = TaskId::parse(id).unwrap();
        let backend_id = BackendId::parse(backend).unwrap();
        // Credentials are issued here, not in the task: the lease is the
        // fixture's.
        let lease = self.lease(&backend_id);
        let store = self.storage(id);
        let execution = execution_of(id);
        tokio::spawn(async move {
            loop {
                let record = service.control_plane().task(&task_id).unwrap();
                if record.reader_lease_active(Utc::now()) {
                    let issued = service
                        .issue_reader_credential(
                            &ControllerIdentity {
                                controller_id: CONTROLLER.to_string(),
                                backend_id: backend_id.clone(),
                            },
                            task_id.as_str(),
                            RunnerCredentialRequest {
                                lease_epoch: lease.epoch,
                                generation: record.generation,
                                incarnation: "reader-1".to_string(),
                            },
                        )
                        .expect("a reader credential");
                    let reader = service.authenticate_runner(&issued.credential).unwrap();
                    serve_reads(service, reader, store, execution).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (_, runner) in self.runners.lock().unwrap().drain() {
            runner.abort();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A runner's side of the relayed reads: answers every events, state,
/// conversation and details read of its task from `store`, as
/// `narsor-runner` does, until it is told to go.
async fn serve_reads(
    service: Arc<ProtocolService>,
    identity: RunnerIdentity,
    store: ExecutionStore,
    execution: String,
) {
    let query = RunnerPollQuery {
        wait_secs: Some(1),
        defer_materials: true,
        ..RunnerPollQuery::default()
    };
    while let Ok(response) = service.runner_poll(&identity, query.clone()).await {
        // A runner told to go goes, as `narsor-runner` does.
        if response.directive != RunnerDirective::Continue {
            return;
        }
        for request in response.execution_events {
            let (events, has_more) = store
                .event_log(&execution)
                .read_after(request.after, request.limit as usize)
                .unwrap();
            let _ = service.runner_execution_events(
                &identity,
                &request.request_id,
                ExecutionEventsPage { events, has_more },
            );
        }
        for request in response.conversation_pages {
            let snapshot = store
                .conversation_store(&execution)
                .read(
                    &request.attempt_id,
                    request.from_entry,
                    request.byte_budget as usize,
                )
                .unwrap();
            let _ = service.runner_conversation_page(
                &identity,
                &request.request_id,
                conversation_response_from_snapshot(snapshot),
            );
        }
        for request in response.execution_details {
            let _ = service.runner_execution_details(
                &identity,
                &request.request_id,
                execution_details(&store, &execution).unwrap(),
            );
        }
        for request in response.execution_states {
            let _ = match execution_state(&store, &execution) {
                Ok(state) => service.runner_execution_state(&identity, &request.request_id, state),
                Err(error) => {
                    service.runner_relay_failure(&identity, &request.request_id, &error.to_string())
                }
            };
        }
    }
}

fn launch_spec() -> TaskLaunchSpec {
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
    TaskLaunchSpec::new(
        "steps",
        1,
        json!({}),
        workspace,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    )
}

/// An execution of `steps` that ran one agent attempt in `plan` and waits
/// in `review`: its record, its events and the attempt's conversation.
fn write_execution(store: &ExecutionStore, execution: &str) {
    store
        .save_execution(&WorkflowExecution {
            id: execution.to_string(),
            workflow_id: "steps".to_string(),
            workflow_version: 1,
            status: ExecutionStatus::Running,
            current_state: Some("review".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            inputs: json!({}),
            context: Context::new(),
            workspace: Workspace::git_checkout(
                "/tmp/workspace".to_string(),
                RepositoryInfo {
                    id: "repo-1".to_string(),
                    name: "narsor".to_string(),
                    provider: "gitlab".to_string(),
                    remote: "git@example.com:team/narsor.git".to_string(),
                },
                Some("main".to_string()),
            ),
            current_action_execution: None,
            metadata: json!({}),
        })
        .unwrap();
    let log = store.event_log(execution);
    let events = [
        EventData::WorkflowStarted {
            workflow_id: "steps".to_string(),
            workflow_version: 1,
            inputs: json!({}),
        },
        EventData::StateEntered {
            state_id: "plan".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: ATTEMPT.to_string(),
            state_id: "plan".to_string(),
            action_type: "agent".to_string(),
            decision_by: None,
        },
        EventData::ActionCompleted {
            action_execution_id: ATTEMPT.to_string(),
            output_label: "success".to_string(),
            reason: None,
            additional_data: Default::default(),
            warnings: Vec::new(),
        },
        EventData::StateEntered {
            state_id: "review".to_string(),
        },
    ];
    for (index, data) in events.into_iter().enumerate() {
        log.append(&Event::new(execution.to_string(), data, index as u64 + 1))
            .unwrap();
    }
    let conversations = store.conversation_store(execution);
    let mut writer = conversations.writer(ATTEMPT).unwrap();
    writer.append_narsor("prompt: plan the work", &[]).unwrap();
    writer
        .append(ConversationSource::Stdout, b"a plain line", &[])
        .unwrap();
    writer.finish().unwrap();
    conversations.close_attempt(ATTEMPT).unwrap();
}

/// Fails the step `plan` of the execution in `store` for good, as the engine
/// records it, with context variables beside it: the conductor reported the
/// failure as a problem and has a chat message queued.
fn fail_the_step_with_a_conductor_report(store: &ExecutionStore, execution: &str) {
    let mut record = store.load_execution(execution).unwrap();
    record.status = ExecutionStatus::Failed;
    record.current_state = Some("plan".to_string());
    record.metadata = json!({"engine": {"step_failure": {
        "state_id": "plan",
        "action_execution_id": ATTEMPT,
        "episode": "episode-1",
        "error": "exit status 1",
        "side_effect_free": false,
    }}});
    store.save_execution(&record).unwrap();
    let mut context = Context::new();
    context
        .variables
        .insert("branch".to_string(), json!("feat/x"));
    context.variables.insert("attempts".to_string(), json!(2));
    store.save_context(execution, &context).unwrap();
    ConductorStore::new(store.paths(), execution)
        .save_state(&ConductorState {
            queue: vec![QueuedRequest::new(
                ConductorTrigger::Chat,
                ConductorPurpose::ReplyToChat,
            )],
            failure: Some(FailureReport {
                key: ATTEMPT.to_string(),
                episode: "episode-1".to_string(),
                state_id: "plan".to_string(),
                reason: "the tests do not compile".to_string(),
                outcome: FailureOutcome::Unanswered {
                    error: "exit status 1".to_string(),
                },
                concluded_at: Utc::now(),
            }),
            ..ConductorState::default()
        })
        .unwrap();
}

/// What `show --details` adds to the task record's lines for the execution
/// [`fail_the_step_with_a_conductor_report`] leaves — as `show` printed it
/// when the server still read local storage.
const FAILED_STEP_DETAILS: &[&str] = &[
    "Context variables:",
    "  attempts = 2",
    "  branch = \"feat/x\"",
    "Problem (failure): the tests do not compile",
    "  Failure: attempt-1",
    "  Restart with: narsor-cli task restart-step <TASK> --failure-key attempt-1 [--note TEXT]",
    "Conductor working: reply_to_chat (chat, since <T>)",
];

/// Makes the execution in `store` wait for its owner's input in `review`,
/// with the conductor's summary of the gate beside it.
fn wait_for_input_with_a_conductor_summary(store: &ExecutionStore, execution: &str) {
    let mut record = store.load_execution(execution).unwrap();
    record.status = ExecutionStatus::InputNeeded;
    store.save_execution(&record).unwrap();
    let pending: PendingHumanInput = serde_json::from_value(json!({
        "execution_id": execution,
        "state_id": "review",
        "action_execution_id": "review-1",
        "instruction": "Approve the plan?",
        "episode": "episode-2",
        "requested_at": Utc::now(),
    }))
    .unwrap();
    store.save_pending_input(execution, &pending).unwrap();
    ConductorStore::new(store.paths(), execution)
        .save_annotation(&Annotation {
            key: pending.episode_key().to_string(),
            summary: "the plan covers both modules".to_string(),
            recommendation: Some("approve".to_string()),
            created_at: Utc::now(),
        })
        .unwrap();
}

/// What `show --details` adds for the execution
/// [`wait_for_input_with_a_conductor_summary`] leaves.
const INPUT_WITH_SUMMARY_DETAILS: &[&str] = &[
    "Context variables:",
    "  (none)",
    "Conductor's summary: the plan covers both modules",
    "  Recommends: approve",
];

/// What the runner of the execution in `store` reports: its status, its
/// cursor and the timeline it works out of the store.
fn runner_snapshot(
    store: &ExecutionStore,
    execution: &str,
    sequence: u64,
    safe_point: bool,
) -> RunnerSnapshot {
    let record = store.load_execution(execution).unwrap();
    let events = store.event_log(execution).read_all().unwrap();
    let recorded = store
        .conversation_store(execution)
        .recorded_attempt_ids()
        .unwrap();
    let timeline = current_timeline(store, execution, &events, record.status, &recorded);
    RunnerSnapshot {
        snapshot_sequence: sequence,
        execution_status: record.status,
        execution_cursor: ExecutionCursor {
            execution_id: Some(execution.to_string()),
            current_state: record.current_state.clone(),
            ..Default::default()
        },
        event_cursor: events.last().map_or(0, |event| event.sequence),
        pending_input: None,
        gate_watch: None,
        conversation: ConversationAvailability::Live {
            attempts: vec![ConversationAttemptSummary {
                attempt_id: ATTEMPT.to_string(),
                state_id: Some("plan".to_string()),
                entries: 2,
                closed: true,
                last_entry_at: None,
            }],
        },
        timeline: Some(timeline),
        safe_point,
        problem: None,
        attention: Vec::new(),
        conductor_available: false,
        conductor_unavailable_reason: None,
        default_harness_unavailable: None,
        workspaces: Vec::new(),
        generated_title: None,
        observed_at: Utc::now(),
    }
}

/// `lines` with every RFC 3339 timestamp replaced by `<T>`, closing
/// parentheses after it kept.
fn without_times(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            line.split(' ')
                .map(|word| {
                    let time = word.trim_end_matches(')');
                    if chrono::DateTime::parse_from_rfc3339(time).is_ok() {
                        format!("<T>{}", &word[time.len()..])
                    } else {
                        word.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn expected_steps() -> Vec<String> {
    vec![
        "plan #1        succeeded          agent      <T>  ->  <T>".to_string(),
        "    attempt id: attempt-1".to_string(),
    ]
}

/// What the CLI prints of the execution: steps, events, conversation —
/// asserted against what it printed when the server read local storage.
async fn assert_prints_the_execution(fx: &Fixture) {
    let steps = without_times(execution_steps_lines(&fx.client, EXECUTION).await.unwrap());
    assert_eq!(steps, expected_steps());

    let events = without_times(
        execution_event_lines(&fx.client, EXECUTION, READER_WAIT)
            .await
            .unwrap(),
    );
    assert_eq!(events.len(), 5, "{events:#?}");
    assert!(
        events[0].starts_with("    1  <T>  workflow_started"),
        "{events:#?}"
    );
    assert!(
        events[4].starts_with("    5  <T>  state_entered"),
        "{events:#?}"
    );

    let conversation = without_times(
        execution_conversation_lines(&fx.client, EXECUTION, ATTEMPT, READER_WAIT)
            .await
            .unwrap(),
    );
    assert_eq!(conversation.len(), 2, "{conversation:#?}");
    assert!(
        conversation[0].starts_with("    1  <T>") && conversation[0].ends_with("plan the work"),
        "{conversation:#?}"
    );
    assert!(
        conversation[1].starts_with("    2  <T>"),
        "{conversation:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_lists_and_shows_the_execution_of_a_local_task() {
    let fx = Fixture::new().await;
    fx.running_task("task-1", LOCAL_OWNER, "local");

    let list = execution_list_lines(&fx.client).await.unwrap();
    assert_eq!(
        list,
        vec![
            format!("{:<38} {:<10} {:<22} WORKFLOW", "ID", "STATUS", "STATE"),
            format!("{:<38} {:<10} {:<22} steps", EXECUTION, "running", "review"),
        ]
    );

    let show = without_times(execution_show_lines(&fx.client, EXECUTION).await.unwrap());
    assert_eq!(
        show[..5],
        [
            "Execution:     exec-1",
            "Workflow:      steps (version 1)",
            "Status:        running",
            "Current state: review",
            "Started:       <T>",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_shows_the_steps_events_and_conversation_of_a_running_task_of_any_backend() {
    for backend in ["local", "k8s"] {
        let fx = Fixture::new().await;
        fx.running_task("task-1", LOCAL_OWNER, backend);

        assert_prints_the_execution(&fx).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_reads_a_stopped_local_task_through_a_reader_started_on_demand() {
    let fx = Fixture::new().await;
    fx.running_task("task-1", LOCAL_OWNER, "local");
    fx.stop("task-1", "local");
    let reader = fx.start_readers("task-1", "local");

    assert_prints_the_execution(&fx).await;
    reader.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_details_adds_what_the_task_storage_holds_of_a_running_or_stopped_task() {
    type Prepare = fn(&ExecutionStore, &str);
    let cases: [(&str, Prepare, bool, &[&str]); 3] = [
        (
            "a failed step, running",
            fail_the_step_with_a_conductor_report,
            false,
            FAILED_STEP_DETAILS,
        ),
        (
            "a failed step, stopped",
            fail_the_step_with_a_conductor_report,
            true,
            FAILED_STEP_DETAILS,
        ),
        (
            "an input with the conductor's summary",
            wait_for_input_with_a_conductor_summary,
            false,
            INPUT_WITH_SUMMARY_DETAILS,
        ),
    ];
    for (name, prepare, stopped, expected) in cases {
        let fx = Fixture::new().await;
        prepare(&fx.storage("task-1"), EXECUTION);
        fx.running_task("task-1", LOCAL_OWNER, "local");
        let reader = stopped.then(|| {
            fx.stop("task-1", "local");
            fx.start_readers("task-1", "local")
        });

        let show = without_times(execution_show_lines(&fx.client, EXECUTION).await.unwrap());
        let details = without_times(
            execution_show_details_lines(&fx.client, EXECUTION, READER_WAIT)
                .await
                .unwrap(),
        );

        // The record's lines, then what only the storage holds; `show`
        // itself reads the record only.
        let split = details
            .iter()
            .position(|line| line == "Context variables:")
            .unwrap_or_else(|| panic!("{name}: no details: {details:#?}"));
        assert_eq!(details[..split], show[..], "{name}");
        assert_eq!(details[split..], expected[..], "{name}");
        if let Some(reader) = reader {
            reader.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_task_whose_reader_does_not_start_is_a_readable_error() {
    let fx = Fixture::new().await;
    fx.running_task("task-1", LOCAL_OWNER, "local");
    fx.stop("task-1", "local");

    let wait = Duration::from_millis(300);
    let events = execution_event_lines(&fx.client, EXECUTION, wait)
        .await
        .unwrap_err();
    assert!(
        events
            .message
            .starts_with("The execution's events cannot be read: ")
            && events.message.contains("read-only runner is starting"),
        "{}",
        events.message
    );
    let conversation = execution_conversation_lines(&fx.client, EXECUTION, ATTEMPT, wait)
        .await
        .unwrap_err();
    assert!(
        conversation
            .message
            .starts_with("The execution's conversation cannot be read: ")
            && conversation
                .message
                .contains("read-only runner is starting"),
        "{}",
        conversation.message
    );
    // The steps are the task record's: they need no reader.
    assert_eq!(
        without_times(execution_steps_lines(&fx.client, EXECUTION).await.unwrap()),
        expected_steps()
    );
    // `show --details` prints the record's part and says why the rest is
    // missing.
    let show = execution_show_lines(&fx.client, EXECUTION).await.unwrap();
    let details = execution_show_details_lines(&fx.client, EXECUTION, wait)
        .await
        .unwrap();
    let (unavailable, record) = details.split_last().unwrap();
    assert_eq!(record, &show[..]);
    assert!(
        unavailable
            .starts_with("Details (context variables, conductor, step problem) cannot be read: ")
            && unavailable.contains("read-only runner is starting"),
        "{unavailable}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listing_stopped_tasks_starts_no_reader() {
    let fx = Fixture::new().await;
    for id in ["task-1", "task-2", "task-3"] {
        fx.running_task(id, LOCAL_OWNER, "local");
        fx.stop(id, "local");
    }

    let list = execution_list_lines(&fx.client).await.unwrap();
    assert_eq!(list.len(), 4, "a header and three executions: {list:#?}");
    for id in ["task-1", "task-2", "task-3"] {
        let record = fx
            .service
            .control_plane()
            .task(&TaskId::parse(id).unwrap())
            .unwrap();
        assert!(record.reader_lease.is_none(), "{id} leased a reader");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_execution_of_a_task_the_caller_does_not_own_is_refused_like_its_task() {
    let fx = Fixture::new().await;
    fx.running_task("task-1", "someone-else", "local");

    let refusals = [
        execution_show_lines(&fx.client, EXECUTION)
            .await
            .unwrap_err(),
        execution_steps_lines(&fx.client, EXECUTION)
            .await
            .unwrap_err(),
        execution_event_lines(&fx.client, EXECUTION, READER_WAIT)
            .await
            .unwrap_err(),
        execution_show_details_lines(&fx.client, EXECUTION, READER_WAIT)
            .await
            .unwrap_err(),
    ];
    for refusal in refusals {
        assert_eq!(
            refusal.message,
            format!("Could not load execution '{EXECUTION}': not found")
        );
    }
    assert_eq!(
        execution_list_lines(&fx.client).await.unwrap(),
        vec!["No executions."]
    );
    // The same refusal as a read of the task itself.
    assert!(matches!(
        fx.client.control_execution(EXECUTION).await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        fx.client.control_timeline("task-1").await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        fx.client.control_execution_details("task-1").await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        fx.client.control_execution_state("task-1").await,
        Err(ClientError::NotFound)
    ));
}

/// The server reads no task's storage (R8): its sources do not name the
/// controller's work directory or the execution store.
#[test]
fn the_server_sources_never_name_task_storage() {
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/narsor-server/src"),
        &mut files,
    );
    assert!(!files.is_empty());
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for name in [
            "narsor-controller-local",
            "ControllerWorkPaths",
            "ExecutionStore",
        ] {
            assert!(!text.contains(name), "{} names {name}", file.display());
        }
    }
}

/// Gives the execution in `store` a multi-line context variable, the local
/// values of a step, and a conductor that diagnoses a failed step.
fn give_the_execution_state(store: &ExecutionStore, execution: &str) {
    let mut record = store.load_execution(execution).unwrap();
    record
        .context
        .variables
        .insert("plan".to_string(), json!("first step\nsecond step"));
    record
        .context
        .variables
        .insert("branch".to_string(), json!("feat/x"));
    record.metadata = json!({"engine": {"step_locals": {"review": {"NOTE": "keep it short"}}}});
    store.save_execution(&record).unwrap();
    let request = QueuedRequest::new(
        ConductorTrigger::StepFailure,
        ConductorPurpose::DiagnoseFailure,
    );
    ConductorStore::new(store.paths(), execution)
        .save_state(&ConductorState {
            pending: Some(PendingInvocation {
                invocation_id: "inv-1".to_string(),
                request,
                started_at: Utc::now(),
            }),
            failure: Some(FailureReport {
                key: ATTEMPT.to_string(),
                episode: "episode-1".to_string(),
                state_id: "plan".to_string(),
                reason: "the tests do not compile".to_string(),
                outcome: FailureOutcome::Unanswered {
                    error: "exit status 1".to_string(),
                },
                concluded_at: Utc::now(),
            }),
            ..ConductorState::default()
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_state_prints_every_value_the_workflow_sees_of_a_running_or_stopped_task() {
    for stopped in [false, true] {
        let fx = Fixture::new().await;
        give_the_execution_state(&fx.storage("task-1"), EXECUTION);
        fx.running_task("task-1", LOCAL_OWNER, "local");
        let reader = stopped.then(|| {
            fx.stop("task-1", "local");
            fx.start_readers("task-1", "local")
        });

        let lines = task_state_lines(&fx.client, "task-1", READER_WAIT, false)
            .await
            .unwrap();

        let at = |prefix: &str| {
            lines
                .iter()
                .position(|line| line.starts_with(prefix))
                .unwrap_or_else(|| panic!("stopped {stopped}: no {prefix}: {lines:#?}"))
        };
        let plan = at("context:variables:plan: ");
        assert_eq!(lines[plan], "context:variables:plan: first step");
        assert_eq!(
            lines[plan + 1],
            "    second step",
            "the next line, indented"
        );
        assert!(at("context:variables:branch: feat/x") < plan, "{lines:#?}");
        assert!(lines.contains(&"locals:review:NOTE: keep it short".to_string()));
        // The conductor waits on the failed step: what it concluded and
        // what it works on.
        assert!(lines.contains(&"conductor:failure:reason: the tests do not compile".to_string()));
        assert!(lines.contains(&"conductor:pending:invocation_id: inv-1".to_string()));
        assert!(at("context:") < at("locals:") && at("locals:") < at("conductor:"));

        let json = task_state_lines(&fx.client, "task-1", READER_WAIT, true)
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&json.join("\n")).unwrap();
        assert_eq!(
            state["context"]["value"]["variables"]["plan"],
            "first step\nsecond step"
        );
        assert_eq!(state["locals"]["value"]["review"]["NOTE"], "keep it short");
        assert_eq!(state["conductor"]["status"], "available");
        if let Some(reader) = reader {
            reader.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_state_reaches_the_owner_without_terminal_controls_and_with_its_line_feeds() {
    let fx = Fixture::new().await;
    let store = fx.storage("task-1");
    let mut record = store.load_execution(EXECUTION).unwrap();
    record.context.variables.insert(
        "na\u{1b}[31mme\u{202e}".to_string(),
        json!("one\u{1b}[2J\ntwo\u{202e}"),
    );
    store.save_execution(&record).unwrap();
    fx.running_task("task-1", LOCAL_OWNER, "local");

    let ExecutionStateResponse::Available { state, read_at, .. } =
        fx.client.control_execution_state("task-1").await.unwrap()
    else {
        panic!("the state of a running task is available");
    };

    let narsor_engine::controller_protocol::StateSection::Available { value } = &state.context
    else {
        panic!("the context is available: {:?}", state.context);
    };
    let variables = value["variables"].as_object().unwrap();
    let (name, value) = variables.iter().next().unwrap();
    assert!(
        !name.contains('\u{1b}') && !name.contains('\u{202e}'),
        "{name:?}"
    );
    let value = value.as_str().unwrap();
    assert!(
        !value.contains('\u{1b}') && !value.contains('\u{202e}'),
        "{value:?}"
    );
    assert_eq!(value.lines().count(), 2, "{value:?}");
    assert!(read_at <= Utc::now());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_state_says_why_when_the_state_cannot_be_read() {
    // A runner whose execution record does not load fails the read: the
    // owner hears it at once, as a retryable refusal.
    let fx = Fixture::new().await;
    fx.running_task("task-1", LOCAL_OWNER, "local");
    fs::remove_file(fx.storage("task-1").paths().state_file(EXECUTION)).unwrap();
    let error = task_state_lines(&fx.client, "task-1", READER_WAIT, false)
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .starts_with("The execution's state cannot be read: ")
            && error
                .message
                .contains("could not read the execution's state"),
        "{}",
        error.message
    );

    // An archived task has no storage left to read.
    let fx = Fixture::new().await;
    fx.running_task("task-1", LOCAL_OWNER, "local");
    fx.stop("task-1", "local");
    fx.service
        .apply_command(
            &TaskId::parse("task-1").unwrap(),
            CommandRequest::new(OwnerCommand::Archive, "archive-1"),
        )
        .unwrap();
    let error = task_state_lines(&fx.client, "task-1", READER_WAIT, false)
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "The task's state cannot be read: the task is archived: its storage was deleted"
    );

    // A task without an execution has no state yet.
    let fx = Fixture::new().await;
    fx.service
        .control_plane()
        .create_task(NewTask::new(
            TaskId::parse("task-2").unwrap(),
            OwnerRef::new(LOCAL_OWNER),
            BackendId::parse("local").unwrap(),
            launch_spec(),
        ))
        .unwrap();
    let error = task_state_lines(&fx.client, "task-2", READER_WAIT, false)
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "The task's state cannot be read: the task has no execution yet"
    );
}
