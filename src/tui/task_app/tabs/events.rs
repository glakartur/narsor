//! *Zdarzenia*: the raw event log of the task's execution, with the detail of
//! one event.
//!
//! Events come from `GET /control/v1/tasks/:id/execution-events` — the live
//! runner through the relay, or the task's own storage. The answer is one of
//! three things, and each has a place in the tab:
//!
//! * events — listed, a page at a time, continued after the last sequence;
//! * "unavailable" with a reason (not started, paused, runner unreachable) —
//!   a normal answer, shown as a note;
//! * a failed read (`503` with a reason, a refusal) — shown as a note too.
//!   Neither touches the state of the connection.
//!
//! `v` opens the detail of the selected event, `Ctrl+v` switches every row to
//! its raw JSON.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::controller_protocol::{
    ConversationUnavailableReason, ExecutionEventsResponse, ExecutionEventsSource,
};
use crate::domain::Event;
use crate::tui::event_text::{event_detail_lines, event_line};
use crate::tui::keymap::Action;
use crate::tui::scroll::sanitize;
use crate::tui::style::{Role, Styles};

use super::super::super::scroll::one_line;
use super::super::list::fit;

/// Events asked for at once.
pub const PAGE_LIMIT: u32 = 500;

/// Pages read in one go; a longer log is read on as the tab is refreshed.
pub const MAX_PAGES: usize = 20;

/// Where the events came from, or why there are none to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Nothing was read yet.
    NotRead,
    /// The events are readable, from here.
    Available(ExecutionEventsSource),
    /// The answer was "not now", with a reason.
    Unavailable {
        /// The machine-readable reason.
        reason: ConversationUnavailableReason,
        /// The same, for people.
        message: String,
    },
}

/// What the tab remembers per task.
#[derive(Debug, Clone)]
pub struct Events {
    events: Vec<Event>,
    has_more: bool,
    availability: Availability,
    failure: Option<String>,
    revision: Option<u64>,
    cursor: usize,
    top: usize,
    detail_open: bool,
    detail_scroll: usize,
    raw: bool,
    height: usize,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            has_more: false,
            availability: Availability::NotRead,
            failure: None,
            revision: None,
            cursor: 0,
            top: 0,
            detail_open: false,
            detail_scroll: 0,
            raw: false,
            height: 10,
        }
    }
}

impl Events {
    /// The events read, oldest first.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Whether the runner said more follow what was read.
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// What the last answer was.
    pub fn availability(&self) -> &Availability {
        &self.availability
    }

    /// Why the last read failed, until one succeeds.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The index of the selected event.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected event.
    pub fn selected(&self) -> Option<&Event> {
        self.events.get(self.cursor)
    }

    /// Whether the detail of the selected event is open.
    pub fn is_detail_open(&self) -> bool {
        self.detail_open
    }

    /// Whether every row shows its raw JSON.
    pub fn is_raw(&self) -> bool {
        self.raw
    }

    /// The sequence a read continues after.
    pub fn after(&self) -> u64 {
        self.events.last().map_or(0, |event| event.sequence)
    }

    /// Whether the log must be read for a task at `revision`.
    pub fn needs_read(&self, revision: u64) -> bool {
        self.revision != Some(revision)
    }

    /// Makes the next [`Self::needs_read`] true, keeping what is shown.
    pub fn invalidate(&mut self) {
        self.revision = None;
    }

    /// Takes one answer in. `after` is the sequence the read continued after;
    /// a read from the start replaces the log. A cursor on the last event stays
    /// on the last.
    pub fn apply(
        &mut self,
        revision: u64,
        after: u64,
        result: Result<ExecutionEventsResponse, String>,
    ) {
        self.revision = Some(revision);
        match result {
            Ok(ExecutionEventsResponse::Available {
                events,
                has_more,
                source,
            }) => {
                let at_end = self.events.is_empty() || self.cursor + 1 >= self.events.len();
                if after == 0 {
                    self.events.clear();
                }
                let last = self.after();
                self.events
                    .extend(events.into_iter().filter(|event| event.sequence > last));
                self.has_more = has_more;
                self.availability = Availability::Available(source);
                self.failure = None;
                if at_end {
                    self.cursor = self.events.len().saturating_sub(1);
                }
            }
            Ok(ExecutionEventsResponse::Unavailable { reason, message }) => {
                self.availability = Availability::Unavailable { reason, message };
                self.has_more = false;
                self.failure = None;
            }
            Err(message) => self.failure = Some(message),
        }
    }

