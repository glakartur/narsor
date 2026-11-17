//! Integration tests of the observers' schedule through the daemon's
//! periodic pass, without a daemon: `run_periodic_pass` on a real
//! `ProtocolService` over a temporary state root and a manual clock.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::controller_protocol::{ProtocolService, RunOutcome, RunReport};
use narsor_engine::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace,
};
use narsor_engine::observers::{
    ObserverConfig, ObserverEffect, ObserverId, ObserverMode, ObserverName, ObserverState,
    QueueEntry, RunStatus, ScheduleConfig,
};
use narsor_engine::orchestration::{
    Clock, CommandRequest, ControlPlane, NewTask, OwnerCommand, OwnerRef,
};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::controller::GateWatch;
use narsor_server::runtime::run_periodic_pass;
use serde_json::json;

/// A clock the test moves by hand.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// A state root, a clock, and a service over both.
struct Fixture {
    root: PathBuf,
    clock: Arc<ManualClock>,
    service: ProtocolService,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("narsor-obs-schedule-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap(),
        )));
        let service = Self::service_on(&root, &clock);
        Self {
            root,
            clock,
            service,
        }
    }

    /// A fresh service on the same state root: what a restart sees.
    fn service_on(root: &PathBuf, clock: &Arc<ManualClock>) -> ProtocolService {
        ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(root), clock.clone()),
            crate::environment_support::seeded_resolver(root),
        )
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    /// An active `on_episodes` observer of the queue of its name, with
    /// approved script version 1.
    fn active_on_episodes(&self, name: &str) -> ObserverId {
        self.active(name, ObserverMode::OnEpisodes, ScheduleConfig::default())
    }

    /// An active observer of the queue of its name in `mode` on `schedule`,
    /// with approved script version 1. It creates no tasks.
    fn active(&self, name: &str, mode: ObserverMode, schedule: ScheduleConfig) -> ObserverId {
        let mut config = ObserverConfig::new(
            ObserverName::parse(name).unwrap(),
            "gitlab",
            ObserverEffect::None,
            mode,
        );
        config.schedule = schedule;
        let store = self.service.observers();
        let id = store
            .create(OwnerRef::new("owner"), config, None, self.now())
            .unwrap()
            .observer_id;
        store
            .add_script(&id, "echo '{\"state\": {}}'\n", self.now())
            .unwrap();
        self.service.approve_observer_version(&id, 1).unwrap();
        id
    }

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
                OwnerRef::new("owner"),
                BackendId::parse("k8s").unwrap(),
                spec,
            ))
            .unwrap();
        task_id
    }

    /// Puts the wait episode `episode` of `task_id` on `queue`.
    fn entry(&self, queue: &str, task_id: &TaskId, episode: &str) {
        let gate = GateWatch {
            step: "review_work".into(),
            queue: queue.into(),
            params: Default::default(),
            choices: Vec::new(),
            variables: Vec::new(),
        };
        let entry = QueueEntry::from_gate(
            OwnerRef::new("owner"),
            task_id.clone(),
            episode,
            &gate,
            self.now(),
        );
        let store = self.service.observers();
        let lock = store.lock_queue_entries().unwrap();
        store.add_queue_entry(&lock, &entry).unwrap();
    }
}

#[test]
fn the_periodic_pass_runs_the_observers_schedule() {
    // R30 through the daemon's tick: an empty queue starts no run of an
    // `on_episodes` observer, and its deadline moves on.
    let fx = Fixture::new();
    let id = fx.active_on_episodes("mr-review");

    run_periodic_pass(&fx.service);

    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.run_due_since, None);
    assert_eq!(record.next_run_at, Some(fx.now() + Duration::seconds(300)));
    assert!(fx.service.claim_observer_run().unwrap().is_none());
    assert_eq!(record.last_run_sequence, 0);

    // With an entry, the next tick after the interval flags the run.
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    fx.advance(Duration::seconds(300));
    run_periodic_pass(&fx.service);
    assert_eq!(
        fx.service.observers().load(&id).unwrap().run_due_since,
        Some(fx.now())
    );
}

