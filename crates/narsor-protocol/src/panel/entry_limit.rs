//! The form of the decision at the entry limit of a step.

use super::{Builder, PanelContextLine, PanelEntry, PanelPlan};
use crate::api::{PendingHumanInputView, ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_RESET_COUNTER};

impl Builder {
    pub(super) fn build_entry_limit(&mut self, pending: &PendingHumanInputView) {
        if let Some(limit) = &pending.entry_limit {
            self.context.push(PanelContextLine::problem(format!(
                "Wejścia: {}/{}",
                limit.entries, limit.max_entries
            )));
        }
        self.build_problem_outputs(pending);
        self.push(
            PanelEntry::new(
                "Kontynuuj",
                "Uruchom krok jeszcze raz, zwiększając licznik.",
            ),
            PanelPlan::EntryLimit {
                decision: ENTRY_LIMIT_CONTINUE.to_string(),
            },
        );
        // The reset takes the loop guard away for a whole new round of
        // entries; the option says so.
        let until = pending.entry_limit.as_ref().map_or_else(
            || "limitu".to_string(),
            |limit| limit.max_entries.to_string(),
        );
        let reset = format!(
            "Wyzeruj licznik wejść i uruchom krok. Limit przestaje chronić przed pętlą, \
             aż licznik znów dojdzie do {until}."
        );
        self.push(
            PanelEntry::new("Wyzeruj licznik", reset),
            PanelPlan::EntryLimit {
                decision: ENTRY_LIMIT_RESET_COUNTER.to_string(),
            },
        );
    }
}
