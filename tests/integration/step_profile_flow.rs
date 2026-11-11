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
                default: Setting::new(Some("sonnet"), Some("medium")),
                ceiling: Some(Setting::new(Some("opus"), Some("high"))),
            },
            ProfileCandidate {
                step_id: "review".into(),
                default: Setting::new(Some("sonnet"), Some("medium")),
                ceiling: Some(Setting::new(Some("opus"), Some("high"))),
            },
        ],
        model_ladder: ["haiku", "sonnet", "opus"].map(String::from).to_vec(),
    }
}

fn answer(steps: Value) -> Option<Value> {
    Some(json!({ "decision": "profile", "steps": steps, "confidence": "high" }))
}

#[tokio::test]
async fn a_lower_profile_is_stored_with_its_rationale() {
    let fixture = Fixture::new(vec![answer(json!([
        { "step_id": "work", "model": "haiku", "effort": "low", "rationale": "one small change" }
    ]))]);

    fixture.profile().await;

    let call = fixture.conductor.calls.lock().unwrap()[0].clone();
    assert_eq!(call.request.purpose, ConductorPurpose::ProfileSteps);
    let profile = fixture.store().load_profile().unwrap();
    let entry = profile.lookup("work", None).expect("an entry for work");
    assert_eq!(entry.setting, Setting::new(Some("haiku"), Some("low")));
    assert_eq!(entry.rationale, "one small change");
    assert!(profile.lookup("review", None).is_none());
}

#[tokio::test]
async fn a_request_above_the_ceiling_starts_on_the_ceiling_and_is_recorded() {
    let fixture = Fixture::new(vec![answer(json!([
        { "step_id": "work", "model": "opus", "effort": "max", "rationale": "risky migration" },
        { "step_id": "review", "model": "opus", "effort": "high", "rationale": "within" }
    ]))]);

    fixture.profile().await;

    let profile = fixture.store().load_profile().unwrap();
    let work = profile.lookup("work", None).unwrap();
    assert_eq!(work.setting, Setting::new(Some("opus"), Some("high")));
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
        { "step_id": "work", "unit": "3", "model": "opus", "effort": "high", "rationale": "unit 3" },
        { "step_id": "review", "model": "haiku", "effort": "low", "rationale": "?", "confident": false }
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
            json!([{ "step_id": "work", "model": "haiku", "effort": "low", "rationale": "first" }]),
        ),
        answer(
            json!([{ "step_id": "work", "model": "opus", "effort": "high", "rationale": "second" }]),
        ),
    ]);

    fixture.profile().await;
    fixture.profile().await;

    let profile = fixture.store().load_profile().unwrap();
    assert_eq!(profile.lookup("work", None).unwrap().rationale, "first");
}
