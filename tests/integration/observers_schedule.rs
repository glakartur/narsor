//! Integration tests of the observers' schedule through the daemon's
//! periodic pass, without a daemon: `run_periodic_pass` on a real
//! `ProtocolService` over a temporary state root and a manual clock.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::controller_protocol::{ProtocolService, RunOutcome, RunReport};
use narsor_engine::domain::TaskId;
use narsor_engine::observers::migrate::DELIVER_EVENT_REMOVED_NOTE;
use narsor_engine::observers::{
    queue_entry_id, ObserverConfig, ObserverEffect, ObserverId, ObserverMode, ObserverName,
    ObserverState, RunStatus, ScheduleConfig,
};
use narsor_engine::orchestration::{Clock, CommandRequest, ControlPlane, OwnerCommand, OwnerRef};
use narsor_engine::persistence::StatePaths;
use narsor_server::runtime::run_periodic_pass;
use serde_json::json;

use crate::gate_support::{queued_task, review_gate, wait_at};

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
        queued_task(&self.service, id, &OwnerRef::new("owner"))
    }

    /// Makes `task_id` wait at a gate on `queue` in `episode`: its runner's
    /// snapshot puts the episode on the queue.
    fn entry(&self, queue: &str, task_id: &TaskId, episode: &str) {
        wait_at(&self.service, task_id, episode, &review_gate(queue));
    }

    /// Runs the one due entry run and reports `exit_code` for it.
    fn run_entry(&self, exit_code: i32) -> narsor_engine::controller_protocol::ClaimedObserverRun {
        let claimed = self.service.claim_observer_run().unwrap().expect("a run");
        self.service
            .finish_observer_run(
                &claimed.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code },
                    stdout: String::new(),
                    stderr: format!("exit {exit_code}"),
                },
            )
            .unwrap();
        claimed
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
fn an_old_deliver_event_observer_is_paused_with_its_note_and_not_run() {
    // KTD-8, U13: an active version 1 observer that delivered events reads
    // as a paused `on_episodes` observer of the queue of its name. It keeps
    // its scripts, and a gate entry on its queue starts no run.
    let fx = Fixture::new();
    let id = fx.active_on_episodes("mr-review");
    let path = fx
        .service
        .observers()
        .paths()
        .observer_record_file(id.as_str());
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["record_version"] = json!(1);
    raw["config"]["effect"] = json!({"kind": "deliver_event"});
    raw["config"]["watch_mode"] = json!("watch_list");
    raw["config"]["watch_objects"] =
        json!({"allowed_prefixes": ["https://gitlab/"], "max_len": 512});
    raw["config"].as_object_mut().unwrap().remove("queue");
    raw["config"].as_object_mut().unwrap().remove("mode");
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    for _ in 0..3 {
        run_periodic_pass(&fx.service);
        assert!(fx.service.claim_observer_run().unwrap().is_none());
        fx.advance(Duration::seconds(300));
    }

    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.state, ObserverState::Paused);
    assert_eq!(record.note.as_deref(), Some(DELIVER_EVENT_REMOVED_NOTE));
    assert_eq!(record.config.effect, ObserverEffect::None);
    assert_eq!(record.config.queue, "mr-review");
    assert_eq!(record.config.mode, ObserverMode::OnEpisodes);
    assert_eq!(record.approved_version, Some(1));
    assert_eq!(record.last_run_sequence, 0);
    assert_eq!(fx.service.observers().list().unwrap().len(), 1);
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

