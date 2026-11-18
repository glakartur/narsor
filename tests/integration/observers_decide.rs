//! Integration tests of the answer route of an automation's `check` script
//! (U6, KTD-6): `task decide` answers the gate of the queue entry its run
//! was started for, with the validation of a person's answer (R13), as the
//! automation and its run (R15), and the first answer of an episode wins.
//!
//! A real `ProtocolService` over a seeded catalogue (the `on_episodes` queue
//! `mr-review`) and a manual clock; the runner channel is reached through
//! its seam (`gate_support`).

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use narsor_engine::controller_protocol::{
    ClaimedObserverRun, OwnerCommandRequest, ProtocolError, ProtocolService, RunOutcome, RunReport,
    SubmitInputRequest,
};
use narsor_engine::domain::TaskId;
use narsor_engine::orchestration::{
    Clock, CommandRequest, ControlPlane, Observation, OwnerCommand, OwnerRef, RunnerState,
    TaskEventKind,
};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::api::HumanInputDeliveryDto;
use narsor_protocol::controller::{
    ObserverDecideRequest, ObserverEffectOutcome, TaskEvent, TaskProvenance,
};
use serde_json::{json, Map, Value};

use crate::automation_support::{always_queue, automation, on_episodes_queue, CHECK_SCRIPT};
use crate::gate_support::{
    observe, past_gates, queued_task, review_gate, runner_problem, running, snapshot, wait_at,
    FIRST_RUNNER,
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

/// A catalogue and state root, a clock, and a service over both.
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
        let root = std::env::temp_dir().join(format!("narsor-obs-decide-{}", uuid::Uuid::new_v4()));
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

    /// [`QUEUE`] (`on_episodes`, every 300 s) served by `automations`.
    fn served_by(&self, automations: &[&str]) {
        for name in automations {
            automation(&self.root, name, CHECK_SCRIPT, &[]);
        }
        on_episodes_queue(&self.root, QUEUE, automations, "{interval_secs: 300}");
    }

    /// A running task waiting at the `review_work` gate on [`QUEUE`] in
    /// `episode`.
    fn waiting_task(&self, id: &str, episode: &str) -> TaskId {
        let task = queued_task(&self.service, id, &owner());
        running(&self.service, &task);
        wait_at(&self.service, &task, episode, &review_gate(QUEUE));
        task
    }

    /// The next run the schedule hands out.
    fn claim(&self) -> ClaimedObserverRun {
        self.service
            .claim_observer_run()
            .unwrap()
            .expect("a run is due")
    }

    /// A run for the entry of a waiting task.
    fn claimed_run(&self) -> ClaimedObserverRun {
        self.service.observer_pass();
        self.claim()
    }

    fn decide(
        &self,
        run: &ClaimedObserverRun,
        request: ObserverDecideRequest,
    ) -> Result<narsor_protocol::controller::ObserverEffectResponse, ProtocolError> {
        self.service.observer_decide(&run.run_id, request)
    }

    fn history(&self, task: &TaskId) -> Vec<TaskEvent> {
        self.service.control_plane().events_since(task, 0).unwrap()
    }

    fn inputs(&self, task: &TaskId) -> usize {
        self.service
            .control_plane()
            .task(task)
            .unwrap()
            .inputs
            .len()
    }
}

/// The answer of `run`'s entry with `choice`, `reason` and `variables`.
fn answer(
    run: &ClaimedObserverRun,
    choice: &str,
    reason: Option<&str>,
    variables: Value,
) -> ObserverDecideRequest {
    let entry = run.input.entry.as_ref().expect("a run for an entry");
    ObserverDecideRequest {
        task_id: entry.task_id.to_string(),
        episode: entry.episode.clone(),
        choice: choice.to_string(),
        reason: reason.map(str::to_string),
        variables: variables.as_object().cloned().unwrap_or_default(),
    }
}

/// The code of a coded refusal; panics on any other.
fn refusal(error: ProtocolError) -> (u16, &'static str, String) {
    match error {
        ProtocolError::BadRequest { code, message } => (400, code, message),
        ProtocolError::Conflict { code, message } => (409, code, message),
        other => panic!("not a coded refusal: {other:?}"),
    }
}

/// The person's answer to the gate in `episode`, through the owner's route.
fn person_answers(fx: &Fixture, task: &TaskId, episode: &str, choice: &str, values: Value) {
    fx.service
        .submit_input(
            &owner(),
            task.as_str(),
            SubmitInputRequest {
                pending_input_id: episode.to_string(),
                idempotency_key: "person-1".to_string(),
                delivery: HumanInputDeliveryDto {
                    chosen_label: Some(choice.to_string()),
                    values: values.as_object().cloned().unwrap_or_default(),
                    episode: Some(episode.to_string()),
                    ..HumanInputDeliveryDto::default()
                },
            },
        )
        .expect("the person's answer is queued");
}

