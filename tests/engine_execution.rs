//! Integration tests for the engine loop (U7).
//!
//! Every test runs a real workflow against a real store and real child
//! processes. What is asserted is mostly the *event log*: it is the contract
//! recovery is derived from, so a change in the order of emission has to break
//! a test here rather than surface as a mysterious `Unknown` execution later.

mod engine_support;

use std::sync::Arc;
use std::time::Duration;

use engine_support::{
    event_type, workflow, workflow_file, AnsweringAgentExecutor, Harness, LabellessAgentExecutor,
    QuestioningAgentExecutor, ResumeRejectingAgentExecutor, StubAgentExecutor, TimeoutRecorder,
};
use narsor_engine::domain::{ActionResult, EventData, ExecutionStatus};
use narsor_engine::engine::actions::human::{RequiredValue, RequiredValueKind};
use narsor_engine::engine::{
    ActionExecutor, ActionKind, AgentDefaults, Engine, EngineError, HumanInputDelivery,
    ManualTransition,
};
use narsor_protocol::api::PendingHumanInputView;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_linear_workflow_runs_to_its_terminal_state() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("linear");

    let execution = engine
        .start_and_run(
            &definition,
            json!({ "subject": "orchestration" }),
            harness.workspace(),
        )
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("completed"));
    assert!(execution.completed_at.is_some());

    // Templates are expanded on entering the state, so the third state sees the
    // variable the first one set.
    let variables = &execution.context.variables;
    assert_eq!(variables.get("first"), Some(&json!("one")));
    assert_eq!(variables.get("second"), Some(&json!("orchestration")));
    assert_eq!(variables.get("third"), Some(&json!("after one")));
}

#[tokio::test]
async fn a_later_step_receives_the_previous_agent_reply() {
    let harness = Harness::new();
    let agent = Arc::new(AnsweringAgentExecutor::new(vec![
        ("findings", json!({}), "P1: validate the request body"),
        ("done", json!({}), "fixed"),
    ]));
    let engine = harness
        .engine()
        .with_executor(narsor_engine::engine::ActionKind::Agent, agent.clone());
    let definition = workflow("step_reply");
    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = agent.prompts();
    assert!(
        prompts[1].contains("P1: validate the request body"),
        "{}",
        prompts[1]
    );
}

#[tokio::test]
async fn an_entry_limit_waits_before_starting_the_limited_action_and_can_continue() {
    let harness = Harness::new();
    let agent = Arc::new(AnsweringAgentExecutor::new(vec![
        ("findings", json!({}), "first findings"),
        ("fixed", json!({}), "fixed once"),
        ("findings", json!({}), "second findings"),
        ("fixed", json!({}), "fixed twice"),
        ("clean", json!({ "summary": "clean now" }), "clean now"),
    ]));
    let engine = harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone());
    let definition = workflow("entry_limit");
    let mut execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("review"));
    assert_eq!(execution.metadata["engine"]["state_entries"]["review"], 2);
    assert_eq!(
        agent.prompts().len(),
        4,
        "the limited third entry has not started"
    );
    let pending = harness
        .store()
        .load_pending_input::<narsor_engine::engine::PendingHumanInput>(&execution.id)
        .unwrap()
        .expect("entry-limit decision is durable");
    assert_eq!(
        pending.continuation,
        narsor_engine::engine::InputContinuation::EntryLimit
    );
    let delivery = HumanInputDelivery::with_values(Map::from_iter([(
        "entry_limit_decision".to_string(),
        json!("continue"),
    )]))
    .for_episode(pending.episode_key());
    let recovered_engine = harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone());
    execution = recovered_engine
        .deliver_human_input(&definition, &execution.id, &delivery)
        .await
        .unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Completed,
        "state={:?}, entries={}, events={:?}",
        execution.current_state,
        execution.metadata["engine"]["state_entries"]["review"],
        harness
            .events(&execution.id)
            .iter()
            .map(event_type)
            .collect::<Vec<_>>()
    );
    assert_eq!(execution.metadata["engine"]["state_entries"]["review"], 3);
    assert_eq!(agent.prompts().len(), 5);
}

#[tokio::test]
async fn an_entry_limit_can_reset_or_choose_an_output_with_required_variables() {
    let harness = Harness::new();
    let agent = Arc::new(AnsweringAgentExecutor::new(vec![
        ("findings", json!({}), "first findings"),
        ("fixed", json!({}), "fixed once"),
        ("findings", json!({}), "second findings"),
        ("fixed", json!({}), "fixed twice"),
    ]));
    let engine = harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone());
    let definition = workflow("entry_limit");
    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .unwrap();
    let pending = harness
        .store()
        .load_pending_input::<narsor_engine::engine::PendingHumanInput>(&execution.id)
        .unwrap()
        .unwrap();

    let missing = HumanInputDelivery::label("manual_complete").for_episode(pending.episode_key());
    let error = engine
        .deliver_human_input(&definition, &execution.id, &missing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("summary"));
    assert_eq!(
        harness
            .store()
            .load_execution(&execution.id)
            .unwrap()
            .status,
        ExecutionStatus::InputNeeded
    );

    let chosen = HumanInputDelivery::label("manual_complete")
        .with_variables(Map::from_iter([(
            "summary".to_string(),
            json!("review complete"),
        )]))
        .for_episode(pending.episode_key());
    let finished = engine
        .deliver_human_input(&definition, &execution.id, &chosen)
        .await
        .unwrap();
    assert_eq!(finished.status, ExecutionStatus::Completed);
    assert_eq!(
        finished.context.variables.get("summary"),
        Some(&json!("review complete"))
    );
    assert_eq!(finished.metadata["engine"]["state_entries"]["review"], 2);
}

#[tokio::test]
async fn resetting_an_entry_limit_counter_is_audited_before_the_decision() {
    let harness = Harness::new();
    let agent = Arc::new(AnsweringAgentExecutor::new(vec![
        ("findings", json!({}), "first findings"),
        ("fixed", json!({}), "fixed once"),
        ("findings", json!({}), "second findings"),
        ("fixed", json!({}), "fixed twice"),
        (
            "clean",
            json!({ "summary": "clean after reset" }),
            "clean after reset",
        ),
    ]));
    let engine = harness.engine().with_executor(ActionKind::Agent, agent);
    let definition = workflow("entry_limit");
    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .unwrap();
    let pending = harness
        .store()
        .load_pending_input::<narsor_engine::engine::PendingHumanInput>(&execution.id)
        .unwrap()
        .unwrap();
    let delivery = HumanInputDelivery::with_values(Map::from_iter([(
        "entry_limit_decision".to_string(),
        json!("reset_counter"),
    )]))
    .for_episode(pending.episode_key());
    let finished = engine
        .deliver_human_input(&definition, &execution.id, &delivery)
        .await
        .unwrap();
    assert_eq!(finished.status, ExecutionStatus::Completed);
    assert_eq!(finished.metadata["engine"]["state_entries"]["review"], 1);
    let events = harness.events(&execution.id);
    let reset = events
        .iter()
        .position(|event| matches!(event.data, EventData::EntryLimitCounterReset { .. }))
        .unwrap();
    let decision = events
        .iter()
        .position(|event| matches!(event.data, EventData::EntryLimitDecision { .. }))
        .unwrap();
    assert!(reset < decision);
}

#[tokio::test]
async fn manual_transition_counts_a_limited_target_without_stopping_on_its_limit() {
    let harness = Harness::new();
    let agent = Arc::new(AnsweringAgentExecutor::new(vec![
        ("findings", json!({}), "first findings"),
        ("fixed", json!({}), "fixed once"),
        ("findings", json!({}), "second findings"),
        ("fixed", json!({}), "fixed twice"),
        ("fixed", json!({}), "manual restart"),
    ]));
    let engine = harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone());
    let definition = workflow("entry_limit");
    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let transition = ManualTransition::by_operator(None, Some("fix".to_string()), true);
    let moved = engine
        .take_manual_transition(&definition, &execution.id, &transition)
        .unwrap();
    assert_eq!(moved.current_state.as_deref(), Some("fix"));
    assert_eq!(moved.metadata["engine"]["state_entries"]["fix"], 3);
    assert_eq!(moved.status, ExecutionStatus::Paused);
}

