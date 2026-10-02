//! The conductor's chat as the task view shows it (F4, R28, R31) — the
//! half that has no terminal in it.
//!
//! The history is the daemon's (`GET /executions/:id/conductor/chat`); this
//! module only reads it: which proposals are still open, which were carried
//! out or rejected, and which decisions of the conductor's an operator may
//! still undo. Everything that changes anything goes through the API, and
//! the daemon has the last word — a proposal this view still shows as open
//! may be refused because the execution moved on (KTD9).
//!
//! # Where the draft lives
//!
//! [`ChatPanel`] is part of the task view's own state, never rebuilt by a
//! refresh: a refresh only appends the entries after the last one held, so
//! what the operator is typing survives it (KTD16).

use serde_json::Value;

use crate::conductor::{ChatAuthor, ChatEntry, ChatEntryKind};
use crate::domain::{ConductorDecisionKind, Confidence};

/// The `kind` a chat decision entry records for a carried-out proposal.
const EXECUTED: &str = "execute_proposal";

/// The `kind` a chat decision entry records for a rejected proposal.
const REJECTED: &str = "proposal_rejected";

/// The `kind` a chat decision entry records for an undone decision.
const UNDONE: &str = "decision_undone";

/// What became of a proposal, as far as the history says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalState {
    /// Waiting for the operator's confirmation.
    Open,
    /// Carried out.
    Executed,
    /// Rejected by an operator.
    Rejected,
}

impl ProposalState {
    /// The word drawn next to the proposal.
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Executed => "carried out",
            Self::Rejected => "rejected",
        }
    }
}

/// Whether a decision of the conductor's can be undone (KTD13): only its
/// choice of an output (a) completes a step in the operator's place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoState {
    /// It can be undone, by this id.
    Undoable {
        /// The decision's id, as `ConductorDecision` names it.
        decision_id: String,
    },
    /// It was undone already.
    Undone,
}

/// One entry of the history, with what the rest of the history says
/// about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatItem {
    /// The entry itself.
    pub entry: ChatEntry,
    /// For a proposal: whether it is still open.
    pub proposal: Option<ProposalState>,
    /// For a chosen output of the conductor's: whether it can be undone.
    pub undo: Option<UndoState>,
    /// How sure the conductor was, when the entry says.
    pub confidence: Option<Confidence>,
    /// Why, when the entry says.
    pub rationale: Option<String>,
    /// The output label the entry is about, when it names one.
    pub label: Option<String>,
}

impl ChatItem {
    /// The id to confirm or reject this entry by, while it is an open
    /// proposal.
    pub fn open_proposal(&self) -> Option<&str> {
        (self.proposal == Some(ProposalState::Open)).then_some(self.entry.id.as_str())
    }

    /// The decision to undo, while this entry is one that can be undone.
    pub fn undoable_decision(&self) -> Option<&str> {
        match &self.undo {
            Some(UndoState::Undoable { decision_id }) => Some(decision_id),
            _ => None,
        }
    }

    /// The episode the entry is about (KTD9), when it names one.
    pub fn episode(&self) -> Option<&str> {
        payload_str(&self.entry, "episode")
    }
}

/// The history `entries`, each with its state.
pub fn chat_items(entries: &[ChatEntry]) -> Vec<ChatItem> {
    entries
        .iter()
        .map(|entry| {
            let payload = entry.payload.as_ref();
            let proposal =
                (entry.kind == ChatEntryKind::Proposal).then(|| proposal_state(entries, &entry.id));
            let undo = undo_state(entries, entry);
            let label = payload_str(entry, "output_label")
                .or_else(|| {
                    payload
                        .and_then(|payload| payload.get("proposal"))
                        .and_then(|proposal| proposal.get("label"))
                        .and_then(Value::as_str)
                })
                .map(ToString::to_string);
            ChatItem {
                entry: entry.clone(),
                proposal,
                undo,
                confidence: payload
                    .and_then(|payload| payload.get("confidence"))
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
                rationale: payload_str(entry, "rationale").map(ToString::to_string),
                label,
            }
        })
        .collect()
}

/// What the later entries say became of the proposal `proposal_id`.
fn proposal_state(entries: &[ChatEntry], proposal_id: &str) -> ProposalState {
    entries
        .iter()
        .filter(|entry| entry.kind == ChatEntryKind::Decision)
        .filter(|entry| payload_str(entry, "proposal_id") == Some(proposal_id))
        .find_map(|entry| match payload_str(entry, "kind") {
            Some(EXECUTED) => Some(ProposalState::Executed),
            Some(REJECTED) => Some(ProposalState::Rejected),
            _ => None,
        })
        .unwrap_or(ProposalState::Open)
}

/// Whether `entry` is a chosen output of the conductor's, and whether it was
/// undone since.
fn undo_state(entries: &[ChatEntry], entry: &ChatEntry) -> Option<UndoState> {
    if entry.kind != ChatEntryKind::Decision || entry.author != ChatAuthor::Conductor {
        return None;
    }
    if payload_str(entry, "kind") != Some(ConductorDecisionKind::ChooseOutput.name()) {
        return None;
    }
    let decision_id = payload_str(entry, "decision_id")?;
    let undone = entries.iter().any(|later| {
        later.kind == ChatEntryKind::Decision
            && payload_str(later, "kind") == Some(UNDONE)
            && payload_str(later, "decision_id") == Some(decision_id)
    });
    Some(if undone {
        UndoState::Undone
    } else {
        UndoState::Undoable {
            decision_id: decision_id.to_string(),
        }
    })
}

