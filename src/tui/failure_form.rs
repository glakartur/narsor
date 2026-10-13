//! What the panel "Czeka na Ciebie" offers for a failed step and for a proposal
//! of the conductor.
//!
//! [`OperatorPanel`] is a choice: every proposal of the conductor's is a pair
//! of entries — confirm and reject, the reasoning in the description — and the
//! restart of the step with a note is the last one. Choosing an entry turns
//! into an operator command: `ConfirmProposal`, `RejectProposal`,
//! `RestartStep` (naming the failure's key, KTD9: one the task has moved on
//! from is refused). When the conductor could not diagnose the failure (R24),
//! the panel shows the raw error and no analysis at all. There is never a
//! choice of a transition label here (R23).

use crate::conductor::{
    ConductorView, FailureOutcome, FailureReport, ProposalKind, ProposalState, ProposalView,
};
use crate::domain::Confidence;
use crate::tui::chooser::{ChooserField, ChooserItem, Submission};
use crate::tui::input_form::ContextLine;
use narsor_protocol::controller::OperatorCommand;

/// The field a restart's note is typed into.
pub const NOTE_FIELD: &str = "Notatka dla następnej próby";

/// The longest a proposal's one-line summary gets in the list.
const LIST_SUMMARY_WIDTH: usize = 40;

/// What an entry of the panel does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Confirm(String),
    Reject(String),
    Restart,
}

/// A failed step or a proposal of the conductor's as the panel offers it.
#[derive(Debug, Clone)]
pub struct OperatorPanel {
    failure_key: Option<String>,
    state_id: Option<String>,
    context: Vec<ContextLine>,
    items: Vec<ChooserItem>,
    commands: Vec<Command>,
    selected: usize,
}

impl OperatorPanel {
    /// The panel of a failed step with `failure_key` (the waiting id the
    /// runner reports), which failed in `state_id`. `view` is what the
    /// conductor says about it, once read; `raw_error` is what the timeline
    /// knows of the failure, for when the conductor has nothing to say.
    pub fn failed_step(
        failure_key: &str,
        state_id: Option<&str>,
        view: Option<&ConductorView>,
        raw_error: Option<&str>,
    ) -> Self {
        let mut panel = Self::empty(Some(failure_key), state_id);
        let report = view
            .and_then(|view| view.failure.as_ref())
            .filter(|report| report.key == failure_key);
        let step = state_id.map_or_else(String::new, |state| format!(" {state}"));
        panel
            .context
            .push(ContextLine::plain(format!("Krok{step} nie powiódł się.")));
        let proposals: Vec<&ProposalView> = view
            .map(|view| {
                view.proposals
                    .iter()
                    .filter(|proposal| proposal.state == ProposalState::Pending)
                    .filter(
                        |proposal| match (report, proposal.record.episode.as_deref()) {
                            (Some(report), Some(episode)) => {
                                episode == report.key || episode == report.episode
                            }
                            (None, Some(episode)) => episode == failure_key,
                            (_, None) => false,
                        },
                    )
                    .collect()
            })
            .unwrap_or_default();
        panel.explain_failure(report, view, raw_error, proposals.first().copied());
        for proposal in &proposals {
            panel.push_proposal(proposal);
        }
        let restart = ChooserItem::new(
            "Restart kroku z notatką",
            "Uruchamia krok od nowa; notatka trafia do następnej próby agenta.",
        )
        .resuming(state_id.unwrap_or_default())
        .with_field(ChooserField::multiline(NOTE_FIELD));
        panel.push(restart, Command::Restart);
        panel
    }

    /// The panel of a proposal of the conductor's with `proposal_id`.
    pub fn proposal(proposal_id: &str, view: Option<&ConductorView>) -> Self {
        let mut panel = Self::empty(None, None);
        let found = view.and_then(|view| {
            view.proposals
                .iter()
                .find(|proposal| proposal.proposal_id == proposal_id)
        });
        match found {
            Some(proposal) => {
                panel.state_id = proposal.record.state_id.clone();
                panel.push_proposal_context(proposal);
                panel.push_proposal(proposal);
            }
            None => {
                let text = match view {
                    None => "wczytuję propozycję dyrygenta…",
                    Some(_) => "propozycja dyrygenta jest już nieaktualna",
                };
                panel.context.push(ContextLine::dim(text));
                panel.push(
                    ChooserItem::new("Potwierdź", "Wykonuje propozycję dyrygenta.").recommended(),
                    Command::Confirm(proposal_id.to_string()),
                );
                panel.push(
                    ChooserItem::new("Odrzuć", "Odrzuca propozycję dyrygenta."),
                    Command::Reject(proposal_id.to_string()),
                );
            }
        }
        panel
    }

    fn empty(failure_key: Option<&str>, state_id: Option<&str>) -> Self {
        Self {
            failure_key: failure_key.map(str::to_string),
            state_id: state_id.map(str::to_string),
            context: Vec::new(),
            items: Vec::new(),
            commands: Vec::new(),
            selected: 0,
        }
    }

    /// What the request is about.
    pub fn context(&self) -> &[ContextLine] {
        &self.context
    }

    /// The entries to choose from.
    pub fn items(&self) -> &[ChooserItem] {
        &self.items
    }