#[tokio::test]
async fn the_event_order_is_the_one_the_domain_model_prescribes() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("linear");

    let execution = engine
        .start_and_run(&definition, json!({ "subject": "x" }), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(
        harness.event_types(&execution.id),
        vec![
            "workflow_started",
            "state_entered",
            "action_started",
            "action_completed",
            "decision_made",
            "transition_taken",
            "state_entered",
            "action_started",
            "action_completed",
            "decision_made",
            "transition_taken",
            "state_entered",
            "action_started",
            "action_completed",
            "decision_made",
            "transition_taken",
            "state_entered",
            "workflow_completed",
        ]
    );

    let sequences: Vec<u64> = harness
        .events(&execution.id)
        .iter()
        .map(|event| event.sequence)
        .collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());
}

#[tokio::test]
async fn variables_reach_the_context_only_after_the_decision() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("human_decision");

    // The action sets `checked`, then the step suspends for the decision.
    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run up to the decision");

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let context = harness.stored_context(&execution.id);
    assert_eq!(
        context["variables"].get("checked"),
        None,
        "the action's variables must not reach context.json before the decision"
    );

    let execution = engine
        .deliver_human_input(&definition, &execution.id, &HumanInputDelivery::label("ok"))
        .await
        .expect("deliver the decision");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let context = harness.stored_context(&execution.id);
    assert_eq!(context["variables"]["checked"], json!(true));

    // The decision was recorded as the person's, not as a rule's.
    let providers: Vec<String> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::DecisionMade { provider, .. } => Some(provider),
            _ => None,
        })
        .collect();
    assert_eq!(providers, vec!["human"]);
}

#[tokio::test]
async fn an_agent_question_pauses_and_resumes_the_same_workflow_state() {
    let harness = Harness::new();
    let agent = Arc::new(QuestioningAgentExecutor::new());
    let engine = Engine::new(harness.store()).with_executor(ActionKind::Agent, agent.clone());
    let definition = workflow("agent_question");

    let paused = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("agent question pauses the execution");
    assert_eq!(paused.status, ExecutionStatus::InputNeeded);
    assert_eq!(paused.current_state.as_deref(), Some("implement"));
    assert_eq!(
        engine
            .pending_input(&paused.id)
            .expect("read pending input")
            .expect("agent question is pending")
            .instruction(),
        "Which API version should I use?"
    );

    let resumed = engine
        .deliver_human_input(
            &definition,
            &paused.id,
            &HumanInputDelivery::with_values(Map::from_iter([("answer".to_string(), json!("v2"))])),
        )
        .await
        .expect("answer resumes the same agent state");
    assert_eq!(resumed.status, ExecutionStatus::Completed);
    assert_eq!(resumed.current_state.as_deref(), Some("finished"));
    assert!(agent.prompts()[1].contains("Human answer to your question:\nv2"));
    assert_eq!(
        harness
            .events(&paused.id)
            .into_iter()
            .filter(|event| matches!(event.data, EventData::TransitionTaken { .. }))
            .count(),
        1,
        "the answer itself must not take a transition"
    );
}

#[tokio::test]
async fn a_rejected_agent_resume_restores_the_message_for_a_new_session() {
    let harness = Harness::new();
    let engine = Engine::new(harness.store()).with_executor(
        ActionKind::Agent,
        Arc::new(ResumeRejectingAgentExecutor::new()),
    );
    let definition = workflow("agent_question");
    let paused = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("initial question");

    let rejected = engine
        .deliver_human_input(
            &definition,
            &paused.id,
            &HumanInputDelivery::with_values(Map::from_iter([("answer".to_string(), json!("v2"))])),
        )
        .await
        .expect("resume rejection becomes a new prompt");
    assert_eq!(rejected.status, ExecutionStatus::InputNeeded);
    let pending = engine
        .pending_input(&paused.id)
        .expect("pending")
        .expect("form");
    assert_eq!(pending.request.agent_session_id, None);
    assert_eq!(pending.request.prefilled_answer.as_deref(), Some("v2"));

    let finished = engine
        .deliver_human_input(
            &definition,
            &paused.id,
            &HumanInputDelivery::with_values(Map::from_iter([("answer".to_string(), json!("v2"))])),
        )
        .await
        .expect("fresh session starts");
    assert_eq!(finished.status, ExecutionStatus::Completed);
}

#[tokio::test]
async fn a_loop_runs_twice_and_leaves_through_success() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("loop");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the loop");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.context.variables.get("passes"), Some(&json!(2)));

    let transitions: Vec<(String, String)> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::TransitionTaken {
                from_state,
                to_state,
                ..
            } => Some((from_state, to_state)),
            _ => None,
        })
        .collect();

    assert_eq!(
        transitions,
        vec![
            ("test".to_string(), "fix_code".to_string()),
            ("fix_code".to_string(), "test".to_string()),
            ("test".to_string(), "completed".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_failure_terminal_fails_the_execution() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("failure_terminal");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(execution.current_state.as_deref(), Some("failed"));

    let types = harness.event_types(&execution.id);
    assert_eq!(types.last().map(String::as_str), Some("workflow_failed"));
    assert!(
        !types.contains(&"action_failed".to_string()),
        "the action succeeded; only the workflow failed"
    );
}

#[tokio::test]
async fn a_terminal_declared_cancelled_cancels_the_execution() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("cancelled_terminal");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Cancelled);
    assert_eq!(
        execution.current_state.as_deref(),
        Some("rejected_by_reviewer")
    );

    let types = harness.event_types(&execution.id);
    assert_eq!(types.last().map(String::as_str), Some("workflow_cancelled"));
}

#[tokio::test]
async fn the_list_form_of_terminal_states_completes_even_a_state_called_failed() {
    // The deliberate consequence of replacing the name heuristic with an
    // explicit declaration: the list form declares nothing, so every terminal
    // it names completes the execution.
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("listed_terminals");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("failed"));

    let types = harness.event_types(&execution.id);
    assert_eq!(types.last().map(String::as_str), Some("workflow_completed"));
}

#[tokio::test]
async fn a_single_terminal_state_finishes_immediately() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("single_terminal");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        harness.event_types(&execution.id),
        vec!["workflow_started", "state_entered", "workflow_completed"]
    );
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_attempts_mean_two_retries() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("retry");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);

    let types = harness.event_types(&execution.id);
    assert_eq!(
        types,
        vec![
            "workflow_started",
            "state_entered",
            "action_started",
            "action_attempt_failed",
            "action_retrying",
            "action_attempt_failed",
            "action_retrying",
            "action_completed",
            "decision_made",
            "transition_taken",
            "state_entered",
            "workflow_completed",
        ]
    );

    let retry_counts: Vec<u32> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ActionRetrying { retry_count, .. } => Some(retry_count),
            _ => None,
        })
        .collect();
    assert_eq!(retry_counts, vec![1, 2]);
    assert_eq!(execution.context.variables.get("attempts"), Some(&json!(3)));

    // Every `ActionAttemptFailed` names the attempt that failed and the state
    // it belongs to, and carries the retry delay that follows it.
    let attempt_failures: Vec<(String, String)> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ActionAttemptFailed {
                action_execution_id,
                state_id,
                ..
            } => Some((action_execution_id, state_id)),
            _ => None,
        })
        .collect();
    assert_eq!(attempt_failures.len(), 2);
    for (_, state_id) in &attempt_failures {
        assert_eq!(state_id, "build");
    }

    // Each attempt's own id is announced, once as the failing attempt and once
    // as the retry that follows it.
    let started_and_retried: Vec<String> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ActionStarted {
                action_execution_id,
                ..
            }
            | EventData::ActionRetrying {
                action_execution_id,
                ..
            } => Some(action_execution_id),
            _ => None,
        })
        .collect();
    assert_eq!(started_and_retried[0], attempt_failures[0].0);
    assert_eq!(started_and_retried[1], attempt_failures[1].0);
}

