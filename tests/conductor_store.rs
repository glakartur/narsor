//! Integration tests for the conductor's store and actor (U8, KTD2, KTD11).
//!
//! The actual invocation of the conductor is U9's; here it is a stub
//! behind [`ConductorInvoker`], so what is under test is everything around
//! it: the files, the queue and its priorities, the marker, the chat
//! answers, the interrupts and the restart rows of the conductor itself.

mod engine_support;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use engine_support::{workflow, Harness};
use narsor::conductor::{
    Annotation, ChatAuthor, ChatEntry, ChatEntryKind, ConductorActor, ConductorAnswer,
    ConductorInvoker, ConductorRegistry, ConductorState, ConductorStore, Invocation,
    InvocationOutcome, InvocationQueue, PendingInvocation, QueuedRequest, SavedDecision,
    UnavailableInvoker, CONDUCTOR_UNAVAILABLE,
};
use narsor::domain::{
    ConductorDecisionKind, ConductorOutcome, ConductorPurpose, ConductorTrigger, Confidence,
    EventData,
};
use narsor::engine::Engine;
use serde_json::json;
use tokio::sync::{oneshot, Notify};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A stub conductor: answers every invocation with a message naming what it
/// was asked, remembers the order it was asked in, and — when told to —
/// holds its first invocation until released.
#[derive(Default)]
struct StubInvoker {
    calls: Mutex<Vec<Invocation>>,
    first_started: Notify,
    gate: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    hang_forever: bool,
    fail: bool,
}

impl StubInvoker {
    /// A stub whose first invocation waits for the returned sender.
    fn gated() -> (Self, oneshot::Sender<()>) {
        let (release, gate) = oneshot::channel();
        let stub = Self {
            gate: tokio::sync::Mutex::new(Some(gate)),
            ..Self::default()
        };
        (stub, release)
    }

    /// A stub whose invocations never end by themselves.
    fn hanging() -> Self {
        Self {
            hang_forever: true,
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().unwrap().clone()
    }

    fn purposes(&self) -> Vec<(ConductorTrigger, Option<String>)> {
        self.calls()
            .into_iter()
            .map(|call| {
                (
                    call.request.trigger,
                    call.chat_message.map(|message| message.text),
                )
            })
            .collect()
    }
}

impl ConductorInvoker for StubInvoker {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        Box::pin(async move {
            let first = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(invocation.clone());
                calls.len() == 1
            };
            if first {
                self.first_started.notify_one();
                if let Some(gate) = self.gate.lock().await.take() {
                    let _ = gate.await;
                }
            }
            if self.hang_forever {
                std::future::pending::<()>().await;
            }
            if self.fail {
                return InvocationOutcome::Failed {
                    error: "the stub failed".to_string(),
                };
            }
            InvocationOutcome::Answered(ConductorAnswer {
                message: Some(format!("answer to {}", invocation.request.request_id)),
                response: json!({}),
            })
        })
    }
}

/// A started execution, its engine and a registry over a stub.
struct Fixture {
    harness: Harness,
    engine: Arc<Engine>,
    execution_id: String,
}

impl Fixture {
    fn new() -> Self {
        let harness = Harness::new();
        let engine = Arc::new(harness.engine());
        let execution = engine
            .start(&workflow("two_steps"), json!({}), harness.workspace())
            .expect("start an execution");
        Self {
            harness,
            engine,
            execution_id: execution.id,
        }
    }

    fn store(&self) -> ConductorStore {
        ConductorStore::new(self.harness.store().paths(), &self.execution_id)
    }

    fn registry(&self, invoker: Arc<dyn ConductorInvoker>) -> ConductorRegistry {
        ConductorRegistry::new(Arc::clone(&self.engine), invoker)
    }

    fn actor(&self, registry: &ConductorRegistry) -> Arc<ConductorActor> {
        registry.actor(&self.execution_id).expect("open the actor")
    }

    fn events(&self) -> Vec<EventData> {
        self.harness
            .events(&self.execution_id)
            .into_iter()
            .map(|event| event.data)
            .collect()
    }
}

/// Polls `condition` every few milliseconds, failing after five seconds.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting until {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn operator_message(text: &str) -> ChatEntry {
    ChatEntry::new(ChatAuthor::Operator, ChatEntryKind::Message, text)
}

