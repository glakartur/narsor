//! Integration tests of the `wait` step (U4): the step suspends the task
//! until a matching external event or its deadline, and ends with the
//! event's data.
//!
//! Most tests run the whole loop in-process, without a daemon and without a
//! network: a real [`ProtocolService`] over a temporary state root and a
//! manual clock, a watch-list observer whose run delivers the events, a
//! real [`Engine`] over its own execution store, and [`Stack`] in the
//! runner's place — it reports the engine's wait in a snapshot, long-polls
//! for what ends it, hands that to the engine and acknowledges it, as
//! `narsor-runner` does over HTTP. Script steps answer from [`Recorder`],
//! which keeps the rendered command, so a test sees what a later step was
//! given.

use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use narsor_engine::controller_protocol::{
    ProtocolService, RunnerIdentity, RunnerRole, RunnerSnapshotRequest,
};
use narsor_engine::domain::{
    ActionConfig, ActionError, ActionResult, BackendId, EventData, ExecutionStatus, GitIdentity,
    ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec, TaskRepository,
    TaskRepositorySource, TaskWorkspace, WorkflowDefinition, WorkflowExecution,
};
use narsor_engine::engine::actions::wait::{self, EVENT_VARIABLE};
use narsor_engine::engine::{
    ActionExecutor, ActionKind, ActionOutcome, ActionRequest, Engine, EngineError,
    ExternalEventDelivery, PendingExternalEvent, WaitActionError, WaitEnding,
};
use narsor_engine::observers::{
    ObserverConfig, ObserverEffect, ObserverId, ObserverName, RunId, WatchEntry, WatchMode,
    WatchObjectPolicy, WatchOrigin,
};
use narsor_engine::orchestration::{
    BackendAvailability, BackendLease, Clock, ControlPlane, ControllerReport, EventInputKind,
    InputStatus, NewTask, Observation, ObservedSummary, OwnerCommand, OwnerRef, RunnerState,
    SchedulingPolicy, SlotReleaseClass, TaskEvent, TaskEventKind, TaskRecord, TaskState,
};
use narsor_engine::persistence::StatePaths;
use narsor_engine::workflow::load_from_str;
use narsor_protocol::controller::{
    ConversationAvailability, EventDelivery, ExecutionCursor, InputAckRequest,
    ObserverEffectOutcome, ObserverEventRequest, RunnerPollQuery,
};
use narsor_server::runtime::run_periodic_pass;
use serde_json::{json, Value};

use crate::engine_support::{event_type, Harness};

const PREFIX: &str = "https://git.example.com/";
const MR: &str = "https://git.example.com/a/-/merge_requests/17";

// ---------------------------------------------------------------------------
// Workflows
// ---------------------------------------------------------------------------

/// Waits for the approval of MR `mr_id`, then merges with what the event
/// said; nobody approving within the hour sends a reminder.
const REVIEW: &str = r#"
schema: v2
name: wait-review
version: 1
inputs:
  - name: mr_id
    type: string
    required: true
states:
  await_review:
    action:
      type: wait
      event_type: review.approved
      filter:
        mr: "{{ inputs.mr_id }}"
      timeout: 1h
    choices:
      received:
        next: merge
      timeout:
        next: remind
  merge:
    action:
      type: script
      command: merge
      args: ["{{ context.event.data.by }}", "{{ context.event.data.mr }}", "{{ context.event.source }}"]
    exit:
      next: done
  remind:
    action:
      type: script
      command: remind
    exit:
      next: reminded
  done:
    description: Merged.
  reminded:
    description: Nobody approved in time.
terminal_states:
  done: completed
  reminded: completed
"#;

/// The same wait without a `timeout` choice: past its deadline the step
/// fails (PO4).
const REVIEW_WITHOUT_TIMEOUT_LABEL: &str = r#"
schema: v2
name: wait-review-strict
version: 1
inputs:
  - name: mr_id
    type: string
    required: true
states:
  await_review:
    action:
      type: wait
      event_type: review.approved
      filter:
        mr: "{{ inputs.mr_id }}"
      timeout: 1h
    exit:
      next: done
  done:
    description: Approved.
terminal_states:
  done: completed
"#;

/// A review loop: every approval is checked, and the check may send the
/// task back to wait for the next one in the same `wait` step.
const REVIEW_LOOP: &str = r#"
schema: v2
name: wait-review-loop
version: 1
inputs:
  - name: mr_id
    type: string
    required: true
states:
  await_review:
    action:
      type: wait
      event_type: review.approved
      filter:
        mr: "{{ inputs.mr_id }}"
    choices:
      received:
        next: check
  check:
    action:
      type: script
      command: check
      args: ["{{ context.event.data.round }}"]
    choices:
      again:
        next: await_review
      merged:
        next: done
  done:
    description: Merged.
