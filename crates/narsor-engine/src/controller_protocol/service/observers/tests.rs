//! The automations' schedule on the service, without a daemon: the pass,
//! the claim, the result and the queue views over a catalogue in a
//! temporary directory, driven by a manual clock.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

use super::*;
use crate::automations::{
    AutomationCatalog, AutomationCatalogError, AutomationCatalogLoad, AutomationScript, HookPhase,
};
use crate::controller_protocol::credentials::{RunnerIdentity, RunnerRole};
use crate::controller_protocol::error::ProtocolError;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::ObserverDecideRequest;
use crate::domain::ExecutionStatus;
use crate::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace, WorkflowDefinition,
};
use crate::observers::{
    queue_entry_id, AutomationKey, AutomationRecord, AutomationState, LastRun, ObserverMode,
    QueueEntry, RunFailure, RunId, RunStatus, SCRIPT_CONTRACT_VERSION,
};
use crate::orchestration::{
    Clock, ControlPlane, ExecutionCursor, NewTask, OwnerRef, PendingInputRef, RunnerSnapshot,
};
use crate::persistence::StatePaths;
use narsor_protocol::controller::{GateWatch, GateWatchChoice};

/// A clock the test moves by hand.
#[derive(Debug)]
pub(super) struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// Resolves no launch: the schedule never launches anything. Serves the
/// automation catalogue under `catalog`.
struct NoLaunches {
    catalog: PathBuf,
}

impl crate::environment::LaunchResolver for NoLaunches {
    fn resolve(
        &self,
        _intent: &crate::environment::LaunchIntent,
    ) -> Result<crate::environment::ResolvedLaunch, crate::environment::LaunchError> {
        Err(crate::environment::LaunchError::UnknownBackend(
            "k8s".to_string(),
        ))
    }

    fn check_backend(&self, backend: &BackendId) -> Result<(), crate::environment::LaunchError> {
        Err(crate::environment::LaunchError::UnknownBackend(
            backend.to_string(),
        ))
    }

    fn slot_release_timings(
        &self,
        backend: &BackendId,
    ) -> Result<crate::environment::SlotReleaseTimings, crate::environment::LaunchError> {
        Err(crate::environment::LaunchError::UnknownBackend(
            backend.to_string(),
        ))
    }

    fn configured_default_backend(&self) -> String {
        "k8s".to_string()
    }

    fn backend_record(
        &self,
        _backend: &BackendId,
    ) -> Result<Option<crate::environment::BackendRecord>, crate::environment::LaunchError> {
        Ok(None)
    }

    fn workflow_definition(
        &self,
        workflow_id: &str,
    ) -> Result<WorkflowDefinition, crate::environment::LaunchError> {
        if workflow_id == "triage" {
            Ok(triage_workflow())
        } else {
            Err(crate::environment::LaunchError::UnknownWorkflow {
                workflow: workflow_id.to_string(),
                message: "not in the catalogue".to_string(),
            })
        }
    }

    fn automation_catalog(&self) -> AutomationCatalogLoad {
        AutomationCatalog::new(&self.catalog).load(|workflow| {
            self.workflow_definition(workflow)
                .map_err(|error| error.to_string())
        })
    }

    fn automation_script(
        &self,
        automation: &str,
        phase: HookPhase,
    ) -> Result<AutomationScript, AutomationCatalogError> {
        AutomationCatalog::new(&self.catalog).script(automation, phase)
    }
}

const INTERVAL: u64 = 300;