fn output_rescue(episode: &str) -> QueuedRequest {
    QueuedRequest::new(
        ConductorTrigger::OutputRescue,
        ConductorPurpose::ChooseOutput,
    )
    .with_episode(episode)
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn state_chat_and_annotations_survive_reopening_the_store() {
    let fixture = Fixture::new();
    let store = fixture.store();

    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "invocation-1".to_string(),
            request: output_rescue("episode-1"),
            started_at: Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.queue.push(
        QueuedRequest::new(
            ConductorTrigger::StepFailure,
            ConductorPurpose::DiagnoseFailure,
        )
        .with_episode("episode-2")
        .with_payload(json!({ "state": "first" })),
    );
    state.invocation_attempts.insert("episode-1".to_string(), 1);
    state.used_retries.insert("episode-2".to_string());
    state.unapplied_decisions.push(SavedDecision {
        decision_id: "decision-1".to_string(),
        kind: ConductorDecisionKind::ChooseOutput,
        episode: Some("episode-1".to_string()),
        state_id: Some("first".to_string()),
        output_label: Some("done".to_string()),
        data: json!({ "units_completed": 3 }),
        variables: json!({}),
        confidence: Confidence::High,
        rationale: Some("the agent said it is done".to_string()),
        context_before: json!({ "variables": {} }),
        saved_at: Utc::now(),
    });
    store.save_state(&state).expect("save the state");

    let first = store
        .append_chat(operator_message("how is it going?"))
        .expect("append a message");
    let second = store
        .append_chat(
            ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Message, "fine")
                .replying_to(&first.id),
        )
        .expect("append a reply");

    let annotation = Annotation {
        key: "attempt-1".to_string(),
        summary: "The plan covers every unit.".to_string(),
        recommendation: Some("approved".to_string()),
        created_at: Utc::now(),
    };
    store
        .save_annotation(&annotation)
        .expect("save an annotation");

    let reopened = ConductorStore::new(fixture.harness.store().paths(), &fixture.execution_id);
    assert_eq!(reopened.load_state().expect("load the state"), state);
    assert_eq!(
        reopened.read_chat().expect("read the chat"),
        vec![first.clone(), second.clone()]
    );
    assert_eq!((first.sequence, second.sequence), (1, 2));
    assert_eq!(
        reopened.read_chat_after(1).expect("read after 1"),
        vec![second]
    );
    assert!(reopened.unanswered_chat().expect("unanswered").is_empty());
    assert_eq!(
        reopened
            .load_annotation("attempt-1")
            .expect("load the annotation"),
        Some(annotation)
    );
}

#[tokio::test]
async fn an_execution_without_a_conductor_directory_has_an_empty_state() {
    let fixture = Fixture::new();
    let store = fixture.store();

    assert!(!store.exists());
    assert_eq!(
        store.load_state().expect("an empty state"),
        ConductorState::default()
    );
    assert!(store.read_chat().expect("an empty chat").is_empty());
    assert!(store
        .unanswered_chat()
        .expect("nothing unanswered")
        .is_empty());
    assert_eq!(store.load_annotation("any").expect("no annotation"), None);
    assert!(!store.exists(), "reading must not create the directory");

    // Opening an actor over it is fine too, and serves nothing.
    let stub = Arc::new(StubInvoker::default());
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);
    assert_eq!(actor.queued(), 0);
    let restore = registry.restore_at_startup().expect("restore");
    assert!(restore.requeued.is_empty() && restore.parked.is_empty());
    assert!(stub.calls().is_empty());
}

#[tokio::test]
async fn a_chat_line_cut_short_by_a_crash_is_skipped() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let entry = store
        .append_chat(operator_message("complete"))
        .expect("append");

    let path = store.dir().join("chat.jsonl");
    let mut raw = std::fs::read(&path).expect("read the chat file");
    raw.extend_from_slice(br#"{"id":"half-writ"#);
    std::fs::write(&path, raw).expect("write a torn line");

    assert_eq!(store.read_chat().expect("read the chat"), vec![entry]);
}