terminal_states:
  done: completed
"#;

pub(crate) fn load(yaml: &str) -> WorkflowDefinition {
    load_from_str(yaml).unwrap_or_else(|error| panic!("the workflow loads: {error}"))
}

// ---------------------------------------------------------------------------
// In-process script steps
// ---------------------------------------------------------------------------

/// Answers every script step with the next label of its list (`exit` once
/// spent) and keeps the rendered command line of each call.
pub(crate) struct Recorder {
    labels: Mutex<VecDeque<&'static str>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl Recorder {
    pub(crate) fn new(labels: &[&'static str]) -> Arc<Self> {
        Arc::new(Self {
            labels: Mutex::new(labels.iter().copied().collect()),
            calls: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl ActionExecutor for Recorder {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let ActionConfig::Script(config) = request.action else {
            panic!("only script steps run here");
        };
        let mut call = vec![config.command.clone()];
        call.extend(config.args.iter().cloned());
        self.calls.lock().unwrap().push(call);
        let label = self.labels.lock().unwrap().pop_front().unwrap_or("exit");
        Box::pin(async move { Ok(ActionOutcome::Completed(ActionResult::new(label))) })
    }
}

// ---------------------------------------------------------------------------
// The stack: control plane, observer, engine and the runner's part
// ---------------------------------------------------------------------------

/// A clock the test moves by hand.
#[derive(Debug)]
pub(crate) struct ManualClock(pub(crate) Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// The control plane, one task with its watch-list observer, the engine
/// running the task's workflow, and the runner between them.
struct Stack {
    state_root: PathBuf,
    harness: Harness,
    clock: Arc<ManualClock>,
    service: ProtocolService,
    observer: ObserverId,
    task: TaskId,
    lease: BackendLease,
    identity: RunnerIdentity,
    snapshots: u64,
    recorder: Arc<Recorder>,
    definition: WorkflowDefinition,
    execution_id: Option<String>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

impl Stack {
    fn new(workflow: &str, labels: &[&'static str]) -> Self {
        let state_root = std::env::temp_dir().join(format!("narsor-wait-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&state_root).unwrap();
        // The engine stamps a wait with the wall clock; the control plane
        // compares its deadline with this one, so both start together.
        let clock = Arc::new(ManualClock(Mutex::new(Utc::now())));
        let service = Self::service_on(&state_root, &clock);
        let observer = active_watch_list(&service, clock.now());
        let task = create_task(&service, "task-1");
        let entry = WatchEntry::new(
            task.clone(),
            Some("await_review".into()),
            MR,
            vec!["review.approved".into()],
            WatchOrigin::Operator,
            clock.now(),
        );
        let store = service.observers();
        let lock = store.lock(&observer).unwrap();
        store.create_watch(&lock, &entry).unwrap();
        drop(lock);
        let lease = service
            .control_plane()
            .acquire_lease(&k8s(), "controller-1", Duration::days(1))
            .unwrap();
        let mut stack = Self {
            state_root,
            harness: Harness::new(),
            clock,
            service,
            observer,
            task,
            lease,
            identity: RunnerIdentity {
                role: RunnerRole::Worker,
                task_id: TaskId::parse("none").unwrap(),
                launch_generation: 0,
                incarnation: String::new(),
            },
            snapshots: 0,
            recorder: Recorder::new(labels),
            definition: load(workflow),
            execution_id: None,
        };
        // The controller starts the task's runner.
        stack
            .service
            .control_plane()
            .schedule(
                &k8s(),
                &SchedulingPolicy {
                    backend: BackendAvailability::Available,
                    max_concurrent_tasks: Some(16),
                },
            )
            .unwrap();
        stack.controller_reports(Observation::StartAccepted);
        stack.controller_reports(Observation::RunnerReady);
        assert_eq!(stack.record().runner_state, RunnerState::Running);
        stack.identity = issue_runner(
            &stack.service,
            &stack.task,
            "pod-1",
            launch_generation(&stack.record()),
        );
        stack
    }

    /// The task's controller observes its workload.
    fn controller_reports(&self, observation: Observation) {
        let control = self.service.control_plane();
        let generation = control.task(&self.task).unwrap().generation;
        control
            .report(ControllerReport {
                task_id: self.task.clone(),
                lease: self.lease.token(),
                desired_generation: generation,
                observation,
                resource: None,
                summary: ObservedSummary {
                    workload_present: true,
                    storage_present: true,
                    runner_status: None,
                    detail: None,
                },
                observed_at: self.clock.now(),
            })
            .expect("the control plane takes the controller's report");
    }

    /// A control plane over the state root, as a fresh daemon builds one.
    fn service_on(root: &PathBuf, clock: &Arc<ManualClock>) -> ProtocolService {
        ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(root), clock.clone()),
            crate::environment_support::seeded_resolver(root),
        )
    }

    /// An engine over the task's execution store, as a fresh runner
    /// process builds one.
    fn engine(&self) -> Engine {
        self.harness
            .engine()
            .with_executor(ActionKind::Script, self.recorder.clone())
    }

    fn execution_id(&self) -> &str {
        self.execution_id.as_deref().expect("the workflow started")
    }

    fn execution(&self) -> WorkflowExecution {
        self.harness
            .store()
            .load_execution(self.execution_id())
            .unwrap()
    }

    fn pending(&self) -> Option<PendingExternalEvent> {
        self.engine().pending_event(self.execution_id()).unwrap()
    }

    fn episode(&self) -> String {
        self.pending().expect("the task waits").episode
    }

    fn record(&self) -> TaskRecord {
        self.service.control_plane().task(&self.task).unwrap()
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    /// Starts the workflow (MR 17) and reports where it stopped.
    async fn start(&mut self) -> WorkflowExecution {
        let execution = self
            .engine()
            .start_and_run(
                &self.definition,
                json!({"mr_id": "17"}),
                self.harness.workspace(),
            )
            .await
            .expect("the workflow starts");
        self.execution_id = Some(execution.id.clone());
        self.report();
        execution
    }

    /// Sends the runner's snapshot of the execution, with its wait.
    fn report(&mut self) {
        self.send_snapshot(false);
    }

    /// Sends the snapshot of a runner that stopped at a safe point.
    fn report_safe_point(&mut self) {
        self.send_snapshot(true);
    }

    fn send_snapshot(&mut self, safe_point: bool) {
        self.snapshots += 1;
        let execution = self.execution();
        let event_wait = self.pending().map(|pending| pending.event_wait());
        let event_cursor = self
            .harness
            .store()
            .event_log(&execution.id)
            .next_sequence()
            .unwrap();
        let request = RunnerSnapshotRequest {
            snapshot_sequence: self.snapshots,
            execution_status: execution.status,
            execution_cursor: ExecutionCursor {
                execution_id: Some(execution.id.clone()),
                current_state: execution.current_state.clone(),
                action_execution_id: execution.current_action_execution.clone(),
                ..ExecutionCursor::default()
            },
            event_cursor,
            pending_input: None,
            event_wait,
            conversation: ConversationAvailability::default(),
            timeline: None,
            safe_point,
            problem: None,
            attention: Vec::new(),
            conductor_available: false,
            conductor_unavailable_reason: None,
            default_harness_unavailable: None,
            workspaces: Vec::new(),
            generated_title: None,
            observed_at: self.clock.now(),
        };
        self.service
            .runner_snapshot(&self.identity, request)
            .expect("the control plane takes the snapshot");
    }

    /// One long-poll of the runner, naming the wait the engine is in, and
    /// what it does with each delivery: hand it to the engine, acknowledge
    /// the verdict, report again. Returns the verdicts.
    async fn poll(&mut self) -> Vec<(String, InputStatus)> {
        let query = RunnerPollQuery {
            wait_secs: Some(0),
            event_wait_id: self.pending().map(|pending| pending.episode),
            ..RunnerPollQuery::default()
        };
        let response = self
            .service
            .runner_poll(&self.identity, query)
            .await
            .expect("the poll answers");
        let mut verdicts = Vec::new();
        for event in response.events {
            let delivery = ExternalEventDelivery {
                episode: event.episode_id,
                ending: match event.delivery {
                    EventDelivery::Event {
                        event_type,
                        source,
                        data,
                    } => WaitEnding::Event {
                        event_type,
                        source,
                        data,
                    },
                    EventDelivery::Timeout => WaitEnding::Timeout,
                },
            };
            let result = self
                .engine()
                .deliver_external_event(&self.definition, self.execution_id(), &delivery)
                .await;
            let request = InputAckRequest {
                accepted: result.is_ok(),
                detail: result.err().map(|error| error.to_string()),
            };
            let ack = self
                .service
                .runner_ack_event(&self.identity, &event.input_id, request)
                .expect("the control plane takes the verdict");
            verdicts.push((ack.input_id, ack.status));
            self.report();
        }
        verdicts
    }

    /// The observer's run, claimed as its worker would.
    fn claim(&self) -> RunId {
        self.service.observer_pass();
        self.service
            .claim_observer_run()
            .unwrap()
            .expect("a run is due")
            .run_id
    }

    /// The observer's script sends an event for the task.
    fn send(&self, run: &RunId, key: &str, event_type: &str, data: Value) -> ObserverEffectOutcome {
        self.service
            .observer_deliver_event(
                run,
                self.task.as_str(),
                ObserverEventRequest {
                    effect_key: key.to_string(),
                    event_type: event_type.to_string(),
                    data,
                },
            )
            .expect("the control plane takes the event")
            .outcome
    }

    /// The task's history entries of external events.
    fn history_events(&self) -> Vec<TaskEvent> {
        self.service
            .control_plane()
            .events_since(&self.task, 0)
            .unwrap()
            .into_iter()
            .filter(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. }))
            .collect()
    }

    /// The execution log's `ExternalEventReceived` entries, as
    /// `(event_type, source, payload)`.
    fn log_events(&self) -> Vec<(String, String, Value)> {
        self.harness
            .events(self.execution_id())
            .into_iter()
            .filter_map(|event| match event.data {
                EventData::ExternalEventReceived {
                    event_type,
                    source,
                    payload,
                } => Some((event_type, source, payload)),
                _ => None,
            })
            .collect()
    }
}

/// An active watch-list observer of `gitlab` with approved script version 1.
fn active_watch_list(service: &ProtocolService, now: DateTime<Utc>) -> ObserverId {
    let mut config = ObserverConfig::new(
        ObserverName::parse("mr-review").unwrap(),
        "gitlab",
        ObserverEffect::DeliverEvent,
    );
    config.watch_mode = WatchMode::WatchList;
    config.watch_objects = WatchObjectPolicy {
        allowed_prefixes: vec![PREFIX.into()],
        ..WatchObjectPolicy::default()
    };
    let store = service.observers();
    let id = store
        .create(OwnerRef::new("owner"), config, None, now)
        .unwrap()
        .observer_id;
    store
        .add_script(&id, "echo '{\"state\": {}}'\n", now)
        .unwrap();
    service.approve_observer_version(&id, 1).unwrap();
    id
}

/// A task of the observer's owner on backend `k8s`.
pub(crate) fn create_task(service: &ProtocolService, id: &str) -> TaskId {
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
        "wait-review",
        1,
        json!({"mr_id": "17"}),
        workspace,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    );
    let task_id = TaskId::parse(id).unwrap();
    service
        .control_plane()
        .create_task(NewTask::new(
            task_id.clone(),
            OwnerRef::new("owner"),
            BackendId::parse("k8s").unwrap(),
            spec,
        ))
        .unwrap();
    task_id
}

pub(crate) fn k8s() -> BackendId {
    BackendId::parse("k8s").unwrap()
}

/// The generation the task's current runner was launched at.
pub(crate) fn launch_generation(record: &TaskRecord) -> u64 {
    record
        .commands
        .iter()
        .filter(|receipt| matches!(receipt.command, OwnerCommand::Start | OwnerCommand::Resume))
        .map(|receipt| receipt.generation_after)
        .max()
        .unwrap_or(1)
}

/// The credential of the task's working runner `incarnation`, launched at
/// `launch_generation`, and the identity it proves.
pub(crate) fn issue_runner(
    service: &ProtocolService,
    task: &TaskId,
    incarnation: &str,
    launch_generation: u64,
) -> RunnerIdentity {
    service
        .credentials()
        .issue_runner_session(
            RunnerRole::Worker,
            task,
            incarnation,
            launch_generation,
            0,
            Duration::days(1),
        )
        .unwrap();
    RunnerIdentity {
        role: RunnerRole::Worker,
        task_id: task.clone(),
        launch_generation,
        incarnation: incarnation.to_string(),
    }
}

fn approval(by: &str) -> Value {
    json!({"mr": 17, "by": by})
}

// ---------------------------------------------------------------------------
// End to end, through the control plane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_matching_event_ends_the_wait_and_the_next_step_sees_its_data() {
    // Covers AE5 (the event part) and the happy path: the task waits for
    // `review.approved` with filter `{mr: "{{ inputs.mr_id }}"}` (text
    // "17"), the observer sends `{mr: 17}` (a number), and the task goes on.
    let mut stack = Stack::new(REVIEW, &[]);
    let waiting = stack.start().await;
    assert_eq!(waiting.status, ExecutionStatus::WaitingForEvent);
    let pending = stack.pending().expect("the wait is on record");
    assert_eq!(pending.request.event_type, "review.approved");
    assert_eq!(pending.request.filter, json!({"mr": "17"}));
    assert_eq!(
        pending.deadline,
        Some(pending.since + Duration::seconds(3600))
    );
    // The runner's report put the wait on the task record.
    assert_eq!(
        stack.record().event_wait.as_ref().map(|w| &w.episode_id),
        Some(&pending.episode)
    );
    assert!(
        stack.recorder.calls().is_empty(),
        "nothing ran past the wait"
    );

    let run = stack.claim();
    let key = format!("MR!17:approved:{}", pending.episode);
    let outcome = stack.send(&run, &key, "review.approved", approval("alice"));
    assert_eq!(outcome, ObserverEffectOutcome::Delivered);
    let verdicts = stack.poll().await;

    assert_eq!(
        verdicts,
        vec![("event-1".to_string(), InputStatus::Consumed)]
    );
    let execution = stack.execution();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));
    // The next step saw the event's data and its source.
    assert_eq!(
        stack.recorder.calls(),
        vec![vec![
            "merge".to_string(),
            "alice".to_string(),
            "17".to_string(),
            "gitlab".to_string()
        ]]
    );
    assert_eq!(
        execution.context.variables[EVENT_VARIABLE],
        json!({"type": "review.approved", "source": "gitlab", "data": approval("alice")})
    );
    // One entry in the execution log, one in the task's history.
    assert_eq!(
        stack.log_events(),
        vec![(
            "review.approved".to_string(),
            "gitlab".to_string(),
            approval("alice")
        )]
    );
    let history = stack.history_events();
    assert_eq!(history.len(), 1);
    assert!(matches!(
        &history[0].kind,
        TaskEventKind::ExternalEventReceived { input_id: Some(id), provenance, .. }
            if id == "event-1" && provenance.observer_id == stack.observer.to_string()
    ));
    // The wait is over on both sides.
    assert!(stack.pending().is_none());
    let record = stack.record();
    assert_eq!(record.event_wait, None);
    assert_eq!(record.event_inputs[0].status, InputStatus::Consumed);
    assert_eq!(record.event_inputs[0].data, None, "settled data is dropped");
    assert!(!record.owner_work_pending());
}

#[tokio::test]
async fn an_event_before_the_wait_only_goes_to_the_history() {
    // Covers AE7 (the `wait` part): the event came while the task did not
    // wait yet; the step that starts later never sees it and waits on.
    let mut stack = Stack::new(REVIEW, &[]);
    let run = stack.claim();
    let early = stack.send(&run, "MR!17:approved", "review.approved", approval("alice"));
    assert_eq!(early, ObserverEffectOutcome::Recorded);

    stack.start().await;
    let verdicts = stack.poll().await;

    assert!(verdicts.is_empty(), "nothing is handed to the step");
    assert_eq!(stack.execution().status, ExecutionStatus::WaitingForEvent);
    assert!(stack.log_events().is_empty());
    let history = stack.history_events();
    assert_eq!(history.len(), 1);
    assert!(matches!(
        &history[0].kind,
        TaskEventKind::ExternalEventReceived { input_id: None, .. }
    ));
    assert!(stack.record().event_inputs.is_empty());
}

#[tokio::test]
async fn an_event_outside_the_filter_leaves_the_step_waiting() {
    let mut stack = Stack::new(REVIEW, &[]);
    stack.start().await;
    let run = stack.claim();

    let other_mr = stack.send(&run, "MR!18:approved", "review.approved", json!({"mr": 18}));
    let other_type = stack.send(
        &run,
        "MR!17:commented",
        "review.commented",
        json!({"mr": 17}),
    );
    let verdicts = stack.poll().await;

    assert_eq!(other_mr, ObserverEffectOutcome::Recorded);
    assert_eq!(other_type, ObserverEffectOutcome::Recorded);
    assert!(verdicts.is_empty());
    assert_eq!(stack.execution().status, ExecutionStatus::WaitingForEvent);
    assert_eq!(stack.history_events().len(), 2);
}

#[tokio::test]
async fn a_review_loop_gets_an_event_in_each_of_its_two_waits() {
    // The script keys its events with the wait's episode: the same state of
    // the MR reaches both waits, and a repeat within one wait is a
    // duplicate.
    let mut stack = Stack::new(REVIEW_LOOP, &["again", "merged"]);
    stack.start().await;
    let run = stack.claim();

    let first = stack.episode();
    let key = format!("MR!17:approved:{first}");
    assert_eq!(
        stack.send(&run, &key, "review.approved", json!({"mr": 17, "round": 1})),
        ObserverEffectOutcome::Delivered
    );
    stack.poll().await;

    assert_eq!(stack.execution().status, ExecutionStatus::WaitingForEvent);
    let second = stack.episode();
    assert_ne!(first, second, "waiting again is a new episode");
    assert_eq!(
        stack.send(&run, &key, "review.approved", json!({"mr": 17, "round": 1})),
        ObserverEffectOutcome::Duplicate
    );
    let key = format!("MR!17:approved:{second}");
    assert_eq!(
        stack.send(&run, &key, "review.approved", json!({"mr": 17, "round": 2})),
        ObserverEffectOutcome::Delivered
    );
    stack.poll().await;

    assert_eq!(stack.execution().status, ExecutionStatus::Completed);
    assert_eq!(
        stack.recorder.calls(),
        vec![
            vec!["check".to_string(), "1".to_string()],
            vec!["check".to_string(), "2".to_string()],
        ]
    );
    assert_eq!(stack.log_events().len(), 2);
    assert_eq!(stack.history_events().len(), 2);
    let inputs = stack.record().event_inputs;
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].episode_id, first);
    assert_eq!(inputs[1].episode_id, second);
    assert!(inputs
        .iter()
        .all(|input| input.status == InputStatus::Consumed));
}

