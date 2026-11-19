//! Integration tests of entry (`in`) and exit (`out`) scripts at watched
//! gates (U4, U5): `in` runs when an episode enters its queue, `check`
//! waits until every `in` succeeded (R8, R9), a failed `in` is retried and
//! then fails the entry (R12, AE2), a queue without entry scripts is ready
//! at once (R17, AE7), and neither a runner problem nor a restored entry
//! runs `in` again (R19). Every exit of the gate is held until `out` ended
//! (R10, R11, R14, R15, AE1, AE3, AE5), a task that ends at the gate runs
//! `out` once (R18, AE6), and `out` of an episode ends before `in` of the
//! next one (AE4). A restart of the waiting gate step is an exit too: `out`
//! of the old episode, then `in` of the new one (U7, R16).
//!
//! A real `ProtocolService` over a temporary root (catalogue and state) and
//! a manual clock; the runner channel is reached through `gate_support`, the
//! worker through the claim and result methods it calls.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::automations::HookPhase;
use narsor_engine::controller_protocol::{
    ClaimedObserverRun, OwnerCommandRequest, ProtocolError, ProtocolService, RunOutcome, RunReport,
    SubmitInputRequest, SubmitOperatorCommandRequest,
};
use narsor_engine::domain::TaskId;
use narsor_engine::observers::{queue_entry_id, ScriptExit};
use narsor_engine::orchestration::{
    Clock, ControlPlane, GateAutomationPhase, OperatorCommand, OperatorCommandRequest,
    OwnerCommand, OwnerRef, TaskEventKind,
};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::api::HumanInputDeliveryDto;
use narsor_protocol::controller::ObserverDecideRequest;
use serde_json::{json, Value};

use crate::automation_support::{always_queue, automation, on_episodes_queue, write, CHECK_SCRIPT};
use crate::gate_support::{
    past_gates, queued_task, review_gate, runner_problem, running, wait_at, FIRST_RUNNER,
};

const QUEUE: &str = "mr-review";
const EPISODE: &str = "input-1";
const LABEL: &str = "gitlab-mr-label";
const IN_OUT: &[HookPhase] = &[HookPhase::In, HookPhase::Out];

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

