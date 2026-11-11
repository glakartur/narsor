//! The conductor profiles the steps that have not started, once, after the
//! plan step. The conductor is scripted behind the real registry and actor
//! (as in `conductor_title`); what is under test is what lands in the
//! store: lower profiles kept, a request above the ceiling started on the
//! ceiling and recorded, a conductor that fails leaving nothing behind.

use crate::engine_support;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use engine_support::{workflow, Harness};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::profile::{ProfileCandidate, ProfileRequest};
use narsor_engine::conductor::{
    ConductorAnswer, ConductorInvoker, ConductorRegistry, ConductorStore, Invocation,
    InvocationOutcome,
};
use narsor_engine::domain::ConductorPurpose;
use narsor_engine::engine::step_profile::Setting;
use narsor_engine::engine::Engine;
use serde_json::{json, Value};

struct Scripted {
    answers: Mutex<VecDeque<Option<Value>>>,
    calls: Mutex<Vec<Invocation>>,
}

impl ConductorInvoker for Scripted {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(invocation.clone());
        let answer = self.answers.lock().unwrap().pop_front().flatten();
        Box::pin(async move {
            match answer {
                Some(response) => InvocationOutcome::Answered(ConductorAnswer {
                    message: None,
                    response,
                }),
                None => InvocationOutcome::Failed {
                    error: "the scripted conductor failed".to_string(),
                },
            }
        })
    }
}

struct Fixture {
    harness: Harness,
    _engine: Arc<Engine>,
    registry: Arc<ConductorRegistry>,
    conductor: Arc<Scripted>,
    execution_id: String,
}

impl Fixture {
    fn new(answers: Vec<Option<Value>>) -> Self {
        let harness = Harness::new();
        let engine = Arc::new(harness.engine());
        let conductor = Arc::new(Scripted {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
        });
        let registry = assemble(&engine, conductor.clone(), None);
        let execution = engine
            .start(&workflow("human_decision"), json!({}), harness.workspace())
            .expect("start the execution");
        Self {
            harness,
            _engine: engine,
            registry,
            conductor,
            execution_id: execution.id,
        }
    }

    fn store(&self) -> ConductorStore {
        ConductorStore::new(self.harness.store().paths(), &self.execution_id)
    }

    async fn profile(&self) -> InvocationOutcome {
        self.registry
            .actor(&self.execution_id)
            .expect("the actor")
            .invoke(request().into_queued())
            .await
            .expect("the invocation")
    }
}

fn request() -> ProfileRequest {
    ProfileRequest {
        plan_step: "plan".into(),
        variables: json!({ "complexity": { "files": 2 } }),
        artifacts: json!({ "plan": "docs/plan.md" }),
        complexity: Some(json!({ "files": 2 })),
        steps: vec![
            ProfileCandidate {
                step_id: "work".into(),
                default: Setting::new(Some("medium"), Some("medium")),
                ceiling: Some(Setting::new(Some("large"), Some("high"))),
            },
            ProfileCandidate {
                step_id: "review".into(),
                default: Setting::new(Some("medium"), Some("medium")),
                ceiling: Some(Setting::new(Some("large"), Some("high"))),
            },
        ],
    }
}

fn answer(steps: Value) -> Option<Value> {
    Some(json!({ "decision": "profile", "steps": steps, "confidence": "high" }))
}

#[tokio::test]
async fn a_lower_profile_is_stored_with_its_rationale() {
    let fixture = Fixture::new(vec![answer(json!([
        { "step_id": "work", "model": "small", "effort": "low", "rationale": "one small change" }
    ]))]);

    fixture.profile().await;

    let call = fixture.conductor.calls.lock().unwrap()[0].clone();
    assert_eq!(call.request.purpose, ConductorPurpose::ProfileSteps);
    let profile = fixture.store().load_profile().unwrap();
    let entry = profile.lookup("work", None).expect("an entry for work");
    assert_eq!(entry.setting, Setting::new(Some("small"), Some("low")));
    assert_eq!(entry.rationale, "one small change");
    assert!(profile.lookup("review", None).is_none());
}