#[tokio::test]
async fn a_restarted_runner_keeps_waiting_and_an_event_after_the_restart_ends_the_wait() {
    let mut stack = Stack::new(REVIEW, &[]);
    stack.start().await;
    let episode = stack.episode();

    // Every in-memory object goes: a new daemon over the same state root,
    // a new runner incarnation with a new engine over the same storage.
    stack.service = Stack::service_on(&stack.state_root, &stack.clock);
    let generation = stack.identity.launch_generation;
    stack.identity = issue_runner(&stack.service, &stack.task, "pod-2", generation);
    stack.snapshots = 0;
    stack.report();

    assert_eq!(
        stack.episode(),
        episode,
        "the wait is restored, not restarted"
    );
    assert_eq!(
        stack.record().event_wait.map(|wait| wait.episode_id),
        Some(episode.clone())
    );
    let run = stack.claim();
    let key = format!("MR!17:approved:{episode}");
    stack.send(&run, &key, "review.approved", approval("bob"));
    let verdicts = stack.poll().await;

    assert_eq!(
        verdicts,
        vec![("event-1".to_string(), InputStatus::Consumed)]
    );
    assert_eq!(stack.execution().status, ExecutionStatus::Completed);
    assert_eq!(stack.recorder.calls()[0][1], "bob");
}

