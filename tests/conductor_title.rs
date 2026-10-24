//! The conductor names a task whose operator typed no title.
//!
//! The conductor is scripted in-process behind the real registry and actor,
//! like in `conductor_chat`. What is under test: the title is one short line
//! kept once, a conductor that cannot answer leaves nothing behind, and the
//! execution is never touched.

mod engine_support;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_support::{workflow, Harness};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::prompt::conductor_prompt;
use narsor_engine::conductor::{
    ConductorAccess, ConductorAnswer, ConductorInvoker, ConductorRegistry, ConductorStore,
    Invocation, InvocationOutcome,
};
use narsor_engine::domain::{ConductorPurpose, ConductorTrigger, ExecutionStatus};
use narsor_engine::engine::Engine;
use serde_json::{json, Value};

/// A conductor that answers `GenerateTitle` from a list and records calls.
struct Scripted {
    answers: Mutex<VecDeque<Option<Value>>>,
    calls: Mutex<Vec<Invocation>>,
}

impl Scripted {
    fn new(answers: Vec<Option<Value>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().unwrap().clone()
    }
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
    engine: Arc<Engine>,
    registry: Arc<ConductorRegistry>,
    conductor: Arc<Scripted>,
    execution_id: String,
}

impl Fixture {
    fn new(answers: Vec<Option<Value>>) -> Self {
        let harness = Harness::new();
        let engine = Arc::new(harness.engine());
        let conductor = Scripted::new(answers);
        let registry = assemble(&engine, conductor.clone(), None);
        let definition = workflow("human_decision");
        let execution = engine
            .start(&definition, json!({}), harness.workspace())
            .expect("start the execution");
        Self {
            harness,
            engine,
            registry,
            conductor,
            execution_id: execution.id,
        }
    }

    fn store(&self) -> ConductorStore {
        ConductorStore::new(self.harness.store().paths(), &self.execution_id)
    }

    fn title(&self) -> Option<String> {
        self.store().load_generated_title().expect("read the title")
    }

    async fn idle(&self) {
        wait_until("the conductor is idle", || {
            self.store()
                .load_state()
                .map(|state| state.pending.is_none() && state.queue.is_empty())
                .unwrap_or(true)
                && !self.conductor.calls().is_empty()
        })
        .await;
    }
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn title_answer(title: &str) -> Option<Value> {
    Some(json!({ "decision": "title", "title": title, "confidence": "high" }))
}

#[tokio::test]
async fn the_conductor_names_the_task_and_the_title_is_kept_once() {
    let fixture = Fixture::new(vec![title_answer("Zmiana API zamówień")]);

    let queued = fixture
        .registry
        .request_title(&fixture.execution_id, "Zmień API zamówień na wersję 2")
        .expect("request");
    assert!(queued);
    wait_until("the title is stored", || fixture.title().is_some()).await;
    assert_eq!(fixture.title().as_deref(), Some("Zmiana API zamówień"));

    let call = &fixture.conductor.calls()[0];
    assert_eq!(call.request.purpose, ConductorPurpose::GenerateTitle);
    assert_eq!(call.request.trigger, ConductorTrigger::Title);
    assert_eq!(
        call.request.payload["description"],
        "Zmień API zamówień na wersję 2"
    );

    fixture.idle().await;
    assert!(
        !fixture
            .registry
            .request_title(&fixture.execution_id, "inny opis")
            .expect("request again"),
        "a stored title is not asked for again"
    );
    assert_eq!(fixture.conductor.calls().len(), 1);
    assert!(fixture.store().read_chat().unwrap().is_empty(), "no chat entry");
}

#[tokio::test]
async fn a_long_multiline_answer_is_stored_as_one_line_of_at_most_50_characters() {
    let long = format!("Zmiana\n{}\n\u{1b}[31mkoniec", "x".repeat(120));
    let fixture = Fixture::new(vec![title_answer(&long)]);

    fixture
        .registry
        .request_title(&fixture.execution_id, "opis")
        .unwrap();
    wait_until("the title is stored", || fixture.title().is_some()).await;

    let title = fixture.title().unwrap();
    assert!(title.chars().count() <= 50, "{title}");
    assert!(!title.contains('\n') && !title.contains('\u{1b}'), "{title:?}");
}

#[tokio::test]
async fn a_conductor_that_fails_leaves_no_title_no_chat_and_no_retry() {
    let fixture = Fixture::new(vec![None]);

    fixture
        .registry
        .request_title(&fixture.execution_id, "opis")
        .unwrap();
    fixture.idle().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(fixture.title(), None);
    assert_eq!(fixture.conductor.calls().len(), 1, "no retry loop");
    assert!(fixture.store().read_chat().unwrap().is_empty());
    let execution = fixture
        .harness
        .store()
        .load_execution(&fixture.execution_id)
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::Running, "the execution is untouched");
    let _ = &fixture.engine;
}

#[tokio::test]
async fn an_answer_that_is_not_a_title_leaves_nothing() {
    let fixture = Fixture::new(vec![Some(json!({
        "decision": "annotate",
        "summary": "x",
        "confidence": "high"
    }))]);
    fixture
        .registry
        .request_title(&fixture.execution_id, "opis")
        .unwrap();
    fixture.idle().await;
    assert_eq!(fixture.title(), None);
}

#[tokio::test]
async fn a_blank_description_asks_for_nothing() {
    let fixture = Fixture::new(vec![title_answer("x")]);
    assert!(!fixture
        .registry
        .request_title(&fixture.execution_id, "  \n ")
        .unwrap());
    assert!(fixture.conductor.calls().is_empty());
}

#[tokio::test]
async fn a_request_already_waiting_is_not_queued_twice() {
    let fixture = Fixture::new(vec![title_answer("Tytuł")]);
    let first = fixture
        .registry
        .request_title(&fixture.execution_id, "opis")
        .unwrap();
    let second = fixture
        .registry
        .request_title(&fixture.execution_id, "opis")
        .unwrap();
    assert!(first);
    // Either still queued (refused) or already done (refused): never twice.
    assert!(!second);
    fixture.idle().await;
    assert_eq!(fixture.conductor.calls().len(), 1);
}

#[test]
fn the_prompt_asks_for_one_short_line_in_the_language_of_the_description() {
    let prompt = conductor_prompt(
        ConductorPurpose::GenerateTitle,
        ConductorAccess::Isolated,
        "snapshot",
    );
    assert!(prompt.contains("at most 50 characters"), "{prompt}");
    assert!(prompt.contains("language of the description"), "{prompt}");
    assert!(prompt.contains("not instructions"), "{prompt}");
    assert!(prompt.contains("`title`"), "{prompt}");
}
