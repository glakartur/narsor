//! Integration tests of watch entries from a workflow (U5): a `wait` step
//! with a `watch` block asks an observer to watch an object (a merge
//! request) while the task waits, and the script's exit code 0 ends the
//! entry.
//!
//! The tests run in-process, without a daemon and without a network, as
//! `actions_wait` does: a real [`ProtocolService`] over a temporary state
//! root and a manual clock, a real [`Engine`] over its own execution store,
//! and [`Stack`] in the runner's place — it reports the engine's wait in a
//! snapshot, as `narsor-runner` does over HTTP.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use narsor_engine::controller_protocol::{
    ProtocolService, RunOutcome, RunReport, RunnerIdentity, RunnerSnapshotRequest,
};
use narsor_engine::domain::{
    ActionConfig, ExecutionStatus, TaskId, ValidationError, WorkflowDefinition, WorkflowExecution,
};
use narsor_engine::engine::{
    ActionKind, Engine, ExternalEventDelivery, PendingExternalEvent, WaitEnding,
};
use narsor_engine::observers::{
    CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverId, ObserverName, WatchEntry,
    WatchMode, WatchObjectPolicy, WatchOrigin,
};
use narsor_engine::orchestration::{
    BackendAvailability, BackendLease, Clock, ControlPlane, ControllerReport, Observation,
    ObservedSummary, OwnerRef, RunnerState, SchedulingPolicy, TaskEventKind, TaskRecord,
};
use narsor_engine::persistence::StatePaths;
use narsor_engine::workflow::{load_from_file, validate_path, validate_source};
use narsor_protocol::controller::{ConversationAvailability, ExecutionCursor};
use serde_json::{json, Value};

use crate::actions_wait::{
    create_task, issue_runner, k8s, launch_generation, load, ManualClock, Recorder,
};
use crate::engine_support::{fixtures, Harness};

const PREFIX: &str = "https://git.example.com/";
const MR: &str = "https://git.example.com/a/-/merge_requests/17";
const OBSERVER: &str = "mr-review";
const OWNER: &str = "owner";

/// The workflow of `docs/dev/workflow-schema.md` §3.3.D, verbatim.
const FIXTURE: &str = "wait-watch.yaml";

/// A review loop: every approval is checked, and the check may send the
/// task back to wait in the same step for the next one.
const REVIEW_LOOP: &str = r#"
schema: v2
name: watch-review-loop
version: 1
inputs:
  - name: mr_url
    type: string
    required: true
states:
  await_review:
    action:
      type: wait
      event_type: review.approved
      filter:
        mr_url: "{{ inputs.mr_url }}"
      watch:
        observer: mr-review
        object: "{{ inputs.mr_url }}"
        event_types: [review.approved]
    choices:
      received:
        next: check
  check:
    action:
      type: script
      command: check
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

fn fixture() -> WorkflowDefinition {
    load_from_file(fixtures().join(FIXTURE))
        .unwrap_or_else(|error| panic!("the fixture loads: {error}"))
}

// ---------------------------------------------------------------------------
// The stack: control plane, engine and the runner's part
// ---------------------------------------------------------------------------

