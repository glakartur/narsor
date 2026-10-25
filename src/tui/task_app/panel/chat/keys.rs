//! The keys of the chat form.

use crossterm::event::KeyEvent;

use super::Chat;
use crate::tui::chooser::ChooserAction;

/// What a key in the chat asks of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEffect {
    /// The key was folded into the box.
    None,
    /// Say something on the bar.
    Notice(String),
    /// Send the text to the agent: the operator confirmed.
    Send(String),
}

impl Chat {
    /// Folds a key into the box.
    pub fn on_key(&mut self, key: KeyEvent) -> ChatEffect {
        let before = self.text().to_string();
        match self.chooser.on_key(key) {
            ChooserAction::Submit(_) => {
                let text = self.draft_text();
                if text.is_empty() {
                    ChatEffect::Notice("Napisz, co agent ma zobaczyć.".to_string())
                } else {
                    ChatEffect::Send(text)
                }
            }
            ChooserAction::Blocked(reason) => ChatEffect::Notice(reason),
            // `Esc` is the switch of the panel and never reaches here.
            ChooserAction::Cancel => ChatEffect::None,
            ChooserAction::None => {
                if self.text() != before {
                    self.edited();
                }
                ChatEffect::None
            }
        }
    }

    /// Folds pasted text into the box; never sends. Returns whether it took it.
    pub fn paste(&mut self, pasted: &str) -> bool {
        let before = self.text().to_string();
        let used = self.chooser.paste(pasted);
        if used && self.text() != before {
            self.edited();
        }
        used
    }
}
