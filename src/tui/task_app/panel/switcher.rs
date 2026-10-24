//! Which form of the panel is shown (R11, R13, R14).
//!
//! The switcher holds the keys of the forms the server offered and the one
//! shown. It knows nothing of what a form holds: the drafts stay with the
//! forms, so stepping away and back loses nothing.

use narsor_protocol::controller::{PanelFormKind, PanelOffer};

/// What tells one form from another across reads: its kind and the wait it
/// answers (R13).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FormKey {
    /// The kind of form.
    pub kind: PanelFormKind,
    /// The wait it answers, if it answers one.
    pub waiting_id: Option<String>,
}

/// The forms on offer and the one shown.
#[derive(Debug, Clone, Default)]
pub struct Switcher {
    keys: Vec<FormKey>,
    active: usize,
    /// The form the server said opens first, at the last read (R14).
    default: Option<FormKey>,
}

impl Switcher {
    /// Takes a new offer in. The form shown stays when it is still on offer
    /// and the server did not point at a new default (R13); a new default
    /// takes the view, whatever was shown (R14); a form that went away gives
    /// the view to the default.
    pub fn sync(&mut self, offer: &PanelOffer) {
        let keys: Vec<FormKey> = offer
            .forms
            .iter()
            .map(|form| FormKey {
                kind: form.kind,
                waiting_id: form.waiting_id.clone(),
            })
            .collect();
        let default = offer.default.and_then(|index| keys.get(index)).cloned();
        let shown = self.active().cloned();
        let default_moved = default.is_some() && default != self.default;
        let still_there = shown.and_then(|key| keys.iter().position(|other| *other == key));
        self.active = match (default_moved, still_there) {
            (false, Some(index)) => index,
            _ => default
                .as_ref()
                .and_then(|key| keys.iter().position(|other| other == key))
                .unwrap_or(0),
        };
        self.keys = keys;
        self.default = default;
    }

    /// `Esc`: the next form, round; one form stays (R11).
    pub fn next(&mut self) {
        if !self.keys.is_empty() {
            self.active = (self.active + 1) % self.keys.len();
        }
    }

    /// The form shown.
    pub fn active(&self) -> Option<&FormKey> {
        self.keys.get(self.active)
    }

    /// The forms on offer.
    pub fn keys(&self) -> &[FormKey] {
        &self.keys
    }

    /// How many forms there are.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether there is no form.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The place of the form shown, from zero.
    pub fn position(&self) -> usize {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use narsor_protocol::controller::PanelForm;

    fn form(kind: PanelFormKind, waiting: Option<&str>) -> PanelForm {
        PanelForm {
            kind,
            waiting_id: waiting.map(str::to_string),
            fields: Vec::new(),
            choices: Vec::new(),
        }
    }

    fn offer(forms: Vec<PanelForm>, default: Option<usize>) -> PanelOffer {
        PanelOffer { forms, default }
    }

    fn three() -> PanelOffer {
        offer(
            vec![
                form(PanelFormKind::Answer, Some("w1")),
                form(PanelFormKind::Chat, Some("w1")),
                form(PanelFormKind::Action, None),
            ],
            Some(0),
        )
    }

    #[test]
    fn esc_goes_round_the_three_forms() {
        let mut switcher = Switcher::default();
        switcher.sync(&three());
        let seen: Vec<_> = (0..4)
            .map(|_| {
                let at = switcher.position();
                switcher.next();
                at
            })
            .collect();
        assert_eq!(seen, [0, 1, 2, 0]);
    }

    #[test]
    fn esc_with_one_form_changes_nothing() {
        let mut switcher = Switcher::default();
        switcher.sync(&offer(vec![form(PanelFormKind::Action, None)], Some(0)));
        switcher.next();
        assert_eq!(switcher.position(), 0);
    }

    #[test]
    fn a_read_with_the_same_default_keeps_the_form_shown() {
        let mut switcher = Switcher::default();
        switcher.sync(&three());
        switcher.next();
        switcher.sync(&three());
        assert_eq!(switcher.position(), 1);
    }

    #[test]
    fn the_form_shown_going_away_gives_the_view_to_the_default() {
        let mut switcher = Switcher::default();
        switcher.sync(&three());
        switcher.next();
        switcher.next();
        switcher.sync(&offer(
            vec![
                form(PanelFormKind::Answer, Some("w1")),
                form(PanelFormKind::Chat, Some("w1")),
            ],
            Some(0),
        ));
        assert_eq!(switcher.position(), 0);
    }

    #[test]
    fn a_new_default_takes_the_view_from_the_form_shown() {
        let mut switcher = Switcher::default();
        switcher.sync(&offer(
            vec![
                form(PanelFormKind::Chat, None),
                form(PanelFormKind::Action, None),
            ],
            Some(0),
        ));
        assert_eq!(switcher.active().map(|key| key.kind), Some(PanelFormKind::Chat));
        // The agent asks a question: the answer is the new default.
        switcher.sync(&offer(
            vec![
                form(PanelFormKind::Answer, Some("w2")),
                form(PanelFormKind::Chat, None),
                form(PanelFormKind::Action, None),
            ],
            Some(0),
        ));
        assert_eq!(switcher.active().map(|key| key.kind), Some(PanelFormKind::Answer));
        // And the chat is one `Esc` away.
        switcher.next();
        assert_eq!(switcher.active().map(|key| key.kind), Some(PanelFormKind::Chat));
    }

    #[test]
    fn an_empty_offer_has_no_form_shown() {
        let mut switcher = Switcher::default();
        switcher.sync(&three());
        switcher.sync(&PanelOffer::default());
        assert!(switcher.is_empty());
        assert_eq!(switcher.active(), None);
    }
}