#[tokio::test]
async fn a_state_file_of_another_version_is_refused() {
    let fixture = Fixture::new();
    let store = fixture.store();
    std::fs::create_dir_all(store.dir()).expect("create the directory");
    std::fs::write(store.dir().join("state.json"), r#"{"version": 99}"#)
        .expect("write a future state");

    let error = store.load_state().expect_err("a newer version is refused");
    assert!(error.to_string().contains("99"), "{error}");
}

// ---------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------

#[test]
fn automatic_requests_are_taken_before_operator_requests() {
    let mut queue = InvocationQueue::new();
    for text in ["one", "two", "three"] {
        let message = operator_message(text);
        queue.push(QueuedRequest::for_chat(&message), text);
    }
    queue.push(output_rescue("episode"), "rescue");
    queue.push(
        QueuedRequest::new(
            ConductorTrigger::Annotation,
            ConductorPurpose::SummarizeGate,
        ),
        "annotation",
    );

    let order: Vec<&str> = std::iter::from_fn(|| queue.pop().map(|(_, item)| item)).collect();
    assert_eq!(order, ["rescue", "one", "two", "three", "annotation"]);
}

#[tokio::test]
async fn an_output_rescue_behind_three_chat_messages_runs_before_them() {
    let fixture = Fixture::new();
    let (stub, release) = StubInvoker::gated();
    let stub = Arc::new(stub);
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);

    // The first message is taken at once and held; the other two wait.
    let started = stub.first_started.notified();
    actor.post_chat("one").expect("post one");
    started.await;
    actor.post_chat("two").expect("post two");
    actor.post_chat("three").expect("post three");
    actor
        .submit(output_rescue("episode-1"))
        .expect("submit the rescue");
    let _ = release.send(());

    wait_until("every request was served", || stub.calls().len() == 4).await;
    assert_eq!(
        stub.purposes(),
        vec![
            (ConductorTrigger::Chat, Some("one".to_string())),
            (ConductorTrigger::OutputRescue, None),
            (ConductorTrigger::Chat, Some("two".to_string())),
            (ConductorTrigger::Chat, Some("three".to_string())),
        ]
    );
}

#[tokio::test]
async fn one_invocation_runs_at_a_time() {
    let fixture = Fixture::new();
    let (stub, release) = StubInvoker::gated();
    let stub = Arc::new(stub);
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);

    let started = stub.first_started.notified();
    actor.post_chat("one").expect("post one");
    started.await;
    actor.post_chat("two").expect("post two");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stub.calls().len(), 1, "the second waits for the first");

    let _ = release.send(());
    wait_until("both were served", || stub.calls().len() == 2).await;
}

#[tokio::test]
async fn concurrent_chat_messages_are_all_written_in_the_order_accepted() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubInvoker::default());
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);

    let posts: Vec<_> = (0..16)
        .map(|index| {
            let actor = Arc::clone(&actor);
            tokio::spawn(async move { actor.post_chat(format!("message {index}")) })
        })
        .collect();
    let mut accepted = Vec::new();
    for post in posts {
        accepted.push(post.await.expect("join").expect("post"));
    }

    let store = fixture.store();
    wait_until("every message was answered", || {
        store.unanswered_chat().expect("read").is_empty()
    })
    .await;

    let chat = store.read_chat().expect("read the chat");
    let messages: Vec<&ChatEntry> = chat.iter().filter(|entry| entry.asks_for_reply()).collect();
    assert_eq!(messages.len(), 16, "no message was lost");

    // The file order is the acceptance order: the sequence each post got.
    let sequences: Vec<u64> = chat.iter().map(|entry| entry.sequence).collect();
    assert_eq!(sequences, (1..=32).collect::<Vec<u64>>());
    accepted.sort_by_key(|entry| entry.sequence);
    let written: Vec<ChatEntry> = messages.into_iter().cloned().collect();
    assert_eq!(written, accepted);

    // Every message got exactly one answer.
    for message in &accepted {
        let answers = chat
            .iter()
            .filter(|entry| entry.in_reply_to.as_deref() == Some(message.id.as_str()))
            .count();
        assert_eq!(answers, 1, "answers to {}", message.text);
    }
}