#[tokio::test]
async fn past_its_deadline_the_wait_takes_the_timeout_label() {
    // PO4 with a `timeout` choice: the periodic pass hands the runner the
    // end of the wait, once, and the step goes on by that label.
    let mut stack = Stack::new(REVIEW, &[]);
    stack.start().await;
    let episode = stack.episode();

    run_periodic_pass(&stack.service);
    assert!(stack.record().event_inputs.is_empty(), "not due yet");

    stack.advance(Duration::hours(2));
    run_periodic_pass(&stack.service);
    run_periodic_pass(&stack.service);
    let record = stack.record();
    assert_eq!(record.event_inputs.len(), 1, "one timeout per episode");
    assert_eq!(record.event_inputs[0].kind, EventInputKind::Timeout);
    assert_eq!(record.event_inputs[0].episode_id, episode);
    let timed_out = stack
        .service
        .control_plane()
        .events_since(&stack.task, 0)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.kind, TaskEventKind::WaitTimedOut { .. }))
        .count();
    assert_eq!(timed_out, 1);

    let verdicts = stack.poll().await;

    assert_eq!(
        verdicts,
        vec![("event-1".to_string(), InputStatus::Consumed)]
    );
    let execution = stack.execution();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("reminded"));
    assert_eq!(stack.recorder.calls(), vec![vec!["remind".to_string()]]);
    assert!(stack.log_events().is_empty(), "a timeout is no event");
    assert_eq!(stack.record().event_wait, None);
}