/// The control plane with one running task of [`OWNER`], the engine running
/// the task's workflow, and the runner between them. Observers are the
/// test's to create, before [`Stack::start`].
struct Stack {
    state_root: PathBuf,
    harness: Harness,
    clock: Arc<ManualClock>,
    service: ProtocolService,
    task: TaskId,
    _lease: BackendLease,
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
    fn new(definition: WorkflowDefinition, labels: &[&'static str]) -> Self {
        let state_root =
            std::env::temp_dir().join(format!("narsor-watch-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&state_root).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(Utc::now())));
        let service = ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(&state_root), clock.clone()),
            crate::environment_support::seeded_resolver(&state_root),
        );
        let task = create_task(&service, "task-1");
        let control = service.control_plane();
        let lease = control
            .acquire_lease(&k8s(), "controller-1", Duration::days(1))
            .unwrap();
        control
            .schedule(
                &k8s(),
                &SchedulingPolicy {
                    backend: BackendAvailability::Available,
                    max_concurrent_tasks: Some(16),
                },
            )
            .unwrap();
        for observation in [Observation::StartAccepted, Observation::RunnerReady] {
            let generation = control.task(&task).unwrap().generation;
            control
                .report(ControllerReport {
                    task_id: task.clone(),
                    lease: lease.token(),
                    desired_generation: generation,
                    observation,
                    resource: None,
                    summary: ObservedSummary {
                        workload_present: true,
                        storage_present: true,
                        runner_status: None,
                        detail: None,
                    },
                    observed_at: clock.now(),
                })
                .expect("the control plane takes the controller's report");
        }
        let record = control.task(&task).unwrap();
        assert_eq!(record.runner_state, RunnerState::Running);
        let identity = issue_runner(&service, &task, "pod-1", launch_generation(&record));
        Self {
            state_root,
            harness: Harness::new(),
            clock,
            service,
            task,
            _lease: lease,
            identity,
            snapshots: 0,
            recorder: Recorder::new(labels),
            definition,
            execution_id: None,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

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

    fn record(&self) -> TaskRecord {
        self.service.control_plane().task(&self.task).unwrap()
    }

    /// Starts the workflow for merge request `mr_url` and reports where it
    /// stopped.
    async fn start(&mut self, mr_url: &str) -> WorkflowExecution {
        let execution = self
            .engine()
            .start_and_run(
                &self.definition,
                json!({ "mr_url": mr_url }),
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
        self.snapshots += 1;
        let execution = self.execution();
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
            event_wait: self.pending().map(|pending| pending.event_wait()),
            conversation: ConversationAvailability::default(),
            timeline: None,
            safe_point: false,
            problem: None,
            attention: Vec::new(),
            conductor_available: false,
            conductor_unavailable_reason: None,
            default_harness_unavailable: None,
            workspaces: Vec::new(),
            generated_title: None,
            observed_at: self.now(),
        };
        self.service
            .runner_snapshot(&self.identity, request)
            .expect("the control plane takes the snapshot");
    }

    /// Ends the current wait with an approval of `mr_url`, as the runner
    /// hands a delivered event to its engine, and reports again.
    async fn approve(&mut self, mr_url: &str) {
        let episode = self.pending().expect("the task waits").episode;
        let delivery = ExternalEventDelivery {
            episode,
            ending: WaitEnding::Event {
                event_type: "review.approved".into(),
                source: "gitlab".into(),
                data: json!({ "mr_url": mr_url }),
            },
        };
        self.engine()
            .deliver_external_event(&self.definition, self.execution_id(), &delivery)
            .await
            .expect("the engine takes the approval");
        self.report();
    }

    /// The task's `WatchRefused` warnings, as `(step, observer, object,
    /// reason)`.
    fn warnings(&self) -> Vec<(String, String, String, String)> {
        self.service
            .control_plane()
            .events_since(&self.task, 0)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskEventKind::WatchRefused {
                    step,
                    observer,
                    object,
                    reason,
                } => Some((step, observer, object, reason)),
                _ => None,
            })
            .collect()
    }

    fn watches(&self, observer: &ObserverId) -> Vec<WatchEntry> {
        self.service.observers().list_watches(observer).unwrap()
    }

    /// The task still waits in a `wait` step, on both sides.
    fn assert_waiting(&self) {
        assert_eq!(self.execution().status, ExecutionStatus::WaitingForEvent);
        let pending = self.pending().expect("the wait is on disk");
        assert_eq!(
            self.record()
                .event_wait
                .as_ref()
                .map(|wait| wait.episode_id.as_str()),
            Some(pending.episode.as_str()),
            "the control plane knows the wait"
        );
    }
}

/// An approved observer `name` of `owner`, watching objects under
/// [`PREFIX`] up to `max_len` characters.
fn observer(
    service: &ProtocolService,
    now: DateTime<Utc>,
    name: &str,
    owner: &str,
    watch_mode: WatchMode,
    max_len: usize,
) -> ObserverId {
    // Only a watch-list observer delivers events; any other creates tasks,
    // of the fixture's workflow here.
    let workflow = fixture();
    let effect = match watch_mode {
        WatchMode::WatchList => ObserverEffect::DeliverEvent,
        _ => ObserverEffect::CreateTask(CreateTaskEffect {
            workflow_id: workflow.id.clone(),
            workflow_version: workflow.version,
            workspaces: Default::default(),
            backend_id: None,
            profile: None,
            mapping: Default::default(),
        }),
    };
    let mut config = ObserverConfig::new(ObserverName::parse(name).unwrap(), "gitlab", effect);
    config.watch_mode = watch_mode;
    config.watch_objects = WatchObjectPolicy {
        allowed_prefixes: vec![PREFIX.into()],
        max_len,
    };
    let store = service.observers();
    let id = store
        .create(OwnerRef::new(owner), config, Some(&workflow), now)
        .unwrap()
        .observer_id;
    store
        .add_script(&id, "echo '{\"state\": {}}'\n", now)
        .unwrap();
    id
}

/// The `mr-review` watch-list observer of the task's owner.
fn mr_review(stack: &Stack) -> ObserverId {
    observer(
        &stack.service,
        stack.now(),
        OBSERVER,
        OWNER,
        WatchMode::WatchList,
        WatchObjectPolicy::default().max_len,
    )
}

// ---------------------------------------------------------------------------
// Happy path and the end of an entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_waiting_step_with_a_watch_adds_one_entry_for_its_task_and_object() {
    // Verification of U5: the workflow of the docs loads and registers its
    // entry when its `wait` suspends.
    let mut stack = Stack::new(fixture(), &[]);
    let observer = mr_review(&stack);
    assert!(stack.watches(&observer).is_empty());