// ---------------------------------------------------------------------------
// Invocations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_chat_invocation_leaves_an_error_entry_and_nothing_else() {
    let fixture = Fixture::new();
    let registry = fixture.registry(Arc::new(UnavailableInvoker));
    let actor = fixture.actor(&registry);

    let message = actor.post_chat("are you there?").expect("post");
    let store = fixture.store();
    wait_until("the message was answered", || {
        store.read_chat().expect("read").len() == 2
    })
    .await;

    let chat = store.read_chat().expect("read the chat");
    assert_eq!(chat[1].kind, ChatEntryKind::Error);
    assert_eq!(chat[1].author, ChatAuthor::Conductor);
    assert_eq!(chat[1].in_reply_to.as_deref(), Some(message.id.as_str()));
    assert_eq!(chat[1].text, CONDUCTOR_UNAVAILABLE);

    wait_until("the invocation was recorded", || {
        fixture
            .events()
            .iter()
            .any(|data| matches!(data, EventData::ConductorFinished { .. }))
    })
    .await;
    let state = store.load_state().expect("load the state");
    assert_eq!(state.pending, None, "the marker is cleared");
    assert!(state.queue.is_empty());
    assert!(
        state.invocation_attempts.is_empty(),
        "chat is not counted (R24)"
    );

    let events = fixture.events();
    let started = events
        .iter()
        .find_map(|data| match data {
            EventData::ConductorStarted {
                invocation_id,
                trigger,
                purpose,
                ..
            } => Some((invocation_id.clone(), *trigger, *purpose)),
            _ => None,
        })
        .expect("ConductorStarted");
    assert_eq!(
        (started.1, started.2),
        (ConductorTrigger::Chat, ConductorPurpose::ReplyToChat)
    );
    let finished = events
        .iter()
        .find_map(|data| match data {
            EventData::ConductorFinished {
                invocation_id,
                outcome,
                error,
            } => Some((invocation_id.clone(), *outcome, error.clone())),
            _ => None,
        })
        .expect("ConductorFinished");
    assert_eq!(finished.0, started.0);
    assert_eq!(finished.1, ConductorOutcome::Failed);
    assert_eq!(finished.2.as_deref(), Some(CONDUCTOR_UNAVAILABLE));
}

#[tokio::test]
async fn an_automatic_invocation_counts_its_attempt_and_writes_its_marker_first() {
    let fixture = Fixture::new();
    let (stub, release) = StubInvoker::gated();
    let stub = Arc::new(stub);
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);

    let started = stub.first_started.notified();
    let waiting = {
        let actor = Arc::clone(&actor);
        tokio::spawn(async move { actor.invoke(output_rescue("episode-1")).await })
    };
    started.await;

    // While it runs, the marker and the count are already on disk.
    let during = fixture.store().load_state().expect("load the state");
    let pending = during.pending.clone().expect("the marker");
    assert_eq!(pending.request.trigger, ConductorTrigger::OutputRescue);
    assert_eq!(during.attempts("episode-1"), 1);
    assert_eq!(
        during.queue.len(),
        1,
        "the request is durable until it ends"
    );
    assert_eq!(stub.calls()[0].attempt, 1);

    let _ = release.send(());
    let outcome = waiting.await.expect("join").expect("invoke");
    assert!(matches!(outcome, InvocationOutcome::Answered(_)));

    let after = fixture.store().load_state().expect("load the state");
    assert_eq!(after.pending, None);
    assert!(after.queue.is_empty());
    assert_eq!(after.attempts("episode-1"), 1);

    // A second invocation of the same episode is its one repeat (R24).
    actor
        .invoke(output_rescue("episode-1"))
        .await
        .expect("invoke again");
    assert_eq!(stub.calls()[1].attempt, 2);
}

#[tokio::test]
async fn an_advisory_invocation_is_interrupted_through_its_own_registry() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubInvoker::hanging());
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);

    let started = stub.first_started.notified();
    let message = actor.post_chat("take your time").expect("post");
    started.await;
    wait_until("the invocation is registered", || {
        registry
            .advisory_interrupts()
            .is_running(&fixture.execution_id)
    })
    .await;

    assert!(registry.interrupt_advisory(&fixture.execution_id));

    let store = fixture.store();
    wait_until("the message was answered", || {
        store.read_chat().expect("read").len() == 2
    })
    .await;
    let answer = &store.read_chat().expect("read")[1];
    assert_eq!(answer.kind, ChatEntryKind::Error);
    assert_eq!(answer.in_reply_to.as_deref(), Some(message.id.as_str()));
    assert!(!registry.interrupt_advisory(&fixture.execution_id));

    wait_until("the interruption was recorded", || {
        fixture.events().iter().any(|data| {
            matches!(
                data,
                EventData::ConductorFinished {
                    outcome: ConductorOutcome::Interrupted,
                    ..
                }
            )
        })
    })
    .await;
}

#[tokio::test]
async fn dropping_the_wait_aborts_an_inline_invocation_and_keeps_its_marker() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubInvoker::hanging());
    let registry = fixture.registry(stub.clone());
    let actor = fixture.actor(&registry);

    // The engine pass waiting for F1 is interrupted: its wait is dropped.
    let wait = tokio::time::timeout(
        Duration::from_millis(200),
        actor.invoke(output_rescue("episode-1")),
    )
    .await;
    assert!(wait.is_err(), "the invocation never ends by itself");
    assert!(
        !registry.interrupt_advisory(&fixture.execution_id),
        "an inline invocation is not advisory"
    );

    wait_until("the interruption was recorded", || {
        fixture.events().iter().any(|data| {
            matches!(
                data,
                EventData::ConductorFinished {
                    outcome: ConductorOutcome::Interrupted,
                    ..
                }
            )
        })
    })
    .await;
    let state = fixture.store().load_state().expect("load the state");
    assert_eq!(
        state.pending.map(|pending| pending.request.trigger),
        Some(ConductorTrigger::OutputRescue),
        "an interrupted automatic invocation keeps its marker for the restart rows"
    );
}