#[tokio::test]
async fn past_its_deadline_a_wait_without_the_timeout_label_fails_its_step() {
    // PO4 without a `timeout` choice: a failed step, like any other.
    let mut stack = Stack::new(REVIEW_WITHOUT_TIMEOUT_LABEL, &[]);
    stack.start().await;

    stack.advance(Duration::hours(2));
    run_periodic_pass(&stack.service);
    let verdicts = stack.poll().await;

    assert_eq!(
        verdicts,
        vec![("event-1".to_string(), InputStatus::Consumed)]
    );
    let execution = stack.execution();
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(execution.current_state.as_deref(), Some("await_review"));
    let failure = stack
        .harness
        .events(stack.execution_id())
        .into_iter()
        .find_map(|event| match event.data {
            EventData::ActionFailed {
                error, error_type, ..
            } => Some((error, error_type)),
            _ => None,
        })
        .expect("the step failed");
    assert_eq!(failure.1.as_deref(), Some("timeout"));
    assert!(failure.0.contains("review.approved"), "{}", failure.0);
}

#[tokio::test]
async fn the_timeout_resumes_a_task_whose_slot_was_freed_while_it_waited() {
    // The task waits without compute; the deadline brings it back, once,
    // and the new runner takes the timeout.
    let mut stack = Stack::new(REVIEW, &[]);
    stack.start().await;
    let episode = stack.episode();
    // Past the grace a waiting task gets, the slot-release policy asks the
    // runner to stop, and its safe point frees the slot.
    stack.advance(Duration::minutes(10));
    run_periodic_pass(&stack.service);
    assert_eq!(stack.record().runner_state, RunnerState::PauseRequested);
    stack.report_safe_point();
    stack.controller_reports(Observation::PausedAtSafePoint);
    assert_eq!(stack.record().runner_state, RunnerState::Stopped);
    assert_ne!(stack.record().task_state(), TaskState::HumanNeeded);

    stack.advance(Duration::hours(2));
    run_periodic_pass(&stack.service);
    run_periodic_pass(&stack.service);

    let record = stack.record();
    assert_ne!(record.runner_state, RunnerState::Stopped);
    let resume_key = format!("work-resume:wait-timeout:{episode}");
    let resumes = record
        .commands
        .iter()
        .filter(|receipt| receipt.idempotency_key == resume_key)
        .count();
    assert_eq!(resumes, 1);

    // The controller starts a new runner; it restores the wait and takes
    // the timeout.
    stack
        .service
        .control_plane()
        .schedule(
            &k8s(),
            &SchedulingPolicy {
                backend: BackendAvailability::Available,
                max_concurrent_tasks: Some(16),
            },
        )
        .unwrap();
    stack.controller_reports(Observation::StartAccepted);
    stack.controller_reports(Observation::RunnerReady);
    let generation = launch_generation(&stack.record());
    assert!(generation > stack.identity.launch_generation);
    stack.identity = issue_runner(&stack.service, &stack.task, "pod-2", generation);
    stack.snapshots = 0;
    stack.report();
    let verdicts = stack.poll().await;

    assert_eq!(
        verdicts,
        vec![("event-1".to_string(), InputStatus::Consumed)]
    );
    assert_eq!(stack.execution().current_state.as_deref(), Some("reminded"));
}

