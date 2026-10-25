//! The first line of the panel: which form of how many, and the key that
//! steps through them (R12).

use super::switcher::Switcher;

/// `2/3 · Esc przełącza`: the place of the form shown among those on offer.
/// Always there, with one form too: the operator learns the key and the count
/// from the same place.
pub fn position_hint(switcher: &Switcher) -> String {
    format!(
        "{}/{} · Esc przełącza",
        switcher.position() + 1,
        switcher.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use narsor_protocol::controller::{PanelForm, PanelFormKind, PanelOffer};

    #[test]
    fn the_hint_counts_from_one_and_names_the_key() {
        let form = |kind| PanelForm {
            kind,
            waiting_id: None,
            choices: Vec::new(),
            definition: None,
        };
        let mut switcher = Switcher::default();
        switcher.sync(&PanelOffer {
            forms: vec![
                form(PanelFormKind::Answer),
                form(PanelFormKind::Chat),
                form(PanelFormKind::Action),
            ],
            default: Some(0),
        });
        assert_eq!(position_hint(&switcher), "1/3 · Esc przełącza");
        switcher.next();
        assert_eq!(position_hint(&switcher), "2/3 · Esc przełącza");
    }
}