#[test]
fn the_pass_drops_the_entry_of_an_archived_task() {
    // R19: the task of an entry was archived.
    let fx = Fixture::new();
    fx.active_on_episodes("mr-review");
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    fx.service
        .control_plane()
        .command(&task, CommandRequest::new(OwnerCommand::Archive, "archive"))
        .unwrap();

    run_periodic_pass(&fx.service);

    assert!(fx
        .service
        .observers()
        .queue_entries(&OwnerRef::new("owner"), "mr-review")
        .unwrap()
        .is_empty());
    assert!(fx.service.claim_observer_run().unwrap().is_none());
}

#[test]
fn a_restart_after_hours_of_downtime_owes_one_run_and_keeps_the_error() {
    let fx = Fixture::new();
    let id = fx.active_on_episodes("mr-review");
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    run_periodic_pass(&fx.service);
    let claimed = fx.service.claim_observer_run().unwrap().unwrap();
    fx.service
        .finish_observer_run(
            &claimed.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 3 },
                stdout: String::new(),
                stderr: "source unavailable".into(),
            },
        )
        .unwrap();

    fx.advance(Duration::hours(2));
    let restarted = Fixture::service_on(&fx.root, &fx.clock);
    run_periodic_pass(&restarted);
    run_periodic_pass(&restarted);

    let record = restarted.observers().load(&id).unwrap();
    // A failed run for an entry is not counted (R14): the entry is checked
    // again.
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(record.state, ObserverState::Active);
    let run = restarted.claim_observer_run().unwrap().expect("one run");
    assert_eq!(run.run_id.sequence, 2);
    assert!(restarted.claim_observer_run().unwrap().is_none());
    let errors = restarted.observers().list_failures(&id).unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].sequence, 1);
    assert_eq!(errors[0].status, RunStatus::Failed);
    assert_eq!(errors[0].stderr.as_deref(), Some("source unavailable"));
}

#[test]
fn an_old_record_with_only_an_interval_runs_unchanged() {
    // KTD-2: a record written before cron existed keeps its interval.
    let fx = Fixture::new();
    let id = fx.active("jira-new", ObserverMode::Always, ScheduleConfig::default());
    let path = fx
        .service
        .observers()
        .paths()
        .observer_record_file(id.as_str());
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["record_version"] = json!(1);
    raw["config"]["schedule"] = json!({"interval_secs": 600});
    raw["config"].as_object_mut().unwrap().remove("queue");
    raw["config"].as_object_mut().unwrap().remove("mode");
    raw["config"]["watch_mode"] = json!("all");
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

    run_periodic_pass(&fx.service);
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    fx.service
        .finish_observer_run(
            &claimed.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 0 },
                stdout: "{\"state\": {}}".into(),
                stderr: String::new(),
            },
        )
        .unwrap();

    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(
        record.config.schedule,
        ScheduleConfig::Interval { secs: 600 }
    );
    assert_eq!(record.config.queue, "jira-new");
    assert_eq!(record.next_run_at, Some(fx.now() + Duration::seconds(600)));
}

#[test]
fn a_cron_observer_runs_at_every_fire_time() {
    // Covers AE6 for the schedule: `*/10 * * * *` from 12:00 runs at 12:10
    // and 12:20, never in between.
    let fx = Fixture::new();
    let cron = ScheduleConfig::from_parts(None, Some("*/10 * * * *".into())).unwrap();
    let id = fx.active("jira-new", ObserverMode::Always, cron);
    let at = |minute| Utc.with_ymd_and_hms(2026, 11, 13, 12, minute, 0).unwrap();
    let mut starts = Vec::new();
    for _ in 0..30 {
        run_periodic_pass(&fx.service);
        if let Some(claimed) = fx.service.claim_observer_run().unwrap() {
            starts.push(fx.now());
            fx.service
                .finish_observer_run(
                    &claimed.run_id,
                    RunReport {
                        outcome: RunOutcome::Exited { exit_code: 0 },
                        stdout: "{\"state\": {}}".into(),
                        stderr: String::new(),
                    },
                )
                .unwrap();
        }
        fx.advance(Duration::seconds(60));
    }

    // The approval makes the first run due at once.
    assert_eq!(starts, vec![at(0), at(10), at(20)]);
    assert_eq!(
        fx.service.observers().load(&id).unwrap().next_run_at,
        Some(at(30))
    );
}