/// The code of a coded refusal; panics on any other.
fn refusal_code(error: ProtocolError) -> &'static str {
    match error {
        ProtocolError::BadRequest { code, .. } | ProtocolError::Conflict { code, .. } => code,
        other => panic!("not a coded refusal: {other:?}"),
    }
}

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
    service: Arc<ProtocolService>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Queue `mr-review` (every 300 s) served by `automations`, each given
    /// as its name and its scripts besides `check` (`in`, `out`).
    fn new(automations: &[(&str, &[HookPhase])]) -> Self {
        let root =
            std::env::temp_dir().join(format!("narsor-queue-hooks-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        for (name, hooks) in automations {
            automation(&root, name, CHECK_SCRIPT, &[]);
            for hook in *hooks {
                let file = hook.script_file();
                write(&root, &format!("automations/{name}/{file}"), "exit 0\n");
            }
        }
        let names: Vec<&str> = automations.iter().map(|(name, _)| *name).collect();
        on_episodes_queue(&root, QUEUE, &names, "{interval_secs: 300}");
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap(),
        )));
        let service = Self::service_over(&root, &clock);
        Self {
            root,
            clock,
            service,
        }
    }

    fn service_over(root: &PathBuf, clock: &Arc<ManualClock>) -> Arc<ProtocolService> {
        Arc::new(ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(root), clock.clone()),
            crate::environment_support::seeded_resolver(root),
        ))
    }

    /// The server restarts: a new service over the same state.
    fn restart(&mut self) {
        self.service = Self::service_over(&self.root, &self.clock);
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    /// Task `task-1`, running and waiting at the review gate of
    /// `mr-review`.
    fn waiting_task(&self) -> TaskId {
        let task = queued_task(&self.service, "task-1", &owner());
        running(&self.service, &task);
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
        self.claim_hook(HookPhase::In, automation, EPISODE)
    }

    /// The claimed `hook` run of `automation` for `episode`.
    fn claim_hook(&self, hook: HookPhase, automation: &str, episode: &str) -> ClaimedObserverRun {
        let run = self.claim().expect("an entry or exit script run");
        assert_eq!(run.phase, hook);
        assert_eq!(run.input.phase, hook);
        assert_eq!(run.run_id.key.automation, automation);
        assert_eq!(run.input.entry.as_ref().unwrap().episode, episode);
        run
    }

    /// The task's first runner polls: the answers and the operator commands
    /// it gets, by their choice and target.
    fn runner_gets(&self, task: &TaskId) -> Vec<String> {
        let control = self.service.control_plane();
        let (inputs, _) = control
            .deliver_inputs(task, FIRST_RUNNER, 1, Some(EPISODE))
            .unwrap();
        let (commands, _) = control
            .deliver_operator_commands(task, FIRST_RUNNER, 1)
            .unwrap();
        inputs
            .into_iter()
            .filter_map(|input| input.delivery?.chosen_label)
            .chain(
                commands
                    .into_iter()
                    .filter_map(|queued| match queued.command {
                        OperatorCommand::Transition { target, .. } => Some(target),
                        OperatorCommand::RestartStep { .. } => Some("restart".to_string()),
                        _ => None,
                    }),
            )
            .collect()
    }

    /// The person answers the gate through the owner's route.
    fn person_answers(
        &self,
        task: &TaskId,
        choice: &str,
        values: Value,
    ) -> Result<(), ProtocolError> {
        self.service
            .submit_input(
                &owner(),
                task.as_str(),
                SubmitInputRequest {
                    pending_input_id: EPISODE.to_string(),
                    idempotency_key: format!("person-{choice}"),
                    delivery: HumanInputDeliveryDto {
                        chosen_label: Some(choice.to_string()),
                        values: values.as_object().cloned().unwrap_or_default(),
                        episode: Some(EPISODE.to_string()),
                        ..HumanInputDeliveryDto::default()
                    },
                },
            )
            .map(|_| ())
    }

    /// The owner sends a transition to `target` (the control plane's
    /// queue, where the hold is; the route adds only offer checks).
    fn operator_transition(&self, task: &TaskId, target: &str) {
        self.service
            .control_plane()
            .queue_operator_command(
                task,
                OperatorCommandRequest {
                    command: OperatorCommand::Transition {
                        target: target.to_string(),
                        forced: false,
                        reason: None,
                    },
                    idempotency_key: "transition-1".into(),
                    expected_generation: None,
                    requested_by: owner(),
                },
            )
            .unwrap();
    }

    /// The owner restarts the gate step (U7): the command's id, or why the
    /// control plane refused it.
    fn operator_restart(&self, task: &TaskId) -> Result<String, String> {
        self.service
            .control_plane()
            .queue_operator_command(
                task,
                OperatorCommandRequest {
                    command: OperatorCommand::RestartStep {
                        note: None,
                        failure_key: None,
                    },
                    idempotency_key: "restart-1".into(),
                    expected_generation: None,
                    requested_by: owner(),
                },
            )
            .map(|outcome| outcome.command.command_id)
            .map_err(|error| error.to_string())
    }

    fn has_entry(&self, task: &TaskId, episode: &str) -> bool {
        self.service
            .observers()
            .queue_entry(&queue_entry_id(task, episode))
            .unwrap()
            .is_some()
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

    /// The reasons of the gate warnings in the task's history that say a
    /// script waits for a catalogue file.
    fn waiting_warnings(&self, task: &TaskId) -> Vec<String> {
        self.service
            .control_plane()
            .events_since(task, 0)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskEventKind::GateQueueWarning { reason, .. }
                    if reason.contains(" script of ") =>
                {
                    Some(reason)
                }
                _ => None,
            })
            .collect()
    }

    /// The entry script attempts in the task's history: `(automation,
    /// failed, gate phase it moved to)`.
    fn attempts(&self, task: &TaskId) -> Vec<(String, bool, Option<GateAutomationPhase>)> {
        self.hook_attempts(task, "in")
            .into_iter()
            .map(|(automation, failed, _, phase)| (automation, failed, phase))
            .collect()
    }

    /// The `hook` script attempts in the task's history: `(automation,
    /// failed, retried, gate phase it moved to)`.
    fn hook_attempts(
        &self,
        task: &TaskId,
        wanted: &str,
    ) -> Vec<(String, bool, bool, Option<GateAutomationPhase>)> {
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
                    retry_at,
                    gate_phase,
                    ..
                } if hook == wanted => {
                    Some((automation, error.is_some(), retry_at.is_some(), gate_phase))
                }
                _ => None,
            })
            .collect()
    }
}