    let waiting = stack.start(MR).await;

    assert_eq!(waiting.status, ExecutionStatus::WaitingForEvent);
    let watches = stack.watches(&observer);
    assert_eq!(watches.len(), 1, "{watches:?}");
    let entry = &watches[0];
    assert_eq!(entry.task_id, stack.task);
    assert_eq!(entry.step.as_deref(), Some("await_review"));
    assert_eq!(entry.object, MR, "the object template was rendered");
    assert_eq!(
        entry.event_types,
        vec![
            "review.approved".to_string(),
            "review.changes_requested".to_string()
        ]
    );
    assert_eq!(entry.origin, WatchOrigin::Workflow);
    assert!(stack.warnings().is_empty());
    // The watch travelled with the wait the runner reported.
    let reported = stack.record().event_wait.expect("the task waits");
    let watch = reported.watch.expect("the wait carries its watch");
    assert_eq!(watch.step, "await_review");
    assert_eq!(watch.observer, OBSERVER);
    assert_eq!(watch.object, MR);

    // The observer's next run sees the entry, with the wait's episode.
    stack
        .service
        .approve_observer_version(&observer, 1)
        .unwrap();
    stack.service.observer_pass();
    let claimed = stack
        .service
        .claim_observer_run()
        .unwrap()
        .expect("a watch-list observer with an entry runs");
    let watch = claimed.input.watch.expect("a watch run checks one entry");
    assert_eq!(watch.object, MR);
    assert_eq!(watch.task_id, stack.task);
    assert_eq!(
        watch.wait_episode.as_deref(),
        Some(reported.episode_id.as_str())
    );
}

#[tokio::test]
async fn the_entry_goes_when_the_script_exits_0_for_it() {
    // Covers AE5 (the end): the script exits 0 for MR 17 (D18), and its
    // entry is removed. A later report of the same wait does not add it
    // back.
    let mut stack = Stack::new(fixture(), &[]);
    let observer = mr_review(&stack);
    stack.start(MR).await;
    assert_eq!(stack.watches(&observer).len(), 1);

    stack
        .service
        .approve_observer_version(&observer, 1)
        .unwrap();
    stack.service.observer_pass();
    let claimed = stack.service.claim_observer_run().unwrap().expect("due");
    stack
        .service
        .finish_observer_run(
            &claimed.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 0 },
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .unwrap();

    assert!(stack.watches(&observer).is_empty());
    // The runner reports the same wait again (its next snapshot): the
    // episode is not new, so the entry stays gone.
    stack.report();
    assert!(stack.watches(&observer).is_empty());
    stack.assert_waiting();
}

