//! A step's model and effort without a `profile` step: escalation on the
//! defaults within the step's ceiling, and a task whose conductor still holds
//! a request of the removed conductor step profiling. The profile a `profile`
//! step stores is in `profile_step_flow`.

use crate::engine_support;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_support::{exit, workflow_file, AgentCall, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    ConductorInvoker, ConductorStore, Invocation, InvocationOutcome, QueuedRequest,
};
use narsor_engine::domain::{ConductorPurpose, ConductorTrigger, EventData, ExecutionStatus};
use narsor_engine::engine::ActionKind;
use serde_json::{json, Value};

/// The workflow: a first agent step, a script, `work` (with a ceiling and
/// retries) and `review`.
const WORKFLOW: &str = "v2/engine_step_profile.yaml";

/// The `step_profile` events: (message, context).
fn profile_events(harness: &Harness, execution_id: &str) -> Vec<(String, Value)> {
    harness
        .events(execution_id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::Error { message, context } if context.get("step_profile").is_some() => {
                Some((message, context))
            }
            _ => None,
        })
        .collect()
}

fn calls_of(agent: &ScriptedAgentExecutor, prompt: &str) -> Vec<AgentCall> {
    agent
        .calls()
        .into_iter()
        .filter(|call| call.prompt.starts_with(prompt))
        .collect()
}

#[tokio::test]
async fn a_step_that_fails_repeatedly_is_raised_within_its_ceiling() {
    // plan; work fails twice, then succeeds; review.
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        exit(),
        AgentTurn::Fail,
        AgentTurn::Fail,
        exit(),
        exit(),
    ]));
    let engine = harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone())
        .with_escalation_threshold(2);

    let execution = engine
        .start_and_run(&workflow_file(WORKFLOW), json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let work: Vec<_> = calls_of(&agent, "Do it")
        .iter()
        .map(|c| (c.model.clone(), c.effort.clone()))
        .collect();
    let seen = |model: &str, effort: &str| (Some(model.to_string()), Some(effort.to_string()));
    assert_eq!(
        work,
        [
            seen("medium", "low"),
            seen("medium", "low"),
            seen("medium", "medium")
        ]
    );
    let events = profile_events(&harness, &execution.id);
    let raised: Vec<_> = events
        .iter()
        .filter(|(message, _)| message.contains("is raised"))
        .collect();
    assert_eq!(raised.len(), 1, "{events:?}");
    assert_eq!(raised[0].1["step_profile"]["step"], "work");
}

// ---------------------------------------------------------------------------
// A conductor request left by a build that still profiled the steps
// ---------------------------------------------------------------------------

/// A conductor that must not be asked anything.
#[derive(Default)]
struct Silent {
    calls: Mutex<Vec<Invocation>>,
}

impl ConductorInvoker for Silent {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(invocation.clone());
        Box::pin(async {
            InvocationOutcome::Failed {
                error: "the conductor was asked".to_string(),
            }
        })
    }
}

/// `state.json` of the conductor as an older build wrote it, with its step
/// profile request queued and under way.
fn old_conductor_state(queued_at: &str) -> Value {
    let request = json!({
        "request_id": "old-profile",
        "trigger": "profile",
        "purpose": "profile_steps",
        "payload": {
            "plan_step": "plan",
            "variables": {},
            "artifacts": {},
            "complexity": { "files": 3 },
            "steps": [{ "step_id": "work", "default": {}, "ceiling": null }],
        },
        "queued_at": queued_at,
    });
    json!({
        "version": 1,
        "pending": {
            "invocation_id": "old-invocation",
            "request": request,
            "started_at": queued_at,
        },
        "queue": [request],
    })
}

#[test]
fn an_old_step_profile_request_reads_as_unrecognized() {
    let state = old_conductor_state("2026-11-01T10:00:00Z");

    let request: QueuedRequest = serde_json::from_value(state["queue"][0].clone()).unwrap();

    assert_eq!(request.trigger, ConductorTrigger::Unrecognized);
    assert_eq!(request.purpose, ConductorPurpose::Unrecognized);
}

#[tokio::test]
async fn a_task_whose_conductor_holds_an_old_step_profile_request_completes() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![exit(); 3]));
    let engine = Arc::new(
        harness
            .engine()
            .with_executor(ActionKind::Agent, agent.clone()),
    );
    let conductor = Arc::new(Silent::default());
    let registry = assemble(&engine, conductor.clone(), None);
    let definition = workflow_file(WORKFLOW);
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start the execution");
    let store = ConductorStore::new(harness.store().paths(), &execution.id);
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(
        store.dir().join("state.json"),
        serde_json::to_vec(&old_conductor_state("2026-11-01T10:00:00Z")).unwrap(),
    )
    .unwrap();

    // The daemon starts: the old request is served from the queue.
    let restore = registry
        .restore_at_startup()
        .expect("restore the conductor");
    let finished = engine
        .run(&definition, &execution.id)
        .await
        .expect("run the workflow");

    assert_eq!(finished.status, ExecutionStatus::Completed);
    assert!(restore.parked.is_empty(), "{:?}", restore.parked);
    let actor = registry.actor(&execution.id).expect("the actor");
    for _ in 0..100 {
        if actor.state().queue.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let state = store.load_state().expect("the conductor's state reads");
    assert!(state.queue.is_empty(), "{:?}", state.queue);
    assert!(state.pending.is_none(), "{:?}", state.pending);
    assert!(conductor.calls.lock().unwrap().is_empty());
    // Every step ran on its default.
    let work = calls_of(&agent, "Do it");
    assert_eq!(
        (work[0].model.as_deref(), work[0].effort.as_deref()),
        (Some("medium"), Some("low"))
    );
    // Nothing of the old request reached the event log.
    assert!(!harness
        .events(&execution.id)
        .iter()
        .any(|event| matches!(event.data, EventData::ConductorStarted { .. })));
}
