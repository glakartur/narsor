//! Integration tests of the lifecycle of observer queue entries (U4): the
//! runner's snapshot adds the entry of a watched gate's episode and removes
//! it at every exit (R16), pause and restart change nothing (R17), a queue
//! no active `on_episodes` observer serves gets one warning per episode
//! (R9, R27), the schedule's guard drops an entry whose episode is gone
//! (R18, R19), and the task summary marks the episode on a queue (R21).
//!
//! A real `ProtocolService` over a temporary state root and a manual clock;
//! the runner channel is reached through its seam (`gate_support`).

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::controller_protocol::{OwnerCommandRequest, ProtocolService};
use narsor_engine::domain::TaskId;
use narsor_engine::observers::{
    queue_entry_id, ObserverConfig, ObserverEffect, ObserverId, ObserverMode, ObserverName,
    QueueEntry,
};
use narsor_engine::orchestration::{
    Clock, CommandRequest, ControlPlane, Observation, OwnerCommand, OwnerRef, RunnerState,
    TaskEventKind,
};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::controller::{GateWatch, RunnerProblem, TaskQueueEntryView};
use serde_json::{json, Value};

use crate::gate_support::{
    observe, past_gates, queued_task, report, review_gate, running, snapshot, wait_at, FIRST_RUNNER,
};

const QUEUE: &str = "mr-review";

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

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("narsor-obs-entries-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 17, 12, 0, 0).unwrap(),
        )));
        let service = ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(&root), clock.clone()),
            crate::environment_support::seeded_resolver(&root),
        );
        Self {
            root,
            clock,
            service,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    /// An observer of `owner`'s `queue` in `mode` with approved script
    /// version 1. It creates no tasks.
    fn active_of(
        &self,
        owner: OwnerRef,
        name: &str,
        queue: &str,
        mode: ObserverMode,
    ) -> ObserverId {
        let mut config = ObserverConfig::new(
            ObserverName::parse(name).unwrap(),
            ObserverEffect::None,
            mode,
        );
        config.queue = queue.to_string();
        let store = self.service.observers();
        let id = store
            .create(owner, config, None, self.now())
            .unwrap()
            .observer_id;
        store
            .add_script(&id, "echo '{\"state\": {}}'\n", self.now())
            .unwrap();
        self.service.approve_observer_version(&id, 1).unwrap();
        id
    }

    /// The same, of the default owner.
    fn active(&self, name: &str, queue: &str, mode: ObserverMode) -> ObserverId {
        self.active_of(owner(), name, queue, mode)
    }

    fn task(&self, id: &str) -> TaskId {
        queued_task(&self.service, id, &owner())
    }

    /// The entries of the default owner's `queue`.
    fn entries(&self, queue: &str) -> Vec<QueueEntry> {
        self.service
            .observers()
            .queue_entries(&owner(), queue)
            .unwrap()
    }

    /// The queue warnings in the task's history: `(episode, reason)`.
    fn warnings(&self, task: &TaskId) -> Vec<(String, String)> {
        self.service
            .control_plane()
            .events_since(task, 0)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskEventKind::GateQueueWarning {
                    episode, reason, ..
                } => Some((episode, reason)),
                _ => None,
            })
            .collect()
    }

    /// The queue mark of the task's summary (R21).
    fn mark(&self, task: &TaskId) -> Option<TaskQueueEntryView> {
        let record = self.service.control_plane().task(task).unwrap();
        self.service.summarize(&record).unwrap().queue_entry
    }
}

// ---------------------------------------------------------------------------
// Adding and removing the entry of an episode (R16, R17)
// ---------------------------------------------------------------------------

