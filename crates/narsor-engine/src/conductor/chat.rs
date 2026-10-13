//! The conductor's advisory work for the operator: the chat (F4) and the
//! summaries of `human` gates (R30), U12.
//!
//! Both run beside the execution, without its claim (KTD10), and neither
//! ever changes it: a chat answer is a message and, at most, proposals the
//! operator confirms before anything happens (R28, see [`super::proposal`]);
//! a gate's summary is an annotation of the pending input, never its answer
//! — a `human` gate always waits for the operator (R30).
//!
//! # Proposals carry where the execution stood
//!
//! Every proposal entry records the key of the moment it was made about
//! ([`crate::engine::execution_episode`], KTD9), taken when the invocation
//! began — what the conductor saw. Confirming a proposal once the execution
//! has moved on from there is refused.
//!
//! # Failures stay in the chat (KTD12)
//!
//! A chat or annotation invocation that fails leaves an error entry in the
//! chat and nothing else: it is never a problem, never retried by itself,
//! and never touches the execution.
//!
//! # A summary that comes too late
//!
//! An annotation is kept under the key of the input it summarizes, and only
//! while that input is still pending when the summary arrives: one for an
//! input the operator already answered is dropped. A summary still queued
//! when its input closes is never run, and one under way is interrupted
//! (see [`super::actor`]). Either way it never
//! reopens anything — the pending input is the execution's own record, which
//! nothing here writes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::conductor::actor::{ConductorAnswer, InvocationOutcome};
use crate::conductor::response::{ConductorResponse, Decision, ProposedAction};
use crate::conductor::store::{
    Annotation, ChatAuthor, ChatEntry, ChatEntryKind, ConductorStore, QueuedRequest,
};
use crate::conductor::ConductorRegistry;
use crate::domain::{ConductorPurpose, ConductorTrigger, Confidence, ExecutionStatus};
use crate::engine::{execution_episode, PendingHumanInput};
use crate::persistence::{ExecutionStore, Result};

/// Where the execution stood when an invocation began: what the proposals
/// of its answer are about (KTD9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposalContext {
    /// The key of the moment ([`crate::engine::execution_episode`]).
    pub episode: Option<String>,
    /// The state the execution was at.
    pub state_id: Option<String>,
}

impl ProposalContext {
    /// Where `execution_id` stands now; empty when it cannot be read.
    pub fn of(store: &ExecutionStore, execution_id: &str) -> Self {
        match store.load_execution(execution_id) {
            Ok(execution) => Self {
                episode: execution_episode(store, &execution),
                state_id: execution.current_state.clone(),
            },
            Err(_) => Self::default(),
        }
    }
}

/// The payload of a proposal entry of the chat (R28).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalRecord {
    /// The moment the proposal is about (KTD9).
    #[serde(default)]
    pub episode: Option<String>,
    /// The state the execution was at.
    #[serde(default)]
    pub state_id: Option<String>,
    /// What is proposed.
    pub proposal: ProposedAction,
    /// How sure the conductor was of the answer that proposed it.
    #[serde(default)]
    pub confidence: Option<Confidence>,
    /// Why, as that answer said.
    #[serde(default)]
    pub rationale: Option<String>,
}

impl ConductorRegistry {
    /// Writes the operator's chat `text` for `execution_id` and queues the
    /// conductor's answer (F4). Returns the entry as written.
    pub fn post_chat(&self, execution_id: &str, text: &str) -> Result<ChatEntry> {
        self.actor(execution_id)?.post_chat(text)
    }

    /// The engine's trigger (R30): the execution just stopped at a `human`
    /// gate on `pending`. Queues its summary, which runs beside the
    /// execution and never answers the gate. Without a runtime to run it
    /// on, the gate simply goes without one.
    pub fn gate_suspended(&self, execution_id: &str, pending: &PendingHumanInput) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let request = QueuedRequest::new(
            ConductorTrigger::Annotation,
            ConductorPurpose::SummarizeGate,
        )
        .with_episode(pending.episode_key())
        .with_payload(json!({
            "state_id": pending.state_id,
            "instruction": pending.instruction(),
            "fields": pending.fields(),
            "outputs": pending.outputs,
            "question": pending.request.question,
            "answer_tail": pending.request.answer_tail,
        }));
        if let Err(error) = self
            .actor(execution_id)
            .and_then(|actor| actor.submit(request))
        {
            tracing::warn!(
                execution_id,
                %error,
                "conductor: could not queue the summary of a human gate"
            );
        }
    }
}

