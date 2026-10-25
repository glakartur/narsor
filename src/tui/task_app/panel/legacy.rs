//! The panels of the owner's own waits: a failed provisioning and a problem of
//! the runner's, answered with the owner commands `start` and `archive`.
//!
//! Temporary: these waits are not among the server's forms yet, so they keep
//! the panel they had. The panels of a failed step and of a conductor's
//! proposal are in [`crate::tui::failure_form`] for the same reason.

use crate::controller_protocol::TaskSummary;
use narsor_protocol::controller::OwnerCommand;

use crate::tui::chooser::{ChooserItem, Submission};
use crate::tui::input_form::ContextLine;

/// The owner commands behind a failed provisioning or a problem of the
/// runner's.
#[derive(Debug, Clone)]
pub(super) struct OwnerPanel {
    pub(super) context: Vec<ContextLine>,
    pub(super) items: Vec<ChooserItem>,
    pub(super) commands: Vec<OwnerCommand>,
}

impl OwnerPanel {
    pub(super) fn start_item(description: &str) -> ChooserItem {
        ChooserItem::new("Uruchom od nowa", description)
    }

    pub(super) fn provisioning(task: &TaskSummary) -> Self {
        let mut context = vec![ContextLine::plain(
            "Backend nie przygotował workspace'u; zadanie nie wystartowało.",
        )];
        if let Some(detail) = task
            .observation
            .as_ref()
            .and_then(|observed| observed.summary.detail.as_deref())
        {
            context.push(ContextLine::problem(detail));
        }
        Self {
            context,
            items: vec![
                Self::start_item("Ponawia przygotowanie i start zadania na tym samym storage."),
                ChooserItem::new(
                    "Archiwizuj",
                    "Usuwa workload i storage zadania; zadanie trafia do Historii.",
                )
                .irreversible(),
            ],
            commands: vec![OwnerCommand::Start, OwnerCommand::Archive],
        }
    }

    pub(super) fn runner_problem(task: &TaskSummary, code: &str) -> Self {
        let message = task
            .runner
            .as_ref()
            .and_then(|runner| runner.problem.as_ref())
            .filter(|problem| problem.code == code)
            .map_or(String::new(), |problem| format!(": {}", problem.message));
        Self {
            context: vec![
                ContextLine::plain("Runner nie może wykonywać pracy."),
                ContextLine::problem(format!("{code}{message}")),
            ],
            items: vec![Self::start_item(
                "Start zadania jest Twoją decyzją o kontynuacji: runner zaczyna od zapisanego stanu.",
            )],
            commands: vec![OwnerCommand::Start],
        }
    }

    pub(super) fn summary(&self, submission: &Submission) -> String {
        match self.commands.get(submission.index) {
            Some(OwnerCommand::Archive) => "zarchiwizowano",
            _ => "uruchomiono od nowa",
        }
        .to_string()
    }
}
