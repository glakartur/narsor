//! The observers' schedule on the service, without a daemon: the pass, the
//! claim, the result and the owner's decisions, driven by a manual clock.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

use super::*;
use crate::controller_protocol::service::ProtocolService;
use crate::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace, WorkflowDefinition,
};
use crate::observers::{
    CreateTaskEffect, LastRun, ObserverConfig, ObserverEffect, ObserverName, RunFailure, RunId,
    RunStatus, SecretRef, WatchEntry, WatchMode, WatchObjectPolicy, WatchOrigin, KEPT_RUN_FAILURES,
    OBSERVER_RECORD_VERSION,
};
use crate::orchestration::{
    Clock, CommandRequest, ControlPlane, EventWait, NewTask, OwnerCommand, OwnerRef,
};
use crate::persistence::StatePaths;

/// A clock the test moves by hand.
#[derive(Debug)]
pub(super) struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// Resolves no launch: the schedule never launches anything.
struct NoLaunches;

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
}

const INTERVAL: u64 = 300;
pub(super) const PREFIX: &str = "https://git.example.com/";

/// A service over a temporary state root and a manual clock.
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
        let control = ControlPlane::with_clock(StatePaths::new(&root), clock.clone());
        let service = ProtocolService::new(control, Arc::new(NoLaunches));
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

    /// A draft observer with one secret and script version 1.
    pub(super) fn draft(&self, name: &str, mode: WatchMode) -> ObserverId {
        let effect = match mode {
            WatchMode::WatchList => ObserverEffect::DeliverEvent,
            WatchMode::All => ObserverEffect::CreateTask(CreateTaskEffect {
                workflow_id: "triage".into(),
                workflow_version: 1,
                workspaces: Default::default(),
                backend_id: None,
                profile: None,
                mapping: [("ticket".to_string(), "key".to_string())].into(),
            }),
        };
        let mut config = ObserverConfig::new(ObserverName::parse(name).unwrap(), "jira", effect);
        config.watch_mode = mode;
        config.condition = json!({"project": "ABC"});
        config.secrets = vec![SecretRef {
            name: "jira-token".into(),
            env: "JIRA_TOKEN".into(),
        }];
        config.watch_objects = WatchObjectPolicy {
            allowed_prefixes: vec![PREFIX.into()],
            ..WatchObjectPolicy::default()
        };
        let record = self
            .service
            .observers()
            .create(owner(), config, Some(&triage_workflow()), self.now())
            .unwrap();
        self.service
            .observers()
            .add_script(&record.observer_id, "echo '{\"state\": {}}'\n", self.now())
            .unwrap();
        record.observer_id
    }

    /// An active observer running version 1, due now.
    pub(super) fn active(&self, name: &str, mode: WatchMode) -> ObserverId {
        let id = self.draft(name, mode);
        self.service.approve_observer_version(&id, 1).unwrap();
        id
    }

    pub(super) fn record(&self, id: &ObserverId) -> ObserverRecord {
        self.service.observers().load(id).unwrap()
    }

    pub(super) fn claim(&self) -> Option<ClaimedObserverRun> {
        self.service.claim_observer_run().unwrap()
    }

    pub(super) fn finish(&self, claimed: &ClaimedObserverRun, report: RunReport) -> LastRun {
        self.service
            .finish_observer_run(&claimed.run_id, report)
            .unwrap()
    }

    /// The kept execution errors of an observer, newest first.
    pub(super) fn failures(&self, id: &ObserverId) -> Vec<RunFailure> {
        self.service.observers().list_failures(id).unwrap()
    }

    /// Every file below the observer's directory, relative to it, sorted.
    pub(super) fn observer_files(&self, id: &ObserverId) -> Vec<String> {
        let dir = self.service.observers().paths().observer_dir(id.as_str());
        let mut files = Vec::new();
        let mut pending = vec![dir.clone()];
        while let Some(current) = pending.pop() {
            for entry in std::fs::read_dir(&current).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(&dir).unwrap();
                    files.push(relative.to_string_lossy().into_owned());
                }
            }
        }
        files.sort();
        files
    }

    /// A queued task the observer's entries can point at.
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

    pub(super) fn watch(&self, id: &ObserverId, task_id: &TaskId, object: &str) -> WatchEntry {
        let entry = WatchEntry::new(
            task_id.clone(),
            Some("review".into()),
            object,
            vec!["approved".into()],
            WatchOrigin::Workflow,
            self.now(),
        );
        let lock = self.service.observers().lock(id).unwrap();
        self.service
            .observers()
            .create_watch(&lock, &entry)
            .unwrap();
        entry
    }
}