// ---------------------------------------------------------------------------
// An answer that is taken (R12, R13, R15, R22)
// ---------------------------------------------------------------------------

#[test]
fn a_script_answer_is_queued_as_the_automations_and_its_reason_is_kept() {
    // Covers AE1 up to the runner: the script chooses `check_feedback` with
    // a reason; the input names the queue, the automation and its run.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();

    let response = fx
        .decide(
            &run,
            answer(&run, "check_feedback", Some("label removed"), json!({})),
        )
        .unwrap();

    assert_eq!(response.outcome, ObserverEffectOutcome::Delivered);
    assert_eq!(response.task_id.as_deref(), Some("task-1"));
    assert!(
        response.message.contains("review_work") && response.message.contains("check_feedback"),
        "{}",
        response.message
    );
    let provenance = TaskProvenance {
        observer_id: QUEUE.to_string(),
        observer_name: "gitlab-mr".to_string(),
        run_id: run.run_id.to_string(),
    };
    let record = fx.service.control_plane().task(&task).unwrap();
    let input = &record.inputs[0];
    assert_eq!(Some(input.input_id.clone()), response.input_id);
    assert_eq!(input.pending_input_id, "input-1");
    assert_eq!(input.observer.as_ref(), Some(&provenance));
    let delivery = input.delivery.as_ref().expect("the answer waits");
    assert_eq!(delivery.chosen_label.as_deref(), Some("check_feedback"));
    assert_eq!(delivery.comment.as_deref(), Some("label removed"));
    assert_eq!(delivery.episode.as_deref(), Some("input-1"));
    let queued = fx
        .history(&task)
        .into_iter()
        .find_map(|event| match event.kind {
            TaskEventKind::InputQueued { observer, .. } => Some(observer),
            _ => None,
        });
    assert_eq!(queued, Some(Some(provenance.clone())));
    let line = narsor::cli::task_text::event_line(
        &fx.history(&task)
            .into_iter()
            .find(|event| matches!(event.kind, TaskEventKind::InputQueued { .. }))
            .unwrap(),
    );
    assert!(
        line.contains(&format!(
            "queued by observer gitlab-mr (run {})",
            run.run_id
        )),
        "{line}"
    );

    // The runner takes it and leaves the gate: the entry goes (R16).
    past_gates(&fx.service, &task);
    assert!(fx
        .service
        .observers()
        .queue_entries(&owner(), QUEUE)
        .unwrap()
        .is_empty());
}

#[test]
fn a_choice_with_its_required_variable_is_taken() {
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();

    fx.decide(
        &run,
        answer(
            &run,
            "revise_plan",
            None,
            json!({"note": "split the change"}),
        ),
    )
    .unwrap();

    let record = fx.service.control_plane().task(&task).unwrap();
    let delivery = record.inputs[0].delivery.as_ref().unwrap();
    let mut values = Map::new();
    values.insert("note".into(), json!("split the change"));
    assert_eq!(delivery.values, values);
    assert_eq!(delivery.comment, None);
}

#[test]
fn a_retry_of_the_same_answer_returns_its_input_and_another_answer_is_refused() {
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();
    let first = answer(&run, "check_feedback", Some("label removed"), json!({}));

    let taken = fx.decide(&run, first.clone()).unwrap();
    let retried = fx.decide(&run, first).unwrap();
    let other = fx.decide(
        &run,
        answer(&run, "revise_plan", None, json!({"note": "x"})),
    );

    assert_eq!(retried.input_id, taken.input_id);
    let (status, code, _) = refusal(other.unwrap_err());
    assert_eq!((status, code), (409, "idempotency_key_reused"));
    assert_eq!(fx.inputs(&task), 1);
}

// ---------------------------------------------------------------------------
// First wins, and a stale episode (R13, AE2)
// ---------------------------------------------------------------------------

#[test]
fn a_script_answer_after_the_persons_answer_is_refused() {
    // Covers AE2: the person chooses first; the script's answer for the
    // same episode is refused, before and after the gate exits.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();
    person_answers(&fx, &task, "input-1", "revise_plan", json!({"note": "x"}));

    let queued = fx.decide(&run, answer(&run, "check_feedback", None, json!({})));
    past_gates(&fx.service, &task);
    let exited = fx.decide(&run, answer(&run, "check_feedback", None, json!({})));

    let (status, code, _) = refusal(queued.unwrap_err());
    assert_eq!((status, code), (409, "input_already_queued"));
    let (status, code, message) = refusal(exited.unwrap_err());
    assert_eq!((status, code), (409, "stale_episode"));
    assert!(message.contains("input-1"), "{message}");
    assert_eq!(fx.inputs(&task), 1);
}