#[test]
fn only_exit_code_zero_ends_an_entry_run_without_an_error() {
    // R14: 0 is "answered or nothing to report"; 1 is no longer "check
    // again" but an error, like 2. Errors are recorded per entry, the
    // observer stays active and counts no failure, and the entry stays.
    let fx = Fixture::new();
    let id = fx.active_on_episodes("mr-review");
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    let entry_id = queue_entry_id(&task, "input-1");

    for (round, exit_code) in [0, 1, 2].into_iter().enumerate() {
        if round > 0 {
            fx.advance(Duration::seconds(300));
        }
        run_periodic_pass(&fx.service);
        fx.run_entry(exit_code);

        let record = fx.service.observers().load(&id).unwrap();
        assert_eq!(record.state, ObserverState::Active, "exit {exit_code}");
        assert_eq!(record.consecutive_failures, 0, "exit {exit_code}");
        let expected = if exit_code == 0 {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        };
        assert_eq!(
            record.last_run.unwrap().status,
            expected,
            "exit {exit_code}"
        );
        assert!(fx
            .service
            .observers()
            .queue_entry(&entry_id)
            .unwrap()
            .is_some());
    }
    // The newest error of the entry replaced the older one.
    let errors = fx.service.observers().list_failures(&id).unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].exit_code, Some(2));
    assert_eq!(errors[0].entry_id.as_deref(), Some(entry_id.as_str()));
    assert_eq!(errors[0].error, "the script exited with code 2");
}

#[test]
fn an_entry_run_gets_the_whole_gate_in_contract_version_two() {
    // R11: task, episode and its start, step, params, choices, variables.
    let fx = Fixture::new();
    fx.active_on_episodes("mr-review");
    let task = fx.task("task-1");
    let started = fx.now();
    fx.entry("mr-review", &task, "input-1");
    fx.advance(Duration::seconds(30));
    run_periodic_pass(&fx.service);

    let claimed = fx.run_entry(0);

    let input = serde_json::to_value(&claimed.input).unwrap();
    assert_eq!(input["contract_version"], json!(2));
    let gate = review_gate("mr-review");
    assert_eq!(
        input["entry"],
        json!({
            "task_id": "task-1",
            "episode": "input-1",
            "episode_started_at": started,
            "step": "review_work",
            "params": gate.params,
            "choices": [
                {"name": "check_feedback", "description": "Read the reviewer's feedback", "requires_variables": []},
                {"name": "revise_plan", "description": null, "requires_variables": ["note"]},
            ],
            "variables": [
                {"name": "note", "type": "text", "description": "What to change"},
            ],
        })
    );
}

// ---------------------------------------------------------------------------
// Run modes and ticks (U8, KTD-3)
// ---------------------------------------------------------------------------

impl Fixture {
    /// 12:MM:SS of the fixture's day.
    fn at(&self, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, minute, second)
            .unwrap()
    }

    fn set(&self, to: DateTime<Utc>) {
        *self.clock.0.lock().unwrap() = to;
    }

    /// Reports `exit_code` for `run`; exit 0 with a valid output document.
    fn finish(&self, run: &narsor_engine::controller_protocol::ClaimedObserverRun, exit_code: i32) {
        self.service
            .finish_observer_run(
                &run.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code },
                    stdout: "{\"state\": {}}".into(),
                    stderr: String::new(),
                },
            )
            .unwrap();
    }

    fn claim(&self) -> Option<narsor_engine::controller_protocol::ClaimedObserverRun> {
        self.service.claim_observer_run().unwrap()
    }
}

#[test]
fn an_on_episodes_observer_runs_only_at_a_tick_with_an_entry() {
    // Covers AE7: cron `*/5`; at the first tick the queue is empty, no run
    // and no error; at the next tick one entry, one run for it.
    let fx = Fixture::new();
    let cron = ScheduleConfig::from_parts(None, Some("*/5 * * * *".into())).unwrap();
    let id = fx.active("gitlab-mr", ObserverMode::OnEpisodes, cron);

    let empty = fx.service.observer_pass();
    assert_eq!(empty.skipped, vec![id.clone()]);
    assert!(fx.claim().is_none());
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.last_run, None);
    assert_eq!(record.next_run_at, Some(fx.at(5, 0)));

    fx.set(fx.at(2, 0));
    let task = fx.task("task-1");
    fx.entry("gitlab-mr", &task, "input-1");
    run_periodic_pass(&fx.service);
    assert!(fx.claim().is_none(), "no tick yet");

    fx.set(fx.at(5, 0));
    run_periodic_pass(&fx.service);
    let run = fx.claim().expect("one run at the tick");
    assert_eq!(run.input.entry.as_ref().unwrap().task_id, task);
    assert!(fx.claim().is_none());
}