#[tokio::test]
async fn a_request_above_the_ceiling_starts_on_the_ceiling_and_is_recorded() {
    let fixture = Fixture::new(vec![answer(json!([
        { "step_id": "work", "model": "large", "effort": "max", "rationale": "risky migration" },
        { "step_id": "review", "model": "large", "effort": "high", "rationale": "within" }
    ]))]);

    fixture.profile().await;

    let profile = fixture.store().load_profile().unwrap();
    let work = profile.lookup("work", None).unwrap();
    assert_eq!(work.setting, Setting::new(Some("large"), Some("high")));
    assert_eq!(
        work.requested_over_ceiling,
        Some(Setting::new(None, Some("max")))
    );
    let review = profile.lookup("review", None).unwrap();
    assert_eq!(review.requested_over_ceiling, None);
}

#[tokio::test]
async fn an_unsure_entry_and_a_unit_entry_are_handled() {
    let fixture = Fixture::new(vec![answer(json!([
        { "step_id": "work", "unit": "3", "model": "large", "effort": "high", "rationale": "unit 3" },
        { "step_id": "review", "model": "small", "effort": "low", "rationale": "?", "confident": false }
    ]))]);

    fixture.profile().await;

    let profile = fixture.store().load_profile().unwrap();
    assert!(profile.lookup("work", Some("3")).is_some());
    assert!(profile.lookup("review", None).is_none());
}

#[tokio::test]
async fn a_conductor_that_fails_leaves_no_profile() {
    let fixture = Fixture::new(vec![None]);

    let outcome = fixture.profile().await;

    assert!(matches!(outcome, InvocationOutcome::Failed { .. }));
    assert!(fixture.store().load_profile().unwrap().entries.is_empty());
}

#[tokio::test]
async fn a_second_profile_does_not_replace_the_first() {
    let fixture = Fixture::new(vec![
        answer(
            json!([{ "step_id": "work", "model": "small", "effort": "low", "rationale": "first" }]),
        ),
        answer(
            json!([{ "step_id": "work", "model": "large", "effort": "high", "rationale": "second" }]),
        ),
    ]);

    fixture.profile().await;
    fixture.profile().await;

    let profile = fixture.store().load_profile().unwrap();
    assert_eq!(profile.lookup("work", None).unwrap().rationale, "first");
}

#[tokio::test]
async fn a_step_that_already_started_is_not_profiled() {
    // `check` is the state the execution was started in: it was entered once
    // while the conductor was still answering (D7).
    let fixture = Fixture::new(vec![answer(json!([
        { "step_id": "check", "model": "small", "effort": "low", "rationale": "late" },
        { "step_id": "work", "model": "small", "effort": "low", "rationale": "in time" }
    ]))]);
    let mut execution = fixture
        .harness
        .store()
        .load_execution(&fixture.execution_id)
        .expect("the execution");
    execution.metadata = json!({ "engine": { "state_entries": { "check": 1 } } });
    fixture
        .harness
        .store()
        .save_execution(&execution)
        .expect("save the execution");
    let mut asked = request();
    asked.steps.push(ProfileCandidate {
        step_id: "check".into(),
        default: Setting::new(Some("medium"), Some("medium")),
        ceiling: Some(Setting::new(Some("large"), Some("high"))),
    });

    fixture
        .registry
        .actor(&fixture.execution_id)
        .expect("the actor")
        .invoke(asked.into_queued())
        .await
        .expect("the invocation");

    let profile = fixture.store().load_profile().unwrap();
    assert!(profile.lookup("check", None).is_none());
    assert!(profile.lookup("work", None).is_some());
}

// ---------------------------------------------------------------------------
// The engine: the request after the plan step, and the profile applied when a
// step starts.
// ---------------------------------------------------------------------------

use std::time::Duration;

use engine_support::{workflow_file, AgentCall, AgentTurn, ScriptedAgentExecutor};
use narsor_engine::conductor::store::ProfileEntry;
use narsor_engine::domain::{ActionConfig, EventData, ExecutionStatus, WorkflowExecution};
use narsor_engine::engine::ActionKind;