#[test]
fn a_gate_with_a_watch_puts_its_episode_on_the_queue() {
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    let gate = review_gate(QUEUE);

    let outcome = wait_at(&fx.service, &task, "input-1", &gate);

    assert_eq!(outcome.gate_change.ended, None);
    assert_eq!(
        outcome
            .gate_change
            .began
            .as_ref()
            .map(|g| g.episode.as_str()),
        Some("input-1")
    );
    let entries = fx.entries(QUEUE);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.entry_id, queue_entry_id(&task, "input-1"));
    assert_eq!(entry.owner, owner());
    assert_eq!(entry.task_id, task);
    assert_eq!(entry.episode, "input-1");
    assert_eq!(entry.step, "review_work");
    assert_eq!(entry.params, gate.params);
    assert_eq!(entry.choices, gate.choices);
    assert_eq!(entry.variables, gate.variables);
    assert_eq!(entry.created_at, fx.now());
    // An active `on_episodes` observer serves the queue: no warning.
    assert!(fx.warnings(&task).is_empty());
}

#[test]
fn a_gate_without_a_watch_puts_nothing_on_a_queue() {
    let fx = Fixture::new();
    let task = fx.task("task-1");

    let outcome = report(
        &fx.service,
        &task,
        FIRST_RUNNER,
        true,
        Some(("input-1", None)),
    );

    assert!(outcome.gate_change.is_empty());
    assert!(fx
        .service
        .observers()
        .list_queue_entries()
        .unwrap()
        .is_empty());
    assert!(fx.warnings(&task).is_empty());
}

#[test]
fn a_manual_choice_removes_the_entry_before_a_run_can_take_it() {
    // Covers AE2 for the entry: the person answers the gate; the snapshot
    // that shows the exit removes the entry, so no run is due for it. (The
    // refusal of a late script answer is the answer route's.)
    let fx = Fixture::new();
    let observer = fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));

    let outcome = past_gates(&fx.service, &task);

    assert_eq!(
        outcome
            .gate_change
            .ended
            .as_ref()
            .map(|g| g.episode.as_str()),
        Some("input-1")
    );
    assert_eq!(outcome.gate_change.began, None);
    assert!(fx.entries(QUEUE).is_empty());
    let pass = fx.service.observer_pass();
    assert_eq!(pass.skipped, vec![observer]);
    assert!(fx.service.claim_observer_run().unwrap().is_none());
}

#[test]
fn a_run_in_progress_when_the_gate_exits_does_not_bring_the_entry_back() {
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    fx.service.observer_pass();
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    assert_eq!(claimed.input.entry.as_ref().unwrap().episode, "input-1");

    past_gates(&fx.service, &task);
    fx.service
        .finish_observer_run(
            &claimed.run_id,
            narsor_engine::controller_protocol::RunReport {
                outcome: narsor_engine::controller_protocol::RunOutcome::Exited { exit_code: 0 },
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .unwrap();

    assert!(fx.entries(QUEUE).is_empty());
}

#[test]
fn a_review_loop_removes_the_old_entry_before_it_adds_the_new_one() {
    // Covers AE3: the gate is entered again in a new episode; one snapshot
    // ends the old episode and begins the new one.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    let gate = review_gate(QUEUE);
    wait_at(&fx.service, &task, "input-1", &gate);
    fx.advance(Duration::minutes(30));

    let outcome = wait_at(&fx.service, &task, "input-2", &gate);

    assert_eq!(
        outcome
            .gate_change
            .ended
            .as_ref()
            .map(|g| g.episode.as_str()),
        Some("input-1")
    );
    assert_eq!(
        outcome
            .gate_change
            .began
            .as_ref()
            .map(|g| g.episode.as_str()),
        Some("input-2")
    );
    let entries = fx.entries(QUEUE);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].episode, "input-2");
    assert_eq!(entries[0].created_at, fx.now());
    assert_ne!(entries[0].entry_id, queue_entry_id(&task, "input-1"));
    assert!(fx
        .service
        .observers()
        .queue_entry(&queue_entry_id(&task, "input-1"))
        .unwrap()
        .is_none());
}

#[test]
fn a_gate_on_another_queue_moves_the_entry_to_that_queue() {
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    fx.active("plan-review", "plan-review", ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate("plan-review"));

    wait_at(&fx.service, &task, "input-2", &review_gate(QUEUE));

    assert!(fx.entries("plan-review").is_empty());
    assert_eq!(fx.entries(QUEUE).len(), 1);
}