#[tokio::test]
async fn exhausted_retries_fail_the_execution_without_a_transition() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("retry_exhausted");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Failed);

    let types = harness.event_types(&execution.id);
    assert_eq!(
        types,
        vec![
            "workflow_started",
            "state_entered",
            "action_started",
            "action_attempt_failed",
            "action_retrying",
            "action_failed",
            "workflow_failed",
        ]
    );
    assert!(!types.contains(&"transition_taken".to_string()));

    // The exhausting, final attempt gets no `ActionAttemptFailed` of its own —
    // only `ActionFailed`, since there is no retry left to announce.
    let attempt_failures = harness
        .events(&execution.id)
        .into_iter()
        .filter(|event| matches!(event.data, EventData::ActionAttemptFailed { .. }))
        .count();
    assert_eq!(attempt_failures, 1);
}

#[tokio::test]
async fn the_backoff_grows_between_attempts() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("backoff");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);

    let events = harness.events(&execution.id);
    let started = events
        .iter()
        .find(|event| event_type(event) == "action_started")
        .expect("ActionStarted");
    let retries: Vec<_> = events
        .iter()
        .filter(|event| event_type(event) == "action_retrying")
        .collect();
    assert_eq!(retries.len(), 2);

    // Backoff 1s with the default multiplier of 2: one second before the first
    // retry, two before the second.
    let first_wait = (retries[0].timestamp - started.timestamp)
        .to_std()
        .expect("a positive duration");
    let second_wait = (retries[1].timestamp - retries[0].timestamp)
        .to_std()
        .expect("a positive duration");

    assert!(
        first_wait >= Duration::from_millis(900),
        "first wait was {first_wait:?}"
    );
    assert!(
        second_wait >= first_wait + Duration::from_millis(700),
        "waits were {first_wait:?} then {second_wait:?}"
    );

    // `ActionAttemptFailed.retry_in_ms` announces the same delay ahead of time,
    // before the sleep, matching the growing backoff (1s, then 2s).
    let retry_in_ms: Vec<u64> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ActionAttemptFailed { retry_in_ms, .. } => Some(*retry_in_ms),
            _ => None,
        })
        .collect();
    assert_eq!(retry_in_ms, vec![1000, 2000]);
}

#[tokio::test]
async fn the_idempotency_key_is_stable_within_an_attempt_and_new_for_a_retry() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("action_id");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);

    let recorded = std::fs::read_to_string(harness.workspace_file("action_ids.txt"))
        .expect("the script recorded the ids it was given");
    let ids: Vec<&str> = recorded.lines().collect();

    assert_eq!(ids.len(), 4, "two invocations, two lines each: {ids:?}");
    assert_eq!(ids[0], ids[1], "the key is stable within one attempt");
    assert_eq!(ids[2], ids[3], "the key is stable within one attempt");
    assert_ne!(ids[0], ids[2], "a retry gets a new key");
    assert!(!ids[0].is_empty(), "the key reaches the child process");

    // And the ids the events announce are the ids the script saw.
    let announced: Vec<String> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ActionStarted {
                action_execution_id,
                ..
            }
            | EventData::ActionRetrying {
                action_execution_id,
                ..
            } => Some(action_execution_id),
            _ => None,
        })
        .collect();
    assert_eq!(announced, vec![ids[0].to_string(), ids[2].to_string()]);
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_label_fails_the_action_and_names_the_permitted_ones() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("undeclared_label");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Failed);

    let message = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::ActionFailed { error, .. } => Some(error),
            _ => None,
        })
        .expect("an ActionFailed event");

    assert!(message.contains("'weird'"), "{message}");
    assert!(message.contains("ok, broken"), "{message}");
    assert!(!harness
        .event_types(&execution.id)
        .contains(&"transition_taken".to_string()));
}

#[tokio::test]
async fn the_reason_of_a_choice_is_recorded_with_the_action_and_the_decision() {
    let harness = Harness::new();
    let execution = harness
        .engine()
        .start_and_run(&workflow("reasoned_choice"), json!({}), harness.workspace())
        .await
        .expect("run the workflow");
    assert_eq!(execution.status, ExecutionStatus::Completed);

    let events = harness.events(&execution.id);
    let completed: Vec<Option<String>> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ActionCompleted { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    let decided: Vec<Option<String>> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::DecisionMade { reasoning, .. } => Some(reasoning.clone()),
            _ => None,
        })
        .collect();
    let expected = vec![Some("all checks are green".to_string()), None];
    assert_eq!(completed, expected);
    assert_eq!(decided, expected);
}

#[tokio::test]
async fn a_failed_action_records_the_type_of_its_error() {
    let harness = Harness::new();
    let execution = harness
        .engine()
        .start_and_run(&workflow("missing_script"), json!({}), harness.workspace())
        .await
        .expect("run the workflow");
    assert_eq!(execution.status, ExecutionStatus::Failed);

    let error_type = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::ActionFailed { error_type, .. } => Some(error_type),
            _ => None,
        })
        .expect("an ActionFailed event");
    assert_eq!(error_type.as_deref(), Some("spawn_failed"));
}

#[tokio::test]
async fn an_agent_decision_is_refused_explicitly() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("agent_decision");

    let error = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect_err("an agent decision cannot be made in this version");

    let message = error.to_string();
    assert!(message.contains("agent decision provider"), "{message}");
    assert!(
        message.contains("not available in this version"),
        "{message}"
    );

    let EngineError::Decision(_) = error else {
        panic!("expected a decision error, got {error:?}");
    };

    // The execution is left failed, with the reason in its history.
    let stored = harness.store().list_execution_ids().expect("list");
    let execution = harness
        .store()
        .load_execution(&stored[0])
        .expect("load the execution");
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert!(harness
        .event_types(&execution.id)
        .contains(&"error".to_string()));
}

#[tokio::test]
async fn a_missing_context_variable_fails_the_action_on_entry() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("missing_variable");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(execution.current_state.as_deref(), Some("greet"));

    assert!(
        !harness.events(&execution.id).into_iter().any(|event| matches!(
            event.data,
            EventData::TransitionTaken { .. }
        )),
        "an action failure must leave the execution on the failed state rather than take a workflow transition"
    );

    let message = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::ActionFailed { error, .. } => Some(error),
            _ => None,
        })
        .expect("an ActionFailed event");
    assert!(
        message.contains("nobody_set_this"),
        "the message names the variable: {message}"
    );
}

// ---------------------------------------------------------------------------
// Pausing, the event stream and human input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pausing_lets_the_running_action_finish_and_stops_the_next_one() {
    let harness = Harness::new();
    let engine = Arc::new(harness.engine());
    let definition = Arc::new(workflow("pause"));

    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start the workflow");

    let running = {
        let engine = Arc::clone(&engine);
        let definition = Arc::clone(&definition);
        let execution_id = execution.id.clone();
        tokio::spawn(async move { engine.run(&definition, &execution_id).await })
    };

    // While the first action is still running.
    tokio::time::sleep(Duration::from_millis(100)).await;
    engine.pause(&execution.id);

    let execution = running.await.expect("join").expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Paused);
    assert_eq!(
        execution.current_state.as_deref(),
        Some("fast"),
        "the slow action finished and its transition was taken"
    );
    assert_eq!(
        execution.context.variables.get("slow"),
        Some(&json!("finished"))
    );
    assert_eq!(execution.context.variables.get("fast"), None);

    let types = harness.event_types(&execution.id);
    assert_eq!(types.last().map(String::as_str), Some("workflow_paused"));
    assert_eq!(
        types
            .iter()
            .filter(|kind| kind.as_str() == "action_started")
            .count(),
        1,
        "the next action never started"
    );

    // And it picks up where it left off.
    let execution = engine
        .resume(&definition, &execution.id)
        .await
        .expect("resume");
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.context.variables.get("fast"),
        Some(&json!("finished"))
    );
}

#[tokio::test]
async fn every_appended_event_reaches_the_live_stream() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("linear");

    let mut stream = engine.subscribe();
    let execution = engine
        .start_and_run(&definition, json!({ "subject": "x" }), harness.workspace())
        .await
        .expect("run the workflow");

    let mut streamed = Vec::new();
    while let Ok(event) = stream.try_recv() {
        streamed.push(event.sequence);
    }

    let logged: Vec<u64> = harness
        .events(&execution.id)
        .iter()
        .map(|event| event.sequence)
        .collect();
    assert_eq!(streamed, logged);
}