/// What the scripted conductor does with one request.
enum Script {
    Answer(Value),
    Fail,
    Interrupted,
    /// Never answers.
    Hang,
}

struct EngineConductor {
    scripts: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<Invocation>>,
}

impl ConductorInvoker for EngineConductor {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(invocation.clone());
        let script = self.scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            match script.expect("the conductor was asked more often than scripted") {
                Script::Answer(response) => InvocationOutcome::Answered(ConductorAnswer {
                    message: None,
                    response,
                }),
                Script::Fail => InvocationOutcome::Failed {
                    error: "the scripted conductor failed".to_string(),
                },
                Script::Interrupted => InvocationOutcome::Interrupted,
                Script::Hang => std::future::pending().await,
            }
        })
    }
}

struct Flow {
    harness: Harness,
    engine: Arc<Engine>,
    agent: Arc<ScriptedAgentExecutor>,
    conductor: Arc<EngineConductor>,
    /// Kept: the engine holds the registry weakly.
    _registry: Option<Arc<ConductorRegistry>>,
    definition: narsor_engine::domain::WorkflowDefinition,
}

fn exit() -> AgentTurn {
    AgentTurn::label("exit", "done")
}

impl Flow {
    /// The profile workflow, the agent playing `turns`, the conductor
    /// `scripts`; `attach` says whether a conductor exists at all.
    fn new(turns: Vec<AgentTurn>, scripts: Vec<Script>, attach: bool) -> Self {
        Self::of("v2/engine_step_profile.yaml", turns, scripts, attach, |e| e)
    }

    fn of(
        file: &str,
        turns: Vec<AgentTurn>,
        scripts: Vec<Script>,
        attach: bool,
        tune: impl FnOnce(Engine) -> Engine,
    ) -> Self {
        let harness = Harness::new();
        let agent = Arc::new(ScriptedAgentExecutor::new(turns));
        let engine = Arc::new(tune(
            harness
                .engine()
                .with_executor(ActionKind::Agent, agent.clone()),
        ));
        let conductor = Arc::new(EngineConductor {
            scripts: Mutex::new(scripts.into()),
            calls: Mutex::new(Vec::new()),
        });
        let registry = attach.then(|| assemble(&engine, conductor.clone(), None));
        Self {
            harness,
            engine,
            agent,
            conductor,
            _registry: registry,
            definition: workflow_file(file),
        }
    }

    async fn run(&self) -> WorkflowExecution {
        self.engine
            .start_and_run(&self.definition, json!({}), self.harness.workspace())
            .await
            .expect("run the workflow")
    }

    fn calls(&self) -> Vec<Invocation> {
        self.conductor.calls.lock().unwrap().clone()
    }

    /// The `step_profile` events: (message, context).
    fn profile_events(&self, execution_id: &str) -> Vec<(String, Value)> {
        self.harness
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

    fn call_of(&self, prompt: &str) -> Vec<AgentCall> {
        self.agent
            .calls()
            .into_iter()
            .filter(|call| call.prompt.starts_with(prompt))
            .collect()
    }
}

fn profile_answer(steps: Value) -> Script {
    Script::Answer(json!({ "decision": "profile", "steps": steps, "confidence": "high" }))
}

fn setting_of(call: &AgentCall) -> (Option<&str>, Option<&str>) {
    (call.model.as_deref(), call.effort.as_deref())
}

#[tokio::test]
async fn the_request_lists_the_agent_steps_that_have_not_started_and_not_the_plan() {
    let flow = Flow::new(vec![exit(); 3], vec![Script::Fail], true);

    let execution = flow.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let calls = flow.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.purpose, ConductorPurpose::ProfileSteps);
    let asked: ProfileRequest =
        serde_json::from_value(calls[0].request.payload.clone()).expect("the payload");
    let ids: Vec<&str> = asked.steps.iter().map(|s| s.step_id.as_str()).collect();
    assert_eq!(ids, ["review", "work"]);
    assert_eq!(asked.plan_step, "plan");
    let work = asked.steps.iter().find(|s| s.step_id == "work").unwrap();
    assert_eq!(work.default, Setting::new(Some("medium"), Some("low")));
    assert_eq!(
        work.ceiling,
        Some(Setting::new(Some("large"), Some("high")))
    );
}