/// The entries that answer the operator's chat `message` with `outcome`:
/// the conductor's message — or an error entry saying why there is none
/// (KTD12) — then one entry per action it proposes, each carrying
/// `context`.
pub(super) fn chat_answer(
    message: &ChatEntry,
    outcome: &InvocationOutcome,
    context: &ProposalContext,
) -> Vec<ChatEntry> {
    let reply = match outcome {
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
    let mut entries = vec![reply.replying_to(&message.id)];

    let InvocationOutcome::Answered(answer) = outcome else {
        return entries;
    };
    let Ok(response) =
        ConductorResponse::from_value(answer.response.clone(), ConductorPurpose::ReplyToChat)
    else {
        return entries;
    };
    for proposal in response.proposals {
        let text = proposal
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| proposal.describe());
        let record = ProposalRecord {
            episode: context.episode.clone(),
            state_id: context.state_id.clone(),
            proposal,
            confidence: Some(response.confidence),
            rationale: response.rationale.clone(),
        };
        let mut entry = ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Proposal, text)
            .replying_to(&message.id);
        entry.payload = Some(serde_json::to_value(&record).unwrap_or(Value::Null));
        entries.push(entry);
    }
    entries
}

/// What a gate's summary leaves (R30): the annotation of the input it is
/// about while that input is still pending, nothing when it is not, and an
/// error entry in the chat when the conductor could not make one (KTD12).
pub(super) fn conclude_annotation(
    executions: &ExecutionStore,
    store: &ConductorStore,
    request: &QueuedRequest,
    outcome: &InvocationOutcome,
) -> Result<()> {
    let key = request.episode.clone().unwrap_or_default();
    let answer = match outcome {
        InvocationOutcome::Answered(answer) => answer,
        InvocationOutcome::Failed { error } => {
            store.append_chat(ChatEntry::new(
                ChatAuthor::Conductor,
                ChatEntryKind::Error,
                format!("Could not summarize the human gate: {error}"),
            ))?;
            return Ok(());
        }
        InvocationOutcome::Interrupted => return Ok(()),
    };
    let summary = match ConductorResponse::from_value(
        answer.response.clone(),
        ConductorPurpose::SummarizeGate,
    ) {
        Ok(ConductorResponse {
            decision:
                Decision::Annotate {
                    summary,
                    recommendation,
                },
            ..
        }) => (summary, recommendation),
        Ok(_) | Err(_) => {
            store.append_chat(ChatEntry::new(
                ChatAuthor::Conductor,
                ChatEntryKind::Error,
                "Could not summarize the human gate: the conductor's answer is not a summary",
            ))?;
            return Ok(());
        }
    };

    if !still_pending(executions, store.execution_id(), &key) {
        tracing::debug!(
            execution_id = store.execution_id(),
            key,
            "conductor: dropped the summary of a gate that is no longer pending"
        );
        return Ok(());
    }
    store.save_annotation(&Annotation {
        key,
        summary: summary.0,
        recommendation: summary.1,
        created_at: chrono::Utc::now(),
    })
}

/// Whether `execution_id` still waits on the input `key`.
pub(super) fn still_pending(executions: &ExecutionStore, execution_id: &str, key: &str) -> bool {
    let Ok(execution) = executions.load_execution(execution_id) else {
        return false;
    };
    if execution.status != ExecutionStatus::InputNeeded {
        return false;
    }
    matches!(
        executions.load_pending_input::<PendingHumanInput>(execution_id),
        Ok(Some(pending)) if pending.episode_key() == key
    )
}

/// The annotation of the input `pending` of `execution_id`, if the
/// conductor made one (R30).
pub fn annotation_of(
    executions: &ExecutionStore,
    execution_id: &str,
    pending: &PendingHumanInput,
) -> Option<Annotation> {
    let store = ConductorStore::new(executions.paths(), execution_id);
    if !store.exists() {
        return None;
    }
    store.load_annotation(pending.episode_key()).ok().flatten()
}