#[tokio::test]
async fn a_review_loop_that_waits_again_keeps_one_entry() {
    let mut stack = Stack::new(load(REVIEW_LOOP), &["again", "merged"]);
    let observer = mr_review(&stack);
    stack.start(MR).await;
    let first = stack.watches(&observer);
    assert_eq!(first.len(), 1);
    let first_episode = stack.pending().unwrap().episode;

    // The first approval is checked and sends the task back to wait.
    stack.approve(MR).await;
    stack.assert_waiting();
    let second_episode = stack.pending().unwrap().episode;
    assert_ne!(first_episode, second_episode, "a new wait episode");

    assert_eq!(stack.watches(&observer), first, "still the one entry");
    assert!(stack.warnings().is_empty());

    // The second approval ends the loop.
    stack.approve(MR).await;
    assert_eq!(stack.execution().status, ExecutionStatus::Completed);
    assert_eq!(stack.recorder.calls().len(), 2);
    assert_eq!(stack.watches(&observer), first);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// The loop workflow with its `watch` block replaced by `watch`.
fn with_watch(watch: &str) -> String {
    let original = "      watch:\n        observer: mr-review\n        object: \"{{ inputs.mr_url }}\"\n        event_types: [review.approved]\n";
    assert!(REVIEW_LOOP.contains(original));
    REVIEW_LOOP.replace(original, watch)
}

/// The validation errors of `yaml`, which must parse.
fn watch_errors(yaml: &str) -> Vec<ValidationError> {
    validate_source(yaml).unwrap_or_else(|error| panic!("the workflow parses: {error}"))
}

#[test]
fn the_documented_workflow_and_the_review_loop_validate() {
    let errors = validate_path(fixtures().join(FIXTURE)).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(watch_errors(REVIEW_LOOP).is_empty());
}

#[test]
fn a_watch_without_an_observer_name_does_not_load() {
    let missing = with_watch(
        "      watch:\n        object: \"{{ inputs.mr_url }}\"\n        event_types: [review.approved]\n",
    );
    let error = validate_source(&missing).expect_err("a watch needs its observer");
    assert!(error.to_string().contains("observer"), "{error}");

    let blank = with_watch(
        "      watch:\n        observer: \"  \"\n        object: \"{{ inputs.mr_url }}\"\n        event_types: [review.approved]\n",
    );
    let errors = watch_errors(&blank);
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::MissingActionField {
                field: "watch.observer",
                ..
            }
        )),
        "{errors:?}"
    );

    // Named explicitly: a template is no observer name.
    let templated = with_watch(
        "      watch:\n        observer: \"{{ inputs.mr_url }}\"\n        object: \"{{ inputs.mr_url }}\"\n        event_types: [review.approved]\n",
    );
    let errors = watch_errors(&templated);
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ValidationError::InvalidWaitWatch { .. })),
        "{errors:?}"
    );
}

#[test]
fn the_event_types_of_a_watch_include_the_event_type_of_its_wait() {
    let other_type = with_watch(
        "      watch:\n        observer: mr-review\n        object: \"{{ inputs.mr_url }}\"\n        event_types: [review.merged]\n",
    );
    let errors = watch_errors(&other_type);
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::InvalidWaitWatch { reason, .. } if reason.contains("review.approved")
        )),
        "{errors:?}"
    );

    let none = with_watch(
        "      watch:\n        observer: mr-review\n        object: \"{{ inputs.mr_url }}\"\n        event_types: []\n",
    );
    let errors = watch_errors(&none);
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ValidationError::InvalidWaitWatch { .. })),
        "{errors:?}"
    );

    let no_object = with_watch(
        "      watch:\n        observer: mr-review\n        object: \"\"\n        event_types: [review.approved]\n",
    );
    let errors = watch_errors(&no_object);
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::MissingActionField {
                field: "watch.object",
                ..
            }
        )),
        "{errors:?}"
    );

    // The object's templates are checked like any other field's.
    let unknown_input = with_watch(
        "      watch:\n        observer: mr-review\n        object: \"{{ inputs.nope }}\"\n        event_types: [review.approved]\n",
    );
    let errors = watch_errors(&unknown_input);
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::UnknownInputReference { field, .. } if field == "action.watch.object"
        )),
        "{errors:?}"
    );
}

#[test]
fn the_documented_workflow_is_the_fixture() {
    let docs = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/dev/workflow-schema.md"),
    )
    .unwrap();
    let fixture = fs::read_to_string(fixtures().join(FIXTURE)).unwrap();
    assert!(
        docs.contains(&fixture),
        "docs/dev/workflow-schema.md §3.3.D shows tests/fixtures/{FIXTURE} verbatim"
    );
    let ActionConfig::Wait(wait) = fixture_wait_action() else {
        panic!("await_review is a wait step");
    };
    let watch = wait.watch.expect("the fixture has a watch");
    assert_eq!(watch.observer, OBSERVER);
    assert_eq!(watch.object, "{{ inputs.mr_url }}");
}

fn fixture_wait_action() -> ActionConfig {
    fixture().states["await_review"]
        .action
        .clone()
        .expect("an action")
}