/// A string field of `entry`'s payload.
fn payload_str<'a>(entry: &'a ChatEntry, key: &str) -> Option<&'a str> {
    entry.payload.as_ref()?.get(key)?.as_str()
}

/// The lines one item is drawn as: who wrote it and what it is, its text,
/// and for a proposal or a decision its state, confidence and rationale.
pub fn chat_item_lines(item: &ChatItem) -> Vec<String> {
    let who = match item.entry.author {
        ChatAuthor::Operator => "operator",
        ChatAuthor::Conductor => "conductor",
    };
    let what = match item.entry.kind {
        ChatEntryKind::Message => String::new(),
        ChatEntryKind::Error => " (error)".to_string(),
        ChatEntryKind::Decision => " (decision)".to_string(),
        ChatEntryKind::Proposal => format!(
            " (proposal, {})",
            item.proposal.unwrap_or(ProposalState::Open).label()
        ),
    };
    let mut text_lines = item.entry.text.lines();
    let mut lines = vec![format!(
        "#{} {who}{what}: {}",
        item.entry.sequence,
        text_lines.next().unwrap_or_default()
    )];
    lines.extend(text_lines.map(|line| format!("   {line}")));

    let is_reasoned = matches!(
        item.entry.kind,
        ChatEntryKind::Decision | ChatEntryKind::Proposal
    ) && item.entry.author == ChatAuthor::Conductor;
    if is_reasoned {
        if let Some(confidence) = item.confidence {
            lines.push(format!("   confidence: {}", confidence.name()));
        }
        if let Some(rationale) = &item.rationale {
            lines.push(format!("   why: {rationale}"));
        }
    }
    match (&item.undo, item.open_proposal()) {
        (Some(UndoState::Undoable { .. }), _) => lines.push("   u — undo this decision".into()),
        (Some(UndoState::Undone), _) => lines.push("   (undone)".into()),
        (None, Some(_)) => lines.push("   y — confirm,  n — reject".into()),
        (None, None) => {}
    }
    lines
}

/// Which half of the chat panel takes the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFocus {
    /// The message being written: letters type, `Enter` sends.
    Draft,
    /// The history: `j`/`k` select, `y`/`n` settle a proposal, `u` undoes a
    /// decision.
    History,
}

/// The confirmation an undo goes through (R31): the decision with its
/// confidence and rationale, and the warning that a step after it keeps
/// its side effects.
#[derive(Debug, Clone, PartialEq)]
pub struct UndoConfirm {
    /// The decision to undo.
    pub decision_id: String,
    /// What it decided, as the chat says.
    pub text: String,
    /// The output it chose.
    pub label: Option<String>,
    /// How sure the conductor was.
    pub confidence: Option<Confidence>,
    /// Why it decided so.
    pub rationale: Option<String>,
}

/// The chat panel of the task view.
#[derive(Debug, Clone)]
pub struct ChatPanel {
    /// Whether the panel is showing; closing it keeps everything else.
    pub open: bool,
    /// Whether the history was read at least once.
    pub loaded: bool,
    /// The history, oldest first.
    pub entries: Vec<ChatEntry>,
    /// The message being written.
    pub draft: String,
    /// Which half takes the keys.
    pub focus: ChatFocus,
    /// The highlighted history entry.
    pub selected: usize,
    /// The undo waiting for its confirmation, if one is.
    pub undo: Option<UndoConfirm>,
}

impl Default for ChatPanel {
    fn default() -> Self {
        Self {
            open: false,
            loaded: false,
            entries: Vec::new(),
            draft: String::new(),
            focus: ChatFocus::Draft,
            selected: 0,
            undo: None,
        }
    }
}

impl ChatPanel {
    /// The history with each entry's state.
    pub fn items(&self) -> Vec<ChatItem> {
        chat_items(&self.entries)
    }

    /// The highlighted entry, with its state.
    pub fn selected_item(&self) -> Option<ChatItem> {
        self.items().into_iter().nth(self.selected)
    }

    /// The sequence of the last entry held, from which the next read
    /// continues.
    pub fn last_sequence(&self) -> u64 {
        self.entries.last().map_or(0, |entry| entry.sequence)
    }

    /// Appends the entries newer than the last one held. A selection at the
    /// end of the history follows it.
    pub fn append(&mut self, entries: Vec<ChatEntry>) {
        let following = self.selected + 1 >= self.entries.len();
        let after = self.last_sequence();
        self.entries
            .extend(entries.into_iter().filter(|entry| entry.sequence > after));
        self.loaded = true;
        if following {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    /// Moves the history's selection by `delta`, within its bounds.
    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Asks for the undo of the highlighted entry, when it is a decision
    /// that can be undone; returns whether it is.
    pub fn ask_undo(&mut self) -> bool {
        let Some(item) = self.selected_item() else {
            return false;
        };
        let Some(decision_id) = item.undoable_decision().map(ToString::to_string) else {
            return false;
        };
        self.undo = Some(UndoConfirm {
            decision_id,
            text: item.entry.text.clone(),
            label: item.label.clone(),
            confidence: item.confidence,
            rationale: item.rationale.clone(),
        });
        true
    }
}