pub(super) fn owner() -> OwnerRef {
    OwnerRef::new("owner")
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
fn a_watch_list_observer_without_entries_does_not_run_and_moves_on() {
    // Covers AE1 (R6).
    let fx = Fixture::new();
    let id = fx.active("mr-review", WatchMode::WatchList);

    let pass = fx.service.observer_pass();

    assert_eq!(pass.skipped, vec![id.clone()]);
    assert!(pass.due.is_empty());
    let record = fx.record(&id);
    assert_eq!(record.run_due_since, None);
    assert_eq!(
        record.next_run_at,
        Some(fx.now() + Duration::seconds(INTERVAL as i64))
    );
    assert!(fx.claim().is_none());
    assert_eq!(fx.record(&id).last_run_sequence, 0);
}

#[test]
fn a_due_observer_is_claimed_once_with_a_token_and_its_last_state() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);

    assert_eq!(fx.service.observer_pass().due, vec![id.clone()]);
    let first = fx.claim().expect("a due run");
    assert_eq!(first.run_id, RunId::new(id.clone(), 1));
    assert!(first.input.first_run);
    assert_eq!(first.input.state, None);
    assert_eq!(first.input.condition, json!({"project": "ABC"}));
    assert_eq!(first.secrets.len(), 1);
    assert!(first.token.expose().starts_with("nso."));
    // One run at a time per observer.
    assert!(fx.claim().is_none());
    assert!(fx.service.observer_pass().due.is_empty());
    // The token is in no record: only its digest, in the observer's record.
    let active = fx.record(&id).active_run.expect("the run in progress");
    assert_eq!(active.sequence, 1);
    assert_eq!(active.script_version, 1);
    assert_eq!(active.token_digest, first.token.digest());
    let stored = std::fs::read_to_string(
        fx.service
            .observers()
            .paths()
            .observer_record_file(id.as_str()),
    )
    .unwrap();
    assert!(!stored.contains(first.token.expose()));
    // The script's input is not stored either.
    assert!(!stored.contains("\"first_run\""));

    let run = fx.finish(&first, exited(0, r#"{"state": {"cursor": 5}}"#));
    assert_eq!(run.status, RunStatus::Succeeded);
    let record = fx.record(&id);
    assert_eq!(record.script_state, Some(json!({"cursor": 5})));
    assert!(record.first_run_done);
    assert_eq!(record.active_run, None);

    fx.advance(INTERVAL as i64);
    assert_eq!(fx.service.observer_pass().due, vec![id.clone()]);
    let second = fx.claim().expect("the next run");
    assert_eq!(second.run_id.sequence, 2);
    assert_eq!(second.input.state, Some(json!({"cursor": 5})));
    assert!(!second.input.first_run);
}

#[test]
fn an_unapproved_script_version_never_runs_on_schedule() {
    // Covers AE6 (R11).
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service
        .observers()
        .add_script(&id, "echo changed\n", fx.now())
        .unwrap();

    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();

    assert_eq!(claimed.script_version, 1);
    assert_eq!(claimed.script, "echo '{\"state\": {}}'\n");
    assert_eq!(fx.record(&id).approved_version, Some(1));
}

#[test]
fn a_control_plane_that_stood_still_for_hours_owes_one_run() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.advance(2 * 60 * 60);

    fx.service.observer_pass();
    fx.service.observer_pass();

    assert!(fx.claim().is_some());
    assert!(fx.claim().is_none());
    assert_eq!(fx.record(&id).last_run_sequence, 1);
}