#[test]
fn an_always_observer_runs_once_at_every_tick_with_no_entry() {
    // Covers AE6: no task exists; every tick runs the script once, with no
    // entry in its input.
    let fx = Fixture::new();
    let cron = ScheduleConfig::from_parts(None, Some("*/10 * * * *".into())).unwrap();
    fx.active("jira-new", ObserverMode::Always, cron);

    let mut runs = 0;
    for minute in [0, 3, 10, 10, 15, 20] {
        fx.set(fx.at(minute, 0));
        run_periodic_pass(&fx.service);
        while let Some(run) = fx.claim() {
            assert!(run.input.entry.is_none());
            runs += 1;
            fx.finish(&run, 0);
        }
    }

    assert_eq!(runs, 3, "12:00, 12:10 and 12:20");
}

#[test]
fn the_next_tick_counts_from_the_last_tick_not_from_the_end_of_the_run() {
    let fx = Fixture::new();
    let id = fx.active("jira-new", ObserverMode::Always, ScheduleConfig::default());
    run_periodic_pass(&fx.service);
    let run = fx.claim().expect("the approval's tick");

    fx.set(fx.at(3, 0));
    fx.finish(&run, 0);

    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.next_run_at, Some(fx.at(5, 0)));
}

#[test]
fn a_tick_while_the_run_is_active_starts_nothing_and_is_not_owed() {
    // R29: one run at a time; the tick is skipped, not queued.
    let fx = Fixture::new();
    let id = fx.active("jira-new", ObserverMode::Always, ScheduleConfig::default());
    run_periodic_pass(&fx.service);
    let run = fx.claim().expect("the approval's tick");

    fx.set(fx.at(5, 0));
    let pass = fx.service.observer_pass();
    assert_eq!(pass.busy, vec![id.clone()]);
    assert!(pass.due.is_empty());
    assert!(fx.claim().is_none());
    assert_eq!(
        fx.service.observers().load(&id).unwrap().next_run_at,
        Some(fx.at(10, 0))
    );

    // Within the run's deadline (the script's limit and a minute).
    fx.set(fx.at(5, 40));
    fx.finish(&run, 0);
    run_periodic_pass(&fx.service);
    assert!(fx.claim().is_none(), "the 12:05 tick is not owed");

    fx.set(fx.at(10, 0));
    run_periodic_pass(&fx.service);
    assert!(fx.claim().is_some());
}

#[test]
fn a_tick_the_run_outlasted_without_a_pass_is_not_owed_either() {
    let fx = Fixture::new();
    let id = fx.active("jira-new", ObserverMode::Always, ScheduleConfig::default());
    run_periodic_pass(&fx.service);
    let run = fx.claim().expect("the approval's tick");

    fx.set(fx.at(5, 30));
    fx.finish(&run, 0);
    run_periodic_pass(&fx.service);

    assert!(fx.claim().is_none());
    assert_eq!(
        fx.service.observers().load(&id).unwrap().next_run_at,
        Some(fx.at(10, 0))
    );
}

#[test]
fn a_tick_during_the_backoff_starts_nothing_and_keeps_the_backoff() {
    // R29: a failed `always` run waits `interval × 2^failures`; the tick in
    // between neither runs nor moves the backoff.
    let fx = Fixture::new();
    let id = fx.active("jira-new", ObserverMode::Always, ScheduleConfig::default());
    run_periodic_pass(&fx.service);
    let run = fx.claim().expect("the approval's tick");
    fx.finish(&run, 1);
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.consecutive_failures, 1);
    assert_eq!(record.next_run_at, Some(fx.at(10, 0)));

    fx.set(fx.at(5, 0));
    let pass = fx.service.observer_pass();

    assert!(pass.due.is_empty() && pass.busy.is_empty());
    assert!(fx.claim().is_none());
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.next_run_at, Some(fx.at(10, 0)));
    assert_eq!(record.consecutive_failures, 1);

    fx.set(fx.at(10, 0));
    run_periodic_pass(&fx.service);
    assert!(fx.claim().is_some());
}

