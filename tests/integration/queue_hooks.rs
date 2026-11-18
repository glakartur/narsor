//! Integration tests of entry scripts (`in`) at watched gates (U4): `in`
//! runs when an episode enters its queue, `check` waits until every `in`
//! succeeded (R8, R9), a failed `in` is retried and then fails the entry
//! (R12, AE2), a queue without entry scripts is ready at once (R17, AE7),
//! and neither a runner problem nor a restored entry runs `in` again (R19).
//!
//! A real `ProtocolService` over a temporary root (catalogue and state) and
//! a manual clock; the runner channel is reached through `gate_support`, the
//! worker through the claim and result methods it calls.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::automations::HookPhase;
use narsor_engine::controller_protocol::{
    ClaimedObserverRun, ProtocolService, RunOutcome, RunReport,
};
use narsor_engine::domain::TaskId;
use narsor_engine::observers::queue_entry_id;
use narsor_engine::orchestration::{
    Clock, ControlPlane, GateAutomationPhase, OwnerRef, TaskEventKind,
};
use narsor_engine::persistence::StatePaths;

use crate::automation_support::{automation, on_episodes_queue, write, CHECK_SCRIPT};
use crate::gate_support::{queued_task, review_gate, runner_problem, wait_at};

const QUEUE: &str = "mr-review";
const EPISODE: &str = "input-1";

/// A clock the test moves by hand.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// How one attempt of an entry script ends.
#[derive(Debug, Clone, Copy)]
enum Attempt {
    Exits(i32),
    /// No result before the run's deadline.
    Lost,
}

/// A catalogue root with queue `mr-review`, a clock and a service.
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
    /// Queue `mr-review` (every 300 s) served by `automations`, each given
    /// as its name and whether it has an `in` script.
    fn new(automations: &[(&str, bool)]) -> Self {
        let root =
            std::env::temp_dir().join(format!("narsor-queue-hooks-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        for (name, entry_script) in automations {
            automation(&root, name, CHECK_SCRIPT, &[]);
            if *entry_script {
                write(&root, &format!("automations/{name}/in.sh"), "exit 0\n");
            }
        }
        let names: Vec<&str> = automations.iter().map(|(name, _)| *name).collect();
        on_episodes_queue(&root, QUEUE, &names, "{interval_secs: 300}");
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap(),
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

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    /// Task `task-1`, waiting at the review gate of `mr-review`.
    fn waiting_task(&self) -> TaskId {
        let task = queued_task(&self.service, "task-1", &OwnerRef::new("owner"));
        wait_at(&self.service, &task, EPISODE, &review_gate(QUEUE));
        task
    }

    fn claim(&self) -> Option<ClaimedObserverRun> {
        self.service.claim_observer_run().unwrap()
    }

    /// The worker's result of `run`: the script exited with `code`.
    fn finish(&self, run: &ClaimedObserverRun, code: i32) {
        self.service
            .finish_observer_run(
                &run.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code: code },
                    stdout: String::new(),
                    stderr: "the source API is down".into(),
                },
            )
            .unwrap();
    }

    /// The claimed `in` run of `automation` for the task's episode.
    fn claim_entry_script(&self, automation: &str) -> ClaimedObserverRun {
        let run = self.claim().expect("an entry script run");
        assert_eq!(run.phase, HookPhase::In);
        assert_eq!(run.input.phase, HookPhase::In);
        assert_eq!(run.run_id.key.automation, automation);
        assert_eq!(run.input.entry.as_ref().unwrap().episode, EPISODE);
        run
    }

    /// The tick of the queue comes: the pass flags its pairs, a claim takes
    /// the next run.
    fn next_tick_run(&self) -> Option<ClaimedObserverRun> {
        self.advance(Duration::seconds(300));
        self.service.observer_pass();
        self.claim()
    }

    fn phase(&self, task: &TaskId) -> Option<GateAutomationPhase> {
        self.service
            .control_plane()
            .task(task)
            .unwrap()
            .watched_gate
            .and_then(|gate| gate.automation)
            .map(|automation| automation.phase)
    }

    /// The entry script attempts in the task's history: `(automation,
    /// failed, gate phase it moved to)`.
    fn attempts(&self, task: &TaskId) -> Vec<(String, bool, Option<GateAutomationPhase>)> {
        self.service
            .control_plane()
            .events_since(task, 0)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskEventKind::GateHookAttempt {
                    automation,
                    hook,
                    error,
                    gate_phase,
                    ..
                } => {
                    assert_eq!(hook, "in");
                    Some((automation, error.is_some(), gate_phase))
                }
                _ => None,
            })
            .collect()
    }
}

#[test]
fn the_entry_script_runs_first_and_check_waits_until_it_succeeded() {
    // Covers AE1 (entry part), R8, R9.
    let fx = Fixture::new(&[("gitlab-mr-label", true)]);
    let task = fx.waiting_task();
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Entering));
    // The tick flags the pair while `in` has not run: no `check` before it.
    fx.service.observer_pass();

    let run = fx.claim_entry_script("gitlab-mr-label");
    assert_eq!(run.timeout_secs, 120);
    assert!(fx.claim().is_none(), "no check while the entry script runs");
    fx.finish(&run, 0);

    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready));
    assert_eq!(
        fx.attempts(&task),
        vec![(
            "gitlab-mr-label".to_string(),
            false,
            Some(GateAutomationPhase::Ready)
        )]
    );
    let check = fx.next_tick_run().expect("the first check run");
    assert_eq!(check.phase, HookPhase::Check);
    assert_eq!(check.input.entry.as_ref().unwrap().task_id, task);
}