#[test]
fn the_entry_script_runs_first_and_check_waits_until_it_succeeded() {
    // Covers AE1 (entry part), R8, R9.
    let fx = Fixture::new(&[(LABEL, &[HookPhase::In])]);
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
        let fx = Fixture::new(&[(LABEL, &[HookPhase::In])]);
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
        // R20: the task view lists every attempt under the gate's step.
        let record = fx.service.control_plane().task(&task).unwrap();
        let view = fx.service.summarize(&record).unwrap().gate_automation;
        let view = view.expect("the gate automation status");
        assert_eq!(
            (view.step.as_str(), view.phase),
            ("review_work", expected),
            "{case}"
        );
        let hook = &view.hooks[0];
        assert_eq!(
            (
                hook.hook.as_str(),
                hook.attempts,
                hook.max_attempts,
                hook.log.len()
            ),
            ("in", 3, 3, 3),
            "{case}"
        );
        assert_eq!(
            hook.log
                .iter()
                .filter(|entry| entry.error.is_some())
                .count(),
            failed,
            "{case}"
        );
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
    let fx = Fixture::new(&[("jira-review", &[])]);
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
    let fx = Fixture::new(&[(LABEL, &[HookPhase::In]), ("sonar-gate", &[HookPhase::In])]);
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
    let fx = Fixture::new(&[(LABEL, &[HookPhase::In])]);
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

#[test]
fn an_entry_script_is_claimed_before_a_check_run_due_longer() {
    // U6: a gate waits on its entry scripts (R8).
    let fx = Fixture::new(&[(LABEL, &[HookPhase::In])]);
    automation(&fx.root, "jira-new", CHECK_SCRIPT, &[]);
    always_queue(
        &fx.root,
        "jira-intake",
        "jira-new",
        "{interval_secs: 300}",
        "",
    );
    fx.service.observer_pass();
    fx.advance(Duration::seconds(60));
    fx.waiting_task();

    fx.claim_entry_script(LABEL);
    let check = fx.claim().expect("the check run of jira-intake");

    assert_eq!(check.phase, HookPhase::Check);
    assert_eq!(check.run_id.key.queue, "jira-intake");
}

#[tokio::test]
async fn a_waiting_claim_returns_the_entry_script_run_once_the_gate_is_entered() {
    // U6: `in` starts at once, not at the worker's next poll.
    let fx = Fixture::new(&[(LABEL, &[HookPhase::In])]);
    let task = queued_task(&fx.service, "task-1", &owner());
    running(&fx.service, &task);
    let mut claim = tokio::spawn(Arc::clone(&fx.service).claim_observer_run_waiting());
    let waits = tokio::time::timeout(StdDuration::from_millis(200), &mut claim).await;
    assert!(waits.is_err(), "nothing is due before the gate");

    wait_at(&fx.service, &task, EPISODE, &review_gate(QUEUE));

    let run = tokio::time::timeout(StdDuration::from_secs(5), claim)
        .await
        .expect("the claim answers well before the worker's former 10 s poll")
        .unwrap()
        .unwrap()
        .expect("the entry script run");
    assert_eq!(run.phase, HookPhase::In);
    assert_eq!(run.input.entry.unwrap().task_id, task);
}

// ---------------------------------------------------------------------------
// Exit scripts and the exit hold (U5)
// ---------------------------------------------------------------------------

/// The gate of `fx`'s task is ready: its `in` ran and succeeded.
fn ready_gate(fx: &Fixture) -> TaskId {
    let task = fx.waiting_task();
    let run = fx.claim_entry_script(LABEL);
    fx.finish(&run, 0);
    task
}

#[test]
fn a_check_answer_reaches_the_runner_only_after_out_ended() {
    // Covers AE1 (exit part) and AE5, R10, R11, R15: `out` gets the choice
    // and its author; the answer is held until `out` succeeded, or failed
    // its last attempt (logged), and is then delivered.
    let cases: [(&str, &[i32]); 2] = [("out succeeds", &[0]), ("out fails 3 times", &[1, 1, 1])];
    for (case, exits) in cases {
        let fx = Fixture::new(&[(LABEL, IN_OUT)]);
        let task = ready_gate(&fx);
        let check = fx.next_tick_run().expect("a check run");
        let entry = check.input.entry.as_ref().unwrap();
        fx.service
            .observer_decide(
                &check.run_id,
                ObserverDecideRequest {
                    task_id: entry.task_id.to_string(),
                    episode: entry.episode.clone(),
                    choice: "check_feedback".into(),
                    reason: None,
                    variables: Default::default(),
                },
            )
            .unwrap();
        fx.finish(&check, 0);
        assert_eq!(
            fx.phase(&task),
            Some(GateAutomationPhase::Exiting),
            "{case}"
        );
        // The person answers while `out` runs: the first exit wins.
        let late = fx.person_answers(&task, "check_feedback", json!({}));
        assert_eq!(
            refusal_code(late.unwrap_err()),
            "input_already_queued",
            "{case}"
        );

        for (index, code) in exits.iter().enumerate() {
            if index > 0 {
                fx.advance(Duration::seconds(if index == 1 { 10 } else { 30 }));
            }
            assert!(
                fx.runner_gets(&task).is_empty(),
                "{case}: delivered before out ended"
            );
            let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
            assert_eq!(
                out.input.exit,
                Some(ScriptExit {
                    choice: Some("check_feedback".into()),
                    author: "automation".into(),
                    run_id: Some(check.run_id.to_string()),
                }),
                "{case}"
            );
            fx.finish(&out, *code);
        }

        assert_eq!(
            fx.phase(&task),
            Some(GateAutomationPhase::Released),
            "{case}"
        );
        let last = fx.hook_attempts(&task, "out").pop().unwrap();
        assert_eq!(
            (last.1, last.2, last.3),
            (exits != [0], false, Some(GateAutomationPhase::Released)),
            "{case}: the last attempt in the history"
        );
        assert_eq!(fx.runner_gets(&task), vec!["check_feedback"], "{case}");
        // The runner takes the answer: the next step starts, the entry goes.
        past_gates(&fx.service, &task);
        assert!(!fx.has_entry(&task, EPISODE), "{case}");
        assert!(fx.claim().is_none(), "{case}: no run after the exit");
    }
}

#[test]
fn a_manual_exit_is_held_like_an_answer_and_a_second_exit_is_refused() {
    // Covers AE3, R14: after a failed `in` the person chooses `revise_plan`;
    // an operator transition is held the same way.
    let cases = [
        ("person after a failed in", true),
        ("operator transition", false),
    ];
    for (case, person) in cases {
        let fx = Fixture::new(&[(LABEL, IN_OUT)]);
        let task = fx.waiting_task();
        let (choice, author) = if person {
            for delay in [0, 10, 30] {
                fx.advance(Duration::seconds(delay));
                let run = fx.claim_entry_script(LABEL);
                fx.finish(&run, 1);
            }
            assert_eq!(fx.phase(&task), Some(GateAutomationPhase::EntryFailed));
            fx.person_answers(&task, "revise_plan", json!({"note": "split it"}))
                .unwrap();
            ("revise_plan", "person")
        } else {
            let run = fx.claim_entry_script(LABEL);
            fx.finish(&run, 0);
            fx.operator_transition(&task, "check_work_feedback");
            ("check_work_feedback", "operator")
        };
        assert_eq!(
            fx.phase(&task),
            Some(GateAutomationPhase::Exiting),
            "{case}"
        );
        let second = fx.person_answers(&task, "check_feedback", json!({}));
        assert_eq!(
            refusal_code(second.unwrap_err()),
            "input_already_queued",
            "{case}"
        );
        assert!(
            fx.runner_gets(&task).is_empty(),
            "{case}: delivered before out"
        );

        let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
        let exit = out.input.exit.clone().unwrap();
        assert_eq!(
            (exit.choice.as_deref(), exit.author.as_str()),
            (Some(choice), author),
            "{case}"
        );
        fx.finish(&out, 0);

        assert_eq!(fx.runner_gets(&task), vec![choice], "{case}");
    }
}

#[test]
fn a_held_exit_survives_a_server_restart() {
    // The held exit lives on the task record, not in memory.
    let mut fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = ready_gate(&fx);
    fx.person_answers(&task, "check_feedback", json!({}))
        .unwrap();

    fx.restart();
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Exiting));
    assert!(fx.runner_gets(&task).is_empty());
    let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    assert_eq!(out.input.exit.as_ref().unwrap().author, "person");
    fx.finish(&out, 0);

    assert_eq!(fx.runner_gets(&task), vec!["check_feedback"]);
}