// ---------------------------------------------------------------------------
// Refusals: a warning in the task's history, and the task keeps waiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_watch_naming_no_observer_warns_once_and_the_task_keeps_waiting() {
    let mut stack = Stack::new(fixture(), &[]);
    // Another observer exists; the one the step names does not.
    let other = observer(
        &stack.service,
        stack.now(),
        "other",
        OWNER,
        WatchMode::WatchList,
        WatchObjectPolicy::default().max_len,
    );

    stack.start(MR).await;

    stack.assert_waiting();
    let warnings = stack.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let (step, observer, object, reason) = &warnings[0];
    assert_eq!(step, "await_review");
    assert_eq!(observer, OBSERVER);
    assert_eq!(object, MR);
    assert!(reason.contains("no observer named 'mr-review'"), "{reason}");
    assert!(stack.watches(&other).is_empty());

    // Another report of the same wait warns no more.
    stack.report();
    assert_eq!(stack.warnings().len(), 1);
    stack.assert_waiting();
}

#[tokio::test]
async fn an_object_on_another_host_or_too_long_is_refused_with_a_warning() {
    let lookalike = "https://git.example.com.evil.org/a/-/merge_requests/17";
    let elsewhere = "https://evil.example.org/a/-/merge_requests/17";
    let too_long = format!("{PREFIX}{}", "a".repeat(80));
    for (object, expected) in [
        (lookalike, "not under an allowed prefix"),
        (elsewhere, "not under an allowed prefix"),
        (too_long.as_str(), "more than the limit of 64"),
    ] {
        let mut stack = Stack::new(fixture(), &[]);
        let observer = observer(
            &stack.service,
            stack.now(),
            OBSERVER,
            OWNER,
            WatchMode::WatchList,
            64,
        );

        stack.start(object).await;

        stack.assert_waiting();
        assert!(stack.watches(&observer).is_empty(), "{object}: no entry");
        let warnings = stack.warnings();
        assert_eq!(warnings.len(), 1, "{object}: {warnings:?}");
        assert_eq!(warnings[0].2, object);
        assert!(warnings[0].3.contains(expected), "{object}: {warnings:?}");
    }
}

#[tokio::test]
async fn an_observer_of_another_owner_reads_as_none_and_one_without_a_watch_list_is_refused() {
    // Another owner's observer: its events could never reach the task, and
    // the warning says no more than for a missing one.
    let mut stack = Stack::new(fixture(), &[]);
    let foreign = observer(
        &stack.service,
        stack.now(),
        OBSERVER,
        "someone-else",
        WatchMode::WatchList,
        WatchObjectPolicy::default().max_len,
    );
    stack.start(MR).await;
    stack.assert_waiting();
    assert!(stack.watches(&foreign).is_empty());
    let warnings = stack.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].3.contains("no observer named 'mr-review'"),
        "{warnings:?}"
    );

    // An observer of the owner that keeps no watch list.
    let mut stack = Stack::new(fixture(), &[]);
    let all = observer(
        &stack.service,
        stack.now(),
        OBSERVER,
        OWNER,
        WatchMode::All,
        WatchObjectPolicy::default().max_len,
    );
    stack.start(MR).await;
    stack.assert_waiting();
    assert!(stack.watches(&all).is_empty());
    let warnings = stack.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].3.contains("keeps no watch list"),
        "{warnings:?}"
    );
}

#[test]
fn a_refused_watch_reaches_an_old_client_as_unrecognized() {
    let event = json!({
        "type": "watch_refused",
        "step": "await_review",
        "observer": OBSERVER,
        "object": MR,
        "reason": "no observer named 'mr-review' belongs to the task's owner",
    });
    let kind: TaskEventKind = serde_json::from_value(event.clone()).unwrap();
    assert!(matches!(kind, TaskEventKind::WatchRefused { .. }));
    assert_eq!(serde_json::to_value(&kind).unwrap(), event);
    let unknown: TaskEventKind =
        serde_json::from_value(json!({"type": "something_newer", "x": 1})).unwrap();
    assert_eq!(unknown, TaskEventKind::Unrecognized);
}

#[test]
fn the_runner_reports_the_watch_with_its_wait() {
    let wait = json!({
        "episode_id": "e-1",
        "event_type": "review.approved",
        "filter": {"mr_url": MR},
        "since": "2026-11-13T10:00:00Z",
        "watch": {
            "step": "await_review",
            "observer": OBSERVER,
            "object": MR,
            "event_types": ["review.approved"],
        },
    });
    let parsed: narsor_protocol::controller::EventWait =
        serde_json::from_value(wait.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), wait);
    // A wait without a watch says nothing of one.
    let mut without: Value = wait;
    without.as_object_mut().unwrap().remove("watch");
    let parsed: narsor_protocol::controller::EventWait =
        serde_json::from_value(without.clone()).unwrap();
    assert_eq!(parsed.watch, None);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), without);
}
