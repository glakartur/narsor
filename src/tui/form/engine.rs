//! The state a form keeps between keys, apart from its fields: the edge
//! press that has to come twice, the confirmation before a send, the
//! focus and the growth, and the events the owner is told about.
//!
//! The engine does no I/O and calls nobody back. Every call that changes
//! something returns [`FormEvent`]s as plain data, and the owner reacts to
//! them in its own event loop.

use super::events::{Edge, FormEvent};

/// Where the form stands on a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stage {
    /// Editing.
    #[default]
    Editing,
    /// The popup asks whether to send; the field to return to on `Esc`.
    Confirming { return_field: usize },
}

/// The state of one form.
#[derive(Debug, Clone, Default)]
pub struct FormEngine {
    pending_edge: Option<Edge>,
    stage: Stage,
    focused: bool,
    rows: Option<u16>,
    events: Vec<FormEvent>,
}

impl FormEngine {
    /// A form being edited, with the focus.
    pub fn new() -> Self {
        Self {
            focused: true,
            ..Self::default()
        }
    }

    /// Adds events to the ones the owner has not taken yet.
    pub fn queue(&mut self, events: Vec<FormEvent>) {
        self.events.extend(events);
    }

    /// Hands the owner what happened since it last asked. The owner asks
    /// after a key; the engine never calls it back.
    pub fn take_events(&mut self) -> Vec<FormEvent> {
        std::mem::take(&mut self.events)
    }

    /// Forgets an edge press: any other key than the arrow cancels it.
    pub fn clear_edge(&mut self) {
        self.pending_edge = None;
    }

    /// Notes an arrow press at the edge of a field. Returns whether it is
    /// the second one in a row in that direction, which leaves the field;
    /// the first only arms it.
    pub fn take_edge(&mut self, delta: isize) -> bool {
        let edge = if delta < 0 { Edge::Up } else { Edge::Down };
        if self.pending_edge == Some(edge) {
            self.pending_edge = None;
            true
        } else {
            self.pending_edge = Some(edge);
            false
        }
    }

    /// Whether the popup is open.
    pub fn is_confirming(&self) -> bool {
        matches!(self.stage, Stage::Confirming { .. })
    }

    /// Opens the popup; `Esc` returns to `return_field`.
    pub fn ask_confirmation(&mut self, return_field: usize) -> Vec<FormEvent> {
        self.pending_edge = None;
        self.stage = Stage::Confirming { return_field };
        vec![FormEvent::ConfirmRequested]
    }

    /// `Esc` in the popup: back to the field the form was in.
    pub fn decline(&mut self) -> Option<usize> {
        match self.stage {
            Stage::Confirming { return_field } => {
                self.stage = Stage::Editing;
                Some(return_field)
            }
            _ => None,
        }
    }

    /// `Enter` in the popup: the form is sent, once. The popup is closed, so
    /// another `Enter` on a form the owner kept only asks again.
    pub fn accept(&mut self) -> bool {
        if self.is_confirming() {
            self.stage = Stage::Editing;
            true
        } else {
            false
        }
    }

    /// Gives the focus to the form or takes it away. Without it the form
    /// uses no key and keeps what was typed. Returns what changed.
    pub fn set_focus(&mut self, focused: bool) -> Vec<FormEvent> {
        self.pending_edge = None;
        if self.focused == focused {
            return Vec::new();
        }
        self.focused = focused;
        vec![FormEvent::FocusChanged { gained: focused }]
    }

    /// Whether the form has the focus.
    pub fn has_focus(&self) -> bool {
        self.focused
    }

    /// Notes how many rows the form wants now, against `limit` rows it may
    /// take at most (never fewer than one). Returns a `Resized` event when
    /// the answer differs from the last one.
    pub fn resize(&mut self, wanted: usize, limit: usize) -> Vec<FormEvent> {
        let rows = wanted.min(limit.max(1)).max(1).min(u16::MAX as usize) as u16;
        if self.rows == Some(rows) {
            return Vec::new();
        }
        self.rows = Some(rows);
        vec![FormEvent::Resized { rows }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edge_needs_two_presses_in_one_direction() {
        let mut engine = FormEngine::new();
        assert!(!engine.take_edge(-1));
        assert!(engine.take_edge(-1));
        assert!(!engine.take_edge(-1));
    }

    #[test]
    fn another_key_or_the_other_direction_starts_over() {
        let mut engine = FormEngine::new();
        assert!(!engine.take_edge(-1));
        engine.clear_edge();
        assert!(!engine.take_edge(-1));
        assert!(!engine.take_edge(1));
        assert!(engine.take_edge(1));
    }

    #[test]
    fn confirmation_sends_once() {
        let mut engine = FormEngine::new();
        assert_eq!(
            engine.ask_confirmation(2),
            vec![FormEvent::ConfirmRequested]
        );
        assert!(engine.accept());
        assert!(!engine.accept(), "the popup is closed after the send");
    }

    #[test]
    fn declining_returns_to_the_field() {
        let mut engine = FormEngine::new();
        engine.ask_confirmation(3);
        assert_eq!(engine.decline(), Some(3));
        assert!(!engine.is_confirming());
        assert_eq!(engine.decline(), None);
    }

    #[test]
    fn focus_changes_are_reported_once() {
        let mut engine = FormEngine::new();
        assert_eq!(
            engine.set_focus(false),
            vec![FormEvent::FocusChanged { gained: false }]
        );
        assert!(engine.set_focus(false).is_empty());
        assert_eq!(
            engine.set_focus(true),
            vec![FormEvent::FocusChanged { gained: true }]
        );
    }

    #[test]
    fn growth_stops_at_the_limit_and_reports_changes_only() {
        let mut engine = FormEngine::new();
        assert_eq!(engine.resize(3, 10), vec![FormEvent::Resized { rows: 3 }]);
        assert!(engine.resize(3, 10).is_empty());
        assert_eq!(engine.resize(12, 10), vec![FormEvent::Resized { rows: 10 }]);
        assert!(engine.resize(30, 10).is_empty());
        assert_eq!(engine.resize(2, 10), vec![FormEvent::Resized { rows: 2 }]);
        // A limit below one row still leaves one.
        assert_eq!(engine.resize(5, 0), vec![FormEvent::Resized { rows: 1 }]);
    }
}