#[test]
fn a_successful_run_leaves_no_file_but_the_observers_own() {
    // D15.
    let fx = Fixture::new();
    let id = fx.active("mr-review", WatchMode::WatchList);
    let task = fx.task("task-1");
    let entry = fx.watch(&id, &task, &format!("{PREFIX}a/-/merge_requests/17"));
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();

    fx.finish(&claimed, exited(1, ""));

    assert_eq!(
        fx.observer_files(&id),
        vec![
            "observer.json".to_string(),
            "observer.lock".to_string(),
            "scripts/1.sh".to_string(),
            format!("watches/{}.json", entry.entry_id),
        ]
    );
    assert!(fx.failures(&id).is_empty());
}

#[test]
fn a_failed_run_writes_one_error_file_that_observer_show_lists() {
    // D15.
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();

    fx.finish(
        &claimed,
        RunReport {
            outcome: RunOutcome::Exited { exit_code: 3 },
            stdout: "partial".into(),
            stderr: "curl: (6) could not resolve host\n".into(),
        },
    );

    assert_eq!(
        fx.observer_files(&id),
        vec![
            "errors/00000001.json",
            "observer.json",
            "observer.lock",
            "scripts/1.sh"
        ]
    );
    let failures = fx.failures(&id);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].sequence, 1);
    assert_eq!(failures[0].script_version, 1);
    assert_eq!(failures[0].status, RunStatus::Failed);
    assert_eq!(failures[0].exit_code, Some(3));
    assert_eq!(failures[0].stdout.as_deref(), Some("partial"));
    let detail = fx.service.observer_detail(&owner(), "jira").unwrap();
    assert_eq!(detail.errors.len(), 1);
    let shown = &detail.errors[0];
    assert_eq!(shown.run_id, claimed.run_id.to_string());
    assert_eq!(shown.status, "failed");
    assert_eq!(shown.exit_code, Some(3));
    assert!(shown.error.contains("code 3"), "{}", shown.error);
    assert_eq!(
        shown.stderr.as_deref(),
        Some("curl: (6) could not resolve host\n")
    );
}

#[test]
fn more_errors_than_the_store_keeps_leave_the_newest() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    let runs = KEPT_RUN_FAILURES as u64 + 3;
    for _ in 0..runs {
        fx.service.observer_pass();
        let claimed = fx.claim().expect("a run after the backoff");
        fx.finish(&claimed, exited(1, ""));
        fx.advance(MAX_RETRY_DELAY_SECS as i64);
        // No error state, so the loop runs on.
        let lock = fx.service.observers().lock(&id).unwrap();
        let mut record = fx.record(&id);
        record.consecutive_failures = 0;
        fx.service
            .observers()
            .save(&lock, &mut record, fx.now())
            .unwrap();
    }

    let sequences: Vec<u64> = fx.failures(&id).iter().map(|f| f.sequence).collect();
    let expected: Vec<u64> = (4..=runs).rev().collect();
    assert_eq!(sequences, expected);
    let files = fx
        .observer_files(&id)
        .into_iter()
        .filter(|file| file.starts_with("errors/"))
        .count();
    assert_eq!(files, KEPT_RUN_FAILURES);
}

