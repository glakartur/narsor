//! Integration tests of the observers' schedule (U2) through the daemon's
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
    ObserverConfig, ObserverEffect, ObserverId, ObserverName, ObserverState, RunStatus, WatchEntry,
    WatchMode, WatchObjectPolicy, WatchOrigin,
};
use narsor_engine::orchestration::{
    Clock, CommandRequest, ControlPlane, NewTask, OwnerCommand, OwnerRef,
};
use narsor_engine::persistence::StatePaths;
use narsor_server::runtime::run_periodic_pass;
use serde_json::json;

const PREFIX: &str = "https://git.example.com/";

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

    /// An active watch-list observer with approved script version 1.
    fn active_watch_list(&self, name: &str) -> ObserverId {
        let mut config = ObserverConfig::new(
            ObserverName::parse(name).unwrap(),
            "gitlab",
            ObserverEffect::DeliverEvent,
        );
        config.watch_mode = WatchMode::WatchList;
        config.watch_objects = WatchObjectPolicy {
            allowed_prefixes: vec![PREFIX.into()],
            ..WatchObjectPolicy::default()
        };
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

    fn watch(&self, id: &ObserverId, task_id: &TaskId, object: &str) {
        let entry = WatchEntry::new(
            task_id.clone(),
            None,
            object,
            vec!["approved".into()],
            WatchOrigin::Operator,
            self.now(),
        );
        let store = self.service.observers();
        let lock = store.lock(id).unwrap();
        store.create_watch(&lock, &entry).unwrap();
    }
}

#[test]
fn the_periodic_pass_runs_the_observers_schedule() {
    // Covers AE1 through the daemon's tick: an empty watch list starts no
    // run, and its deadline moves on.
    let fx = Fixture::new();
    let id = fx.active_watch_list("mr-review");

    run_periodic_pass(&fx.service);

    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.run_due_since, None);
    assert_eq!(record.next_run_at, Some(fx.now() + Duration::seconds(300)));
    assert!(fx.service.claim_observer_run().unwrap().is_none());
    assert_eq!(record.last_run_sequence, 0);

    // With an entry, the next tick after the interval flags the run.
    let task = fx.task("task-1");
    fx.watch(&id, &task, &format!("{PREFIX}a/-/merge_requests/17"));
    fx.advance(Duration::seconds(300));
    run_periodic_pass(&fx.service);
    assert_eq!(
        fx.service.observers().load(&id).unwrap().run_due_since,
        Some(fx.now())
    );
}

#[test]
fn the_pass_drops_the_entry_of_an_archived_task() {
    // R7: the task an entry delivers to was archived.
    let fx = Fixture::new();
    let id = fx.active_watch_list("mr-review");
    let task = fx.task("task-1");
    fx.watch(&id, &task, &format!("{PREFIX}a/-/merge_requests/17"));
    fx.service
        .control_plane()
        .command(&task, CommandRequest::new(OwnerCommand::Archive, "archive"))
        .unwrap();

    run_periodic_pass(&fx.service);

    assert!(fx.service.observers().list_watches(&id).unwrap().is_empty());
    assert!(fx.service.claim_observer_run().unwrap().is_none());
}

#[test]
fn a_restart_after_hours_of_downtime_owes_one_run_and_keeps_the_count() {
    let fx = Fixture::new();
    let id = fx.active_watch_list("mr-review");
    let task = fx.task("task-1");
    fx.watch(&id, &task, &format!("{PREFIX}a/-/merge_requests/17"));
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
    assert_eq!(record.consecutive_failures, 1);
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