#[test]
fn a_repeated_snapshot_of_the_same_episode_changes_nothing() {
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    let gate = review_gate(QUEUE);
    wait_at(&fx.service, &task, "input-1", &gate);
    let before = fx.entries(QUEUE);
    fx.advance(Duration::minutes(5));

    let again = wait_at(&fx.service, &task, "input-1", &gate);

    assert!(again.gate_change.is_empty());
    assert_eq!(fx.entries(QUEUE), before);
}

#[test]
fn a_runner_restart_in_the_same_episode_keeps_the_entry() {
    // R17: a pause, a loss and an auto-resume all end with a new runner of
    // the same episode. Until it has its execution it reports no wait; the
    // episode is not ended by that, and the entry is not added a second
    // time after it.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    let gate = review_gate(QUEUE);
    wait_at(&fx.service, &task, "input-1", &gate);
    let before = fx.entries(QUEUE);
    fx.advance(Duration::hours(2));

    let starting = report(&fx.service, &task, "pod-2", false, None);
    assert!(starting.gate_change.is_empty());
    assert_eq!(fx.entries(QUEUE), before);
    let resumed = report(
        &fx.service,
        &task,
        "pod-2",
        true,
        Some(("input-1", Some(&gate))),
    );

    assert!(resumed.gate_change.is_empty());
    assert_eq!(fx.entries(QUEUE), before, "same entry, same start time");
    let watched = fx.service.control_plane().task(&task).unwrap().watched_gate;
    assert_eq!(watched.map(|gate| gate.episode), Some("input-1".into()));
}

#[test]
fn a_runner_problem_in_the_same_episode_keeps_the_entry_and_warns_once() {
    // R17: a runner that cannot start reports its last execution with no
    // pending input and a problem. That is no exit from the gate: the
    // episode keeps its entry, its start time and its single warning.
    let fx = Fixture::new();
    let task = fx.task("task-1");
    let gate = review_gate(QUEUE);
    wait_at(&fx.service, &task, "input-1", &gate);
    let before = fx.entries(QUEUE);
    assert_eq!(before.len(), 1);
    fx.advance(Duration::hours(1));

    let mut broken = snapshot(&fx.service, &task, FIRST_RUNNER, true, None);
    broken.problem = Some(RunnerProblem {
        code: "workspace_unavailable".into(),
        message: "the workspace cannot be read".into(),
    });
    let outcome = fx
        .service
        .control_plane()
        .record_runner_snapshot(&task, FIRST_RUNNER, 1, broken)
        .unwrap();
    fx.service
        .follow_gate_episode(&outcome.task, &outcome.gate_change);

    assert!(outcome.gate_change.is_empty());
    assert_eq!(fx.entries(QUEUE), before);
    fx.advance(Duration::hours(1));

    let recovered = wait_at(&fx.service, &task, "input-1", &gate);

    assert!(recovered.gate_change.is_empty());
    assert_eq!(fx.entries(QUEUE), before, "same entry, same start time");
    assert_eq!(fx.warnings(&task).len(), 1, "one warning for the episode");
}

#[test]
fn an_entry_added_back_keeps_the_start_of_its_episode() {
    // The queue's last observer goes, and its entries with it. A new
    // observer of the queue adds the entry of the waiting task back; its
    // start is the episode's, so a script still sees a review done since
    // the gate began.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    let began = fx.now();
    fx.service.delete_observer(&owner(), "gitlab-mr").unwrap();
    assert!(fx.entries(QUEUE).is_empty());
    fx.advance(Duration::hours(3));

    fx.active("gitlab-mr-2", QUEUE, ObserverMode::OnEpisodes);
    let pass = fx.service.observer_pass();

    assert_eq!(pass.entries_restored, 1);
    let entries = fx.entries(QUEUE);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].episode, "input-1");
    assert_eq!(entries[0].created_at, began, "the episode's start, not now");
}

