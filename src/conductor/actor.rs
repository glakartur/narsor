//! One actor per execution: the conductor's only writer and its queue
//! (KTD2, KTD11).
//!
//! # What the actor serializes
//!
//! Every write to the conductor's store of one execution — the state file,
//! a chat line — happens under the actor's lock, so two operators posting at
//! once cannot interleave their lines or lose each other's update of
//! `state.json`. Invocations are serialized too, differently: one worker
//! task drains the queue, so at most one invocation runs at a time for an
//! execution, whoever asked for it.
//!
//! # The queue
//!
//! Two priorities. Automatic triggers (F1, F3) go before anything an
//! operator asked for (chat, gate annotations), in the order each class was
//! accepted. A request is durable from the moment it is accepted: chat
//! requests *are* the unanswered operator messages of `chat.jsonl`, and every
//! other request sits in `state.json`'s `queue` until its invocation ends.
//! That is what lets [`ConductorActor::open`] rebuild the queue after a
//! restart.
//!
//! # Interrupting an invocation
//!
//! An invocation runs in a task of its own, so it can be stopped without
//! stopping the worker:
//!
//! * an advisory one (chat, annotation, F3 — KTD10) through the registry of
//!   advisory interrupts, which is separate from the API's `active_tasks`
//!   and never holds a handle to an engine pass;
//! * one somebody waits for ([`ConductorActor::invoke`], F1 inline under
//!   the pass's claim) by dropping the wait: interrupting the pass drops it,
//!   and the invocation is aborted with it.
//!
//! An interrupted automatic invocation keeps its marker in `state.json`: it
//! did not finish, and what that means for the execution is the startup
//! hook's to decide (U10, U11). An interrupted advisory one simply ends.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::Utc;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::conductor::store::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore, PendingInvocation,
    QueuedRequest,
};
use crate::domain::{ConductorOutcome, EventData, ExecutionId};
use crate::engine::Engine;
use crate::persistence::Result;

/// One invocation of the conductor, as the invoker receives it.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The invocation's id, as `ConductorStarted`/`Finished` name it.
    pub invocation_id: String,
    /// The execution it is about.
    pub execution_id: ExecutionId,
    /// What was asked for.
    pub request: QueuedRequest,
    /// The operator message it answers, for a chat request.
    pub chat_message: Option<ChatEntry>,
    /// How many invocations were counted for the request's episode,
    /// including this one — 1 on a first try, 2 on the one repeat R24
    /// allows. Always 1 for a request without an episode.
    pub attempt: u32,
}

/// What the conductor answered (KTD6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConductorAnswer {
    /// Its message for the chat, if it has one.
    pub message: Option<String>,
    /// Its structured response, for whoever asked to act on (U9–U12).
    pub response: Value,
}

/// How one invocation ended.
#[derive(Debug, Clone, PartialEq)]
pub enum InvocationOutcome {
    /// The conductor answered.
    Answered(ConductorAnswer),
    /// It could not answer (R24): a process error, a timeout, a response
    /// that does not parse or is not allowed for the purpose.
    Failed {
        /// Why.
        error: String,
    },
    /// It was stopped before it answered.
    Interrupted,
}

impl InvocationOutcome {
    /// The outcome as `ConductorFinished` records it.
    pub fn event_outcome(&self) -> ConductorOutcome {
        match self {
            Self::Answered(_) => ConductorOutcome::Completed,
            Self::Failed { .. } => ConductorOutcome::Failed,
            Self::Interrupted => ConductorOutcome::Interrupted,
        }
    }

    fn error(&self) -> Option<String> {
        match self {
            Self::Failed { error } => Some(error.clone()),
            _ => None,
        }
    }
}

/// The seam through which the conductor is actually run (U9).
///
/// Everything around an invocation — queueing, the marker, the counters,
/// the events, the chat entries — is the actor's; an invoker only turns one
/// [`Invocation`] into an outcome. It is never told to retry: R24's one
/// repeat is the invoker's own business, and [`Invocation::attempt`] says
/// how many tries a restart already counted.
///
/// Hand-rolled async for `dyn` dispatch, the same way
/// [`crate::engine::ActionExecutor`] does it.
pub trait ConductorInvoker: Send + Sync {
    /// Runs the conductor once for `invocation`.
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>>;
}

