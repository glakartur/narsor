//! What `narsor-cli list|show|events|steps|conversation` print for a task's
//! execution, read from a served control plane (U8).
//!
//! The task's runner reports a snapshot to the control plane; its execution
//! lives on the task's storage. The lines are compared with timestamps
//! replaced by `<T>`, so they pin the output format and the content.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use narsor::cli::commands::{
    execution_conversation_lines, execution_event_lines, execution_list_lines,
    execution_show_lines, execution_steps_lines,
};
use narsor_api_client::api::token::TokenSource;
use narsor_api_client::api::ApiClient;
use narsor_engine::controller_protocol::{ProtocolService, LOCAL_OWNER};
use narsor_engine::domain::{
    BackendId, Context, Event, EventData, ExecutionStatus, GitIdentity, RepositoryInfo,
    ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec, TaskRepository,
    TaskRepositorySource, TaskWorkspace, WorkflowExecution, Workspace,
};
use narsor_engine::engine::current_timeline;
use narsor_engine::orchestration::{
    BackendAvailability, ControlPlane, ControllerReport, ConversationAttemptSummary,
    ConversationAvailability, ExecutionCursor, NewTask, Observation, ObservedSummary, OwnerRef,
    RunnerSnapshot, SchedulingPolicy,
};
use narsor_engine::persistence::conversation::ConversationSource;
use narsor_engine::persistence::{ExecutionStore, ServerStatePaths};
use narsor_server::api::{start_with_services, ApiHandle, ApiServices};
use serde_json::json;

const EXECUTION: &str = "exec-1";
const ATTEMPT: &str = "attempt-1";

/// A served control plane, a client of its owner and the roots under it.
struct Fixture {
    root: PathBuf,
    work: PathBuf,
    service: Arc<ProtocolService>,
    /// Serves while the fixture lives.
    _server: ApiHandle,
    client: ApiClient,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-cli-reads-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        let work = root.join("work");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&work).unwrap();
        let service = Arc::new(ProtocolService::new(
            ControlPlane::new(ServerStatePaths::new(&state)),
            crate::environment_support::seeded_resolver(&state),
        ));
        let server = start_with_services(
            root.join("catalog"),
            work.clone(),
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
            work,
            service,
            _server: server,
            client,
        }
    }

    /// The task `id` of `owner` on `backend`, running, whose runner reported
    /// the execution [`EXECUTION`] kept in `store`.
    fn running_task(&self, id: &str, owner: &str, backend: &str, store: &ExecutionStore) {
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
        let lease = control
            .acquire_lease(&backend_id, "controller-a", chrono::Duration::days(1))
            .unwrap();
        control
            .schedule(
                &backend_id,
                &SchedulingPolicy {
                    backend: BackendAvailability::Available,
                    max_concurrent_tasks: Some(16),
                },
            )
            .unwrap();
        for observation in [Observation::StartAccepted, Observation::RunnerReady] {
            control
                .report(ControllerReport {
                    task_id: task_id.clone(),
                    lease: lease.token(),
                    desired_generation: control.task(&task_id).unwrap().generation,
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
        self.service
            .record_runner_snapshot(
                &task_id,
                "pod-1",
                1,
                runner_snapshot(store),
                chrono::Duration::hours(1),
            )
            .expect("runner snapshot");
    }

    /// The storage of the local backend's task `id`, where its runner keeps
    /// the execution.
    fn local_storage(&self, id: &str) -> ExecutionStore {
        ExecutionStore::new(
            self.work
                .join("narsor-controller-local")
                .join("storage")
                .join(id)
                .join("state"),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
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
fn write_execution(store: &ExecutionStore) {
    store
        .save_execution(&WorkflowExecution {
            id: EXECUTION.to_string(),
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
    let log = store.event_log(EXECUTION);
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
        log.append(&Event::new(EXECUTION.to_string(), data, index as u64 + 1))
            .unwrap();
    }
    let conversations = store.conversation_store(EXECUTION);
    let mut writer = conversations.writer(ATTEMPT).unwrap();
    writer.append_narsor("prompt: plan the work", &[]).unwrap();
    writer
        .append(ConversationSource::Stdout, b"a plain line", &[])
        .unwrap();
    writer.finish().unwrap();
    conversations.close_attempt(ATTEMPT).unwrap();
}

/// What the runner of the execution in `store` reports: its status, its
/// cursor and the timeline it works out of the store.
fn runner_snapshot(store: &ExecutionStore) -> RunnerSnapshot {
    let execution = store.load_execution(EXECUTION).unwrap();
    let events = store.event_log(EXECUTION).read_all().unwrap();
    let recorded = store
        .conversation_store(EXECUTION)
        .recorded_attempt_ids()
        .unwrap();
    let timeline = current_timeline(store, EXECUTION, &events, execution.status, &recorded);
    RunnerSnapshot {
        snapshot_sequence: 1,
        execution_status: execution.status,
        execution_cursor: ExecutionCursor {
            execution_id: Some(EXECUTION.to_string()),
            current_state: execution.current_state.clone(),
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
        safe_point: false,
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

/// `lines` with every RFC 3339 timestamp replaced by `<T>`.
fn without_times(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            line.split(' ')
                .map(|word| {
                    if chrono::DateTime::parse_from_rfc3339(word).is_ok() {
                        "<T>"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// A local task's execution as the CLI prints it.
async fn local_task_fixture() -> Fixture {
    let fx = Fixture::new().await;
    let store = fx.local_storage("task-1");
    write_execution(&store);
    fx.running_task("task-1", LOCAL_OWNER, "local", &store);
    fx
}

fn expected_steps() -> Vec<String> {
    vec![
        "plan #1        succeeded          agent      <T>  ->  <T>".to_string(),
        "    attempt id: attempt-1".to_string(),
    ]
}

#[tokio::test]
async fn cli_lists_and_shows_the_execution_of_a_local_task() {
    let fx = local_task_fixture().await;

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

#[tokio::test]
async fn cli_shows_the_steps_events_and_conversation_of_a_running_local_task() {
    let fx = local_task_fixture().await;

    let steps = without_times(execution_steps_lines(&fx.client, EXECUTION).await.unwrap());
    assert_eq!(steps, expected_steps());

    let events = without_times(execution_event_lines(&fx.client, EXECUTION).await.unwrap());
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
        execution_conversation_lines(&fx.client, EXECUTION, ATTEMPT)
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

#[tokio::test]
async fn an_execution_of_a_task_the_caller_does_not_own_is_not_found() {
    let fx = Fixture::new().await;
    let store = fx.local_storage("task-1");
    write_execution(&store);
    fx.running_task("task-1", "someone-else", "local", &store);

    let refusals = [
        execution_show_lines(&fx.client, EXECUTION)
            .await
            .unwrap_err(),
        execution_steps_lines(&fx.client, EXECUTION)
            .await
            .unwrap_err(),
        execution_event_lines(&fx.client, EXECUTION)
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
}