#[tokio::test]
async fn a_waiting_task_frees_its_slot_never_needs_a_human_and_is_not_lost() {
    let mut stack = Stack::new(REVIEW, &[]);
    stack.start().await;

    let record = stack.record();
    assert_eq!(
        record.slot_release_class(),
        Some(SlotReleaseClass::Transient)
    );
    assert!(
        record.slot_release_due_at.is_some(),
        "the slot is freed after the grace"
    );
    assert_ne!(record.task_state(), TaskState::HumanNeeded);

    // Hours of waiting: the resume policy finds nothing lost to resume.
    stack.advance(Duration::minutes(30));
    let pass = run_periodic_pass(&stack.service);
    assert!(pass.resumed.is_empty());
    let record = stack.record();
    assert_ne!(record.runner_state, RunnerState::Interrupted);
    assert_ne!(record.task_state(), TaskState::HumanNeeded);
    assert_eq!(stack.execution().status, ExecutionStatus::WaitingForEvent);
}

// ---------------------------------------------------------------------------
// The engine's half
// ---------------------------------------------------------------------------

/// Runs `workflow` on a bare engine until its `wait` step suspends.
async fn waiting(harness: &Harness, workflow: &WorkflowDefinition) -> WorkflowExecution {
    let execution = harness
        .engine()
        .with_executor(ActionKind::Script, Recorder::new(&[]))
        .start_and_run(workflow, json!({"mr_id": "17"}), harness.workspace())
        .await
        .expect("the workflow starts");
    assert_eq!(execution.status, ExecutionStatus::WaitingForEvent);
    execution
}