/// Overwrites the file of the entry of `task`'s `episode` with bytes no
/// build can read.
fn corrupt_entry(fx: &Fixture, task: &TaskId, episode: &str) {
    let path = StatePaths::new(&fx.root).queue_entry_file(&queue_entry_id(task, episode));
    fs::write(path, b"{\"entry_id\": ").unwrap();
}

#[test]
fn an_unreadable_entry_file_stops_no_queue_and_no_view() {
    // All owners share the entries directory: one bad file is skipped and
    // logged, and every other queue still runs and still shows.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let good = fx.task("task-1");
    wait_at(&fx.service, &good, "input-1", &review_gate(QUEUE));
    let bad = fx.task("task-2");
    wait_at(&fx.service, &bad, "input-1", &review_gate("other-queue"));
    corrupt_entry(&fx, &bad, "input-1");

    let queues = fx.service.observer_queues_of(&owner()).unwrap();
    assert_eq!(queues.queues.len(), 1, "{queues:?}");
    assert_eq!(queues.queues[0].queue, QUEUE);
    let queue = fx.service.observer_queue(&owner(), QUEUE).unwrap();
    assert_eq!(queue.entries.len(), 1);

    fx.service.observer_pass();
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    assert_eq!(claimed.input.entry.unwrap().task_id, good);
}

