//! The free message to the agent (R16, R17).
//!
//! A text box that starts at one row and grows with what is typed, to half
//! the height of the task's details; past that the text scrolls inside it.
//! The editing is the engine of `form`: `Ctrl+J` makes a new line, `Enter`
//! asks before it sends. The message is about the task, not about one wait,
//! so there is one draft, and it stays through any read and any `Esc`.

mod keys;
mod view;

#[cfg(test)]
mod tests;

pub use keys::ChatEffect;

use super::input::MESSAGE_FIELD;
use crate::tui::chooser::{Chooser, ChooserField, ChooserItem};

/// The label of the box.
const LABEL: &str = "Wiadomość do agenta";

/// The state of the chat form.
#[derive(Debug, Clone)]
pub struct Chat {
    chooser: Chooser,
    /// What the last send found wrong, or the server refused with.
    error: Option<String>,
    /// The key of a send that did not get an answer; a retry reuses it.
    key: Option<String>,
}

impl Default for Chat {
    fn default() -> Self {
        Self::new()
    }
}

impl Chat {
    /// An empty chat, with the keys in its box.
    pub fn new() -> Self {
        let item = ChooserItem::new(LABEL, "").with_field(ChooserField::multiline(MESSAGE_FIELD));
        let mut chooser = Chooser::new(vec![item])
            .with_inputs_layout()
            .confirming_submit();
        chooser.enter_fields();
        Self {
            chooser,
            error: None,
            key: None,
        }
    }

    /// What was typed and not sent.
    pub fn draft_text(&self) -> String {
        self.text().trim().to_string()
    }

    /// Whether the keys are typed into the box (`q` and `?` are text then).
    pub fn is_typing(&self) -> bool {
        self.chooser.is_typing()
    }

    /// Whether the confirmation window of a send is open.
    pub fn is_confirming(&self) -> bool {
        self.chooser.is_confirming()
    }

    /// What the last send found wrong.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The key a send of the draft goes under: a retry of the same text
    /// reuses it.
    pub fn send_key(&mut self, make: impl FnOnce() -> String) -> String {
        self.key.get_or_insert_with(make).clone()
    }

    /// Says what a send came to. Success empties the box, unless the text was
    /// edited while the send was in flight (an edit drops the key): then what
    /// was typed since stays. A refusal keeps the text and shows why; a lost
    /// connection keeps the key for the retry.
    pub fn settle(&mut self, outcome: Result<(), (String, bool)>) {
        match outcome {
            Ok(()) if self.key.is_some() => *self = Self::new(),
            Ok(()) => self.error = None,
            Err((reason, retryable)) => {
                if !retryable {
                    self.key = None;
                }
                self.error = Some(reason);
            }
        }
    }

    /// Gives the keys to the box.
    pub fn enter(&mut self) {
        self.chooser.enter_fields();
    }

    /// Takes the keys out of the box; the text stays.
    pub fn leave(&mut self) {
        self.chooser.leave_fields();
    }

    fn text(&self) -> &str {
        self.chooser
            .fields_of(0)
            .first()
            .map_or("", ChooserField::value)
    }

    fn edited(&mut self) {
        self.error = None;
        self.key = None;
    }
}