fn approved(episode: &str) -> ExternalEventDelivery {
    ExternalEventDelivery {
        episode: episode.to_string(),
        ending: WaitEnding::Event {
            event_type: "review.approved".into(),
            source: "gitlab".into(),
            data: json!({"mr": 17, "by": "alice"}),
        },
    }
}

fn wait_error(error: EngineError) -> WaitActionError {
    match error {
        EngineError::WaitEvent(error) => error,
        other => panic!("expected a refused delivery, got {other}"),
    }
}

#[tokio::test]
async fn the_wait_is_on_disk_before_the_status_says_so() {
    let harness = Harness::new();
    let workflow = load(REVIEW);
    let execution = waiting(&harness, &workflow).await;

    let store = harness.store();
    let pending: PendingExternalEvent = store
        .load_pending_event(&execution.id)
        .unwrap()
        .expect("pending_event.json");
    assert_eq!(pending.state_id, "await_review");
    assert_eq!(
        execution.current_action_execution.as_deref(),
        Some(pending.action_execution_id())
    );
    // The status is the authority: once it moves on, the file is inert.
    let mut moved = store.load_execution(&execution.id).unwrap();
    moved.status = ExecutionStatus::Running;
    store.save_execution(&moved).unwrap();
    assert!(wait::pending_event(&store, &execution.id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_delivery_for_another_wait_episode_is_refused_and_changes_nothing() {
    let harness = Harness::new();
    let workflow = load(REVIEW);
    let execution = waiting(&harness, &workflow).await;
    let engine = harness.engine();
    let before = harness.events(&execution.id).len();

    let refused = engine
        .deliver_external_event(&workflow, &execution.id, &approved("an-earlier-wait"))
        .await
        .expect_err("another episode");

    let error = wait_error(refused);
    assert!(error.is_stale(), "{error}");
    assert_eq!(
        harness
            .store()
            .load_execution(&execution.id)
            .unwrap()
            .status,
        ExecutionStatus::WaitingForEvent
    );
    assert!(engine.pending_event(&execution.id).unwrap().is_some());
    let after: Vec<String> = harness.events(&execution.id)[before..]
        .iter()
        .map(event_type)
        .collect();
    assert!(
        !after.iter().any(|kind| kind == "external_event_received"),
        "{after:?}"
    );
}

#[tokio::test]
async fn a_delivery_after_the_task_moved_on_is_refused() {
    // The step ended with one event; a second one for the same, finished
    // wait (a redelivery, a late timeout) ends nothing.
    let harness = Harness::new();
    let workflow = load(REVIEW);
    let execution = waiting(&harness, &workflow).await;
    let engine = harness
        .engine()
        .with_executor(ActionKind::Script, Recorder::new(&[]));
    let episode = engine
        .pending_event(&execution.id)
        .unwrap()
        .unwrap()
        .episode;
    let done = engine
        .deliver_external_event(&workflow, &execution.id, &approved(&episode))
        .await
        .unwrap();
    assert_eq!(done.status, ExecutionStatus::Completed);

    let again = engine
        .deliver_external_event(&workflow, &execution.id, &approved(&episode))
        .await
        .expect_err("the wait is over");
    let late_timeout = engine
        .deliver_external_event(
            &workflow,
            &execution.id,
            &ExternalEventDelivery {
                episode,
                ending: WaitEnding::Timeout,
            },
        )
        .await
        .expect_err("the wait is over");

    assert!(wait_error(again).is_stale());
    assert!(wait_error(late_timeout).is_stale());
    let received = harness
        .events(&execution.id)
        .iter()
        .filter(|event| event_type(event) == "external_event_received")
        .count();
    assert_eq!(received, 1);
    assert_eq!(
        harness
            .store()
            .load_execution(&execution.id)
            .unwrap()
            .status,
        ExecutionStatus::Completed
    );
}

#[tokio::test]
async fn an_event_the_wait_does_not_match_is_refused_by_the_engine_too() {
    // The control plane matches first; the engine checks again.
    let harness = Harness::new();
    let workflow = load(REVIEW);
    let execution = waiting(&harness, &workflow).await;
    let engine = harness.engine();
    let episode = engine
        .pending_event(&execution.id)
        .unwrap()
        .unwrap()
        .episode;

    let refused = engine
        .deliver_external_event(
            &workflow,
            &execution.id,
            &ExternalEventDelivery {
                episode,
                ending: WaitEnding::Event {
                    event_type: "review.approved".into(),
                    source: "gitlab".into(),
                    data: json!({"mr": 18}),
                },
            },
        )
        .await
        .expect_err("another MR");

    assert!(matches!(
        wait_error(refused),
        WaitActionError::EventMismatch { .. }
    ));
    assert_eq!(
        harness
            .store()
            .load_execution(&execution.id)
            .unwrap()
            .status,
        ExecutionStatus::WaitingForEvent
    );
}

#[test]
fn a_wait_step_may_declare_the_timeout_choice() {
    // The validator lets a `wait` step time out (PO4) in the unified shape.
    let workflow = load(REVIEW);
    let labels: Vec<&str> = workflow.states["await_review"]
        .choices
        .iter()
        .map(|choice| choice.label.as_str())
        .collect();
    assert!(labels.contains(&"timeout"), "{labels:?}");
}

#[test]
fn the_runner_protocol_carries_the_wait_and_what_ends_it() {
    // What the runner and the control plane exchange, on the wire.
    let delivered: narsor_protocol::controller::DeliveredEvent = serde_json::from_value(json!({
        "input_id": "event-1",
        "episode_id": "e-1",
        "delivery": {"kind": "event", "event_type": "review.approved", "source": "gitlab",
                     "data": {"mr": 17}}
    }))
    .unwrap();
    assert!(matches!(delivered.delivery, EventDelivery::Event { .. }));
    let timeout: EventDelivery = serde_json::from_value(json!({"kind": "timeout"})).unwrap();
    assert_eq!(timeout, EventDelivery::Timeout);
    // A poll without a wait says nothing of one.
    let query = serde_json::to_value(RunnerPollQuery::default()).unwrap();
    assert!(query.get("event_wait_id").is_none());
}