    /// The entry the panel opens on: the first confirmation, which is the
    /// conductor's recommendation, or the restart.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The operator command that choosing `submission` sends.
    pub fn command(&self, submission: &Submission) -> Option<OperatorCommand> {
        Some(match self.commands.get(submission.index)? {
            Command::Confirm(id) => OperatorCommand::ConfirmProposal {
                proposal_id: id.clone(),
            },
            Command::Reject(id) => OperatorCommand::RejectProposal {
                proposal_id: id.clone(),
            },
            Command::Restart => OperatorCommand::RestartStep {
                note: submission
                    .values
                    .iter()
                    .find(|(name, _)| name == NOTE_FIELD)
                    .map(|(_, note)| note.clone()),
                failure_key: self.failure_key.clone(),
            },
        })
    }

    /// What the notification says after `submission` was sent, without the
    /// task's name.
    pub fn summary(&self, submission: &Submission) -> String {
        match self.commands.get(submission.index) {
            Some(Command::Confirm(_)) => "potwierdzono propozycję dyrygenta".to_string(),
            Some(Command::Reject(_)) => "odrzucono propozycję dyrygenta".to_string(),
            Some(Command::Restart) | None => match &self.state_id {
                Some(state) => format!("restart kroku {state}"),
                None => "restart kroku".to_string(),
            },
        }
    }

    fn push(&mut self, item: ChooserItem, command: Command) {
        self.items.push(item);
        self.commands.push(command);
    }

    /// The diagnosis, or why there is none.
    fn explain_failure(
        &mut self,
        report: Option<&FailureReport>,
        view: Option<&ConductorView>,
        raw_error: Option<&str>,
        reasoned: Option<&ProposalView>,
    ) {
        match report.map(|report| &report.outcome) {
            Some(FailureOutcome::Diagnosed { diagnosis, .. }) => {
                self.context
                    .push(ContextLine::conductor(format!("◆ diagnoza: {diagnosis}")));
                if let Some(proposal) = reasoned {
                    self.push_reasoning(
                        proposal.record.confidence,
                        proposal.record.rationale.as_deref(),
                    );
                }
                return;
            }
            Some(FailureOutcome::Unanswered { error }) => {
                self.context.push(ContextLine::problem(format!(
                    "dyrygent nie zdiagnozował błędu: {error}"
                )));
            }
            Some(FailureOutcome::Retried { note }) => {
                self.context.push(ContextLine::dim(format!(
                    "dyrygent ponowił krok z notatką: {note}"
                )));
            }
            None => match view {
                None => self
                    .context
                    .push(ContextLine::dim("wczytuję diagnozę dyrygenta…")),
                Some(view) if !view.available => {
                    let reason = view.unavailable_reason.as_deref().unwrap_or("brak powodu");
                    self.context
                        .push(ContextLine::dim(format!("dyrygent niedostępny: {reason}")));
                }
                Some(_) => self
                    .context
                    .push(ContextLine::dim("dyrygent jeszcze nie zdiagnozował błędu")),
            },
        }
        if let Some(error) = raw_error {
            self.context.push(ContextLine::problem(error));
        }
    }

    fn push_reasoning(&mut self, confidence: Option<Confidence>, rationale: Option<&str>) {
        if let Some(confidence) = confidence {
            self.context.push(ContextLine::conductor(format!(
                "◆ pewność: {}",
                confidence_word(confidence)
            )));
        }
        if let Some(rationale) = rationale {
            self.context.push(ContextLine::conductor(format!(
                "◆ uzasadnienie: {rationale}"
            )));
        }
    }

    fn push_proposal_context(&mut self, proposal: &ProposalView) {
        self.context.push(ContextLine::conductor(format!(
            "◆ dyrygent proponuje: {}",
            proposal_text(proposal)
        )));
        self.push_reasoning(
            proposal.record.confidence,
            proposal.record.rationale.as_deref(),
        );
    }

    /// Two entries for `proposal`: confirm (the conductor's recommendation)
    /// and reject.
    fn push_proposal(&mut self, proposal: &ProposalView) {
        let text = proposal_text(proposal);
        let mut description = text.clone();
        if let Some(rationale) = &proposal.record.rationale {
            description.push_str(&format!("\n\nUzasadnienie: {rationale}"));
        }
        if let Some(confidence) = proposal.record.confidence {
            description.push_str(&format!("\nPewność: {}", confidence_word(confidence)));
        }
        let short: String = text.chars().take(LIST_SUMMARY_WIDTH).collect();
        let mut confirm =
            ChooserItem::new(format!("Potwierdź: {short}"), description).recommended();
        match proposal.record.proposal.kind {
            ProposalKind::Retry | ProposalKind::MessageAgent => {
                confirm = confirm.resuming(self.state_id.clone().unwrap_or_default());
            }
            ProposalKind::Abort => confirm = confirm.irreversible(),
            ProposalKind::AnswerInput | ProposalKind::Transition => {}
        }
        self.push(confirm, Command::Confirm(proposal.proposal_id.clone()));
        self.push(
            ChooserItem::new(
                format!("Odrzuć: {short}"),
                format!("Odrzuca propozycję; nic się nie dzieje.\n\n{text}"),
            ),
            Command::Reject(proposal.proposal_id.clone()),
        );
    }
}

/// The one-line description of a proposal: its summary, or what the action
/// says of itself.
fn proposal_text(proposal: &ProposalView) -> String {
    proposal
        .record
        .proposal
        .summary
        .clone()
        .filter(|summary| !summary.trim().is_empty())
        .unwrap_or_else(|| proposal.record.proposal.describe())
}

/// `wysoka` / `niska`.
fn confidence_word(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::High => "wysoka",
        Confidence::Low => "niska",
    }
}