#[tokio::test]
async fn nothing_is_asked_without_a_conductor_a_plan_flag_or_steps() {
    // No conductor.
    let flow = Flow::new(vec![exit(); 3], vec![], false);
    assert_eq!(flow.run().await.status, ExecutionStatus::Completed);
    assert!(flow.calls().is_empty());

    // No plan flag.
    let mut flow = Flow::new(vec![exit(); 4], vec![], true);
    let Some(ActionConfig::Agent(plan)) =
        &mut flow.definition.states.get_mut("plan").unwrap().action
    else {
        panic!("an agent step");
    };
    plan.plan = false;
    assert_eq!(flow.run().await.status, ExecutionStatus::Completed);
    assert!(flow.calls().is_empty());

    // A plan step with no agent step after it.
    let flow = Flow::of(
        "v2/engine_step_profile_alone.yaml",
        vec![exit()],
        vec![],
        true,
        |engine| engine,
    );
    assert_eq!(flow.run().await.status, ExecutionStatus::Completed);
    assert!(flow.calls().is_empty());
}

#[tokio::test]
async fn a_failing_interrupted_or_silent_conductor_is_recorded_and_the_task_goes_on() {
    for (script, wait, what) in [
        (Script::Fail, None, "the scripted conductor failed"),
        (Script::Interrupted, None, "interrupted"),
        (
            Script::Hang,
            Some(Duration::from_millis(50)),
            "did not answer in time",
        ),
    ] {
        let flow = Flow::of(
            "v2/engine_step_profile.yaml",
            vec![exit(); 3],
            vec![script],
            true,
            |engine| match wait {
                Some(wait) => engine.with_profile_wait(wait),
                None => engine,
            },
        );

        let started = std::time::Instant::now();
        let execution = flow.run().await;

        assert_eq!(execution.status, ExecutionStatus::Completed, "{what}");
        assert!(started.elapsed() < Duration::from_secs(30), "{what}");
        let events = flow.profile_events(&execution.id);
        assert_eq!(events.len(), 1, "{what}: {events:?}");
        assert!(events[0].0.contains(what), "{what}: {}", events[0].0);
        assert_eq!(events[0].1["step_profile"]["plan_step"], "plan");
        // Every step ran on its default.
        assert_eq!(
            setting_of(&flow.call_of("Do it")[0]),
            (Some("medium"), Some("low"))
        );
    }
}

#[tokio::test]
async fn the_profile_sets_the_model_and_the_effort_of_the_step() {
    let flow = Flow::new(
        vec![exit(); 3],
        vec![profile_answer(json!([
            { "step_id": "work", "model": "large", "effort": "high", "rationale": "risky" },
            { "step_id": "review", "model": "small", "effort": "low", "rationale": "small" }
        ]))],
        true,
    );

    let execution = flow.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        setting_of(&flow.call_of("Plan it")[0]),
        (Some("medium"), Some("medium"))
    );
    assert_eq!(
        setting_of(&flow.call_of("Do it")[0]),
        (Some("large"), Some("high"))
    );
    assert_eq!(
        setting_of(&flow.call_of("Review it")[0]),
        (Some("small"), Some("low"))
    );
    assert!(flow.profile_events(&execution.id).is_empty());
}

fn escalation_flow(turns: Vec<AgentTurn>, scripts: Vec<Script>) -> Flow {
    Flow::of(
        "v2/engine_step_profile.yaml",
        turns,
        scripts,
        true,
        |engine| engine.with_escalation_threshold(2),
    )
}

#[tokio::test]
async fn a_step_that_fails_repeatedly_is_raised_within_its_ceiling() {
    // plan; work fails twice, then succeeds; review.
    let flow = escalation_flow(
        vec![exit(), AgentTurn::Fail, AgentTurn::Fail, exit(), exit()],
        vec![Script::Fail],
    );

    let execution = flow.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let work: Vec<_> = flow
        .call_of("Do it")
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
    let events = flow.profile_events(&execution.id);
    let raised: Vec<_> = events
        .iter()
        .filter(|(message, _)| message.contains("is raised"))
        .collect();
    assert_eq!(raised.len(), 1, "{events:?}");
    assert_eq!(raised[0].1["step_profile"]["step"], "work");
}