#[test]
fn a_failed_run_keeps_the_state_and_backs_off() {
    // Covers AE3, the observer's part (R14, R16).
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let first = fx.claim().unwrap();
    fx.finish(&first, exited(0, r#"{"state": {"cursor": 5}}"#));
    fx.advance(INTERVAL as i64);
    fx.service.observer_pass();
    let second = fx.claim().unwrap();

    let run = fx.finish(&second, exited(1, r#"{"state": {"cursor": 9}}"#));

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(fx.failures(&id)[0].exit_code, Some(1));
    let record = fx.record(&id);
    assert_eq!(record.script_state, Some(json!({"cursor": 5})));
    assert_eq!(record.consecutive_failures, 1);
    assert_eq!(
        record.next_run_at,
        Some(fx.now() + Duration::seconds(2 * INTERVAL as i64))
    );
    assert_eq!(record.last_run.unwrap().status, RunStatus::Failed);
}

#[test]
fn an_exit_code_zero_with_an_invalid_document_keeps_the_state() {
    // R14.
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();

    let run = fx.finish(&claimed, exited(0, "not json"));

    assert_eq!(run.status, RunStatus::InvalidOutput);
    assert!(run.error.is_some());
    let record = fx.record(&id);
    assert_eq!(record.script_state, None);
    assert!(!record.first_run_done);
    assert_eq!(record.consecutive_failures, 1);
}

#[test]
fn five_failures_in_a_row_put_the_observer_in_error_until_its_owner_resumes_it() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    for failure in 1..=MAX_CONSECUTIVE_FAILURES {
        fx.service.observer_pass();
        let claimed = fx.claim().expect("a run after the backoff");
        fx.finish(&claimed, exited(2, ""));
        assert_eq!(fx.record(&id).consecutive_failures, failure);
        fx.advance(MAX_RETRY_DELAY_SECS as i64);
    }

    assert_eq!(fx.record(&id).state, ObserverState::Error);
    fx.advance(10 * MAX_RETRY_DELAY_SECS as i64);
    assert!(fx.service.observer_pass().due.is_empty());
    assert!(fx.claim().is_none());

    let resumed = fx.service.resume_observer(&id).unwrap();
    assert_eq!(resumed.state, ObserverState::Active);
    assert_eq!(resumed.consecutive_failures, 0);
    assert_eq!(fx.service.observer_pass().due, vec![id]);
}

#[test]
fn a_run_without_a_result_by_its_deadline_is_lost_without_counting_a_failure() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    // One failure before, which a lost run neither adds to nor clears.
    fx.service.observer_pass();
    let failed = fx.claim().unwrap();
    fx.finish(&failed, exited(1, ""));
    fx.advance(MAX_RETRY_DELAY_SECS as i64);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();
    assert_eq!(
        fx.service.authenticate_observer_run(claimed.token.expose()),
        Some(claimed.run_id.clone())
    );

    fx.advance((SCRIPT_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS) as i64);
    // Past the deadline the token fails even before the pass runs.
    assert_eq!(
        fx.service.authenticate_observer_run(claimed.token.expose()),
        None
    );
    let pass = fx.service.observer_pass();

    assert_eq!(pass.lost, vec![claimed.run_id.clone()]);
    let failures = fx.failures(&id);
    assert_eq!(failures.len(), 2);
    let lost = &failures[0];
    assert_eq!(lost.sequence, claimed.run_id.sequence);
    assert_eq!(lost.status, RunStatus::Lost);
    assert_eq!(lost.exit_code, None);
    assert!(lost.error.contains("lost by the worker"));
    let shown = fx.service.observer_detail(&owner(), "jira").unwrap();
    assert_eq!(shown.errors[0].status, "lost");
    let record = fx.record(&id);
    assert_eq!(record.consecutive_failures, 1);
    assert_eq!(record.state, ObserverState::Active);
    assert_eq!(record.active_run, None);
    assert_eq!(record.last_run.unwrap().status, RunStatus::Lost);
    // A late result is refused.
    let late = fx
        .service
        .finish_observer_run(&claimed.run_id, exited(0, r#"{"state": {}}"#));
    assert!(matches!(
        late,
        Err(ProtocolError::Conflict {
            code: "observer_run_ended",
            ..
        })
    ));
    assert_eq!(fx.record(&id).script_state, None);
}

#[test]
fn a_claim_records_a_run_past_its_deadline_as_lost() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();
    fx.advance((SCRIPT_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS) as i64);

    assert!(fx.claim().is_none());

    let record = fx.record(&id);
    assert_eq!(record.active_run, None);
    assert_eq!(record.last_run.unwrap().status, RunStatus::Lost);
    let failures = fx.failures(&id);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].sequence, claimed.run_id.sequence);
    assert_eq!(failures[0].status, RunStatus::Lost);
}

#[test]
fn a_result_after_the_deadline_records_the_run_as_lost() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();
    fx.advance((SCRIPT_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS) as i64);

    let late = fx
        .service
        .finish_observer_run(&claimed.run_id, exited(0, r#"{"state": {}}"#));

    assert!(matches!(
        late,
        Err(ProtocolError::Conflict {
            code: "observer_run_lost",
            ..
        })
    ));
    assert_eq!(fx.record(&id).last_run.unwrap().status, RunStatus::Lost);
    assert_eq!(fx.failures(&id)[0].status, RunStatus::Lost);
}

#[test]
fn the_token_works_only_for_its_run_while_it_runs() {
    let fx = Fixture::new();
    fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();
    let token = claimed.token.expose().to_string();
    let (prefix, _secret) = token.rsplit_once('.').unwrap();

    assert!(fx.service.authenticate_observer_run(&token).is_some());
    assert!(fx
        .service
        .authenticate_observer_run(&format!("{prefix}.{}", "0".repeat(64)))
        .is_none());
    assert!(fx
        .service
        .authenticate_observer_run(&token.replacen("nso.", "nsr.", 1))
        .is_none());

    fx.finish(&claimed, exited(0, r#"{"state": {}}"#));
    assert!(fx.service.authenticate_observer_run(&token).is_none());
}

#[test]
fn a_run_waiting_for_a_claim_longer_than_an_interval_shows_a_missing_worker() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    assert!(!fx.record(&id).worker_missing(fx.now()));

    fx.advance(INTERVAL as i64 + 1);

    assert!(fx.record(&id).worker_missing(fx.now()));
    fx.claim().unwrap();
    assert!(!fx.record(&id).worker_missing(fx.now()));
}

#[test]
fn approving_a_new_version_of_a_paused_observer_keeps_it_paused() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.pause_observer(&id).unwrap();
    fx.service
        .observers()
        .add_script(&id, "echo v2\n", fx.now())
        .unwrap();

    let record = fx.service.approve_observer_version(&id, 2).unwrap();

    assert_eq!(record.state, ObserverState::Paused);
    assert_eq!(record.approved_version, Some(2));
    assert!(fx.service.observer_pass().due.is_empty());
}

#[test]
fn approving_a_draft_activates_it_and_an_unknown_version_is_refused() {
    let fx = Fixture::new();
    let id = fx.draft("jira", WatchMode::All);
    assert!(fx.service.observer_pass().due.is_empty());

    assert!(matches!(
        fx.service.approve_observer_version(&id, 7),
        Err(ProtocolError::BadRequest { .. })
    ));
    let record = fx.service.approve_observer_version(&id, 1).unwrap();

    assert_eq!(record.state, ObserverState::Active);
    assert_eq!(record.next_run_at, Some(fx.now()));
    assert_eq!(fx.service.observer_pass().due, vec![id]);
}

#[test]
fn each_watch_run_checks_one_entry_with_its_tasks_wait_episode() {
    // D18: one entry per run; the round takes the entries in order.
    let fx = Fixture::new();
    let id = fx.active("mr-review", WatchMode::WatchList);
    let waiting = fx.task("task-waiting");
    let idle = fx.task("task-idle");
    fx.watch(&id, &waiting, &format!("{PREFIX}a/-/merge_requests/17"));
    fx.advance(1);
    fx.watch(&id, &idle, &format!("{PREFIX}a/-/merge_requests/18"));
    let control = fx.service.control_plane();
    {
        let _lock = control.store().lock_task(&waiting).unwrap();
        let mut record = control.task(&waiting).unwrap();
        record.event_wait = Some(EventWait {
            episode_id: "episode-3".into(),
            event_type: "approved".into(),
            filter: json!({"mr": "17"}),
            since: fx.now(),
            watch: None,
        });
        control.store().save_task(&mut record).unwrap();
    }

    fx.service.observer_pass();
    let first = fx.claim().unwrap();
    assert!(fx.claim().is_none(), "one run at a time");
    fx.finish(&first, exited(1, ""));
    let second = fx.claim().unwrap();
    fx.finish(&second, exited(1, ""));

    let episodes: Vec<_> = [&first, &second]
        .iter()
        .map(|claimed| {
            let watch = claimed.input.watch.as_ref().unwrap();
            (watch.task_id.as_str(), watch.wait_episode.as_deref())
        })
        .collect();
    assert_eq!(
        episodes,
        vec![("task-waiting", Some("episode-3")), ("task-idle", None)]
    );
    assert!(fx.claim().is_none(), "the round is over");
    assert_eq!(fx.service.observers().list_watches(&id).unwrap().len(), 2);
    assert_eq!(
        fx.record(&id).next_run_at,
        Some(fx.now() + Duration::seconds(INTERVAL as i64))
    );
}

#[test]
fn a_watch_run_exit_code_decides_whether_its_entry_stays() {
    // D18: 0 removes the entry, 1 keeps it, another code is an error that
    // keeps it and replaces the object's older error.
    let fx = Fixture::new();
    let id = fx.active("mr-review", WatchMode::WatchList);
    let task = fx.task("task-1");
    let merged = format!("{PREFIX}a/-/merge_requests/17");
    let open = format!("{PREFIX}a/-/merge_requests/18");
    fx.watch(&id, &task, &merged);
    fx.advance(1);
    fx.watch(&id, &task, &open);
    fx.service.observer_pass();

    let first = fx.claim().unwrap();
    assert_eq!(first.input.watch.as_ref().unwrap().object, merged);
    assert_eq!(
        fx.finish(&first, exited(0, "")).status,
        RunStatus::Succeeded
    );
    let second = fx.claim().unwrap();
    assert_eq!(second.input.watch.as_ref().unwrap().object, open);
    assert_eq!(fx.finish(&second, exited(3, "")).status, RunStatus::Failed);

    let objects: Vec<String> = fx
        .service
        .observers()
        .list_watches(&id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.object)
        .collect();
    assert_eq!(objects, vec![open.clone()]);
    let record = fx.record(&id);
    assert_eq!(record.consecutive_failures, 0, "no backoff for a watch run");
    assert_eq!(record.state, ObserverState::Active);
    assert_eq!(
        record.next_run_at,
        Some(fx.now() + Duration::seconds(INTERVAL as i64))
    );
    let errors = fx.failures(&id);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].watch_object.as_deref(), Some(open.as_str()));

    // The next round checks the entry again; a new error replaces the old.
    fx.advance(INTERVAL as i64);
    fx.service.observer_pass();
    let third = fx.claim().unwrap();
    fx.finish(&third, exited(2, ""));
    let errors = fx.failures(&id);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].sequence, third.run_id.sequence);
    assert_eq!(errors[0].exit_code, Some(2));

    fx.advance(INTERVAL as i64);
    fx.service.observer_pass();
    let fourth = fx.claim().unwrap();
    fx.finish(&fourth, exited(1, ""));
    assert_eq!(fx.service.observers().list_watches(&id).unwrap().len(), 1);
}