#[test]
fn a_failing_entry_script_is_retried_and_after_the_last_attempt_the_entry_fails() {
    // Covers AE2, F2, R12: three attempts, 10 s and 30 s apart; a lost run
    // is a failed attempt.
    use Attempt::{Exits, Lost};
    let cases: [(&str, &[Attempt], GateAutomationPhase); 3] = [
        (
            "third attempt succeeds",
            &[Exits(1), Exits(1), Exits(0)],
            GateAutomationPhase::Ready,
        ),
        (
            "three failures",
            &[Exits(1), Exits(2), Exits(1)],
            GateAutomationPhase::EntryFailed,
        ),
        (
            "a lost run counts",
            &[Lost, Exits(1), Exits(1)],
            GateAutomationPhase::EntryFailed,
        ),
    ];
    for (case, attempts, expected) in cases {
        let fx = Fixture::new(&[("gitlab-mr-label", true)]);
        let task = fx.waiting_task();
        for (index, attempt) in attempts.iter().enumerate() {
            if index > 0 {
                let delay = if index == 1 { 10 } else { 30 };
                fx.advance(Duration::seconds(delay - 1));
                assert!(fx.claim().is_none(), "{case}: retried before its delay");
                fx.advance(Duration::seconds(1));
            }
            let run = fx.claim_entry_script("gitlab-mr-label");
            match attempt {
                Exits(code) => fx.finish(&run, *code),
                Lost => {
                    fx.advance(Duration::seconds(181));
                    assert!(fx.claim().is_none(), "{case}: lost run found at the claim");
                }
            }
            if index + 1 < attempts.len() {
                assert_eq!(
                    fx.phase(&task),
                    Some(GateAutomationPhase::Entering),
                    "{case}"
                );
            }
        }

        assert_eq!(fx.phase(&task), Some(expected), "{case}");
        let history = fx.attempts(&task);
        let failed = attempts
            .iter()
            .filter(|attempt| !matches!(attempt, Exits(0)))
            .count();
        assert_eq!(
            history.len(),
            attempts.len(),
            "{case}: one event per attempt"
        );
        assert_eq!(
            history.iter().filter(|(_, failed, _)| *failed).count(),
            failed,
            "{case}"
        );
        assert_eq!(history.last().unwrap().2, Some(expected), "{case}");
        if expected == GateAutomationPhase::EntryFailed {
            // No more automatic attempts, and `check` never runs.
            assert!(
                fx.next_tick_run().is_none(),
                "{case}: a run after entry_failed"
            );
            assert!(
                fx.next_tick_run().is_none(),
                "{case}: a run after entry_failed"
            );
        }
    }
}

#[test]
fn a_queue_without_entry_scripts_is_ready_at_once() {
    // Covers AE7, R17.
    let fx = Fixture::new(&[("jira-review", false)]);
    let task = fx.waiting_task();

    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready));
    fx.service.observer_pass();
    let check = fx.claim().expect("a check run at the first tick");
    assert_eq!(check.phase, HookPhase::Check);
    assert!(fx.attempts(&task).is_empty());
}

#[test]
fn the_entry_stays_entering_until_every_entry_script_ended() {
    // Two automations, one at a time per task: one `in` succeeds, the other
    // fails three times.
    let fx = Fixture::new(&[("gitlab-mr-label", true), ("sonar-gate", true)]);
    let task = fx.waiting_task();

    let first = fx.claim_entry_script("gitlab-mr-label");
    assert!(fx.claim().is_none(), "one entry script of a task at a time");
    fx.finish(&first, 0);
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Entering));
    for delay in [0, 10, 30] {
        fx.advance(Duration::seconds(delay));
        assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Entering));
        let run = fx.claim_entry_script("sonar-gate");
        fx.finish(&run, 1);
    }

    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::EntryFailed));
    assert!(fx.next_tick_run().is_none(), "no check after entry_failed");
}

#[test]
fn neither_a_runner_problem_nor_a_restored_entry_runs_the_entry_script_again() {
    // R19 and the learning of 2026-11-18: a problem snapshot keeps the
    // episode and its phase; an entry added back does not start `in`.
    let fx = Fixture::new(&[("gitlab-mr-label", true)]);
    let task = fx.waiting_task();
    let run = fx.claim_entry_script("gitlab-mr-label");

    runner_problem(&fx.service, &task);
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Entering));
    wait_at(&fx.service, &task, EPISODE, &review_gate(QUEUE));
    assert!(fx.claim().is_none(), "no second entry script run");
    fx.finish(&run, 0);
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready));

    let store = fx.service.observers();
    let lock = store.lock_queue_entries().unwrap();
    assert!(store
        .remove_queue_entry(&lock, &queue_entry_id(&task, EPISODE))
        .unwrap());
    drop(lock);
    fx.advance(Duration::seconds(300));
    assert_eq!(fx.service.observer_pass().entries_restored, 1);
    let next = fx.claim().expect("the check run of the restored entry");

    assert_eq!(next.phase, HookPhase::Check);
    assert_eq!(
        fx.attempts(&task).len(),
        1,
        "one entry script attempt in all"
    );
}