#[test]
fn a_task_archived_at_the_gate_ends_at_once_and_runs_out_once() {
    // Covers AE6, R18: no hold, one attempt, its failure in the history.
    let fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = ready_gate(&fx);

    fx.service
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
    assert!(fx.service.control_plane().task(&task).unwrap().has_ended());
    let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    assert_eq!(out.input.exit.as_ref().unwrap().author, "task_end");
    assert_eq!(out.input.exit.as_ref().unwrap().choice, None);
    fx.finish(&out, 1);

    let attempts = fx.hook_attempts(&task, "out");
    assert_eq!(
        attempts,
        vec![(
            LABEL.to_string(),
            true,
            false,
            Some(GateAutomationPhase::Released)
        )]
    );
    fx.advance(Duration::seconds(60));
    assert!(fx.claim().is_none(), "no second attempt");
    assert!(!fx.has_entry(&task, EPISODE));
}

#[test]
fn out_of_a_left_episode_ends_before_in_of_the_next_one() {
    // Covers AE4: the runner leaves the gate with no held exit (defensive)
    // and loops back to it; `out` of the first episode runs after the fact,
    // and `in` of the second waits for it.
    let fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = ready_gate(&fx);
    past_gates(&fx.service, &task);
    wait_at(&fx.service, &task, "input-2", &review_gate(QUEUE));
    assert!(
        fx.has_entry(&task, EPISODE),
        "the left episode keeps its entry"
    );

    let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    assert_eq!(out.input.exit.as_ref().unwrap().author, "runner");
    assert!(
        fx.claim().is_none(),
        "in of the next episode while out runs"
    );
    fx.finish(&out, 0);

    assert!(!fx.has_entry(&task, EPISODE));
    fx.claim_hook(HookPhase::In, LABEL, "input-2");
}