#[tokio::test]
async fn the_limit_is_announced_only_for_a_step_with_a_ceiling() {
    // `work` already stands on its ceiling; `review` has none.
    let flow = escalation_flow(
        vec![
            exit(),
            AgentTurn::Fail,
            AgentTurn::Fail,
            exit(),
            AgentTurn::Fail,
            AgentTurn::Fail,
            exit(),
        ],
        vec![profile_answer(json!([
            { "step_id": "work", "model": "large", "effort": "high", "rationale": "top" },
            { "step_id": "review", "model": "medium", "effort": "medium", "rationale": "same" }
        ]))],
    );

    let execution = flow.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let events = flow.profile_events(&execution.id);
    let limits: Vec<_> = events
        .iter()
        .filter(|(message, _)| message.contains("already runs on its ceiling"))
        .collect();
    assert_eq!(limits.len(), 1, "{events:?}");
    assert_eq!(limits[0].1["step_profile"]["step"], "work");
}

fn entry(step: &str, model: &str, effort: &str) -> ProfileEntry {
    ProfileEntry {
        step_id: step.to_string(),
        unit: None,
        setting: Setting::new(Some(model), Some(effort)),
        rationale: "test".to_string(),
        requested_over_ceiling: None,
        decided_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn an_entry_that_cannot_be_used_keeps_the_default_and_is_recorded() {
    let flow = Flow::new(vec![exit(); 3], vec![], false);
    let execution = flow
        .engine
        .start(&flow.definition, json!({}), flow.harness.workspace())
        .expect("start");
    ConductorStore::new(flow.harness.store().paths(), &execution.id)
        .add_profile_entries(
            vec![entry("work", "mystery", "max")],
            &std::collections::BTreeSet::new(),
        )
        .expect("write the profile");

    let execution = flow
        .engine
        .run(&flow.definition, &execution.id)
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    // "max" is above the ceiling's "high": clamped; the model is rejected.
    let work = &flow.call_of("Do it")[0];
    assert_eq!(work.model.as_deref(), Some("medium"));
    let events = flow.profile_events(&execution.id);
    assert!(
        events
            .iter()
            .any(|(message, _)| message.contains("cannot be used")),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_damaged_profile_means_no_profile() {
    let flow = Flow::new(vec![exit(); 3], vec![], false);
    let execution = flow
        .engine
        .start(&flow.definition, json!({}), flow.harness.workspace())
        .expect("start");
    let store = ConductorStore::new(flow.harness.store().paths(), &execution.id);
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.dir().join("profile.json"), "{ not json").unwrap();

    let execution = flow
        .engine
        .run(&flow.definition, &execution.id)
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        setting_of(&flow.call_of("Do it")[0]),
        (Some("medium"), Some("low"))
    );
    assert!(flow.profile_events(&execution.id).is_empty());
}

#[tokio::test]
async fn a_reprompt_resumes_the_session_on_the_profiled_model_and_effort() {
    let flow = Flow::new(
        vec![
            exit(),
            AgentTurn::NoLabel {
                reply: Some("no label"),
                session: true,
            },
            exit(),
            exit(),
        ],
        vec![profile_answer(json!([
            { "step_id": "work", "model": "large", "effort": "high", "rationale": "risky" }
        ]))],
        true,
    );

    let execution = flow.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let work: Vec<_> = flow
        .agent
        .calls()
        .into_iter()
        .filter(|call| call.model.as_deref() == Some("large"))
        .collect();
    assert_eq!(work.len(), 2, "{:?}", flow.agent.calls());
    assert!(work[0].resume_session.is_none());
    assert!(work[1].resume_session.is_some());
    assert_eq!(setting_of(&work[1]), (Some("large"), Some("high")));
}