#[test]
fn of_two_scripts_answering_one_episode_the_first_wins() {
    // R8, R12: two automations of the queue run for the same entry; the
    // second answer is refused.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr", "github-pr"]);
    let task = fx.waiting_task("task-1", "input-1");
    fx.service.observer_pass();
    let first = fx.claim();
    let second = fx.claim();
    assert_ne!(first.run_id.key.automation, second.run_id.key.automation);

    let taken = fx.decide(&first, answer(&first, "check_feedback", None, json!({})));
    let refused = fx.decide(&second, answer(&second, "check_feedback", None, json!({})));

    assert_eq!(taken.unwrap().outcome, ObserverEffectOutcome::Delivered);
    let (status, code, _) = refusal(refused.unwrap_err());
    assert_eq!((status, code), (409, "input_already_queued"));
    assert_eq!(fx.inputs(&task), 1);
}

#[test]
fn an_answer_the_runner_already_took_ends_the_episode_for_later_answers() {
    // First wins also between the runner taking an answer and its next
    // snapshot: a consumed input closes the episode.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr", "github-pr"]);
    let task = fx.waiting_task("task-1", "input-1");
    fx.service.observer_pass();
    let first = fx.claim();
    let second = fx.claim();
    let taken = fx
        .decide(&first, answer(&first, "check_feedback", None, json!({})))
        .unwrap();
    let control = fx.service.control_plane();
    let input_id = taken.input_id.unwrap();
    let (delivered, _) = control
        .deliver_inputs(&task, FIRST_RUNNER, 1, Some("input-1"))
        .expect("the runner polls");
    // What the runner gets names the automation's run (R15).
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0].observer.as_ref().map(|by| by.run_id.clone()),
        Some(first.run_id.to_string())
    );
    control
        .settle_input(&task, FIRST_RUNNER, 1, &input_id, true, None)
        .expect("the runner takes the answer");

    let late = fx.decide(&second, answer(&second, "check_feedback", None, json!({})));

    let (status, code, _) = refusal(late.unwrap_err());
    assert_eq!((status, code), (409, "input_already_queued"));
}

#[test]
fn an_answer_while_the_runner_reports_a_problem_is_refused_as_unavailable() {
    // R17: the gate still waits, so the refusal does not say it was
    // answered or left.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();
    runner_problem(&fx.service, &task);

    let refused = fx.decide(&run, answer(&run, "check_feedback", None, json!({})));

    let (status, code, message) = refusal(refused.unwrap_err());
    assert_eq!((status, code), (409, "gate_unavailable"));
    assert!(message.contains("still waits"), "{message}");
    assert_eq!(fx.inputs(&task), 0);
}

#[test]
fn an_entry_is_not_run_during_a_runner_problem_and_is_answered_after_it() {
    // R17: the entry stays while the runner reports a problem, but no
    // script runs for it, since no answer could be taken. Once the runner
    // is back in the episode, the entry runs and its answer is queued.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    runner_problem(&fx.service, &task);

    fx.service.observer_pass();
    let during = fx.service.claim_observer_run().unwrap();

    assert!(during.is_none(), "{during:?}");
    assert_eq!(
        fx.service
            .observers()
            .queue_entries(&owner(), QUEUE)
            .unwrap()
            .len(),
        1,
        "the entry is kept"
    );
    *fx.clock.0.lock().unwrap() +=
        chrono::Duration::seconds(narsor_engine::observers::DEFAULT_INTERVAL_SECS as i64);
    wait_at(&fx.service, &task, "input-1", &review_gate(QUEUE));

    let run = fx.claimed_run();
    let taken = fx
        .decide(&run, answer(&run, "check_feedback", None, json!({})))
        .unwrap();

    assert_eq!(taken.outcome, ObserverEffectOutcome::Delivered);
    assert_eq!(fx.inputs(&task), 1);
}

#[test]
fn an_answer_for_an_archived_task_is_refused_as_stale() {
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();
    fx.service
        .control_plane()
        .command(&task, CommandRequest::new(OwnerCommand::Archive, "archive"))
        .unwrap();

    let refused = fx.decide(&run, answer(&run, "check_feedback", None, json!({})));

    let (status, code, _) = refusal(refused.unwrap_err());
    assert_eq!((status, code), (409, "stale_episode"));
}

// ---------------------------------------------------------------------------
// The person's validation (R13)
// ---------------------------------------------------------------------------