    /// What a key does.
    pub fn on_action(&mut self, action: Action) {
        let last = self.events.len().saturating_sub(1);
        if self.detail_open && matches!(action, Action::PageDown | Action::PageUp) {
            let page = self.height.max(1);
            self.detail_scroll = if action == Action::PageDown {
                self.detail_scroll.saturating_add(page)
            } else {
                self.detail_scroll.saturating_sub(page)
            };
            return;
        }
        match action {
            Action::Down => self.cursor = (self.cursor + 1).min(last),
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::PageDown => self.cursor = (self.cursor + self.height.max(1)).min(last),
            Action::PageUp => self.cursor = self.cursor.saturating_sub(self.height.max(1)),
            Action::Top => self.cursor = 0,
            Action::Bottom | Action::Follow => self.cursor = last,
            Action::Expand | Action::RawEntry => {
                self.detail_open = !self.detail_open && !self.events.is_empty();
            }
            Action::RawAll => self.raw = !self.raw,
            _ => {}
        }
        if matches!(
            action,
            Action::Down
                | Action::Up
                | Action::PageDown
                | Action::PageUp
                | Action::Top
                | Action::Bottom
        ) {
            self.detail_scroll = 0;
        }
    }
}

/// One row of the log.
fn row_text(event: &Event, raw: bool) -> String {
    if raw {
        let json = serde_json::to_string(event).unwrap_or_default();
        return one_line(&json);
    }
    format!(
        "{:>6}  {}  {}",
        event.sequence,
        event.timestamp.format("%H:%M:%S"),
        one_line(&event_line(event))
    )
}

/// Draws the tab.
pub fn render(frame: &mut Frame, area: Rect, styles: &Styles, events: &mut Events, stale: bool) {
    let grey = |role: Role| if stale { Role::Done } else { role };
    let mut notes: Vec<Line<'static>> = Vec::new();
    if let Availability::Unavailable { message, .. } = &events.availability {
        notes.push(Line::styled(
            format!("◌ zdarzenia niedostępne: {}", one_line(message)),
            styles.of(Role::Attention),
        ));
    }
    if let Some(failure) = &events.failure {
        notes.push(Line::styled(
            format!("✗ nie udało się wczytać zdarzeń: {}", one_line(failure)),
            styles.of(Role::Problem),
        ));
    }
    if events.has_more {
        notes.push(Line::styled(
            "… są dalsze zdarzenia — dojdą przy kolejnym odświeżeniu",
            styles.of(Role::Done),
        ));
    }
    let note_rows = u16::try_from(notes.len()).unwrap_or(0).min(area.height);
    frame.render_widget(
        Paragraph::new(notes),
        Rect::new(area.x, area.y, area.width, note_rows),
    );
    let body = Rect::new(
        area.x,
        area.y + note_rows,
        area.width,
        area.height - note_rows,
    );
    if events.events.is_empty() {
        let text = match (&events.availability, &events.failure) {
            (Availability::NotRead, None) => "wczytuję zdarzenia…",
            (Availability::Available(_), _) => "brak zdarzeń",
            _ => "",
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            body,
        );
        return;
    }

    let detail_rows = if events.detail_open {
        body.height / 2
    } else {
        0
    };
    let list = Rect::new(body.x, body.y, body.width, body.height - detail_rows);
    let height = usize::from(list.height).max(1);
    events.height = height;
    if events.cursor < events.top {
        events.top = events.cursor;
    } else if events.cursor >= events.top + height {
        events.top = events.cursor + 1 - height;
    }
    let width = usize::from(list.width).saturating_sub(2);
    let rows: Vec<Line> = events
        .events
        .iter()
        .enumerate()
        .skip(events.top)
        .take(height)
        .map(|(index, event)| {
            let selected = index == events.cursor;
            let marker = Span::styled(
                if selected { "▸ " } else { "  " },
                styles.of(grey(Role::Focus)),
            );
            let text = fit(&row_text(event, events.raw), width);
            let line = Line::from(vec![
                marker,
                Span::styled(text, styles.of(grey(Role::Calm))),
            ]);
            if selected {
                line.style(styles.of(Role::Selected))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list);

    if detail_rows > 0 {
        let detail = Rect::new(body.x, list.y + list.height, body.width, detail_rows);
        let lines = events
            .selected()
            .map(event_detail_lines)
            .unwrap_or_default();
        let visible = usize::from(detail.height).saturating_sub(1).max(1);
        let scroll = events
            .detail_scroll
            .min(lines.len().saturating_sub(visible));
        events.detail_scroll = scroll;
        let mut shown = vec![Line::styled(
            format!(
                "─ szczegóły zdarzenia {} ─",
                events.selected().map_or(0, |e| e.sequence)
            ),
            styles.of(Role::Focus),
        )];
        shown.extend(lines.into_iter().skip(scroll).take(visible).map(|text| {
            Line::styled(
                crate::domain::display::without_unsafe_chars_multiline(&sanitize(&text)),
                styles.of(grey(Role::Calm)),
            )
        }));
        frame.render_widget(Paragraph::new(shown), detail);
    }
}