#[test]
fn a_round_skips_an_entry_removed_before_its_run() {
    let fx = Fixture::new();
    let id = fx.active("mr-review", WatchMode::WatchList);
    let task = fx.task("task-1");
    let gone = format!("{PREFIX}a/-/merge_requests/17");
    fx.watch(&id, &task, &gone);
    fx.service.observer_pass();
    let lock = fx.service.observers().lock(&id).unwrap();
    for entry in fx.service.observers().list_watches(&id).unwrap() {
        fx.service
            .observers()
            .remove_watch(&lock, &entry.entry_id)
            .unwrap();
    }
    drop(lock);

    assert!(fx.claim().is_none());
    let record = fx.record(&id);
    assert!(record.watch_round.is_empty());
    assert_eq!(record.run_due_since, None);
}

#[test]
fn the_pass_removes_the_entries_of_archived_tasks() {
    // R7.
    let fx = Fixture::new();
    let id = fx.active("mr-review", WatchMode::WatchList);
    let archived = fx.task("task-archived");
    let live = fx.task("task-live");
    fx.watch(&id, &archived, &format!("{PREFIX}a/-/merge_requests/1"));
    fx.watch(&id, &live, &format!("{PREFIX}a/-/merge_requests/2"));
    fx.service
        .control_plane()
        .command(
            &archived,
            CommandRequest::new(OwnerCommand::Archive, "archive-1"),
        )
        .unwrap();

    let pass = fx.service.observer_pass();

    assert_eq!(pass.watches_removed, 1);
    assert_eq!(pass.due, vec![id.clone()]);
    let left = fx.service.observers().list_watches(&id).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].task_id, live);
}