#[tokio::test]
async fn a_dropped_subscriber_does_not_disturb_the_engine() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("linear");

    drop(engine.subscribe());

    let execution = engine
        .start_and_run(&definition, json!({ "subject": "x" }), harness.workspace())
        .await
        .expect("run the workflow");
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

#[tokio::test]
async fn running_a_paused_execution_is_refused() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("human_decision");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run up to the decision");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);

    let error = engine
        .run(&definition, &execution.id)
        .await
        .expect_err("a paused execution is not runnable");
    assert!(
        matches!(error, EngineError::AwaitingHumanInput { .. }),
        "{error:?}"
    );

    let error = engine
        .resume(&definition, &execution.id)
        .await
        .expect_err("resuming does not answer the question");
    assert!(
        matches!(error, EngineError::AwaitingHumanInput { .. }),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------
// The reference workflow of WORKFLOW_SCHEMA.md §4
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_recovery_retries_an_interrupted_agent_step_and_passes_the_operator_note() {
    let harness = Harness::new();
    // A real action failure — an undeclared label would be a format error
    // and end as a problem instead (U5).
    let agent = Arc::new(StubAgentExecutor::new(&[
        StubAgentExecutor::FAIL,
        "ready",
        "implemented",
    ]));
    let engine =
        Arc::new(Engine::new(harness.store()).with_executor(ActionKind::Agent, agent.clone()));
    let mut definition = workflow_file("implement_feature.yaml");
    // One attempt, so the failure is final without waiting out a backoff.
    let policies = definition
        .states
        .values_mut()
        .filter_map(|state| state.retry_policy.as_mut())
        .chain(definition.globals.retry_policy.as_mut());
    for policy in policies {
        policy.max_attempts = 1;
    }

    let failed = engine
        .start_and_run(
            &definition,
            json!({ "feature_description": "add login", "target_repo": "acme/web" }),
            harness.workspace(),
        )
        .await
        .expect("the failed run is recorded, not an engine error");
    assert_eq!(failed.status, ExecutionStatus::Failed);

    // Recovery marks an action interrupted by a stopped daemon as `unknown`.
    // The explicit recovery path must accept that state just as it accepts a
    // regular failed attempt.
    let mut interrupted = harness
        .store()
        .load_execution(&failed.id)
        .expect("reload failed execution");
    interrupted.status = ExecutionStatus::Unknown;
    harness
        .store()
        .save_execution(&interrupted)
        .expect("mark action interrupted");

    engine
        .try_begin_recover(
            &failed.id,
            Some("Return the required output JSON.".to_string()),
        )
        .expect("operator can explicitly recover a failed run")
        .run_to_completion(&definition)
        .await
        .expect("recovered run completes");

    let recovered = harness
        .store()
        .load_execution(&failed.id)
        .expect("load recovered execution");
    assert_eq!(recovered.status, ExecutionStatus::Completed);
    assert!(
        agent.prompts()[1].contains("Operator recovery note:\nReturn the required output JSON."),
        "the fresh agent attempt receives the operator's note"
    );
}

#[tokio::test]
async fn the_reference_workflow_runs_end_to_end_on_stubs() {
    let harness = Harness::new();
    let agent = Arc::new(StubAgentExecutor::new(&["ready", "implemented"]));
    let engine = Engine::new(harness.store()).with_executor(ActionKind::Agent, agent.clone());
    let definition = workflow_file("implement_feature.yaml");

    let execution = engine
        .start_and_run(
            &definition,
            json!({ "feature_description": "add login", "target_repo": "acme/web" }),
            harness.workspace(),
        )
        .await
        .expect("run the reference workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("completed"));

    // The §4 runner_state, state by state.
    assert_eq!(
        harness.event_types(&execution.id),
        vec![
            "workflow_started",
            // analyze (agent stub)
            "state_entered",
            "action_started",
            "action_completed",
            "decision_made",
            "transition_taken",
            // implement (agent stub)
            "state_entered",
            "action_started",
            "action_completed",
            "decision_made",
            "transition_taken",
            // test (script)
            "state_entered",
            "action_started",
            "action_completed",
            "decision_made",
            "transition_taken",
            // create_pr (script, reporting an artifact)
            "state_entered",
            "action_started",
            "action_completed",
            "artifact_created",
            "decision_made",
            "transition_taken",
            // completed
            "state_entered",
            "workflow_completed",
        ]
    );

    // No gaps in the numbering (DOMAIN_MODEL.md §5).
    let sequences: Vec<u64> = harness
        .events(&execution.id)
        .iter()
        .map(|event| event.sequence)
        .collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());

    // The agent stub saw expanded prompts, never a raw `{{ }}`.
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("add login"), "{}", prompts[0]);
    assert!(!prompts[0].contains("{{"), "{}", prompts[0]);

    // The script's variable and its artifact both landed in the context.
    assert_eq!(
        execution.context.variables.get("pr_number"),
        Some(&json!(123))
    );
    let artifact = execution
        .context
        .artifacts
        .get("pull_request")
        .expect("the artifact was registered");
    assert_eq!(artifact.location, "https://example.invalid/pr/123");
}

#[tokio::test]
async fn the_reference_workflow_pauses_for_a_person_and_carries_on() {
    let harness = Harness::new();
    let agent = Arc::new(StubAgentExecutor::new(&["ambiguous", "implemented"]));
    let engine = Engine::new(harness.store()).with_executor(ActionKind::Agent, agent.clone());
    let definition = workflow_file("implement_feature.yaml");

    let execution = engine
        .start_and_run(
            &definition,
            json!({ "feature_description": "add login", "target_repo": "acme/web" }),
            harness.workspace(),
        )
        .await
        .expect("run up to the question");

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        execution.current_state.as_deref(),
        Some("human_clarification")
    );

    let pending = engine
        .pending_input(&execution.id)
        .expect("read the pending request")
        .expect("there is one");
    assert_eq!(pending.state_id, "human_clarification");
    assert_eq!(pending.fields().len(), 1);
    assert_eq!(pending.output_labels(), vec!["clarified", "abort"]);

    let mut values = Map::new();
    values.insert(
        "clarification_text".to_string(),
        Value::String("use OAuth".to_string()),
    );
    let delivery = HumanInputDelivery::with_values(values).choosing("clarified");

    let execution = engine
        .deliver_human_input(&definition, &execution.id, &delivery)
        .await
        .expect("deliver the answer");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.context.variables.get("clarification_text"),
        Some(&json!("use OAuth")),
        "the answer became a context variable, written by the engine"
    );

    let types = harness.event_types(&execution.id);
    assert!(types.contains(&"human_input_requested".to_string()));
    assert!(types.contains(&"human_input_received".to_string()));
}

#[tokio::test]
async fn a_blank_optional_note_reaches_the_next_prompt_as_an_empty_string() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("optional_note");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run up to the gate");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);

    let mut values = Map::new();
    values.insert("decision".to_string(), json!("approved"));
    let delivery = HumanInputDelivery::with_values(values).choosing("approved");

    let execution = engine
        .deliver_human_input(&definition, &execution.id, &delivery)
        .await
        .expect("deliver the answer without a note");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.context.variables.get("note"), Some(&json!("")));
    assert_eq!(
        execution.context.variables.get("echoed"),
        Some(&json!("[]"))
    );
}

#[tokio::test]
async fn an_answer_cannot_be_delivered_twice() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("human_decision");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run up to the decision");

    engine
        .deliver_human_input(&definition, &execution.id, &HumanInputDelivery::label("ok"))
        .await
        .expect("deliver the decision");

    let error = engine
        .deliver_human_input(&definition, &execution.id, &HumanInputDelivery::label("ok"))
        .await
        .expect_err("the second delivery is refused");
    assert!(matches!(error, EngineError::HumanInput(_)), "{error:?}");
}

