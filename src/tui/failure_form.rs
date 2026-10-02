//! The failure variant of `i` (KTD4): what an operator settles a failed
//! step with when the execution stays `Failed` beside the conductor's report
//! of it.
//!
//! The form shows the conductor's diagnosis and the actions it proposed —
//! each confirmed or rejected here (R28) — and offers a restart of the step
//! with a note for its next attempt, through the recovery that names the
//! failure's key (KTD9): one the execution has moved on from is refused.
//! There is never a choice of a transition label here (R23). When the
//! conductor could not diagnose the failure (R24), the form shows the raw
//! error and no analysis at all.

use crossterm::event::{KeyCode, KeyEvent};

use crate::conductor::{ChatEntry, ChatEntryKind, FailureOutcome, FailureReport};
use crate::domain::Confidence;
use crate::tui::app::RecoveryCommentForm;
use crate::tui::chat::{chat_items, ChatItem};

/// One row of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureRow {
    /// An open proposal of the conductor's, by index.
    Proposal(usize),
    /// The note a restart of the step is sent with.
    Note,
}

/// What a key press asks of whoever holds the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureAction {
    /// The key was folded into the form.
    None,
    /// Close the form.
    Cancel,
    /// Carry out the proposal with this id.
    Confirm(String),
    /// Reject the proposal with this id.
    Reject(String),
    /// Restart the failed step with this note.
    Restart(String),
}

/// The form of a failed step the conductor reported on.
#[derive(Debug, Clone)]
pub struct FailureForm {
    /// The execution that failed.
    pub execution_id: String,
    /// The key of the failure (KTD9): what a restart from this form names.
    pub key: String,
    /// The state whose step failed.
    pub state_id: String,
    /// The diagnosis, when the conductor made one.
    pub diagnosis: Option<String>,
    /// How sure it was of it.
    pub confidence: Option<Confidence>,
    /// Why it concluded so.
    pub rationale: Option<String>,
    /// The raw error, when the conductor could not diagnose the failure.
    pub raw_error: Option<String>,
    /// The proposals of this failure still waiting for the operator.
    pub proposals: Vec<ChatItem>,
    /// The note a restart is sent with (the recovery note of an agent
    /// step's next attempt).
    pub note: RecoveryCommentForm,
    /// The focused row.
    pub row: usize,
    /// The last failure of a request made from the form.
    pub error: Option<String>,
    /// Why the form can no longer act, once the execution moved on from
    /// the failure it was opened on.
    pub closed: Option<String>,
}

impl FailureForm {
    /// The form for `report`, with the open proposals `chat` holds for it;
    /// `None` when the report leaves no problem (the conductor retried).
    pub fn new(execution_id: &str, report: &FailureReport, chat: &[ChatEntry]) -> Option<Self> {
        let (diagnosis, raw_error) = match &report.outcome {
            FailureOutcome::Diagnosed { diagnosis, .. } => (Some(diagnosis.clone()), None),
            FailureOutcome::Unanswered { .. } => (None, Some(report.reason.clone())),
            FailureOutcome::Retried { .. } => return None,
        };
        let items = chat_items(chat);
        let of_failure = |item: &&ChatItem| item.episode() == Some(report.key.as_str());
        // The diagnosis message carries the answer's confidence and
        // rationale; the proposals carry them too, when it has none.
        let reasoned = items
            .iter()
            .filter(of_failure)
            .filter(|item| {
                item.entry.kind == ChatEntryKind::Message
                    || item.entry.kind == ChatEntryKind::Proposal
            })
            .find(|item| item.confidence.is_some() || item.rationale.is_some());
        let (confidence, rationale) = match (diagnosis.is_some(), reasoned) {
            (true, Some(item)) => (item.confidence, item.rationale.clone()),
            _ => (None, None),
        };
        let proposals = items
            .iter()
            .filter(of_failure)
            .filter(|item| item.open_proposal().is_some())
            .cloned()
            .collect();

        Some(Self {
            execution_id: execution_id.to_string(),
            key: report.key.clone(),
            state_id: report.state_id.clone(),
            diagnosis,
            confidence,
            rationale,
            raw_error,
            proposals,
            note: RecoveryCommentForm::default(),
            row: 0,
            error: None,
            closed: None,
        })
    }

    /// The rows, in the order they are drawn and navigated.
    pub fn rows(&self) -> Vec<FailureRow> {
        let mut rows: Vec<FailureRow> = (0..self.proposals.len())
            .map(FailureRow::Proposal)
            .collect();
        rows.push(FailureRow::Note);
        rows
    }

    /// The focused row.
    pub fn current_row(&self) -> FailureRow {
        self.rows()
            .get(self.row)
            .copied()
            .unwrap_or(FailureRow::Note)
    }

    /// Moves the focus onto `row`, when the form has it.
    pub fn focus(&mut self, row: FailureRow) {
        if let Some(index) = self.rows().iter().position(|candidate| *candidate == row) {
            self.row = index;
        }
    }

    /// Folds one key press into the form.
    pub fn on_key(&mut self, key: KeyEvent) -> FailureAction {
        let rows = self.rows().len();
        let proposal = match self.current_row() {
            FailureRow::Proposal(index) => {
                self.proposals.get(index).map(|item| item.entry.id.clone())
            }
            FailureRow::Note => None,
        };
        match (key.code, proposal) {
            (KeyCode::Esc, _) => return FailureAction::Cancel,
            (KeyCode::Tab | KeyCode::Down, _) => self.row = (self.row + 1) % rows,
            (KeyCode::BackTab | KeyCode::Up, _) => self.row = (self.row + rows - 1) % rows,
            (KeyCode::Enter | KeyCode::Char('y'), Some(id)) => return FailureAction::Confirm(id),
            (KeyCode::Char('n'), Some(id)) => return FailureAction::Reject(id),
            (KeyCode::Char('j'), Some(_)) => self.row = (self.row + 1) % rows,
            (KeyCode::Char('k'), Some(_)) => self.row = (self.row + rows - 1) % rows,
            (KeyCode::Enter, None) => return FailureAction::Restart(self.note.value.clone()),
            (KeyCode::Backspace, None) => {
                self.note.value.pop();
            }
            (KeyCode::Char(character), None) => self.note.value.push(character),
            _ => {}
        }
        FailureAction::None
    }
}