#[test]
fn a_pass_with_nothing_due_writes_nothing() {
    let fx = Fixture::new();
    let id = fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();
    fx.finish(&claimed, exited(0, r#"{"state": {}}"#));
    let draft = fx.draft("draft", WatchMode::All);
    let revisions = (fx.record(&id).revision, fx.record(&draft).revision);

    let pass = fx.service.observer_pass();

    assert_eq!(pass, ObserverPass::default());
    assert_eq!(
        (fx.record(&id).revision, fx.record(&draft).revision),
        revisions
    );
}

#[test]
fn the_pass_and_the_claim_skip_unreadable_records() {
    // D7.
    let fx = Fixture::new();
    let corrupt = fx.active("broken", WatchMode::All);
    let newer = fx.active("newer", WatchMode::All);
    let healthy = fx.active("jira", WatchMode::All);
    let paths = fx.service.observers().paths().clone();
    std::fs::write(paths.observer_record_file(corrupt.as_str()), "{ not json").unwrap();
    let newer_file = paths.observer_record_file(newer.as_str());
    let mut stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&newer_file).unwrap()).unwrap();
    stored["record_version"] = json!(OBSERVER_RECORD_VERSION + 1);
    std::fs::write(&newer_file, stored.to_string()).unwrap();

    let pass = fx.service.observer_pass();

    assert_eq!(pass.due, vec![healthy.clone()]);
    let claimed = fx.claim().expect("the healthy observer's run");
    assert_eq!(claimed.run_id.observer_id, healthy);
    // The owner's views keep the strict listing.
    assert!(fx.service.observers().list().is_err());
}