// ---------------------------------------------------------------------------
// Restart of a waiting gate (U7)
// ---------------------------------------------------------------------------

#[test]
fn a_restart_runs_out_of_the_old_episode_then_in_of_the_new_one() {
    // Covers R16, F2 and the plan's verification: a gate whose `in` failed
    // is recovered by a restart, with no new task.
    let fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = fx.waiting_task();
    for delay in [0, 10, 30] {
        fx.advance(Duration::seconds(delay));
        let run = fx.claim_entry_script(LABEL);
        fx.finish(&run, 1);
    }
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::EntryFailed));

    let restart = fx.operator_restart(&task).expect("the restart is queued");
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Exiting));
    assert!(
        fx.runner_gets(&task).is_empty(),
        "the restart reaches the runner before out ended"
    );
    let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    let exit = out.input.exit.clone().unwrap();
    assert_eq!((exit.choice, exit.author.as_str()), (None, "operator"));
    fx.finish(&out, 0);
    assert_eq!(fx.runner_gets(&task), vec!["restart"]);

    // The runner restarts the step: it asks again, in a new episode.
    fx.service
        .control_plane()
        .settle_operator_command(&task, FIRST_RUNNER, 1, &restart, true, None)
        .unwrap();
    wait_at(&fx.service, &task, "input-2", &review_gate(QUEUE));
    assert!(!fx.has_entry(&task, EPISODE), "the old entry is removed");
    assert!(fx.has_entry(&task, "input-2"), "the new entry is added");
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Entering));
    // The tick flags the pair while the new `in` runs: no `check` yet.
    fx.service.observer_pass();
    let entry = fx.claim_hook(HookPhase::In, LABEL, "input-2");
    assert!(fx.claim().is_none(), "check before the new in ended");
    fx.finish(&entry, 0);

    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready));
    let check = fx.next_tick_run().expect("a check run of the new episode");
    assert_eq!(check.phase, HookPhase::Check);
    let checked = check.input.entry.as_ref().unwrap();
    assert_eq!(
        (&checked.task_id, checked.episode.as_str()),
        (&task, "input-2")
    );
}