#[test]
fn an_entry_of_a_round_that_cannot_be_read_is_skipped_not_fatal() {
    let fx = Fixture::new();
    let observer = fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let first = fx.task("task-1");
    wait_at(&fx.service, &first, "input-1", &review_gate(QUEUE));
    fx.advance(Duration::seconds(1));
    let second = fx.task("task-2");
    wait_at(&fx.service, &second, "input-1", &review_gate(QUEUE));
    fx.service.observer_pass();
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    assert_eq!(claimed.input.entry.as_ref().unwrap().task_id, first);
    fx.service
        .finish_observer_run(
            &claimed.run_id,
            narsor_engine::controller_protocol::RunReport {
                outcome: narsor_engine::controller_protocol::RunOutcome::Exited { exit_code: 0 },
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .unwrap();
    // The second entry is the rest of the round; its file goes bad.
    corrupt_entry(&fx, &second, "input-1");

    let next = fx.service.claim_observer_run();

    assert!(matches!(next, Ok(None)), "{next:?}");
    // The round is over and saved: the observer waits for its next tick
    // instead of failing on the same entry at every claim.
    let record = fx.service.observers().load(&observer).unwrap();
    assert!(record.entry_round.is_empty(), "{:?}", record.entry_round);
    assert_eq!(record.run_due_since, None);
}

#[test]
fn a_corrupt_entry_of_a_waiting_task_is_rewritten_at_the_next_tick() {
    // A corrupt entry file of a waiting task kept its gate from every
    // observer: the tick rewrites it from the task, with the episode's
    // start, and runs it. The next tick writes nothing.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    let before = fx.entries(QUEUE);
    corrupt_entry(&fx, &task, "input-1");
    assert!(fx.entries(QUEUE).is_empty());
    fx.advance(Duration::hours(1));

    let pass = fx.service.observer_pass();

    assert_eq!(pass.entries_restored, 1);
    assert_eq!(fx.entries(QUEUE), before, "same entry, same start time");
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    assert_eq!(claimed.input.entry.unwrap().task_id, task);
    fx.advance(Duration::hours(1));
    assert_eq!(fx.service.observer_pass().entries_restored, 0);
}

#[test]
fn an_entry_of_a_newer_version_is_left_as_it_is() {
    // Another build's file: never rewritten, and not tried at every tick.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    let path = StatePaths::new(&fx.root).queue_entry_file(&queue_entry_id(&task, "input-1"));
    let mut newer: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    newer["record_version"] = json!(99);
    let bytes = serde_json::to_vec(&newer).unwrap();
    fs::write(&path, &bytes).unwrap();

    let pass = fx.service.observer_pass();
    fx.advance(Duration::hours(1));
    let again = fx.service.observer_pass();

    assert_eq!((pass.entries_restored, pass.entries_removed), (0, 0));
    assert_eq!((again.entries_restored, again.entries_removed), (0, 0));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn an_unreadable_entry_of_an_episode_no_task_waits_in_is_removed() {
    // A bad entry file whose task left the gate: the next tick of an
    // on_episodes observer removes it.
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    corrupt_entry(&fx, &task, "input-1");
    let path = StatePaths::new(&fx.root).queue_entry_file(&queue_entry_id(&task, "input-1"));
    // The snapshot that ends the episode removes the file; one left behind
    // (a failed removal, a restored backup) is put back here.
    let bad = fs::read(&path).unwrap();
    past_gates(&fx.service, &task);
    fs::write(&path, &bad).unwrap();

    let pass = fx.service.observer_pass();

    assert_eq!(pass.entries_removed, 1);
    assert!(!path.exists());
}

#[test]
fn a_paused_task_keeps_its_entry() {
    // Covers AE5 for the entry: the owner pauses the task waiting at the
    // gate; the runner stops at a safe point in the same episode, and
    // nothing ends it. (The script's answer that resumes the task is the
    // answer route's.)
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    let lease = running(&fx.service, &task);
    let gate = review_gate(QUEUE);
    wait_at(&fx.service, &task, "input-1", &gate);
    let before = fx.entries(QUEUE);

    fx.service
        .command(
            &owner(),
            task.as_str(),
            OwnerCommandRequest {
                command: OwnerCommand::Pause,
                idempotency_key: "pause-1".into(),
                expected_generation: None,
            },
        )
        .unwrap();
    let mut at_safe_point = snapshot(
        &fx.service,
        &task,
        FIRST_RUNNER,
        true,
        Some(("input-1", Some(&gate))),
    );
    at_safe_point.safe_point = true;
    let outcome = fx
        .service
        .control_plane()
        .record_runner_snapshot(&task, FIRST_RUNNER, 1, at_safe_point)
        .unwrap();
    fx.service
        .follow_gate_episode(&outcome.task, &outcome.gate_change);
    let paused = observe(&fx.service, &task, &lease, Observation::PausedAtSafePoint);

    assert_eq!(paused.runner_state, RunnerState::Stopped);
    assert!(outcome.gate_change.is_empty());
    assert_eq!(fx.entries(QUEUE), before);
    assert_eq!(
        fx.mark(&task).map(|mark| mark.episode),
        Some("input-1".into())
    );
    fx.service.observer_pass();
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    assert_eq!(claimed.input.entry.unwrap().task_id, task);
}

// ---------------------------------------------------------------------------
// The warning of a queue nobody serves (R9, R27)
// ---------------------------------------------------------------------------

#[test]
fn a_queue_of_always_observers_warns_and_the_gate_waits() {
    // Covers AE11.
    let fx = Fixture::new();
    fx.active("jira-new", "jira-intake", ObserverMode::Always);
    let task = fx.task("task-1");

    wait_at(&fx.service, &task, "input-1", &review_gate("jira-intake"));

    let warnings = fx.warnings(&task);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].0, "input-1");
    assert!(
        warnings[0].1.contains("mode always") && warnings[0].1.contains("waits for a person"),
        "{}",
        warnings[0].1
    );
    // The entry is kept for an observer that may serve the queue later;
    // the `always` observer's run never gets it (R25).
    assert_eq!(fx.entries("jira-intake").len(), 1);
    fx.service.observer_pass();
    let claimed = fx.service.claim_observer_run().unwrap().expect("a run");
    assert!(claimed.input.entry.is_none());
    let record = fx.service.control_plane().task(&task).unwrap();
    assert_eq!(
        record.runner.unwrap().snapshot.pending_input.map(|p| p.id),
        Some("input-1".into()),
        "the gate still waits"
    );
}

#[test]
fn a_queue_without_observers_warns_once_per_episode_not_per_snapshot() {
    let fx = Fixture::new();
    let task = fx.task("task-1");
    let gate = review_gate(QUEUE);

    for _ in 0..3 {
        wait_at(&fx.service, &task, "input-1", &gate);
    }
    report(&fx.service, &task, "pod-2", false, None);
    report(
        &fx.service,
        &task,
        "pod-2",
        true,
        Some(("input-1", Some(&gate))),
    );

    let warnings = fx.warnings(&task);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].1.contains("has no observer"),
        "{}",
        warnings[0].1
    );

    // A new episode is a new entry, and a new warning.
    report(
        &fx.service,
        &task,
        "pod-2",
        true,
        Some(("input-2", Some(&gate))),
    );
    let episodes: Vec<String> = fx.warnings(&task).into_iter().map(|w| w.0).collect();
    assert_eq!(episodes, ["input-1", "input-2"]);
}