/// A script state that declares no limit gets none — the behaviour every
/// script workflow written before the configuration layer existed relies on.
///
/// Characterization: written before the timeout chains were split, so that the
/// split could be shown not to move it (D-C). The script sleeps for longer
/// than the default agent timeout the next test configures.
#[tokio::test]
async fn a_script_state_without_a_declared_limit_runs_without_one() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("slow_script");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(
        execution.status,
        ExecutionStatus::Completed,
        "a script with no declared limit is not interrupted"
    );
}

/// A configured agent timeout does not reach a script state (D-C).
///
/// The counterpart of [`a_script_state_without_a_declared_limit_runs_without_one`]:
/// the same workflow, now with a default agent timeout configured. What is
/// asserted is the limit the script executor is actually handed — `None`, the
/// same as before. A shared chain would have handed it one second here, and a
/// script that outran it would have become a timeout, then a retryable action
/// failure, then a failed action.
#[tokio::test]
async fn a_configured_agent_timeout_leaves_script_states_alone() {
    let harness = Harness::new();
    let recorder = Arc::new(TimeoutRecorder::answering("done"));
    let engine = harness
        .engine()
        .with_agent_defaults(AgentDefaults {
            harness: "claude-code".to_string(),
            timeout_secs: Some(1),
        })
        .with_executor(
            ActionKind::Script,
            Arc::clone(&recorder) as Arc<dyn ActionExecutor>,
        );
    let definition = workflow("slow_script");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        recorder.timeouts(),
        vec![None],
        "a script state that declares no limit still has none"
    );
}

/// An agent state that declares no limit gets the configured one.
#[tokio::test]
async fn a_configured_agent_timeout_reaches_an_agent_state_without_one() {
    let harness = Harness::new();
    let recorder = Arc::new(TimeoutRecorder::answering("done"));
    let engine = harness
        .engine()
        .with_agent_defaults(AgentDefaults {
            harness: "claude-code".to_string(),
            timeout_secs: Some(90),
        })
        .with_executor(
            ActionKind::Agent,
            Arc::clone(&recorder) as Arc<dyn ActionExecutor>,
        );
    let definition = workflow("agent_default");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(recorder.timeouts(), vec![Some(Duration::from_secs(90))]);
}

/// A limit declared by the workflow wins over the configured one.
#[tokio::test]
async fn the_workflow_timeout_wins_over_the_configured_one() {
    // R13 / AE4: the setting is a default, never an instruction the workflow
    // cannot override.
    let harness = Harness::new();
    let recorder = Arc::new(TimeoutRecorder::answering("done"));
    let engine = harness
        .engine()
        .with_agent_defaults(AgentDefaults {
            harness: "claude-code".to_string(),
            timeout_secs: Some(90),
        })
        .with_executor(
            ActionKind::Agent,
            Arc::clone(&recorder) as Arc<dyn ActionExecutor>,
        );
    let definition = workflow("agent_declared_timeout");

    engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(
        recorder.timeouts(),
        vec![Some(Duration::from_secs(5))],
        "the workflow declared five seconds; the configuration does not overrule it"
    );
}

/// No configured limit leaves an agent state without one, exactly as before.
#[tokio::test]
async fn no_configured_timeout_leaves_an_agent_state_unlimited() {
    let harness = Harness::new();
    let recorder = Arc::new(TimeoutRecorder::answering("done"));
    let engine = harness
        .engine()
        .with_agent_defaults(AgentDefaults {
            harness: "claude-code".to_string(),
            timeout_secs: None,
        })
        .with_executor(
            ActionKind::Agent,
            Arc::clone(&recorder) as Arc<dyn ActionExecutor>,
        );
    let definition = workflow("agent_default");

    engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(
        recorder.timeouts(),
        vec![None],
        "a configuration that revokes the limit leaves the action running as it does today"
    );
}

// ---------------------------------------------------------------------------
// Checkpointing (R4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkpoints_are_written_at_every_step_boundary() {
    let harness = Harness::new();
    let definition = workflow("two_steps");
    let engine = harness.engine().with_checkpoints();

    let execution = engine
        .start_and_run(&definition, serde_json::json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    let bundle = harness.store().paths().checkpoint_file(&execution.id);

    assert!(
        bundle.is_file(),
        "a finished execution must have left a checkpoint at {}",
        bundle.display()
    );
}

/// A `preserve_paths` entry the workflow declares must reach a real
/// checkpoint bundle through the engine's own `write_checkpoint` hook — not
/// just through `capture()` called directly (`tests/checkpoint.rs` proves the
/// mechanism in isolation; this proves it is actually wired in).
#[tokio::test]
async fn preserve_paths_declared_on_the_workflow_reaches_the_checkpoint() {
    let harness = Harness::new();
    let definition = workflow("checkpoint_preserve");
    let engine = harness.engine().with_checkpoints();

    // A file the workspace ignores, but the workflow asks to preserve.
    std::fs::write(harness.workspace_file(".gitignore"), "secret.local\n")
        .expect("write .gitignore");
    std::fs::write(harness.workspace_file("secret.local"), "keep-me\n")
        .expect("write the preserved file");

    let execution = engine
        .start_and_run(&definition, serde_json::json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    let bundle = harness.store().paths().checkpoint_file(&execution.id);
    assert!(bundle.is_file(), "a checkpoint must have been written");

    // Read the checkpoint commit back out of the bundle, the same way
    // `materialise` does: fetch its ref into a scratch repository, then look
    // at the tree directly, without touching the workspace itself.
    let scratch = harness.root().join("bundle-scratch");
    std::fs::create_dir_all(&scratch).expect("create scratch dir");
    let run_git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&scratch)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap_or_else(|error| panic!("run git {args:?}: {error}"))
    };
    let init = run_git(&["init", "--quiet"]);
    assert!(init.status.success(), "git init failed");
    let fetch = run_git(&[
        "fetch",
        "--quiet",
        &bundle.display().to_string(),
        "refs/narsor/checkpoint:refs/narsor/checkpoint",
    ]);
    assert!(
        fetch.status.success(),
        "fetching the checkpoint bundle failed: {}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    let show = run_git(&["show", "refs/narsor/checkpoint:secret.local"]);
    assert!(
        show.status.success(),
        "the checkpoint commit does not contain secret.local: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&show.stdout),
        "keep-me\n",
        "preserve_paths declared on the workflow must have travelled into the real checkpoint"
    );
}

#[tokio::test]
async fn an_engine_without_checkpoints_writes_none() {
    // The default stays off in M1, so no existing test starts shelling out to
    // git on every step.
    let harness = Harness::new();
    let definition = workflow("two_steps");

    let execution = harness
        .engine()
        .start_and_run(&definition, serde_json::json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert!(
        !harness
            .store()
            .paths()
            .checkpoint_file(&execution.id)
            .exists(),
        "checkpointing is opt-in in M1"
    );
}

// ---------------------------------------------------------------------------
// The gate after an agent or script step: its reason and required values (U4)
// ---------------------------------------------------------------------------

const NO_REASON: &str = "brak uzasadnienia";

/// An engine whose agent states answer with `results`, in order.
fn engine_answering(harness: &Harness, results: Vec<ActionResult>) -> Engine {
    Engine::new(harness.store()).with_executor(
        ActionKind::Agent,
        Arc::new(AnsweringAgentExecutor::from_results(results)),
    )
}

/// An agent's answer: `choice` with `reason` and `variables`, after `answer`.
fn agent_choice(choice: &str, reason: &str, variables: Value, answer: &str) -> ActionResult {
    ActionResult {
        output_label: choice.to_string(),
        reason: reason.to_string(),
        variables: variables.as_object().cloned().expect("an object"),
        exit_code: Some(0),
        stdout: answer.to_string(),
        reply: Some(answer.to_string()),
        ..ActionResult::default()
    }
}

/// The `reason` and `required_values` of every `HumanInputRequested`, in
/// order.
fn requested_gates(harness: &Harness, execution_id: &str) -> Vec<(Option<String>, Vec<RequiredValue>)> {
    harness
        .events(execution_id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::HumanInputRequested {
                reason,
                required_values,
                ..
            } => Some((reason, required_values)),
            _ => None,
        })
        .collect()
}