#[test]
fn a_restart_while_an_exit_is_held_is_refused() {
    // The first exit wins (R10): the person's answer waits for `out`.
    let fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = ready_gate(&fx);
    fx.person_answers(&task, "check_feedback", json!({}))
        .unwrap();

    let refused = fx.operator_restart(&task).unwrap_err();

    assert!(refused.contains("first exit wins"), "{refused}");
    let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    fx.finish(&out, 0);
    assert_eq!(fx.runner_gets(&task), vec!["check_feedback"]);
}

// ---------------------------------------------------------------------------
// Held exits that do not happen, and recovery of exit scripts
// ---------------------------------------------------------------------------

#[test]
fn a_held_exit_the_runner_refuses_returns_the_gate_to_ready() {
    // The runner refuses the held operator transition after `out` ran: the
    // task still waits in the episode, so `check` runs again, and the next
    // exit is held and runs `out` again (R13: `out` is idempotent).
    let fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = ready_gate(&fx);
    fx.operator_transition(&task, "check_work_feedback");
    let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    fx.finish(&out, 0);
    let control = fx.service.control_plane();
    let (commands, _) = control
        .deliver_operator_commands(&task, FIRST_RUNNER, 1)
        .unwrap();
    control
        .settle_operator_command(
            &task,
            FIRST_RUNNER,
            1,
            &commands[0].command_id,
            false,
            Some("no such transition".into()),
        )
        .unwrap();

    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready));
    let warned = control
        .events_since(&task, 0)
        .unwrap()
        .into_iter()
        .any(|event| matches!(event.kind, TaskEventKind::GateQueueWarning { .. }));
    assert!(warned, "the history says the exit did not happen");
    let check = fx.next_tick_run().expect("check runs again");
    assert_eq!(check.phase, HookPhase::Check);
    fx.finish(&check, 0);
    fx.person_answers(&task, "check_feedback", json!({}))
        .unwrap();
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Exiting));
    assert!(fx.runner_gets(&task).is_empty(), "delivered before out");
    let again = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    assert_eq!(again.input.exit.as_ref().unwrap().author, "person");
    fx.finish(&again, 0);
    assert_eq!(fx.runner_gets(&task), vec!["check_feedback"]);
}

#[test]
fn an_exit_script_run_of_an_exit_that_did_not_happen_does_not_count_for_the_next_exit() {
    // The held transition is refused while its `out` still runs: the gate
    // is back to ready and the person's answer is the new exit. The old run
    // ran with the old choice, so its end does not release the new exit:
    // `out` runs again with the person's choice first.
    let fx = Fixture::new(&[(LABEL, IN_OUT)]);
    let task = ready_gate(&fx);
    fx.operator_transition(&task, "check_work_feedback");
    let old = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    let control = fx.service.control_plane();
    let command_id = control.task(&task).unwrap().operator_commands[0]
        .command_id
        .clone();
    control
        .settle_operator_command(&task, FIRST_RUNNER, 1, &command_id, false, None)
        .unwrap();
    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready));
    fx.person_answers(&task, "check_feedback", json!({}))
        .unwrap();

    fx.finish(&old, 0);

    assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Exiting));
    assert!(fx.runner_gets(&task).is_empty(), "delivered before out");
    let new = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
    assert_eq!(
        new.input.exit.as_ref().unwrap().choice.as_deref(),
        Some("check_feedback")
    );
    fx.finish(&new, 0);
    assert_eq!(fx.runner_gets(&task), vec!["check_feedback"]);
}

#[test]
fn a_held_answer_survives_a_runner_report_without_a_request_while_out_runs() {
    // A runner sends no pending input while a pass is active or while it
    // reports a problem; the held answer is not obsolete for that, while
    // `out` runs nor once it ended and the answer waits for the next poll.
    for (problem, after_out) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = format!("problem {problem}, after out {after_out}");
        let fx = Fixture::new(&[(LABEL, IN_OUT)]);
        let task = ready_gate(&fx);
        fx.person_answers(&task, "check_feedback", json!({}))
            .unwrap();
        let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
        if after_out {
            fx.finish(&out, 0);
            assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Released));
        }
        if problem {
            runner_problem(&fx.service, &task);
        } else {
            fx.service
                .control_plane()
                .deliver_inputs(&task, FIRST_RUNNER, 1, None)
                .unwrap();
        }
        if !after_out {
            fx.finish(&out, 0);
        }
        if problem {
            wait_at(&fx.service, &task, EPISODE, &review_gate(QUEUE));
        }

        assert_eq!(fx.runner_gets(&task), vec!["check_feedback"], "{case}");
        assert_eq!(
            fx.hook_attempts(&task, "out").len(),
            1,
            "{case}: out ran once"
        );
    }
}

