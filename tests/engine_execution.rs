//! Integration tests for the engine loop (U7).
//!
//! Every test runs a real workflow against a real store and real child
//! processes. What is asserted is mostly the *event log*: it is the contract
//! recovery is derived from, so a change in the order of emission has to break
//! a test here rather than surface as a mysterious `Unknown` execution later.

mod engine_support;

use std::sync::Arc;
use std::time::Duration;

use engine_support::{event_type, workflow, workflow_file, Harness, StubAgentExecutor};
use orchestrator::domain::{EventData, ExecutionStatus};
use orchestrator::engine::{ActionKind, Engine, EngineError, HumanInputDelivery};
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

    assert_eq!(execution.status, ExecutionStatus::Paused);
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
            "action_retrying",
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
            "action_retrying",
            "action_failed",
            "workflow_failed",
        ]
    );
    assert!(!types.contains(&"transition_taken".to_string()));
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
    assert_eq!(execution.status, ExecutionStatus::Paused);

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

    // The §4 lifecycle, state by state.
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

    assert_eq!(execution.status, ExecutionStatus::Paused);
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
    assert_eq!(pending.outputs, vec!["clarified", "abort"]);

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