/// The pending input of `execution_id` as clients see it.
fn pending_view(engine: &Engine, execution_id: &str) -> PendingHumanInputView {
    let pending = engine
        .pending_input(execution_id)
        .expect("read pending input")
        .expect("a gate is pending");
    serde_json::from_value(serde_json::to_value(&pending).expect("serialize"))
        .expect("the view reads the pending record")
}

fn variable(name: &str, value: &str) -> RequiredValue {
    RequiredValue {
        name: name.to_string(),
        kind: RequiredValueKind::Variable,
        value: value.to_string(),
    }
}

/// Covers AE6. The choice's reason and the value of the variable it requires
/// reach the gate: in its event and in the view of the pending input.
#[tokio::test]
async fn a_gate_after_an_agent_shows_its_reason_and_the_required_variable() {
    let harness = Harness::new();
    let engine = engine_answering(
        &harness,
        vec![agent_choice(
            "needs_requirements_revision",
            "the requirements name no API",
            json!({ "requirements_blocker": "which API version is in scope?" }),
            "I could not plan it.",
        )],
    );

    let execution = engine
        .start_and_run(&workflow("plan_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the gate");

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("review"));
    let expected = (
        Some("the requirements name no API".to_string()),
        vec![variable(
            "requirements_blocker",
            "which API version is in scope?",
        )],
    );
    assert_eq!(requested_gates(&harness, &execution.id), vec![expected.clone()]);
    let view = pending_view(&engine, &execution.id);
    assert_eq!((view.reason, view.required_values), expected);
    assert_eq!(view.answer_tail.as_deref(), Some("I could not plan it."));
    assert_eq!(view.instruction, "Review the plan.");
}