/// The invoker of a daemon that has no conductor to run yet: every
/// invocation fails, saying so.
///
/// Stands in until the harness-backed invoker exists (U9), so the queue,
/// the chat and the restart rows can already be driven end to end — a chat
/// message gets an error entry (KTD12) rather than waiting forever.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableInvoker;

/// The error an [`UnavailableInvoker`] fails every invocation with.
pub const CONDUCTOR_UNAVAILABLE: &str = "the conductor cannot be invoked in this version";

impl ConductorInvoker for UnavailableInvoker {
    fn invoke<'a>(
        &'a self,
        _invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        Box::pin(async {
            InvocationOutcome::Failed {
                error: CONDUCTOR_UNAVAILABLE.to_string(),
            }
        })
    }
}

/// Handles to the advisory invocations under way, per execution (KTD10).
///
/// Deliberately a registry of its own rather than entries in the API's
/// `active_tasks`: those hold the engine pass of an execution, and an
/// advisory invocation runs while no pass does — or while one does, and
/// must then not replace its handle.
#[derive(Debug, Default)]
pub struct AdvisoryInterrupts {
    running: Mutex<HashMap<ExecutionId, (String, AbortHandle)>>,
}

impl AdvisoryInterrupts {
    /// Stops the advisory invocation running for `execution_id`, if one is.
    /// Returns whether there was one.
    pub fn interrupt(&self, execution_id: &str) -> bool {
        match self.lock().remove(execution_id) {
            Some((_, handle)) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    /// Whether an advisory invocation runs for `execution_id`.
    pub fn is_running(&self, execution_id: &str) -> bool {
        self.lock().contains_key(execution_id)
    }

    fn register(&self, execution_id: &str, invocation_id: &str, handle: AbortHandle) {
        self.lock().insert(
            execution_id.to_string(),
            (invocation_id.to_string(), handle),
        );
    }

    fn unregister(&self, execution_id: &str, invocation_id: &str) {
        let mut running = self.lock();
        if running
            .get(execution_id)
            .is_some_and(|(current, _)| current == invocation_id)
        {
            running.remove(execution_id);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ExecutionId, (String, AbortHandle)>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The two-priority queue of one execution (KTD11).
///
/// Automatic triggers first, each class in the order it was accepted.
#[derive(Debug, Default)]
pub struct InvocationQueue<T> {
    automatic: VecDeque<(QueuedRequest, T)>,
    operator: VecDeque<(QueuedRequest, T)>,
}

impl<T> InvocationQueue<T> {
    /// An empty queue.
    pub fn new() -> Self {
        Self {
            automatic: VecDeque::new(),
            operator: VecDeque::new(),
        }
    }

    /// Adds `request` behind every request of its class.
    pub fn push(&mut self, request: QueuedRequest, item: T) {
        if request.trigger.is_automatic() {
            self.automatic.push_back((request, item));
        } else {
            self.operator.push_back((request, item));
        }
    }

    /// Takes the request to serve next.
    pub fn pop(&mut self) -> Option<(QueuedRequest, T)> {
        self.automatic
            .pop_front()
            .or_else(|| self.operator.pop_front())
    }

    /// Whether a request with this id waits.
    pub fn contains(&self, request_id: &str) -> bool {
        self.automatic
            .iter()
            .chain(self.operator.iter())
            .any(|(request, _)| request.request_id == request_id)
    }

    /// How many requests wait.
    pub fn len(&self) -> usize {
        self.automatic.len() + self.operator.len()
    }

    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What travels with a queued request in memory only.
#[derive(Debug, Default)]
struct Attachment {
    /// The operator message a chat request answers.
    chat: Option<ChatEntry>,
    /// Whoever waits for the outcome (F1 inline).
    waiter: Option<oneshot::Sender<InvocationOutcome>>,
}

struct Inner {
    state: ConductorState,
    queue: InvocationQueue<Attachment>,
    worker_running: bool,
}

/// The conductor of one execution.
pub struct ConductorActor {
    execution_id: ExecutionId,
    store: ConductorStore,
    engine: Arc<Engine>,
    invoker: Arc<dyn ConductorInvoker>,
    advisory: Arc<AdvisoryInterrupts>,
    inner: Mutex<Inner>,
}

impl ConductorActor {
    /// Opens the actor of `execution_id` over its store, rebuilding its
    /// queue from disk; the worker starts with the first request, or with
    /// [`ConductorActor::start`].
    ///
    /// Rebuilding follows the restart matrix for the requests an operator
    /// asked for: an advisory invocation a restart cut short loses its
    /// marker and is simply served again, and every unanswered chat message
    /// is queued in the order it was written. Automatic requests (F1, F3)
    /// and their markers are left exactly as they are on disk — whether
    /// they run again is not the conductor's to decide alone (U10, U11); see
    /// [`ConductorActor::parked_automatic`].
    pub fn open(
        engine: Arc<Engine>,
        invoker: Arc<dyn ConductorInvoker>,
        advisory: Arc<AdvisoryInterrupts>,
        execution_id: &str,
    ) -> Result<Self> {
        let store = ConductorStore::new(engine.store().paths(), execution_id);
        let mut state = store.load_state()?;

        if state
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.request.trigger.is_automatic())
        {
            state.pending = None;
            store.save_state(&state)?;
        }

        let mut queue = InvocationQueue::new();
        for request in &state.queue {
            if !request.trigger.is_automatic() {
                queue.push(request.clone(), Attachment::default());
            }
        }
        for message in store.unanswered_chat()? {
            if queue.contains(&message.id) {
                continue;
            }
            queue.push(
                QueuedRequest::for_chat(&message),
                Attachment {
                    chat: Some(message),
                    waiter: None,
                },
            );
        }

        Ok(Self {
            execution_id: execution_id.to_string(),
            store,
            engine,
            invoker,
            advisory,
            inner: Mutex::new(Inner {
                state,
                queue,
                worker_running: false,
            }),
        })
    }

    /// The execution this actor serves.
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// The store it writes.
    pub fn store(&self) -> &ConductorStore {
        &self.store
    }

    /// The conductor's state as the actor holds it.
    pub fn state(&self) -> ConductorState {
        self.lock().state.clone()
    }

    /// How many requests wait, not counting the one under way.
    pub fn queued(&self) -> usize {
        self.lock().queue.len()
    }

    /// The automatic requests (F1, F3) found on disk that this actor does
    /// not serve by itself, with the marker of one that was under way.
    pub fn parked_automatic(&self) -> (Vec<QueuedRequest>, Option<PendingInvocation>) {
        let inner = self.lock();
        let requests = inner
            .state
            .queue
            .iter()
            .filter(|request| request.trigger.is_automatic())
            .filter(|request| !inner.queue.contains(&request.request_id))
            .cloned()
            .collect();
        let pending = inner
            .state
            .pending
            .clone()
            .filter(|pending| pending.request.trigger.is_automatic());
        (requests, pending)
    }

    /// Starts serving whatever the queue holds, if it holds anything and
    /// the worker is not already running.
    pub fn start(self: &Arc<Self>) {
        let mut inner = self.lock();
        self.ensure_worker(&mut inner);
    }

    /// Writes the operator's chat `text` and queues its answer (F4).
    ///
    /// Returns the entry as written, with its sequence: the order entries
    /// are accepted in is the order they are written in.
    pub fn post_chat(self: &Arc<Self>, text: impl Into<String>) -> Result<ChatEntry> {
        let mut inner = self.lock();
        let entry = self.store.append_chat(ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            text,
        ))?;
        inner.queue.push(
            QueuedRequest::for_chat(&entry),
            Attachment {
                chat: Some(entry.clone()),
                waiter: None,
            },
        );
        self.ensure_worker(&mut inner);
        Ok(entry)
    }

    /// Writes a chat entry of the conductor's (a decision, a proposal) —
    /// one that asks for no answer.
    pub fn append_chat(&self, entry: ChatEntry) -> Result<ChatEntry> {
        let _inner = self.lock();
        self.store.append_chat(entry)
    }

    /// Queues `request` durably, without waiting for its outcome.
    pub fn submit(self: &Arc<Self>, request: QueuedRequest) -> Result<()> {
        self.enqueue(request, None)
    }

    /// Queues `request` and waits for its outcome — for the F1 inline
    /// invocation, which runs under the engine pass's claim (KTD10).
    ///
    /// Dropping the returned future (interrupting the pass) aborts the
    /// invocation. The request is served in its turn: an invocation already
    /// under way finishes first.
    pub async fn invoke(self: &Arc<Self>, request: QueuedRequest) -> Result<InvocationOutcome> {
        let (sender, receiver) = oneshot::channel();
        self.enqueue(request, Some(sender))?;
        Ok(receiver.await.unwrap_or(InvocationOutcome::Interrupted))
    }

    /// Changes the conductor's state under the actor's lock and saves it
    /// (the counters, the used retries, the saved decisions of U9–U12).
    pub fn update_state<R>(&self, change: impl FnOnce(&mut ConductorState) -> R) -> Result<R> {
        let mut inner = self.lock();
        let mut next = inner.state.clone();
        let result = change(&mut next);
        self.store.save_state(&next)?;
        inner.state = next;
        Ok(result)
    }

    fn enqueue(
        self: &Arc<Self>,
        request: QueuedRequest,
        waiter: Option<oneshot::Sender<InvocationOutcome>>,
    ) -> Result<()> {
        let mut inner = self.lock();
        let mut next = inner.state.clone();
        next.queue.push(request.clone());
        self.store.save_state(&next)?;
        inner.state = next;
        inner.queue.push(request, Attachment { chat: None, waiter });
        self.ensure_worker(&mut inner);
        Ok(())
    }

    fn ensure_worker(self: &Arc<Self>, inner: &mut Inner) {
        if inner.worker_running || inner.queue.is_empty() {
            return;
        }
        inner.worker_running = true;
        let actor = Arc::clone(self);
        tokio::spawn(async move { actor.drain().await });
    }

    /// Serves the queue until it is empty.
    async fn drain(self: Arc<Self>) {
        loop {
            let next = {
                let mut inner = self.lock();
                match inner.queue.pop() {
                    Some(next) => next,
                    None => {
                        inner.worker_running = false;
                        return;
                    }
                }
            };
            let (request, attachment) = next;
            if let Err(error) = self.serve(request, attachment).await {
                tracing::warn!(
                    execution_id = %self.execution_id,
                    error = %error,
                    "conductor: could not record an invocation"
                );
            }
        }
    }

    async fn serve(&self, request: QueuedRequest, attachment: Attachment) -> Result<()> {
        let Attachment {
            chat,
            waiter: mut waiter_slot,
        } = attachment;

        let invocation = self.begin(request, chat)?;
        self.record(EventData::ConductorStarted {
            invocation_id: invocation.invocation_id.clone(),
            trigger: invocation.request.trigger,
            purpose: invocation.request.purpose,
            episode: invocation.request.episode.clone(),
        });

        let outcome = self.run(&invocation, waiter_slot.as_mut()).await;

        self.finish(&invocation, &outcome)?;
        self.record(EventData::ConductorFinished {
            invocation_id: invocation.invocation_id.clone(),
            outcome: outcome.event_outcome(),
            error: outcome.error(),
        });

        if let Some(waiter) = waiter_slot {
            let _ = waiter.send(outcome);
        }
        Ok(())
    }

    /// Writes the marker — and, for an automatic trigger, counts the
    /// attempt — before anything runs, so a restart knows what was under
    /// way (R24: the counter moves *before* the invocation).
    fn begin(&self, request: QueuedRequest, chat: Option<ChatEntry>) -> Result<Invocation> {
        let mut inner = self.lock();
        let mut next = inner.state.clone();

        let attempt = match (&request.episode, request.trigger.is_automatic()) {
            (Some(episode), true) => {
                let count = next.invocation_attempts.entry(episode.clone()).or_insert(0);
                *count += 1;
                *count
            }
            _ => 1,
        };
        let invocation_id = Uuid::new_v4().to_string();
        next.pending = Some(PendingInvocation {
            invocation_id: invocation_id.clone(),
            request: request.clone(),
            started_at: Utc::now(),
        });
        self.store.save_state(&next)?;
        inner.state = next;

        Ok(Invocation {
            invocation_id,
            execution_id: self.execution_id.clone(),
            request,
            chat_message: chat,
            attempt,
        })
    }

    /// Runs the invocation in a task of its own, so it can be aborted.
    async fn run(
        &self,
        invocation: &Invocation,
        waiter: Option<&mut oneshot::Sender<InvocationOutcome>>,
    ) -> InvocationOutcome {
        let invoker = Arc::clone(&self.invoker);
        let owned = invocation.clone();
        let task = tokio::spawn(async move { invoker.invoke(&owned).await });

        let advisory = !invocation.request.trigger.is_automatic();
        if advisory {
            self.advisory.register(
                &self.execution_id,
                &invocation.invocation_id,
                task.abort_handle(),
            );
        }
        let abort = task.abort_handle();

        let joined = match waiter {
            Some(waiter) => {
                tokio::select! {
                    joined = task => Some(joined),
                    () = waiter.closed() => {
                        // Nobody waits any more: the pass that asked was
                        // interrupted, and the invocation goes with it.
                        abort.abort();
                        None
                    }
                }
            }
            None => Some(task.await),
        };

        if advisory {
            self.advisory
                .unregister(&self.execution_id, &invocation.invocation_id);
        }

        match joined {
            Some(Ok(outcome)) => outcome,
            Some(Err(error)) if !error.is_cancelled() => InvocationOutcome::Failed {
                error: format!("the conductor's invocation panicked: {error}"),
            },
            Some(Err(_)) | None => InvocationOutcome::Interrupted,
        }
    }

    /// Records what the invocation left behind: the chat's answer, the
    /// request taken off the queue, the marker cleared.
    fn finish(&self, invocation: &Invocation, outcome: &InvocationOutcome) -> Result<()> {
        let mut inner = self.lock();

        if let Some(message) = &invocation.chat_message {
            self.store.append_chat(chat_answer(message, outcome))?;
        }

        let mut next = inner.state.clone();
        next.queue
            .retain(|queued| queued.request_id != invocation.request.request_id);
        let keeps_marker =
            invocation.request.trigger.is_automatic() && *outcome == InvocationOutcome::Interrupted;
        if !keeps_marker {
            next.pending = None;
        }
        self.store.save_state(&next)?;
        inner.state = next;
        Ok(())
    }

    fn record(&self, data: EventData) {
        if let Err(error) = self.engine.record_event(&self.execution_id, data) {
            tracing::warn!(
                execution_id = %self.execution_id,
                error = %error,
                "conductor: could not record an event"
            );
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The chat entry that answers `message` with `outcome`: the conductor's
/// message, or an error entry saying why there is none (KTD12).
fn chat_answer(message: &ChatEntry, outcome: &InvocationOutcome) -> ChatEntry {
    let entry = match outcome {
        InvocationOutcome::Answered(ConductorAnswer {
            message: Some(text),
            ..
        }) => ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Message, text.clone()),
        InvocationOutcome::Answered(_) => ChatEntry::new(
            ChatAuthor::Conductor,
            ChatEntryKind::Error,
            "the conductor gave no answer",
        ),
        InvocationOutcome::Failed { error } => {
            ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Error, error.clone())
        }
        InvocationOutcome::Interrupted => ChatEntry::new(
            ChatAuthor::Conductor,
            ChatEntryKind::Error,
            "the conductor was interrupted before it answered",
        ),
    };
    entry.replying_to(&message.id)
}
