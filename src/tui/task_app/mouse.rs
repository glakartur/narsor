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


use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::super::chooser::ListMouse;
use super::tabs::progress::Spot;
use super::{DetailTab, Focus, Request, StepFocus, TaskApp, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    List,
    Details,
    /// What the open tab of the details shows, under their header and tabs.
    Content,
    /// A window over the screen: the action menu, a list, a form. The action
    /// menu drawn in the details in place of the answer panel is one too.
    Overlay,
    Answer,
}

/// The panels of the last frame, in the order they were drawn.
#[derive(Debug, Clone, Default)]
pub struct Panels {
    drawn: Vec<(Panel, Rect)>,
}

impl Panels {
    pub(super) fn clear(&mut self) {
        self.drawn.clear();
    }

    pub(super) fn record(&mut self, panel: Panel, area: Rect) {
        self.drawn.push((panel, area));
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    WheelUp,
    WheelDown,
    Click,
}

impl Gesture {
    /// `None` for what the interface does not use: moves, drags, releases,
    /// the other buttons, the horizontal wheel.
    fn of(kind: MouseEventKind) -> Option<Self> {
        match kind {
            MouseEventKind::ScrollUp => Some(Self::WheelUp),
            MouseEventKind::ScrollDown => Some(Self::WheelDown),
            MouseEventKind::Down(MouseButton::Left) => Some(Self::Click),
            _ => None,
        }
    }

    fn on_list(self) -> ListMouse {
        match self {
            Self::WheelUp => ListMouse::WheelUp,
            Self::WheelDown => ListMouse::WheelDown,
            Self::Click => ListMouse::Click,
        }
    }
}

/// What holds the screen while a mouse event comes.
enum Hold {
    Free,
    /// Only what is inside this panel reacts.
    Inside(Panel),
    Everything,
}

impl TaskApp {
    pub(super) fn panels_mut(&mut self) -> &mut Panels {
        &mut self.panels
    }

    /// Returns what needs the API: at most the read a new highlight needs
    /// (the branches of the workflow highlighted in the new-task form); a
    /// click selects and never runs or sends what it selected.
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
        match (gesture, panel) {
            (Gesture::WheelUp, Panel::List) => self.step_list(-1),
            (Gesture::WheelDown, Panel::List) => self.step_list(1),
            (Gesture::WheelUp, Panel::Content) => self.wheel_details(-1, column, row),
            (Gesture::WheelDown, Panel::Content) => self.wheel_details(1, column, row),
            (Gesture::Click, Panel::List) => {
                self.click_focus(panel);
                self.click_list(column, row);
            }
            (Gesture::Click, Panel::Content) => {
                self.click_focus(panel);
                self.click_details(column, row);
            }
            (_, Panel::Overlay) => self.on_overlay_mouse(gesture.on_list(), column, row),
            (_, Panel::Answer) => self.answer_mouse(gesture, column, row),
            (_, Panel::Details) => return self.view_mouse(gesture, column, row),
        }
        Request::None
    }

    fn selected_task_id(&self) -> Option<String> {
        self.list.selected_id().map(str::to_string)
    }

    /// A click also gives the panel the keys, as `Tab` in the details does.
    fn answer_mouse(&mut self, gesture: Gesture, column: u16, row: u16) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        if gesture == Gesture::Click {
            self.focus = Focus::Details;
            self.focus_panel();
        }
        self.detail
            .view_mut(&id)
            .waiting
            .on_mouse(gesture.on_list(), column, row);
    }

    fn view_mouse(&mut self, gesture: Gesture, column: u16, row: u16) -> Request {
        if gesture == Gesture::Click {
            self.click_focus(Panel::Details);
        }
        let mouse = gesture.on_list();
        match self.view {
            View::NewTask(_) => return self.on_new_task_mouse(mouse, column, row),
            View::Repositories(_) => self.on_repositories_mouse(mouse, column, row),
            View::Queues(_) => self.on_queues_mouse(mouse, column, row),
            View::Tasks | View::History(_) | View::Opening(_) => {}
        }
        Request::None
    }

    /// `by` is 1 down or -1 up; the focus stays where it is.
    fn wheel_details(&mut self, by: isize, column: u16, row: u16) {
        if let View::History(history) = &mut self.view {
            history.on_detail_wheel(by, column, row);
            return;
        }
        let Some(id) = self.selected_task_id() else {
            return;
        };
        let tab = self.detail.tab;
        self.leave_step_when_wheel_moves_selection(&id, tab, column, row);
        self.detail.view_mut(&id).on_wheel(tab, by, column, row);
    }

    /// The keys on a step's conversation read the selected step, so a notch
    /// over the axis takes them back to it, as `Esc` does.
    fn leave_step_when_wheel_moves_selection(
        &mut self,
        id: &str,
        tab: DetailTab,
        column: u16,
        row: u16,
    ) {
        let over_axis = tab == DetailTab::Progress
            && matches!(
                self.detail.view_mut(id).progress.spot_at(column, row),
                Some(Spot::Axis(_))
            );
        if over_axis {
            self.back_to_axis();
        }
    }

    fn click_details(&mut self, column: u16, row: u16) {
        if !matches!(self.view, View::Tasks) {
            return;
        }
        let Some(id) = self.selected_task_id() else {
            return;
        };
        let tab = self.detail.tab;
        if tab == DetailTab::Progress {
            self.click_progress(&id, column, row);
        } else {
            self.detail.view_mut(&id).on_click(tab, column, row);
        }
    }

    /// On the axis: the keys go to the axis and the step under the pointer is
    /// selected, as `Esc` and then `j` / `k` would. On the selected step: the
    /// keys go into its conversation, as `Enter` takes them, and the entry
    /// under the pointer gets the cursor; a step with no conversation yet
    /// keeps them on the axis.
    fn click_progress(&mut self, id: &str, column: u16, row: u16) {
        match self.detail.view_mut(id).progress.spot_at(column, row) {
            Some(Spot::Axis(index)) => {
                self.back_to_axis();
                if let Some(index) = index {
                    self.detail.view_mut(id).progress.select(index);
                }
            }
            Some(Spot::Step) => {
                self.enter_step();
                if self.step_focus() == StepFocus::Conversation {
                    self.detail
                        .view_mut(id)
                        .progress
                        .click_step_entry(column, row);
                }
            }
            None => {}
        }
    }

    /// Stops at the ends, as `j` and `k` do.
    fn step_list(&mut self, by: isize) {
        if let View::History(history) = &mut self.view {
            history.step(by);
        } else {
            self.change_list(|list| list.step(by));
        }
    }

    /// A click on a rule or below the last item selects nothing.
    fn click_list(&mut self, column: u16, row: u16) {
        if let View::History(history) = &mut self.view {
            if let Some(index) = history.index_at(column, row) {
                history.select(index);
            }
            return;
        }
        if let Some(id) = self.list.id_at(column, row).map(str::to_string) {
            self.change_list(|list| list.select(&id));
        }
    }

    /// From the state, not from what was drawn over what.
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
            Panel::Details | Panel::Content if history => self.focus = Focus::Details,
            Panel::Details | Panel::Content => {
                self.focus = Focus::Details;
                self.panel_focus = false;
            }
            Panel::Overlay | Panel::Answer => {}
        }
    }
}
