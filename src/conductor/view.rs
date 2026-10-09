//! What an owner reads of the conductor of one execution: its chat, the
//! decisions it took, the proposals waiting for the operator and what it
//! concluded about the failed step.
//!
//! The view is derived from the conductor's own files, so it can be built
//! wherever the task's storage is: by the runner that hosts the conductor,
//! or by the daemon reading the storage of a task whose runner is not
//! running. Nothing here writes.
//!
//! Every text of it comes from a model or a remote runner, so a client
//! that cannot trust the process behind it strips what a terminal must not
//! be handed ([`ConductorView::cleaned`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::conductor::chat::{ProposalContext, ProposalRecord};
use crate::conductor::proposal::{EXECUTED, REJECTED};
use crate::conductor::store::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore, FailureReport,
};
use crate::conductor::undo::{undoable_in, UNDONE};
use crate::domain::{
    without_unsafe_chars_json, ConductorPurpose, ConductorTrigger, Confidence,
};
use crate::persistence::{ExecutionStore, Result};

/// The most chat entries one read returns.
pub const MAX_CHAT_PAGE: usize = 1000;

/// The chat entries a read returns when it names no limit.
pub const DEFAULT_CHAT_PAGE: usize = 200;

/// Something the conductor is working on, or has queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConductorWork {
    /// What it is doing.
    pub purpose: ConductorPurpose,
    /// What asked for it.
    pub trigger: ConductorTrigger,
    /// The episode it is about, when it is about one.
    #[serde(default)]
    pub episode: Option<String>,
    /// Since when: the start of the invocation, or the time it was queued.
    pub since: DateTime<Utc>,
    /// Whether an invocation is under way (as opposed to waiting its turn).
    pub running: bool,
}

/// What became of a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    /// Waiting for the operator.
    Pending,
    /// The operator confirmed it and it was carried out.
    Executed,
    /// The operator turned it down.
    Rejected,
    /// The execution moved on from the moment it is about: it can no
    /// longer be confirmed.
    Stale,
}

/// One proposal of the conductor's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalView {
    /// The proposal: the id of its chat entry.
    pub proposal_id: String,
    /// The line the chat shows for it.
    pub text: String,
    /// When it was made.
    pub at: DateTime<Utc>,
    /// What is proposed, and what moment it is about.
    pub record: ProposalRecord,
    /// The chat message it answers; `None` for a proposal that came with
    /// the diagnosis of a failed step.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// What became of it.
    pub state: ProposalState,
}

/// One decision of the conductor's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionView {
    /// The decision's id.
    pub decision_id: String,
    /// What kind of decision (`choose_output`, `execute_proposal`…).
    pub kind: String,
    /// The line the chat shows for it.
    pub text: String,
    /// When it was taken.
    pub at: DateTime<Utc>,
    /// The state it is about.
    #[serde(default)]
    pub state_id: Option<String>,
    /// How sure the conductor was.
    #[serde(default)]
    pub confidence: Option<Confidence>,
    /// Why.
    #[serde(default)]
    pub rationale: Option<String>,
    /// Whether the operator took it back.
    pub undone: bool,
    /// Whether the operator may take it back now.
    pub undoable: bool,
}

/// The conductor of one execution, as an owner reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConductorView {
    /// Whether a conductor runs for the task.
    pub available: bool,
    /// Why not, when it does not.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// The harness the conductor runs on, when a conductor runs.
    #[serde(default)]
    pub harness: Option<String>,
    /// What the conductor is working on: the invocation under way first,
    /// then what waits its turn.
    #[serde(default)]
    pub working: Vec<ConductorWork>,
    /// Ids of the operator's messages that wait for an answer.
    #[serde(default)]
    pub unanswered_chat: Vec<String>,
    /// What it concluded about the failed step it took up last: the
    /// diagnosis or the raw error. A problem only while the execution is
    /// still failed on it.
    #[serde(default)]
    pub failure: Option<FailureReport>,
    /// The chat after the requested entry, in order.
    #[serde(default)]
    pub chat: Vec<ChatEntry>,
    /// Whether more chat entries follow the last one returned.
    #[serde(default)]
    pub chat_has_more: bool,
    /// Every proposal, oldest first.
    #[serde(default)]
    pub proposals: Vec<ProposalView>,
    /// Every decision of the conductor's, oldest first.
    #[serde(default)]
    pub decisions: Vec<DecisionView>,
}

