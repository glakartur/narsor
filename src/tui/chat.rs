//! The conductor's chat as the *Dyrygent* tab shows it (F4, R28, R31) — the
//! half that has no terminal in it.
//!
//! The history is the server's; this module only reads it: which proposals
//! are still open, which were carried out or rejected, and which decisions of
//! the conductor's an operator may still undo. Everything that changes
//! anything goes through the API, and the server has the last word — a
//! proposal this view still shows as open may be refused because the task
//! moved on (KTD9).

use serde_json::Value;

use crate::conductor::{
    ChatAuthor, ChatEntry, ChatEntryKind, DecisionView, ProposalState as ViewProposalState,
    ProposalView,
};
use crate::domain::{ConductorDecisionKind, Confidence};
use crate::tui::keymap::{key_of, Action, Context};
use crate::tui::scroll::sanitize;
use crate::tui::transcript::{
    utterance_header, TranscriptEntry, TranscriptEntryId, TranscriptPresentation,
};

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

/// The history `entries` with each entry's state, settled by what the
/// conductor's view says of its decisions.
pub fn tab_items(entries: &[ChatEntry], decisions: &[DecisionView]) -> Vec<ChatItem> {
    let mut items = chat_items(entries);
    for item in &mut items {
        if item.entry.kind != ChatEntryKind::Decision || item.entry.author != ChatAuthor::Conductor
        {
            continue;
        }
        let Some(decision_id) = payload_str(&item.entry, "decision_id") else {
            continue;
        };
        if let Some(decision) = decisions
            .iter()
            .find(|decision| decision.decision_id == decision_id)
        {
            item.undo = if decision.undone {
                Some(UndoState::Undone)
            } else if decision.undoable {
                Some(UndoState::Undoable {
                    decision_id: decision.decision_id.clone(),
                })
            } else {
                None
            };
        }
    }
    items
}

/// What became of a proposal, in words: from the conductor's view when it
/// knows the proposal, from the history otherwise.
fn proposal_word(item: &ChatItem, proposal: Option<&ProposalView>) -> &'static str {
    match proposal.map(|proposal| proposal.state) {
        Some(ViewProposalState::Pending) => "czeka na Ciebie w panelu na dole",
        Some(ViewProposalState::Executed) => "wykonana",
        Some(ViewProposalState::Rejected) => "odrzucona",
        Some(ViewProposalState::Stale) => "nieaktualna: wykonanie poszło dalej",
        None => match item.proposal.unwrap_or(ProposalState::Open) {
            ProposalState::Open => "czeka na Ciebie w panelu na dole",
            ProposalState::Executed => "wykonana",
            ProposalState::Rejected => "odrzucona",
        },
    }
}

/// One item as a transcript entry of the tab. Every text of it comes from a
/// model or a remote runner and goes through [`sanitize`]; a proposal says
/// where it is answered (the panel, not the tab), a decision that can be
/// undone says which key does it.
pub fn tab_entry(item: &ChatItem, proposal: Option<&ProposalView>) -> TranscriptEntry {
    let raw = serde_json::to_vec(&item.entry).unwrap_or_default();
    let mut lines: Vec<String> = sanitize(&item.entry.text)
        .lines()
        .map(str::to_owned)
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    let conductor = item.entry.author == ChatAuthor::Conductor;
    match item.entry.kind {
        ChatEntryKind::Proposal => {
            lines.push(format!("(propozycja: {})", proposal_word(item, proposal)));
        }
        ChatEntryKind::Decision => match &item.undo {
            Some(UndoState::Undone) => lines.push("(decyzja cofnięta)".to_string()),
            Some(UndoState::Undoable { .. }) => {
                lines.push(match key_of(Context::Details, Action::UndoDecision) {
                    Some(key) => format!("(decyzja; {key} — cofnij)"),
                    None => "(decyzja)".to_string(),
                });
            }
            None => lines.push("(decyzja)".to_string()),
        },
        ChatEntryKind::Message | ChatEntryKind::Error => {}
    }
    if conductor
        && matches!(
            item.entry.kind,
            ChatEntryKind::Decision | ChatEntryKind::Proposal
        )
    {
        let mut why = Vec::new();
        if let Some(confidence) = item.confidence {
            why.push(format!("pewność: {}", confidence.name()));
        }
        if let Some(rationale) = &item.rationale {
            why.push(format!(
                "dlaczego: {}",
                sanitize(rationale).replace('\n', " ")
            ));
        }
        if !why.is_empty() {
            lines.push(why.join(" · "));
        }
    }
    let presentation = if item.entry.kind == ChatEntryKind::Error {
        TranscriptPresentation::Error
    } else if conductor {
        TranscriptPresentation::Conductor
    } else {
        TranscriptPresentation::Operator
    };
    TranscriptEntry::new(TranscriptEntryId::from(item.entry.id.clone()), lines, raw)
        .with_header(utterance_header(
            presentation,
            if conductor { "Dyrygent" } else { "Operator" },
            item.entry.at,
        ))
        .with_technical_lines(vec![format!("wpis #{}", item.entry.sequence)])
        .with_presentation(presentation)
}

/// What undoing `item` costs, for the window that asks: the decision, how sure
/// the conductor was, why, and what an undo leaves alone (R31).
pub fn undo_lines(item: &ChatItem) -> Vec<String> {
    let mut lines = vec![format!(
        "Cofa decyzję dyrygenta: {}",
        sanitize(&item.entry.text).replace('\n', " ")
    )];
    if let Some(label) = &item.label {
        lines.push(format!(
            "Wybrane wyjście: {}",
            sanitize(label).replace('\n', " ")
        ));
    }
    if let Some(confidence) = item.confidence {
        lines.push(format!("Pewność dyrygenta: {}", confidence.name()));
    }
    if let Some(rationale) = &item.rationale {
        lines.push(format!(
            "Uzasadnienie: {}",
            sanitize(rationale).replace('\n', " ")
        ));
    }
    lines.push("! Krok, który ruszył po tej decyzji, zachowuje swoje skutki uboczne.".to_string());
    lines
}