#[test]
fn a_missing_entry_of_an_episode_with_pending_out_is_added_back() {
    // The entry is the index the hook claim walks: without it `out` never
    // runs, and `in` of the task's next episode waits for ever.
    for archived in [false, true] {
        let fx = Fixture::new(&[(LABEL, IN_OUT)]);
        let task = ready_gate(&fx);
        if archived {
            fx.service
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
        } else {
            past_gates(&fx.service, &task);
            wait_at(&fx.service, &task, "input-2", &review_gate(QUEUE));
        }
        let store = fx.service.observers();
        let lock = store.lock_queue_entries().unwrap();
        assert!(store
            .remove_queue_entry(&lock, &queue_entry_id(&task, EPISODE))
            .unwrap());
        drop(lock);
        assert!(fx.claim().is_none(), "archived {archived}: nothing to run");

        fx.advance(Duration::seconds(300));
        fx.service.observer_pass();
        let out = fx.claim_hook(HookPhase::Out, LABEL, EPISODE);
        fx.finish(&out, 0);

        if !archived {
            fx.claim_hook(HookPhase::In, LABEL, "input-2");
        }
        assert!(!fx.has_entry(&task, EPISODE), "archived {archived}");
    }
}

#[test]
fn a_pending_entry_script_waits_while_a_catalogue_file_cannot_be_read() {
    // An editor may save a broken file for a moment: the script is neither
    // skipped nor run until the file is fixed, and the history says once
    // which file it waits for. A problem that leaves the pair usable (a
    // second file of the same queue) holds nothing back.
    let duplicate = format!("queues/{QUEUE}.yml");
    let cases = [
        ("queue file", format!("queues/{QUEUE}.yaml"), true),
        (
            "automation file",
            format!("automations/{LABEL}/automation.yaml"),
            true,
        ),
        ("duplicate queue file", duplicate, false),
    ];
    for (case, file, waits) in cases {
        let fx = Fixture::new(&[(LABEL, IN_OUT)]);
        let good = fs::read_to_string(fx.root.join(format!("queues/{QUEUE}.yaml"))).unwrap();
        let task = fx.waiting_task();
        if !waits {
            write(&fx.root, &file, &good);
            let run = fx.claim_entry_script(LABEL);
            fx.finish(&run, 0);
            assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready), "{case}");
            assert!(fx.waiting_warnings(&task).is_empty(), "{case}");
            continue;
        }
        let good = fs::read_to_string(fx.root.join(&file)).unwrap();
        write(&fx.root, &file, "automations: [unclosed\n");

        assert!(fx.claim().is_none(), "{case}: claimed with a broken file");
        assert!(fx.claim().is_none(), "{case}: claimed with a broken file");
        assert!(
            fx.attempts(&task).is_empty(),
            "{case}: the script was skipped"
        );
        assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Entering));
        let warnings = fx.waiting_warnings(&task);
        assert_eq!(warnings.len(), 1, "{case}: {warnings:?}");
        assert!(warnings[0].contains(&file), "{case}: {}", warnings[0]);

        write(&fx.root, &file, &good);
        let run = fx.claim_entry_script(LABEL);
        fx.finish(&run, 0);
        assert_eq!(fx.phase(&task), Some(GateAutomationPhase::Ready), "{case}");
    }
}

/// What makes a run claimable in [`a_waiting_claim_returns_a_run_as_soon_as_it_becomes_due`].
#[derive(Debug, Clone, Copy)]
enum Trigger {
    /// The `check` run of one entry ends: the pair is free for the next.
    CheckRunEnds,
    /// The `in` of one automation ends: the next automation's `in` is due.
    EntryScriptEnds,
    /// The person answers the gate: the exit is held, `out` is due.
    PersonAnswers,
    /// A `check` run answers the gate (`task decide`).
    CheckDecides,
    /// The owner restarts the gate step through the operator route.
    OperatorRestarts,
    /// The owner archives the task at the gate.
    Archive,
}

