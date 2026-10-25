//! The form of the decision at the entry limit of a step.

#[cfg(test)]
mod tests;

use super::input::*;
use crate::tui::chooser::ChooserItem;
use narsor_protocol::api::*;

impl InputPanel {
    pub(in crate::tui::task_app::panel) fn build_entry_limit(
        &mut self,
        pending: &PendingHumanInputView,
    ) {
        if let Some(limit) = &pending.entry_limit {
            self.context.push(ContextLine::problem(format!(
                "Wejścia: {}/{}",
                limit.entries, limit.max_entries
            )));
        }
        self.build_problem(pending);
        if self.plans.last() == Some(&Plan::Message) {
            self.plans.pop();
            self.items.pop();
        }
        let continue_index = self.items.len();
        self.push(
            ChooserItem::new(
                "Kontynuuj",
                "Uruchom krok jeszcze raz, zwiększając licznik.",
            ),
            Plan::EntryLimitContinue,
        );
        // The reset takes the loop guard away for a whole new round of
        // entries; the option says so.
        let reset = match &pending.entry_limit {
            Some(limit) => format!(
                "Wyzeruj licznik wejść i uruchom krok. Limit przestaje chronić przed pętlą, \
                 aż licznik znów dojdzie do {}.",
                limit.max_entries
            ),
            None => "Wyzeruj licznik wejść i uruchom krok. Limit przestaje chronić przed pętlą, \
                     aż licznik znów dojdzie do limitu."
                .to_string(),
        };
        self.push(
            ChooserItem::new("Wyzeruj licznik", reset),
            Plan::EntryLimitReset,
        );
        self.selected = continue_index;
    }
}
