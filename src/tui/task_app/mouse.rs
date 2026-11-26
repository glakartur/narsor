//! The mouse: a wheel notch or a left click goes to the panel under the
//! pointer, as the last frame drew it.
//!
//! The keys stay complete; the mouse only adds a way in. A notch acts on the
//! panel under the pointer without taking the focus there; a click gives
//! that panel the focus, as the keys would. Both go through the same changes
//! of state the keys go through, never through made-up key events, so what a
//! key change sets off (the details read, the step held, the follow stopped)
//! follows a mouse change too.
//!
//! # Where things are
//!
//! The drawing decides the geometry, so the drawing records it: each frame
//! clears [`Panels`] and every panel drawn puts its rectangle in. A point no
//! drawn panel covers — the header, the bar, the screen that asks for a
//! bigger window — hits nothing, and a panel the layout did not draw cannot
//! be hit. A panel inside another is recorded after it, and the last one that
//! covers a point wins.
//!
//! # What holds the screen
//!
//! While a window over the screen or a form holds the keys, only what is
//! inside it reacts: a click or a notch elsewhere would bypass the key that
//! closes it. While the list filter is typed or a task is being moved, the
//! mouse does nothing at all: a stray notch must not reorder the queue nor
//! leave the filter half typed.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::{Focus, Request, TaskApp, View};

/// A panel a click or a notch can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// The task list, or the list of the history.
    List,
    /// The details, or the view open in their place.
    Details,
    /// A window over the screen: the action menu, a list, a form.
    Overlay,
}

/// The panels of the last frame, in the order they were drawn.
#[derive(Debug, Clone, Default)]
pub struct Panels {
    drawn: Vec<(Panel, Rect)>,
}

impl Panels {
    /// Forgets the last frame.
    pub(super) fn clear(&mut self) {
        self.drawn.clear();
    }

    /// `panel` was drawn into `area`.
    pub(super) fn record(&mut self, panel: Panel, area: Rect) {
        self.drawn.push((panel, area));
    }

    /// The panel drawn last at `column`, `row`.
    fn at(&self, column: u16, row: u16) -> Option<Panel> {
        self.drawn
            .iter()
            .rev()
            .find(|(_, area)| area.contains(Position::new(column, row)))
            .map(|&(panel, _)| panel)
    }

    /// Whether `panel` was drawn at `column`, `row`, whatever is over it.
    fn covers(&self, panel: Panel, column: u16, row: u16) -> bool {
        self.drawn
            .iter()
            .any(|&(drawn, area)| drawn == panel && area.contains(Position::new(column, row)))
    }
}

/// What the mouse did, of what the interface uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    /// One notch of the wheel away from the operator: up the list.
    WheelUp,
    /// One notch of the wheel towards the operator: down the list.
    WheelDown,
    /// The left button went down.
    Click,
}

impl Gesture {
    /// The gesture of `kind`; `None` for what the interface does not use:
    /// moves, drags, releases, the other buttons, the horizontal wheel.
    fn of(kind: MouseEventKind) -> Option<Self> {
        match kind {
            MouseEventKind::ScrollUp => Some(Self::WheelUp),
            MouseEventKind::ScrollDown => Some(Self::WheelDown),
            MouseEventKind::Down(MouseButton::Left) => Some(Self::Click),
            _ => None,
        }
    }
}

/// What holds the screen while a mouse event comes.
enum Hold {
    /// Nothing: every panel reacts.
    Free,
    /// Only what is inside this panel reacts.
    Inside(Panel),
    /// Nothing reacts.
    Everything,
}

impl TaskApp {
    /// Where the last frame drew its panels; the drawing fills it.
    pub(super) fn panels_mut(&mut self) -> &mut Panels {
        &mut self.panels
    }

    /// Folds a mouse event into the state, at the panel under the pointer.
    /// Returns what needs the API: nothing, since a click selects and never
    /// runs what it selected.
    pub fn on_mouse(&mut self, event: MouseEvent) -> Request {
        let Some(gesture) = Gesture::of(event.kind) else {
            return Request::None;
        };
        let (column, row) = (event.column, event.row);
        let Some(panel) = self.panels.at(column, row) else {
            return Request::None;
        };
        match self.hold() {
            Hold::Free => {}
            Hold::Inside(holder) if self.panels.covers(holder, column, row) => {}
            Hold::Inside(_) | Hold::Everything => return Request::None,
        }
        if gesture == Gesture::Click {
            self.click_focus(panel);
        }
        Request::None
    }

    /// What holds the screen now, from the state and not from what was drawn
    /// over what.
    fn hold(&self) -> Hold {
        let typing_filter = matches!(self.view, View::Tasks)
            && self.focus == Focus::List
            && self.list.is_filtering();
        if self.help_open
            || self.quit_confirm
            || self.form_confirming()
            || typing_filter
            || self.list.moving().is_some()
            || matches!(self.view, View::Opening(_))
        {
            return Hold::Everything;
        }
        if self.overlay.is_some() {
            return Hold::Inside(Panel::Overlay);
        }
        let form_has_keys = self.focus == Focus::Details
            && matches!(self.view, View::NewTask(_) | View::Repositories(_));
        if form_has_keys {
            return Hold::Inside(Panel::Details);
        }
        Hold::Free
    }

    /// A click gives `panel` the focus, as the keys that move it there do.
    fn click_focus(&mut self, panel: Panel) {
        let history = matches!(self.view, View::History(_));
        match panel {
            Panel::List if history => self.focus = Focus::List,
            Panel::List => {
                self.focus = Focus::List;
                self.panel_focus = false;
                self.back_to_axis();
            }
            Panel::Details if history => self.focus = Focus::Details,
            Panel::Details => {
                self.focus = Focus::Details;
                self.panel_focus = false;
            }
            Panel::Overlay => {}
        }
    }
}