/// A service over a temporary state root, a catalogue next to it and a
/// manual clock.
pub(super) struct Fixture {
    pub(super) root: PathBuf,
    pub(super) clock: Arc<ManualClock>,
    pub(super) service: ProtocolService,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    pub(super) fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-schedule-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap(),
        )));
        let control = ControlPlane::with_clock(StatePaths::new(root.join("state")), clock.clone());
        let service = ProtocolService::new(
            control,
            Arc::new(NoLaunches {
                catalog: root.join("catalog"),
            }),
        );
        Self {
            root,
            clock,
            service,
        }
    }

    pub(super) fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    pub(super) fn advance(&self, secs: i64) {
        *self.clock.0.lock().unwrap() += Duration::seconds(secs);
    }

    fn catalog(&self) -> PathBuf {
        self.root.join("catalog")
    }

    pub(super) fn write(&self, relative: &str, text: &str) {
        let path = self.catalog().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The automation `name` with a `check` script and one secret.
    pub(super) fn automation(&self, name: &str) {
        self.write(
            &format!("automations/{name}/automation.yaml"),
            "description: test\nsecrets:\n  - {name: jira-token, env: JIRA_TOKEN}\n",
        );
        self.write(
            &format!("automations/{name}/check.sh"),
            "echo '{\"state\": {}}'\n",
        );
    }

    /// Queue `queue` served by `automation`, every 300 s: an `on_episodes`
    /// queue, or an `always` one that creates tasks of `triage`.
    pub(super) fn queue(&self, queue: &str, automation: &str, mode: ObserverMode) {
        self.automation(automation);
        let tasks = match mode {
            ObserverMode::OnEpisodes => String::new(),
            ObserverMode::Always => {
                "tasks:\n  workflow_id: triage\n  mapping: {ticket: key}\n  task_limit_per_run: 1\n"
                    .to_string()
            }
        };
        self.write(
            &format!("queues/{queue}.yaml"),
            &format!(
                "automations: [{automation}]\nmode: {mode}\nschedule: {{interval_secs: {INTERVAL}}}\n\
                 condition: {{project: ABC}}\n{tasks}"
            ),
        );
    }

    pub(super) fn record(&self, key: &AutomationKey) -> AutomationRecord {
        self.service
            .observers()
            .load(key)
            .unwrap()
            .expect("the pair has a record")
    }

    pub(super) fn claim(&self) -> Option<ClaimedObserverRun> {
        self.service.claim_observer_run().unwrap()
    }

    pub(super) fn finish(&self, claimed: &ClaimedObserverRun, report: RunReport) -> LastRun {
        self.service
            .finish_observer_run(&claimed.run_id, report)
            .unwrap()
    }

    /// The kept execution errors of a pair, newest first.
    pub(super) fn failures(&self, key: &AutomationKey) -> Vec<RunFailure> {
        self.service.observers().list_failures(key).unwrap()
    }

    /// A queued task an entry can point at.
    pub(super) fn task(&self, id: &str) -> TaskId {
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

    /// Makes `task_id` wait at the gate `review_work` in episode `episode`,
    /// its `watch` naming `queue`: the runner's snapshot adds the entry.
    pub(super) fn entry(&self, queue: &str, task_id: &TaskId, episode: &str) -> QueueEntry {
        self.report_gate(task_id, Some((episode, review_gate(queue, task_id))));
        self.service
            .observers()
            .queue_entry(&queue_entry_id(task_id, episode))
            .unwrap()
            .expect("the snapshot added the entry")
    }

    /// The task's runner reports a snapshot through the runner channel:
    /// waiting at `gate` in its episode, or (`None`) past every gate.
    pub(super) fn report_gate(&self, task_id: &TaskId, gate: Option<(&str, GateWatch)>) {
        let identity = RunnerIdentity {
            role: RunnerRole::Worker,
            task_id: task_id.clone(),
            launch_generation: 1,
            incarnation: "pod-1".into(),
        };
        self.service
            .credentials
            .issue_runner_session(
                RunnerRole::Worker,
                task_id,
                "pod-1",
                1,
                0,
                Duration::hours(1),
            )
            .unwrap();
        let sequence = self
            .service
            .control_plane()
            .task(task_id)
            .unwrap()
            .runner
            .map_or(1, |runner| runner.snapshot.snapshot_sequence + 1);
        let (pending_input, gate_watch) = match gate {
            Some((episode, watch)) => (Some(pending_input(episode)), Some(watch)),
            None => (None, None),
        };
        let snapshot = RunnerSnapshot {
            snapshot_sequence: sequence,
            execution_status: if pending_input.is_some() {
                ExecutionStatus::InputNeeded
            } else {
                ExecutionStatus::Running
            },
            execution_cursor: ExecutionCursor {
                execution_id: Some("exec-1".into()),
                ..Default::default()
            },
            event_cursor: sequence,
            pending_input,
            gate_watch,
            conversation: Default::default(),
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
        let response = self
            .service
            .runner_snapshot(&identity, snapshot.to_request().unwrap())
            .unwrap();
        assert!(response.applied);
    }
}

/// The `watch` of the gate `review_work` of `task_id`, on `queue`.
pub(super) fn review_gate(queue: &str, task_id: &TaskId) -> GateWatch {
    GateWatch {
        step: "review_work".into(),
        queue: queue.into(),
        params: [("mr_url".to_string(), format!("https://x/{task_id}"))].into(),
        choices: vec![GateWatchChoice {
            name: "check_feedback".into(),
            description: Some("Read the feedback".into()),
            requires_variables: Vec::new(),
        }],
        variables: Vec::new(),
    }
}

/// The pending input request of a gate in `episode`.
fn pending_input(episode: &str) -> PendingInputRef {
    let form = serde_json::from_value(json!({
        "execution_id": "exec-1",
        "state_id": "review_work",
        "action_execution_id": "act-1",
        "instruction": "Review the merge request",
        "fields": [],
        "outputs": [],
        "episode": episode,
        "requested_at": "2026-11-17T12:00:00Z",
    }))
    .unwrap();
    PendingInputRef {
        id: episode.to_string(),
        attempt_id: None,
        form,
    }
}

pub(super) fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

fn key(queue: &str, automation: &str) -> AutomationKey {
    AutomationKey::new(queue, automation).unwrap()
}

fn triage_workflow() -> WorkflowDefinition {
    serde_json::from_value(json!({
        "id": "triage",
        "name": "triage",
        "version": 1,
        "inputs": [{"name": "ticket", "required": true}],
        "states": {},
        "initial_state": "start",
    }))
    .unwrap()
}

fn exited(code: i32, stdout: &str) -> RunReport {
    RunReport {
        outcome: RunOutcome::Exited { exit_code: code },
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

#[test]
fn the_backoff_doubles_the_interval_up_to_an_hour_and_never_below_it() {
    assert_eq!(retry_delay_secs(300, 0), 300);
    assert_eq!(retry_delay_secs(300, 1), 600);
    assert_eq!(retry_delay_secs(300, 3), 2400);
    assert_eq!(retry_delay_secs(300, 4), 3600);
    assert_eq!(retry_delay_secs(300, 200), 3600);
    assert_eq!(retry_delay_secs(7200, 3), 7200);
}

#[test]
fn the_first_pass_creates_the_pairs_record_and_the_next_reuses_it() {
    // U2: the record is created on first need, keyed by (queue, automation).
    let fx = Fixture::new();
    fx.queue("mr-review", "gitlab-mr-label", ObserverMode::OnEpisodes);
    let pair = key("mr-review", "gitlab-mr-label");

    let first = fx.service.observer_pass();
    assert_eq!(first.created, vec![pair.clone()]);
    assert_eq!(first.skipped, vec![pair.clone()], "no entry, no run (R30)");
    let created = fx.record(&pair);
    assert_eq!(
        created.next_run_at,
        Some(fx.now() + Duration::seconds(INTERVAL as i64))
    );
    assert!(fx
        .service
        .observers()
        .paths()
        .automation_state_file("mr-review", "gitlab-mr-label")
        .is_file());

    fx.advance(INTERVAL as i64);
    let second = fx.service.observer_pass();
    assert!(second.created.is_empty());
    assert_eq!(second.skipped, vec![pair.clone()]);
    let reused = fx.record(&pair);
    assert_eq!(reused.created_at, created.created_at);
    assert!(reused.revision > created.revision);
}

#[test]
fn a_pair_that_left_the_catalogue_loses_its_record_once_no_run_is_active() {
    let fx = Fixture::new();
    fx.queue("jira-intake", "jira-new", ObserverMode::Always);
    let pair = key("jira-intake", "jira-new");
    fx.service.observer_pass();
    let run = fx.claim().expect("a due run");

    std::fs::remove_file(fx.catalog().join("queues/jira-intake.yaml")).unwrap();
    let kept = fx.service.observer_pass();
    assert!(
        kept.removed.is_empty(),
        "a run in progress keeps the record"
    );
    assert!(
        fx.claim().is_none(),
        "no claim for a pair out of the catalogue"
    );
    // Its result still comes.
    fx.finish(&run, exited(0, r#"{"state": {}}"#));

    let removed = fx.service.observer_pass();
    assert_eq!(removed.removed, vec![pair.clone()]);
    assert_eq!(fx.service.observers().load(&pair).unwrap(), None);
    assert!(!fx
        .service
        .observers()
        .paths()
        .automation_state_dir("jira-intake", "jira-new")
        .exists());
}

#[test]
fn an_always_run_is_claimed_once_with_contract_three_and_the_declared_secrets() {
    let fx = Fixture::new();
    fx.queue("jira-intake", "jira-new", ObserverMode::Always);
    let pair = key("jira-intake", "jira-new");

    assert_eq!(fx.service.observer_pass().due, vec![pair.clone()]);
    let first = fx.claim().expect("a due run");
    assert_eq!(first.run_id, RunId::new(pair.clone(), 1));
    assert_eq!(first.phase, HookPhase::Check);
    assert_eq!(first.input.contract_version, SCRIPT_CONTRACT_VERSION);
    assert_eq!(SCRIPT_CONTRACT_VERSION, 3);
    assert_eq!(first.input.phase, HookPhase::Check);
    assert_eq!(first.input.queue, "jira-intake");
    assert_eq!(first.input.automation, "jira-new");
    assert!(first.input.first_run);
    assert_eq!(first.input.condition, json!({"project": "ABC"}));
    assert_eq!(first.script, "echo '{\"state\": {}}'\n");
    assert_eq!(first.secrets.len(), 1);
    assert_eq!(first.secrets[0].name, "jira-token");
    assert!(first.token.expose().starts_with("nso."));
    // One run at a time per pair.
    assert!(fx.claim().is_none());
    let active = fx.record(&pair).active_run.expect("the run in progress");
    assert_eq!(active.script_digest, first.script_digest);
    assert_eq!(active.token_digest, first.token.digest());
    let stored = std::fs::read_to_string(
        fx.service
            .observers()
            .paths()
            .automation_state_file("jira-intake", "jira-new"),
    )
    .unwrap();
    assert!(!stored.contains(first.token.expose()));

    fx.finish(&first, exited(0, r#"{"state": {"cursor": 5}}"#));
    let record = fx.record(&pair);
    assert_eq!(record.script_state, Some(json!({"cursor": 5})));
    assert!(record.first_run_done);
}

#[test]
fn a_changed_script_runs_at_the_next_tick_with_its_new_digest() {
    // R2: no cache; the run records the digest of the text it got.
    let fx = Fixture::new();
    fx.queue("jira-intake", "jira-new", ObserverMode::Always);
    fx.service.observer_pass();
    let first = fx.claim().unwrap();
    fx.finish(&first, exited(0, r#"{"state": {}}"#));

    fx.write(
        "automations/jira-new/check.sh",
        "echo '{\"state\": {\"v\": 2}}'\n",
    );
    fx.advance(INTERVAL as i64);
    fx.service.observer_pass();
    let second = fx.claim().unwrap();

    assert_eq!(second.script, "echo '{\"state\": {\"v\": 2}}'\n");
    assert_ne!(second.script_digest, first.script_digest);
    assert!(second.script_digest.starts_with("sha256:"));
}

#[test]
fn a_script_unreadable_at_the_claim_is_a_failed_run_with_a_clear_error() {
    let fx = Fixture::new();
    fx.queue("jira-intake", "jira-new", ObserverMode::Always);
    let pair = key("jira-intake", "jira-new");
    fx.service.observer_pass();
    // The loader saw the file; the text read at the claim cannot be used
    // (a file that is gone by then makes the queue inactive instead).
    std::fs::write(
        fx.catalog().join("automations/jira-new/check.sh"),
        b"\xff\xfe",
    )
    .unwrap();

    assert!(fx.claim().is_none());

    let errors = fx.failures(&pair);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].status, RunStatus::Failed);
    assert!(
        errors[0].error.contains("check script could not be read"),
        "{}",
        errors[0].error
    );
    let record = fx.record(&pair);
    assert_eq!(record.consecutive_failures, 1);
    assert_eq!(record.active_run, None);
    assert_eq!(record.last_run.unwrap().status, RunStatus::Failed);
}

#[test]
fn five_failures_stop_the_pair_until_its_check_script_changes() {
    // R16: no resume route; a new script is what puts it back.
    let fx = Fixture::new();
    fx.queue("jira-intake", "jira-new", ObserverMode::Always);
    let pair = key("jira-intake", "jira-new");
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        fx.service.observer_pass();
        let run = fx.claim().expect("a due run");
        fx.finish(&run, exited(1, ""));
        fx.advance(MAX_RETRY_DELAY_SECS as i64);
    }
    assert_eq!(fx.record(&pair).state, AutomationState::Error);
    fx.service.observer_pass();
    assert!(fx.claim().is_none());

    fx.write(
        "automations/jira-new/check.sh",
        "echo '{\"state\": {}}' # fixed\n",
    );
    let pass = fx.service.observer_pass();
    assert_eq!(pass.restarted, vec![pair.clone()]);
    let record = fx.record(&pair);
    assert_eq!(record.state, AutomationState::Active);
    assert_eq!(record.consecutive_failures, 0);
    fx.service.observer_pass();
    assert!(fx.claim().is_some());
}

#[test]
fn a_run_without_a_result_by_its_deadline_is_lost_without_counting_a_failure() {
    let fx = Fixture::new();
    fx.queue("jira-intake", "jira-new", ObserverMode::Always);
    let pair = key("jira-intake", "jira-new");
    fx.service.observer_pass();
    let run = fx.claim().unwrap();

    fx.advance((SCRIPT_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS) as i64);
    let pass = fx.service.observer_pass();

    assert_eq!(pass.lost, vec![run.run_id.clone()]);
    let record = fx.record(&pair);
    assert_eq!(record.active_run, None);
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(fx.failures(&pair)[0].status, RunStatus::Lost);
    assert!(fx
        .service
        .authenticate_observer_run(run.token.expose())
        .is_none());
}

#[test]
fn each_run_checks_one_entry_of_the_queue_with_its_episode() {
    // R30: one entry per run; the round takes the entries in order.
    let fx = Fixture::new();
    fx.queue("mr-review", "gitlab-mr-label", ObserverMode::OnEpisodes);
    let first_task = fx.task("task-first");
    let second_task = fx.task("task-second");
    fx.entry("mr-review", &first_task, "input-3");
    fx.advance(1);
    fx.entry("mr-review", &second_task, "input-1");

    fx.service.observer_pass();
    let first = fx.claim().unwrap();
    assert!(fx.claim().is_none(), "one run at a time");
    fx.finish(&first, exited(0, ""));
    let second = fx.claim().unwrap();
    fx.finish(&second, exited(3, ""));

    let entries: Vec<_> = [&first, &second]
        .iter()
        .map(|claimed| {
            let entry = claimed.input.entry.as_ref().unwrap();
            (entry.task_id.as_str(), entry.episode.as_str())
        })
        .collect();
    assert_eq!(
        entries,
        vec![("task-first", "input-3"), ("task-second", "input-1")]
    );
    assert_eq!(first.input.phase, HookPhase::Check);
    assert!(fx.claim().is_none(), "the round is over");
    // A failed entry run: an error for its entry, no backoff (R14, R23).
    let pair = key("mr-review", "gitlab-mr-label");
    assert_eq!(fx.record(&pair).consecutive_failures, 0);
    let errors = fx.failures(&pair);
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].entry_id.as_deref(),
        Some(queue_entry_id(&second_task, "input-1").as_str())
    );
}

#[test]
fn a_run_whose_phase_is_not_check_answers_no_gate() {
    // Only `check` decides; `in` and `out` runs come with U4/U5, so the
    // phase of the run in progress is set by hand here.
    let fx = Fixture::new();
    fx.queue("mr-review", "gitlab-mr-label", ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    fx.service.observer_pass();
    let run = fx.claim().unwrap();
    let pair = key("mr-review", "gitlab-mr-label");
    {
        let lock = fx.service.observers().lock(&pair).unwrap();
        let mut record = fx.record(&pair);
        record.active_run.as_mut().unwrap().phase = HookPhase::In;
        fx.service
            .observers()
            .save(&lock, &mut record, fx.now())
            .unwrap();
    }

    let refused = fx
        .service
        .observer_decide(
            &run.run_id,
            ObserverDecideRequest {
                task_id: "task-1".into(),
                episode: "input-1".into(),
                choice: "check_feedback".into(),
                reason: None,
                variables: Default::default(),
            },
        )
        .unwrap_err();

    match refused {
        ProtocolError::BadRequest { code, .. } => assert_eq!(code, "not_a_check_run"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_queue_views_list_catalogue_queues_bad_files_and_pair_state() {
    // R21, AE8: read only; a bad queue file is listed with its problem, the
    // others normally.
    let fx = Fixture::new();
    fx.queue("mr-review", "gitlab-mr-label", ObserverMode::OnEpisodes);
    fx.write(
        "queues/broken.yaml",
        "automations: [missing]\nmode: on_episodes\nschedule: {interval_secs: 300}\n",
    );
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    fx.service.observer_pass();
    let run = fx.claim().unwrap();
    fx.finish(&run, exited(2, ""));

    let list = fx.service.observer_queues_of(&owner()).unwrap();
    let names: Vec<_> = list
        .queues
        .iter()
        .map(|queue| queue.queue.as_str())
        .collect();
    assert_eq!(names, vec!["broken", "mr-review"]);
    assert!(list.queues[0]
        .problem
        .as_deref()
        .unwrap()
        .contains("missing"));
    assert_eq!(list.queues[1].problem, None);
    assert_eq!(list.queues[1].automations, vec!["gitlab-mr-label"]);
    assert_eq!(list.queues[1].entries, 1);
    assert_eq!(list.problems.len(), 1);
    assert_eq!(list.problems[0].file, "queues/broken.yaml");

    let view = fx.service.observer_queue(&owner(), "mr-review").unwrap();
    let automation = &view.automations[0];
    assert_eq!(automation.scripts, vec!["check"]);
    assert_eq!(automation.state, "active");
    assert_eq!(automation.errors.len(), 1);
    assert_eq!(automation.errors[0].phase, "check");
    let processed = &view.entries[0].automations[0];
    assert_eq!(processed.automation, "gitlab-mr-label");
    assert_eq!(processed.last_error.as_ref().unwrap().exit_code, Some(2));

    let broken = fx.service.observer_queue(&owner(), "broken").unwrap();
    assert_eq!(broken.mode, None);
    assert_eq!(broken.problems.len(), 1);
    // Another owner sees the queue, but not this owner's entries.
    let stranger = fx
        .service
        .observer_queue(&OwnerRef::new("someone-else"), "mr-review")
        .unwrap();
    assert!(stranger.entries.is_empty());
    assert!(matches!(
        fx.service.observer_queue(&owner(), "nothing"),
        Err(ProtocolError::NotFound)
    ));
}