#[test]
fn a_paused_observer_does_not_serve_its_queue() {
    let fx = Fixture::new();
    let observer = fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    fx.service.pause_observer(&observer).unwrap();
    let task = fx.task("task-1");

    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));

    let warnings = fx.warnings(&task);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].1.contains("is active"), "{}", warnings[0].1);
}

#[test]
fn an_observer_of_another_owner_does_not_serve_the_queue() {
    // R7: the queue is the task's owner's.
    let fx = Fixture::new();
    fx.active_of(
        OwnerRef::new("someone-else"),
        "foreign-mr",
        QUEUE,
        ObserverMode::OnEpisodes,
    );
    let task = fx.task("task-1");

    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));

    let warnings = fx.warnings(&task);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].1.contains("has no observer"),
        "{}",
        warnings[0].1
    );
    assert!(fx
        .service
        .observers()
        .queue_entries(&OwnerRef::new("someone-else"), QUEUE)
        .unwrap()
        .is_empty());
}

#[test]
fn an_entry_the_store_refuses_is_a_warning_and_the_gate_waits() {
    let fx = Fixture::new();
    let task = fx.task("task-1");
    let mut gate = review_gate(QUEUE);
    gate.params = (0..100)
        .map(|n| (format!("param-{n}"), "x".to_string()))
        .collect();

    wait_at(&fx.service, &task, "input-1", &gate);

    let warnings = fx.warnings(&task);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].1.contains("was refused"), "{}", warnings[0].1);
    assert!(fx
        .service
        .observers()
        .list_queue_entries()
        .unwrap()
        .is_empty());
    // The episode is known: a later snapshot does not warn again.
    wait_at(&fx.service, &task, "input-1", &gate);
    assert_eq!(fx.warnings(&task).len(), 1);
}

#[test]
fn a_queue_warning_reads_as_unrecognized_in_an_old_client() {
    let event = json!({
        "type": "gate_queue_warning",
        "step": "review_work",
        "queue": QUEUE,
        "episode": "input-1",
        "reason": "queue 'mr-review' has no observer; the gate waits for a person",
    });
    let kind: TaskEventKind = serde_json::from_value(event.clone()).unwrap();
    assert!(matches!(kind, TaskEventKind::GateQueueWarning { .. }));
    assert_eq!(serde_json::to_value(&kind).unwrap(), event);
    let unknown: TaskEventKind =
        serde_json::from_value(json!({"type": "something_newer", "x": 1})).unwrap();
    assert_eq!(unknown, TaskEventKind::Unrecognized);
}

// ---------------------------------------------------------------------------
// The guard before a run, pruning and archive (R18, R19)
// ---------------------------------------------------------------------------

/// The task's runner reports `waiting`, but the queue entries never hear of
/// it: a snapshot the control plane did not see through to its entries.
fn report_unfollowed(fx: &Fixture, task: &TaskId, waiting: Option<(&str, Option<&GateWatch>)>) {
    let snapshot = snapshot(&fx.service, task, FIRST_RUNNER, true, waiting);
    fx.service
        .control_plane()
        .record_runner_snapshot(task, FIRST_RUNNER, 1, snapshot)
        .unwrap();
}

#[test]
fn the_guard_drops_an_entry_whose_gate_exited_and_runs_nothing() {
    let fx = Fixture::new();
    let observer = fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    assert_eq!(fx.service.observer_pass().due, vec![observer.clone()]);

    report_unfollowed(&fx, &task, None);

    assert!(fx.service.claim_observer_run().unwrap().is_none());
    assert!(fx.entries(QUEUE).is_empty());
    let record = fx.service.observers().load(&observer).unwrap();
    assert_eq!(record.last_run_sequence, 0, "no run was started");
    assert_eq!(record.run_due_since, None);
}