#[tokio::test]
async fn a_choice_without_required_values_shows_only_its_reason() {
    let harness = Harness::new();
    let engine = engine_answering(
        &harness,
        vec![agent_choice(
            "needs_human_decision",
            "two designs fit equally well",
            json!({ "requirements_blocker": "not required by this choice" }),
            "",
        )],
    );

    let execution = engine
        .start_and_run(&workflow("plan_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the gate");

    let view = pending_view(&engine, &execution.id);
    assert_eq!(view.reason.as_deref(), Some("two designs fit equally well"));
    assert!(view.required_values.is_empty(), "{:?}", view.required_values);
}

#[tokio::test]
async fn a_choice_without_a_reason_shows_the_placeholder() {
    let harness = Harness::new();
    let engine = engine_answering(
        &harness,
        vec![agent_choice("needs_human_decision", "  ", json!({}), "")],
    );

    let execution = engine
        .start_and_run(&workflow("plan_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the gate");

    assert_eq!(
        pending_view(&engine, &execution.id).reason.as_deref(),
        Some(NO_REASON)
    );
    assert_eq!(
        requested_gates(&harness, &execution.id),
        vec![(Some(NO_REASON.to_string()), Vec::new())]
    );
}

#[tokio::test]
async fn a_required_value_reaches_the_gate_cut_to_two_kilobytes_and_cleaned() {
    let harness = Harness::new();
    let long = format!("\u{1b}[31m{}", "x".repeat(10 * 1024));
    let engine = engine_answering(
        &harness,
        vec![agent_choice(
            "needs_requirements_revision",
            "blocked\u{1b}[2J",
            json!({ "requirements_blocker": long }),
            "",
        )],
    );

    let execution = engine
        .start_and_run(&workflow("plan_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the gate");

    let view = pending_view(&engine, &execution.id);
    assert_eq!(view.reason.as_deref(), Some("blocked[2J"));
    let value = &view.required_values[0].value;
    assert!(value.len() <= 2 * 1024, "got {} bytes", value.len());
    assert!(value.starts_with("[31mxxx"), "{value:.20}");
    assert!(!value.contains('\u{1b}'));
}

#[tokio::test]
async fn a_question_in_additional_data_is_not_shown_on_the_gate() {
    let harness = Harness::new();
    let mut answer = agent_choice("needs_human_decision", "", json!({}), "");
    answer
        .additional_data
        .insert("question".to_string(), json!("Account or user limit?"));
    let engine = engine_answering(&harness, vec![answer]);

    let execution = engine
        .start_and_run(&workflow("plan_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the gate");

    let pending = engine
        .pending_input(&execution.id)
        .expect("read")
        .expect("the gate is pending");
    let record = serde_json::to_string(&pending).expect("serialize");
    assert!(!record.contains("Account or user limit?"), "{record}");
    assert_eq!(pending.request.reason.as_deref(), Some(NO_REASON));
    assert!(pending.request.required_values.is_empty());
}

#[tokio::test]
async fn a_gate_reached_again_shows_the_newest_reason() {
    let harness = Harness::new();
    let engine = engine_answering(
        &harness,
        vec![
            agent_choice("needs_human_decision", "first reason", json!({}), "first answer"),
            agent_choice("needs_human_decision", "second reason", json!({}), "second answer"),
        ],
    );
    let definition = workflow("agent_gate");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run up to the gate");
    let execution = engine
        .deliver_human_input(
            &definition,
            &execution.id,
            &HumanInputDelivery::label("clarified"),
        )
        .await
        .expect("loop through apply_clarification back to the gate");

    assert_eq!(
        execution.current_state.as_deref(),
        Some("resolve_ambiguity")
    );
    let view = pending_view(&engine, &execution.id);
    assert_eq!(view.reason.as_deref(), Some("second reason"));
    assert_eq!(view.answer_tail.as_deref(), Some("second answer"));
    assert_eq!(
        requested_gates(&harness, &execution.id),
        vec![
            (Some("first reason".to_string()), Vec::new()),
            (Some("second reason".to_string()), Vec::new()),
        ]
    );
}

#[tokio::test]
async fn a_gate_after_a_script_shows_the_scripts_reason_not_the_agents() {
    // The agent gave a reason, but a script ran between it and the gate.
    let harness = Harness::new();
    let engine = engine_answering(
        &harness,
        vec![agent_choice("done", "stale reason", json!({}), "stale answer")],
    );

    let execution = engine
        .start_and_run(&workflow("agent_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the review gate");

    assert_eq!(execution.current_state.as_deref(), Some("review"));
    let view = pending_view(&engine, &execution.id);
    assert_eq!(view.reason.as_deref(), Some(NO_REASON));
    assert!(view
        .answer_tail
        .as_deref()
        .is_some_and(|tail| !tail.contains("stale")));
}

#[tokio::test]
async fn a_human_decision_on_an_agent_shows_its_reason() {
    let harness = Harness::new();
    let engine = engine_answering(
        &harness,
        vec![agent_choice("done", "every test passes", json!({}), "the answer")],
    );

    let execution = engine
        .start_and_run(
            &workflow("agent_human_decision"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run up to the decision");

    assert_eq!(execution.current_state.as_deref(), Some("work"));
    let view = pending_view(&engine, &execution.id);
    assert_eq!(view.reason.as_deref(), Some("every test passes"));
    assert_eq!(view.answer_tail.as_deref(), Some("the answer"));
}

#[tokio::test]
async fn a_script_choosing_blocked_leads_to_a_gate_with_its_reason() {
    let harness = Harness::new();

    let execution = harness
        .engine()
        .start_and_run(&workflow("script_gate"), json!({}), harness.workspace())
        .await
        .expect("run up to the gate");

    assert_eq!(
        execution.current_state.as_deref(),
        Some("finalize_blocked")
    );
    assert_eq!(
        requested_gates(&harness, &execution.id),
        vec![(
            Some("the pull request has merge conflicts".to_string()),
            Vec::new()
        )]
    );
}

#[tokio::test]
async fn a_script_out_of_time_leads_to_a_gate_with_a_reason_naming_the_limit() {
    let harness = Harness::new();

    let execution = harness
        .engine()
        .start_and_run(
            &workflow("script_timeout_gate"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run up to the gate");

    assert_eq!(execution.current_state.as_deref(), Some("merge_timed_out"));
    let gates = requested_gates(&harness, &execution.id);
    let reason = gates[0].0.clone().expect("a reason");
    assert_ne!(reason, NO_REASON);
    assert!(reason.contains("1s"), "{reason}");
}

/// KTD12: `revise` resumes the agent's session with the comment and needs
/// one; `approve`, on the same optional comment field, does not.
mod revise_comment {
    use super::*;

    use narsor_engine::engine::HumanActionError;

    const SESSION: &str = "work-session";

    async fn run_to_review(harness: &Harness) -> (Arc<AnsweringAgentExecutor>, Engine, String) {
        let mut first = agent_choice("done", "done", json!({}), "first");
        first.data = json!({
            "agent_session": { "harness": "fake", "config": { "harness_session_id": SESSION } }
        });
        let agent = Arc::new(AnsweringAgentExecutor::from_results(vec![
            first,
            agent_choice("done", "revised", json!({}), "second"),
        ]));
        let engine = Engine::new(harness.store()).with_executor(ActionKind::Agent, agent.clone());
        let execution = engine
            .start_and_run(&workflow("revise_gate"), json!({}), harness.workspace())
            .await
            .expect("run up to the review");
        assert_eq!(execution.current_state.as_deref(), Some("review"));
        (agent, engine, execution.id)
    }

    #[tokio::test]
    async fn revise_without_a_comment_is_refused_and_with_one_resumes_the_author() {
        let harness = Harness::new();
        let (agent, engine, execution_id) = run_to_review(&harness).await;
        let definition = workflow("revise_gate");

        let error = engine
            .deliver_human_input(
                &definition,
                &execution_id,
                &HumanInputDelivery::with_values(Map::from_iter([(
                    "comment".to_string(),
                    json!("  "),
                )]))
                .choosing("revise"),
            )
            .await
            .expect_err("revise needs a comment");
        assert!(
            matches!(
                &error,
                EngineError::HumanInput(HumanActionError::MissingRequiredField { field })
                    if field == "comment"
            ),
            "{error:?}"
        );
        assert!(harness.store().has_pending_input(&execution_id));

        let execution = engine
            .deliver_human_input(
                &definition,
                &execution_id,
                &HumanInputDelivery::with_values(Map::from_iter([(
                    "comment".to_string(),
                    json!("cover the error path"),
                )]))
                .choosing("revise"),
            )
            .await
            .expect("revise with a comment");

        assert_eq!(execution.current_state.as_deref(), Some("review"));
        let prompts = agent.prompts();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[1].contains("cover the error path"), "{}", prompts[1]);
        assert_eq!(agent.resumed_sessions(), vec![None, Some(SESSION.to_string())]);
    }

    #[tokio::test]
    async fn approve_without_a_comment_goes_through() {
        let harness = Harness::new();
        let (_agent, engine, execution_id) = run_to_review(&harness).await;

        let execution = engine
            .deliver_human_input(
                &workflow("revise_gate"),
                &execution_id,
                &HumanInputDelivery::label("approve"),
            )
            .await
            .expect("approve needs no comment");

        assert_eq!(execution.status, ExecutionStatus::Completed);
    }
}

// ---------------------------------------------------------------------------
// Problems: an agent step without a usable output label (U5)
// ---------------------------------------------------------------------------

mod problems {
    use super::*;

    use narsor_engine::domain::{AnsweredBy, InputCloseReason, ProblemKind, WorkflowDefinition};
    use narsor_engine::engine::{HumanActionError, InputContinuation, PendingHumanInput};

    const ANSWER: &str = "Done: 3 units implemented, every test passes.";

    /// Runs `agent_problem` with `inputs` until its agent step becomes a
    /// problem.
    async fn raise_problem(
        harness: &Harness,
        inputs: Value,
    ) -> (Engine, WorkflowDefinition, String, PendingHumanInput) {
        let engine = Engine::new(harness.store()).with_executor(
            ActionKind::Agent,
            Arc::new(LabellessAgentExecutor::answering(ANSWER)),
        );
        let definition = workflow("agent_problem");
        let execution = engine
            .start_and_run(&definition, inputs, harness.workspace())
            .await
            .expect("run to the problem");
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
        let pending = engine
            .pending_input(&execution.id)
            .expect("read the pending input")
            .expect("a problem is pending");
        (engine, definition, execution.id, pending)
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    #[tokio::test]
    async fn a_problem_records_what_each_label_needs_and_announces_itself() {
        let harness = Harness::new();
        let (_engine, _definition, execution_id, pending) =
            raise_problem(&harness, json!({})).await;

        assert_eq!(pending.continuation, InputContinuation::ResolveOutput);
        let problem = pending.problem.as_ref().expect("the problem record");
        let implemented = problem.requirement_for("implemented").expect("implemented");
        assert_eq!(implemented.variables, vec!["summary".to_string()]);
        assert!(implemented.artifacts.is_empty());
        let decision = problem
            .requirement_for("needs_human_decision")
            .expect("needs_human_decision");
        assert!(decision.variables.is_empty(), "a gate needs nothing");
        assert!(decision.artifacts.is_empty());
        assert_eq!(problem.answers.len(), 1);
        assert_eq!(problem.answers[0].answer_tail.as_deref(), Some(ANSWER));

        let types = harness.event_types(&execution_id);
        let tail: Vec<&str> = types
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            vec![
                "action_output_rejected",
                "human_input_requested",
                "problem_raised"
            ]
        );
        let raised = harness
            .events(&execution_id)
            .into_iter()
            .find_map(|event| match event.data {
                EventData::ProblemRaised { kind, episode, .. } => Some((kind, episode)),
                _ => None,
            })
            .expect("ProblemRaised");
        assert_eq!(raised.0, ProblemKind::Output);
        assert_eq!(raised.1.as_deref(), Some(pending.episode_key()));
    }

    #[tokio::test]
    async fn a_variable_the_context_already_holds_is_not_required() {
        let harness = Harness::new();
        let (_engine, _definition, _execution_id, pending) =
            raise_problem(&harness, json!({ "summary": "from the inputs" })).await;

        let problem = pending.problem.expect("the problem record");
        assert!(problem
            .requirement_for("implemented")
            .expect("implemented")
            .variables
            .is_empty());
    }

    #[tokio::test]
    async fn the_operator_resolves_a_problem_and_the_workflow_goes_on() {
        let harness = Harness::new();
        let (engine, definition, execution_id, pending) = raise_problem(&harness, json!({})).await;

        let delivery = HumanInputDelivery::label("implemented")
            .with_variables(object(json!({ "summary": "login works" })))
            .with_comment("the answer says it is done")
            .for_episode(pending.episode_key());
        let execution = engine
            .deliver_human_input(&definition, &execution_id, &delivery)
            .await
            .expect("resolve the problem");

        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert_eq!(
            harness.stored_context(&execution_id)["variables"]["summary"],
            json!("login works")
        );

        let events = harness.events(&execution_id);
        let completed = events
            .iter()
            .find_map(|event| match &event.data {
                EventData::ActionCompleted {
                    action_execution_id,
                    output_label,
                    ..
                } => Some((action_execution_id.clone(), output_label.clone())),
                _ => None,
            })
            .expect("the step completes");
        assert_eq!(completed.0, pending.action_execution_id());
        assert_eq!(completed.1, "implemented");
        let decision = events
            .iter()
            .find_map(|event| match &event.data {
                EventData::DecisionMade {
                    provider,
                    output_label,
                    reasoning,
                } => Some((provider.clone(), output_label.clone(), reasoning.clone())),
                _ => None,
            })
            .expect("a decision");
        assert_eq!(decision.0, "human");
        assert_eq!(decision.1, "implemented");
        assert_eq!(decision.2.as_deref(), Some("the answer says it is done"));
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::TransitionTaken { from_state, to_state, .. }
                if from_state == "work" && to_state == "report"
        )));
    }

    #[tokio::test]
    async fn a_label_leading_to_a_gate_needs_nothing_and_the_gate_shows_the_settled_answer() {
        let harness = Harness::new();
        let (engine, definition, execution_id, _pending) = raise_problem(&harness, json!({})).await;

        let execution = engine
            .deliver_human_input(
                &definition,
                &execution_id,
                &HumanInputDelivery::label("needs_human_decision"),
            )
            .await
            .expect("a gate needs no value");
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
        assert_eq!(
            execution.current_state.as_deref(),
            Some("resolve_ambiguity")
        );

        let gate = engine
            .pending_input(&execution_id)
            .expect("read")
            .expect("the gate is pending");
        assert_eq!(gate.continuation, InputContinuation::CompleteStep);
        assert_eq!(gate.request.reason.as_deref(), Some(NO_REASON));
        assert_eq!(gate.request.answer_tail.as_deref(), Some(ANSWER));
    }

    /// Runs `plan_problem` until its plan step becomes a problem.
    async fn raise_plan_problem(harness: &Harness) -> (Engine, WorkflowDefinition, PendingHumanInput) {
        let engine = Engine::new(harness.store()).with_executor(
            ActionKind::Agent,
            Arc::new(LabellessAgentExecutor::answering(ANSWER)),
        );
        let definition = workflow("plan_problem");
        let execution = engine
            .start_and_run(&definition, json!({}), harness.workspace())
            .await
            .expect("run to the problem");
        let pending = engine
            .pending_input(&execution.id)
            .expect("read the pending input")
            .expect("a problem is pending");
        (engine, definition, pending)
    }

    #[tokio::test]
    async fn the_operator_supplies_a_missing_artifact_and_the_step_goes_on() {
        let harness = Harness::new();
        let (engine, definition, pending) = raise_plan_problem(&harness).await;
        let execution_id = pending.execution_id.clone();
        let requirement = pending
            .problem
            .as_ref()
            .and_then(|problem| problem.requirement_for("plan_ready"))
            .expect("plan_ready");
        assert_eq!(requirement.artifacts, vec!["plan_doc".to_string()]);

        let error = engine
            .deliver_human_input(
                &definition,
                &execution_id,
                &HumanInputDelivery::label("plan_ready"),
            )
            .await
            .expect_err("the artifact is missing");
        assert!(
            matches!(
                &error,
                EngineError::HumanInput(HumanActionError::MissingRequiredKey { key, .. })
                    if key == "artifacts.plan_doc"
            ),
            "{error:?}"
        );

        let execution = engine
            .deliver_human_input(
                &definition,
                &execution_id,
                &HumanInputDelivery::label("plan_ready").with_artifacts(
                    [("plan_doc".to_string(), "docs/plan.md".to_string())].into(),
                ),
            )
            .await
            .expect("resolve with the artifact");

        assert_eq!(execution.status, ExecutionStatus::Completed);
        let created = harness
            .events(&execution_id)
            .into_iter()
            .find_map(|event| match event.data {
                EventData::ArtifactCreated { location, .. } => Some(location),
                _ => None,
            })
            .expect("ArtifactCreated");
        assert_eq!(created, "docs/plan.md");
        assert_eq!(
            harness.stored_context(&execution_id)["artifacts"]["plan_doc"]["location"],
            json!("docs/plan.md")
        );
    }

    #[tokio::test]
    async fn a_declared_variable_the_next_step_renders_is_not_required() {
        let harness = Harness::new();
        let (engine, definition, pending) = raise_plan_problem(&harness).await;
        let requirement = pending
            .problem
            .as_ref()
            .and_then(|problem| problem.requirement_for("blocked"))
            .expect("blocked");
        assert!(requirement.variables.is_empty(), "{requirement:?}");

        let execution = engine
            .deliver_human_input(
                &definition,
                &pending.execution_id,
                &HumanInputDelivery::label("blocked"),
            )
            .await
            .expect("plan_blocker renders empty");

        assert_eq!(execution.status, ExecutionStatus::Completed);
    }

    #[tokio::test]
    async fn the_conductor_resolving_a_problem_is_named_as_the_decider() {
        let harness = Harness::new();
        let (engine, definition, execution_id, pending) =
            raise_problem(&harness, json!({ "summary": "done" })).await;

        engine
            .deliver_human_input(
                &definition,
                &execution_id,
                &HumanInputDelivery::label("implemented")
                    .for_episode(pending.episode_key())
                    .answered_by(AnsweredBy::Conductor),
            )
            .await
            .expect("resolve the problem");

        let events = harness.events(&execution_id);
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::DecisionMade { provider, .. } if provider == "conductor"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::HumanInputReceived {
                answered_by: Some(AnsweredBy::Conductor),
                close_reason: Some(InputCloseReason::Answered),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn an_answer_to_a_closed_problem_is_refused() {
        let harness = Harness::new();
        let (engine, definition, execution_id, pending) =
            raise_problem(&harness, json!({ "summary": "done" })).await;
        let delivery = HumanInputDelivery::label("implemented").for_episode(pending.episode_key());

        engine
            .deliver_human_input(&definition, &execution_id, &delivery)
            .await
            .expect("the first answer is taken");
        let error = engine
            .deliver_human_input(&definition, &execution_id, &delivery)
            .await
            .expect_err("the second finds nothing pending");
        assert!(
            matches!(&error, EngineError::HumanInput(human) if human.is_stale()),
            "{error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Operator transitions (U3)
// ---------------------------------------------------------------------------

mod operator_transitions {
    use narsor_engine::engine::{plan_operator_transition, EngineError, ManualTransition};

    fn definition() -> narsor_engine::domain::WorkflowDefinition {
        narsor_engine::workflow::load_from_str(
            r#"
name: operator-plan
version: 1
states:
  open:
    allow_manual_override: true
    action:
      type: script
      command: "true"
    choices: [a, b, c]
    transitions:
      a: review
      b: review
      c: closed
  review:
    action:
      type: script
      command: "true"
    choices: [done]
    transitions:
      done: closed
  closed:
    description: Closed.
terminal_states: [closed]
"#,
        )
        .expect("a valid workflow")
    }

    fn invalid(error: EngineError) -> String {
        match error {
            EngineError::InvalidTransition { message } => message,
            other => panic!("expected an invalid transition, got {other:?}"),
        }
    }

    #[test]
    fn a_declared_exit_is_named_by_the_state_it_leads_to() {
        let definition = definition();
        let open = &definition.states["open"];

        let plan = plan_operator_transition(&definition, open, "closed", false).unwrap();

        assert_eq!(plan.label.as_deref(), Some("c"));
        assert_eq!(plan.target, "closed");
    }

    #[test]
    fn several_labels_into_one_state_pick_the_lowest_label() {
        let definition = definition();
        let open = &definition.states["open"];

        let plan = plan_operator_transition(&definition, open, "review", false).unwrap();

        assert_eq!(plan.label.as_deref(), Some("a"));
    }

    #[test]
    fn a_state_no_declared_transition_leads_to_is_refused_without_override() {
        let definition = definition();
        let review = &definition.states["review"];

        let message =
            invalid(plan_operator_transition(&definition, review, "open", false).unwrap_err());

        assert!(
            message.contains("no declared transition into 'open'"),
            "{message}"
        );
    }

    #[test]
    fn an_override_needs_a_state_that_allows_it_and_a_target_that_exists() {
        let definition = definition();
        let open = &definition.states["open"];
        let review = &definition.states["review"];

        let plan = plan_operator_transition(&definition, open, "closed", true).unwrap();
        assert_eq!(plan.label, None, "an override has no declared label");
        assert!(
            invalid(plan_operator_transition(&definition, open, "nowhere", true).unwrap_err())
                .contains("no state 'nowhere'")
        );
        assert!(invalid(
            plan_operator_transition(&definition, review, "closed", true).unwrap_err()
        )
        .contains("allow_manual_override"));
    }

    #[test]
    fn the_operators_ordinary_and_forced_choices_keep_their_audit_providers() {
        let ordinary = ManualTransition::by_operator(Some("done".into()), None, false);
        let forced = ManualTransition::by_operator(None, Some("closed".into()), true);

        assert_eq!(ordinary.provider, "human");
        assert_eq!(forced.provider, "human_override");
        assert_ne!(ordinary.reason, forced.reason);
        assert!(forced.force && !ordinary.force);
    }
}