#[test]
fn three_entries_at_one_tick_run_one_per_run_over_the_round() {
    // R30: one entry per run, never three at once; a tick while the round
    // goes on is skipped.
    let fx = Fixture::new();
    let id = fx.active_on_episodes("mr-review");
    for (task, episode) in [
        ("task-1", "input-1"),
        ("task-2", "input-2"),
        ("task-3", "input-3"),
    ] {
        let task = fx.task(task);
        fx.entry("mr-review", &task, episode);
    }
    run_periodic_pass(&fx.service);

    let first = fx.claim().expect("the round's first entry");
    assert!(fx.claim().is_none(), "one run at a time");
    fx.finish(&first, 0);
    let second = fx.claim().expect("the round's second entry");
    fx.set(fx.at(5, 0));
    let pass = fx.service.observer_pass();
    assert_eq!(pass.busy, vec![id.clone()]);
    fx.finish(&second, 0);
    let third = fx.claim().expect("the round's third entry");
    fx.finish(&third, 0);
    assert!(
        fx.claim().is_none(),
        "the round is over; the 12:05 tick is not owed"
    );

    let mut episodes: Vec<String> = [&first, &second, &third]
        .iter()
        .map(|run| run.input.entry.as_ref().unwrap().episode.clone())
        .collect();
    episodes.sort();
    assert_eq!(episodes, vec!["input-1", "input-2", "input-3"]);
    assert_eq!(
        fx.service.observers().load(&id).unwrap().next_run_at,
        Some(fx.at(10, 0))
    );
}

// ---------------------------------------------------------------------------
// An entry the control plane lost (U4's gap)
// ---------------------------------------------------------------------------

#[test]
fn the_tick_adds_back_the_entry_of_a_waiting_task_once_and_warns_nothing() {
    // A crash between the task's record and its entry: the task waits in
    // its watched episode with no entry.
    let fx = Fixture::new();
    fx.active_on_episodes("mr-review");
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    let entry_id = queue_entry_id(&task, "input-1");
    let store = fx.service.observers();
    let lock = store.lock_queue_entries().unwrap();
    assert!(store.remove_queue_entry(&lock, &entry_id).unwrap());
    drop(lock);
    let events_before = fx
        .service
        .control_plane()
        .events_since(&task, 0)
        .unwrap()
        .len();

    let pass = fx.service.observer_pass();

    assert_eq!(pass.entries_restored, 1);
    assert!(store.queue_entry(&entry_id).unwrap().is_some());
    let run = fx.claim().expect("a run for the entry");
    assert_eq!(run.input.entry.as_ref().unwrap().episode, "input-1");
    fx.finish(&run, 0);
    assert_eq!(
        fx.service
            .control_plane()
            .events_since(&task, 0)
            .unwrap()
            .len(),
        events_before,
        "no warning, nothing in the task's history"
    );

    fx.advance(Duration::seconds(300));
    let again = fx.service.observer_pass();
    assert_eq!(again.entries_restored, 0);
}

#[test]
fn no_entry_comes_back_for_an_episode_that_ended() {
    let fx = Fixture::new();
    fx.active_on_episodes("mr-review");
    let task = fx.task("task-1");
    fx.entry("mr-review", &task, "input-1");
    crate::gate_support::past_gates(&fx.service, &task);

    let pass = fx.service.observer_pass();

    assert_eq!(pass.entries_restored, 0);
    assert!(fx
        .service
        .observers()
        .queue_entries(&OwnerRef::new("owner"), "mr-review")
        .unwrap()
        .is_empty());
}