#[tokio::test]
async fn a_waiting_claim_returns_a_run_as_soon_as_it_becomes_due() {
    // U6: every change that makes a run claimable wakes a waiting claim.
    // The `in` row also covers the wake of the attempt's end (settle_hook):
    // it is on the same call as the wake of the run's result.
    use Trigger::*;
    let cases = [
        (CheckRunEnds, HookPhase::Check),
        (EntryScriptEnds, HookPhase::In),
        (PersonAnswers, HookPhase::Out),
        (CheckDecides, HookPhase::Out),
        (OperatorRestarts, HookPhase::Out),
        (Archive, HookPhase::Out),
    ];
    for (trigger, expected) in cases {
        let (fx, fire) = trigger_setup(trigger);
        let mut claim = tokio::spawn(Arc::clone(&fx.service).claim_observer_run_waiting());
        let waits = tokio::time::timeout(StdDuration::from_millis(200), &mut claim).await;
        assert!(
            waits.is_err(),
            "{trigger:?}: a run was due before the trigger"
        );

        fire(&fx);

        let run = tokio::time::timeout(StdDuration::from_secs(5), claim)
            .await
            .unwrap_or_else(|_| panic!("{trigger:?}: the claim was not woken"))
            .unwrap()
            .unwrap()
            .unwrap_or_else(|| panic!("{trigger:?}: no run"));
        assert_eq!(run.phase, expected, "{trigger:?}");
    }
}

/// The action that fires a [`Trigger`] on its fixture.
type Fire = Box<dyn Fn(&Fixture)>;

/// A fixture where nothing is claimable until `trigger` fires, and the
/// action that fires it.
fn trigger_setup(trigger: Trigger) -> (Fixture, Fire) {
    match trigger {
        Trigger::CheckRunEnds => {
            let fx = Fixture::new(&[("jira-review", &[])]);
            fx.waiting_task();
            let other = queued_task(&fx.service, "task-2", &owner());
            running(&fx.service, &other);
            wait_at(&fx.service, &other, EPISODE, &review_gate(QUEUE));
            fx.service.observer_pass();
            let first = fx.claim().expect("the first check run");
            (fx, Box::new(move |fx| fx.finish(&first, 0)))
        }
        Trigger::EntryScriptEnds => {
            let fx = Fixture::new(&[(LABEL, &[HookPhase::In]), ("sonar-gate", &[HookPhase::In])]);
            fx.waiting_task();
            let first = fx.claim_entry_script(LABEL);
            (fx, Box::new(move |fx| fx.finish(&first, 0)))
        }
        Trigger::PersonAnswers => {
            let fx = Fixture::new(&[(LABEL, IN_OUT)]);
            let task = ready_gate(&fx);
            (
                fx,
                Box::new(move |fx| {
                    fx.person_answers(&task, "check_feedback", json!({}))
                        .unwrap()
                }),
            )
        }
        Trigger::CheckDecides => {
            let fx = Fixture::new(&[(LABEL, IN_OUT)]);
            ready_gate(&fx);
            let check = fx.next_tick_run().expect("a check run");
            (
                fx,
                Box::new(move |fx| {
                    let entry = check.input.entry.as_ref().unwrap();
                    fx.service
                        .observer_decide(
                            &check.run_id,
                            ObserverDecideRequest {
                                task_id: entry.task_id.to_string(),
                                episode: entry.episode.clone(),
                                choice: "check_feedback".into(),
                                reason: None,
                                variables: Default::default(),
                            },
                        )
                        .unwrap();
                }),
            )
        }
        Trigger::OperatorRestarts => {
            let fx = Fixture::new(&[(LABEL, IN_OUT)]);
            let task = ready_gate(&fx);
            (
                fx,
                Box::new(move |fx| {
                    fx.service
                        .submit_operator_command(
                            &owner(),
                            task.as_str(),
                            SubmitOperatorCommandRequest {
                                command: OperatorCommand::RestartStep {
                                    note: None,
                                    failure_key: None,
                                },
                                idempotency_key: "restart-1".into(),
                                expected_generation: None,
                            },
                        )
                        .unwrap();
                }),
            )
        }
        Trigger::Archive => {
            let fx = Fixture::new(&[(LABEL, IN_OUT)]);
            let task = ready_gate(&fx);
            (
                fx,
                Box::new(move |fx| {
                    fx.service
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
                }),
            )
        }
    }
}