#[test]
fn pause_and_resume_refuse_a_draft() {
    let fx = Fixture::new();
    let id = fx.draft("jira", WatchMode::All);

    for result in [
        fx.service.pause_observer(&id),
        fx.service.resume_observer(&id),
    ] {
        assert!(matches!(
            result,
            Err(ProtocolError::Conflict {
                code: "observer_not_approved",
                ..
            })
        ));
    }
}

#[test]
fn a_worker_outcome_from_a_newer_build_reads_as_a_worker_error() {
    let report: RunReport =
        serde_json::from_value(json!({"outcome": {"kind": "oom_killed"}})).unwrap();
    assert_eq!(report.outcome, RunOutcome::Unrecognized);
    let (status, ..) = schedule_judge(&report);
    assert_eq!(status, RunStatus::WorkerError);
}

fn schedule_judge(report: &RunReport) -> (RunStatus, Option<i32>) {
    let fx = Fixture::new();
    fx.active("jira", WatchMode::All);
    fx.service.observer_pass();
    let claimed = fx.claim().unwrap();
    let run = fx.finish(&claimed, report.clone());
    let exit_code = fx
        .failures(&claimed.run_id.observer_id)
        .first()
        .and_then(|failure| failure.exit_code);
    (run.status, exit_code)
}