impl ConductorView {
    /// A view of a conductor that is not there: nothing to show, and why.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            unavailable_reason: Some(reason.into()),
            harness: None,
            working: Vec::new(),
            unanswered_chat: Vec::new(),
            failure: None,
            chat: Vec::new(),
            chat_has_more: false,
            proposals: Vec::new(),
            decisions: Vec::new(),
        }
    }

    /// Reads the conductor of `execution_id` from `executions`: the chat
    /// after sequence `after`, at most `limit` entries (at most
    /// [`MAX_CHAT_PAGE`]). An execution the conductor never worked on has
    /// the empty view.
    ///
    /// The view says `available` when the caller can vouch for a running
    /// conductor; the caller sets that (see [`Self::available_on`]).
    pub fn read(
        executions: &ExecutionStore,
        execution_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Self> {
        let store = ConductorStore::new(executions.paths(), execution_id);
        let state = store.load_state()?;
        let chat = store.read_chat()?;
        let context = ProposalContext::of(executions, execution_id);

        let proposals = proposals_of(&chat, context.episode.as_deref());
        let decisions = decisions_of(&chat, execution_id);
        let unanswered_chat = chat
            .iter()
            .filter(|entry| entry.asks_for_reply())
            .filter(|entry| {
                !chat
                    .iter()
                    .any(|other| other.in_reply_to.as_deref() == Some(entry.id.as_str()))
            })
            .map(|entry| entry.id.clone())
            .collect();

        let limit = limit.clamp(1, MAX_CHAT_PAGE);
        let mut page: Vec<ChatEntry> = chat
            .into_iter()
            .filter(|entry| entry.sequence > after)
            .take(limit + 1)
            .collect();
        let chat_has_more = page.len() > limit;
        page.truncate(limit);

        Ok(Self {
            available: false,
            unavailable_reason: None,
            harness: None,
            working: work_of(&state),
            unanswered_chat,
            failure: state.failure,
            chat: page,
            chat_has_more,
            proposals,
            decisions,
        })
    }

    /// The same view, marked as served by a running conductor on `harness`.
    pub fn available_on(mut self, harness: impl Into<String>) -> Self {
        self.available = true;
        self.unavailable_reason = None;
        self.harness = Some(harness.into());
        self
    }

    /// The same view, marked as having no running conductor, for `reason`.
    pub fn without_conductor(mut self, reason: impl Into<String>) -> Self {
        self.available = false;
        self.unavailable_reason = Some(reason.into());
        self
    }

    /// The view with every text stripped of what a terminal must not be
    /// handed (control characters, bidirectional and zero-width marks);
    /// line breaks stay. For a client that does not trust the process the
    /// view came from.
    pub fn cleaned(self) -> Self {
        let Ok(mut value) = serde_json::to_value(&self) else {
            return self;
        };
        without_unsafe_chars_json(&mut value);
        serde_json::from_value(value).unwrap_or(self)
    }
}

/// The proposals of the conductor's chat that wait for the operator now:
/// unsettled, and about the moment `execution` stands at. A proposal that
/// came with the diagnosis of a failed step belongs to that failure and is
/// not among them.
pub fn pending_chat_proposals(
    executions: &ExecutionStore,
    execution_id: &str,
) -> Result<Vec<ProposalView>> {
    let chat = ConductorStore::new(executions.paths(), execution_id).read_chat()?;
    let context = ProposalContext::of(executions, execution_id);
    Ok(proposals_of(&chat, context.episode.as_deref())
        .into_iter()
        .filter(|proposal| proposal.state == ProposalState::Pending)
        .filter(|proposal| proposal.in_reply_to.is_some())
        .collect())
}

fn work_of(state: &ConductorState) -> Vec<ConductorWork> {
    let running = state.pending.iter().map(|pending| ConductorWork {
        purpose: pending.request.purpose,
        trigger: pending.request.trigger,
        episode: pending.request.episode.clone(),
        since: pending.started_at,
        running: true,
    });
    let waiting = state.queue.iter().map(|queued| ConductorWork {
        purpose: queued.purpose,
        trigger: queued.trigger,
        episode: queued.episode.clone(),
        since: queued.queued_at,
        running: false,
    });
    running.chain(waiting).collect()
}

/// The payload string `key` of `entry`.
fn payload_str<'a>(entry: &'a ChatEntry, key: &str) -> Option<&'a str> {
    entry.payload.as_ref()?.get(key)?.as_str()
}

/// How the later decision entries settled the proposal `proposal_id`.
fn settlement(chat: &[ChatEntry], proposal_id: &str) -> Option<ProposalState> {
    chat.iter()
        .filter(|entry| entry.kind == ChatEntryKind::Decision)
        .filter(|entry| payload_str(entry, "proposal_id") == Some(proposal_id))
        .find_map(|entry| match payload_str(entry, "kind") {
            Some(EXECUTED) => Some(ProposalState::Executed),
            Some(REJECTED) => Some(ProposalState::Rejected),
            _ => None,
        })
}

fn proposals_of(chat: &[ChatEntry], current_episode: Option<&str>) -> Vec<ProposalView> {
    chat.iter()
        .filter(|entry| entry.kind == ChatEntryKind::Proposal)
        .filter_map(|entry| {
            let record: ProposalRecord = serde_json::from_value(entry.payload.clone()?).ok()?;
            let state = settlement(chat, &entry.id).unwrap_or_else(|| {
                if record.episode.as_deref() == current_episode && current_episode.is_some() {
                    ProposalState::Pending
                } else {
                    ProposalState::Stale
                }
            });
            Some(ProposalView {
                proposal_id: entry.id.clone(),
                text: entry.text.clone(),
                at: entry.at,
                record,
                in_reply_to: entry.in_reply_to.clone(),
                state,
            })
        })
        .collect()
}

fn decisions_of(chat: &[ChatEntry], execution_id: &str) -> Vec<DecisionView> {
    chat.iter()
        .filter(|entry| entry.kind == ChatEntryKind::Decision)
        .filter(|entry| entry.author == ChatAuthor::Conductor)
        .filter_map(|entry| {
            let decision_id = payload_str(entry, "decision_id")?.to_string();
            let undone = chat.iter().any(|other| {
                other.kind == ChatEntryKind::Decision
                    && payload_str(other, "kind") == Some(UNDONE)
                    && payload_str(other, "decision_id") == Some(decision_id.as_str())
            });
            let payload = entry.payload.as_ref()?;
            Some(DecisionView {
                undoable: undoable_in(chat, execution_id, &decision_id).is_ok(),
                kind: payload_str(entry, "kind")?.to_string(),
                text: entry.text.clone(),
                at: entry.at,
                state_id: payload_str(entry, "state_id").map(ToString::to_string),
                confidence: payload
                    .get("confidence")
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
                rationale: payload_str(entry, "rationale").map(ToString::to_string),
                undone,
                decision_id,
            })
        })
        .collect()
}