// ---------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unanswered_chat_message_is_served_again_after_a_restart() {
    let fixture = Fixture::new();
    let store = fixture.store();

    // What a daemon killed mid-answer leaves: the message, and the marker of
    // the invocation that was answering it.
    let message = store
        .append_chat(operator_message("still there?"))
        .expect("append");
    let answered = store
        .append_chat(operator_message("an older one"))
        .expect("append");
    store
        .append_chat(
            ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Message, "yes")
                .replying_to(&answered.id),
        )
        .expect("append");
    let state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "cut-short".to_string(),
            request: QueuedRequest::for_chat(&message),
            started_at: Utc::now(),
        }),
        ..ConductorState::default()
    };
    store.save_state(&state).expect("save the state");

    let stub = Arc::new(StubInvoker::default());
    let registry = fixture.registry(stub.clone());
    let restore = registry.restore_at_startup().expect("restore");
    assert_eq!(restore.requeued, vec![(fixture.execution_id.clone(), 1)]);

    wait_until("the message was answered", || {
        store.unanswered_chat().expect("read").is_empty()
    })
    .await;
    assert_eq!(
        stub.purposes(),
        vec![(ConductorTrigger::Chat, Some("still there?".to_string()))]
    );
    wait_until("the marker was cleared", || {
        store.load_state().expect("load").pending.is_none()
    })
    .await;
}

#[tokio::test]
async fn automatic_requests_on_disk_are_left_for_their_own_restart_rows() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let rescue = output_rescue("episode-1");
    let diagnosis = QueuedRequest::new(
        ConductorTrigger::StepFailure,
        ConductorPurpose::DiagnoseFailure,
    )
    .with_episode("episode-2");
    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "cut-short".to_string(),
            request: rescue.clone(),
            started_at: Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.queue = vec![rescue.clone(), diagnosis.clone()];
    state.invocation_attempts.insert("episode-1".to_string(), 1);
    store.save_state(&state).expect("save the state");

    let stub = Arc::new(StubInvoker::default());
    let registry = fixture.registry(stub.clone());
    let restore = registry.restore_at_startup().expect("restore");

    assert!(restore.requeued.is_empty());
    assert_eq!(restore.parked.len(), 1);
    let (execution_id, parked, pending) = &restore.parked[0];
    assert_eq!(execution_id, &fixture.execution_id);
    assert_eq!(parked, &vec![rescue.clone(), diagnosis]);
    assert_eq!(
        pending
            .as_ref()
            .map(|pending| pending.invocation_id.as_str()),
        Some("cut-short")
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(stub.calls().is_empty(), "nothing automatic runs by itself");
    assert_eq!(
        store.load_state().expect("load"),
        state,
        "the disk is untouched"
    );
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[test]
fn a_conductor_decision_without_optional_fields_reads_with_low_confidence() {
    let data: EventData = serde_json::from_value(json!({
        "type": "conductor_decision",
        "decision_id": "decision-1",
        "kind": "choose_output",
    }))
    .expect("the optional fields have defaults");

    match data {
        EventData::ConductorDecision {
            confidence,
            rationale,
            output_label,
            ..
        } => {
            assert_eq!(confidence, Confidence::Low);
            assert_eq!(rationale, None);
            assert_eq!(output_label, None);
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[test]
fn conductor_events_describe_themselves_on_one_line() {
    let event = narsor::domain::Event::new(
        "execution".to_string(),
        EventData::ConductorDecision {
            decision_id: "decision-1".to_string(),
            kind: ConductorDecisionKind::ChooseOutput,
            state_id: Some("work".to_string()),
            output_label: Some("implemented".to_string()),
            episode: None,
            confidence: Confidence::High,
            rationale: Some("the agent said so".to_string()),
        },
        1,
    );
    assert_eq!(
        event.description(),
        "Conductor decided to choose_output 'implemented' (high confidence)"
    );
    assert!(narsor::tui::app::event_line(&event)
        .ends_with("conductor_decision  choose_output → implemented (high)"));
}