#[test]
fn an_answer_the_gate_does_not_accept_is_refused_and_queues_nothing() {
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();

    let cases = [
        (answer(&run, "merge", None, json!({})), "merge"),
        (answer(&run, "revise_plan", None, json!({})), "note"),
        (
            answer(&run, "revise_plan", None, json!({"note": "   "})),
            "note",
        ),
        (
            answer(&run, "check_feedback", None, json!({"verdict": "ok"})),
            "verdict",
        ),
    ];
    for (request, named) in cases {
        let choice = request.choice.clone();
        let (status, code, message) = refusal(fx.decide(&run, request).unwrap_err());
        assert_eq!(
            (status, code),
            (400, "invalid_answer"),
            "{choice}: {message}"
        );
        assert!(message.contains(named), "{choice}: {message}");
    }
    assert_eq!(fx.inputs(&task), 0);
    assert!(fx
        .history(&task)
        .iter()
        .all(|event| !matches!(event.kind, TaskEventKind::InputQueued { .. })));
}

// ---------------------------------------------------------------------------
// A run answers its own entry only (KTD-6)
// ---------------------------------------------------------------------------

#[test]
fn a_run_answers_only_the_entry_it_was_started_for() {
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let first = fx.waiting_task("task-1", "input-1");
    let second = fx.waiting_task("task-2", "input-7");
    let run = fx.claimed_run();
    let entry_task = run.input.entry.as_ref().unwrap().task_id.clone();
    let other = if entry_task == first { &second } else { &first };

    let mut elsewhere = answer(&run, "check_feedback", None, json!({}));
    elsewhere.task_id = other.to_string();
    let mut old_episode = answer(&run, "check_feedback", None, json!({}));
    old_episode.episode = "input-0".to_string();

    for request in [elsewhere, old_episode] {
        let (status, code, _) = refusal(fx.decide(&run, request).unwrap_err());
        assert_eq!((status, code), (400, "not_the_runs_entry"));
    }
    assert_eq!(fx.inputs(&first) + fx.inputs(&second), 0);
}

#[test]
fn an_always_run_answers_no_gate() {
    // R27: an `always` run has no entry.
    let fx = Fixture::new();
    automation(&fx.root, "jira-new", CHECK_SCRIPT, &[]);
    always_queue(
        &fx.root,
        "jira-intake",
        "jira-new",
        "{interval_secs: 300}",
        "",
    );
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();
    assert!(run.input.entry.is_none());

    let refused = fx.service.observer_decide(
        &run.run_id,
        ObserverDecideRequest {
            task_id: task.to_string(),
            episode: "input-1".to_string(),
            choice: "check_feedback".to_string(),
            reason: None,
            variables: Map::new(),
        },
    );

    let (status, code, _) = refusal(refused.unwrap_err());
    assert_eq!((status, code), (400, "no_queue_entry"));
    assert_eq!(fx.inputs(&task), 0);
}

#[test]
fn a_run_that_ended_can_answer_no_more() {
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = fx.waiting_task("task-1", "input-1");
    let run = fx.claimed_run();
    fx.service
        .finish_observer_run(
            &run.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 0 },
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .unwrap();

    let refused = fx.decide(&run, answer(&run, "check_feedback", None, json!({})));

    assert!(matches!(refused, Err(ProtocolError::Unauthorized)));
    assert_eq!(fx.inputs(&task), 0);
}

// ---------------------------------------------------------------------------
// A paused task (AE5)
// ---------------------------------------------------------------------------

#[test]
fn a_script_answer_resumes_a_paused_task() {
    // Covers AE5: the task was paused at the gate; the answer waits on the
    // record and the task is resumed to take it, as after a person's answer.
    let fx = Fixture::new();
    fx.served_by(&["gitlab-mr"]);
    let task = queued_task(&fx.service, "task-1", &owner());
    let lease = running(&fx.service, &task);
    let gate = review_gate(QUEUE);
    wait_at(&fx.service, &task, "input-1", &gate);
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
    fx.service
        .control_plane()
        .record_runner_snapshot(&task, FIRST_RUNNER, 1, at_safe_point)
        .unwrap();
    let paused = observe(&fx.service, &task, &lease, Observation::PausedAtSafePoint);
    assert_eq!(paused.runner_state, RunnerState::Stopped);
    let run = fx.claimed_run();

    fx.decide(
        &run,
        answer(&run, "check_feedback", Some("label removed"), json!({})),
    )
    .unwrap();

    let record = fx.service.control_plane().task(&task).unwrap();
    assert_ne!(record.runner_state, RunnerState::Stopped);
    assert_eq!(record.inputs.len(), 1);
    assert!(fx.history(&task).iter().any(|event| matches!(
        &event.kind,
        TaskEventKind::OwnerCommandAccepted {
            command: OwnerCommand::Resume,
            idempotency_key,
        } if idempotency_key.starts_with("work-resume:observer:")
    )));
}