#[test]
fn the_guard_drops_the_entry_of_an_archived_task() {
    let fx = Fixture::new();
    let observer = fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let archived = fx.task("task-archived");
    let live = fx.task("task-live");
    wait_at(&fx.service, &archived, "input-1", &review_gate(QUEUE));
    wait_at(&fx.service, &live, "input-1", &review_gate(QUEUE));
    assert_eq!(fx.service.observer_pass().due, vec![observer]);
    // Archived after the round was taken, without the owner route's
    // removal: the guard catches it.
    fx.service
        .control_plane()
        .command(
            &archived,
            CommandRequest::new(OwnerCommand::Archive, "archive"),
        )
        .unwrap();

    let mut ran = Vec::new();
    while let Some(claimed) = fx.service.claim_observer_run().unwrap() {
        ran.push(claimed.input.entry.as_ref().unwrap().task_id.clone());
        fx.service
            .finish_observer_run(
                &claimed.run_id,
                narsor_engine::controller_protocol::RunReport {
                    outcome: narsor_engine::controller_protocol::RunOutcome::Exited {
                        exit_code: 0,
                    },
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .unwrap();
    }

    assert_eq!(ran, vec![live.clone()]);
    let left = fx.entries(QUEUE);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].task_id, live);
}

#[test]
fn the_pass_drops_an_entry_whose_episode_is_gone() {
    let fx = Fixture::new();
    let observer = fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    report_unfollowed(&fx, &task, Some(("input-2", None)));

    let pass = fx.service.observer_pass();

    assert_eq!(pass.entries_removed, 1);
    assert_eq!(pass.skipped, vec![observer]);
    assert!(fx.entries(QUEUE).is_empty());
}

#[test]
fn the_owners_archive_removes_the_entry_at_once() {
    // R19, also for a queue no observer runs for.
    let fx = Fixture::new();
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    assert_eq!(fx.entries(QUEUE).len(), 1);

    let archived = fx
        .service
        .command(
            &owner(),
            task.as_str(),
            OwnerCommandRequest {
                command: OwnerCommand::Archive,
                idempotency_key: "archive-1".into(),
                expected_generation: None,
            },
        )
        .unwrap();

    assert!(fx.entries(QUEUE).is_empty());
    assert_eq!(archived.task.queue_entry, None);
}

// ---------------------------------------------------------------------------
// The task view's mark (R21)
// ---------------------------------------------------------------------------

#[test]
fn the_summary_marks_the_task_while_its_episode_has_an_entry() {
    let fx = Fixture::new();
    fx.active("gitlab-mr", QUEUE, ObserverMode::OnEpisodes);
    let task = fx.task("task-1");
    assert_eq!(fx.mark(&task), None);

    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    assert_eq!(
        fx.mark(&task),
        Some(TaskQueueEntryView {
            queue: QUEUE.into(),
            step: "review_work".into(),
            episode: "input-1".into(),
            since: fx.now(),
        })
    );

    past_gates(&fx.service, &task);
    assert_eq!(fx.mark(&task), None);
    let record = fx.service.control_plane().task(&task).unwrap();
    let summary: Value = serde_json::to_value(fx.service.summarize(&record).unwrap()).unwrap();
    assert!(summary.get("queue_entry").is_none(), "{summary}");
}

#[test]
fn the_summary_has_no_mark_when_the_entry_is_gone() {
    // The mark follows the control plane's entries, not the runner's word.
    let fx = Fixture::new();
    let task = fx.task("task-1");
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));
    let store = fx.service.observers();
    let lock = store.lock_queue_entries().unwrap();
    store
        .remove_queue_entry(&lock, &queue_entry_id(&task, "input-1"))
        .unwrap();
    drop(lock);

    assert_eq!(fx.mark(&task), None);
}
