//! *Stan*: every value the workflow sees now — the execution context, the
//! step locals and the conductor's state — as a flat list of `path: value`
//! rows ([`crate::state_rows`]).
//!
//! # What is read
//!
//! One read, `GET /control/v1/tasks/:id/execution-state`, relayed to the
//! task's runner. The tab reads when the operator opens it and when the
//! operator asks again (`Ctrl+R`); nothing else makes it read, not even a new
//! revision of the task. The one exception is "the reader is starting": the
//! tab asks again after [`READER_RETRY`] until it gets the state or another
//! answer. A read that runs keeps the rows and the time of the last one; a
//! failed read keeps them too and says why.
//!
//! # Rows
//!
//! A value that fits on one row has the neutral marker. A value of more than
//! one line, or a row wider than the tab, is folded to its first line, clipped
//! to the width, with the folded marker; `Enter` unfolds it to every line,
//! wrapped to the width, and folds it again. The markers are those of the
//! conversation view. What is unfolded is remembered by path, so it survives a
//! new read while the path is still there.
//!
//! # Cursor
//!
//! The cursor is on one row. The keys move it a row at a time, except inside
//! an unfolded row taller than the view: there a line key scrolls one line,
//! and the cursor moves on only once the last (or, going up, the first) line
//! of the row is in view.

use std::collections::BTreeSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::controller_protocol::{ConversationUnavailableReason, ExecutionStateResponse};
use crate::state_rows::{state_rows, StateRow};
use crate::tui::keymap::{key_of, Action, Context};
use crate::tui::scroll::{one_line, sanitize, ScrollState};
use crate::tui::selection::{bar_prefix, PREFIX_WIDTH};
use crate::tui::style::{Role, Styles};
use crate::tui::transcript::{FOLDED_MARKER, UNFOLDABLE_MARKER, UNFOLDED_MARKER};

use super::super::detail::{cells, fit_cells};
use super::READER_RETRY;

const DEFAULT_WIDTH: usize = 80;

const VALUE_INDENT: &str = "  ";

#[derive(Debug, Clone)]
pub struct State {
    rows: Vec<StateRow>,
    read_at: Option<DateTime<Utc>>,
    note: Option<String>,
    failure: Option<String>,
    starting: bool,
    wanted: bool,
    reading: bool,
    requested_while_reading: bool,
    answered_at: Option<Instant>,
    unfolded: BTreeSet<String>,
    cursor: usize,
    scroll: ScrollState,
    width: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLine {
    pub text: String,
    pub selected: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            read_at: None,
            note: None,
            failure: None,
            starting: false,
            wanted: true,
            reading: false,
            requested_while_reading: false,
            answered_at: None,
            unfolded: BTreeSet::new(),
            cursor: 0,
            scroll: ScrollState::new(),
            width: DEFAULT_WIDTH,
        }
    }

    pub fn rows(&self) -> &[StateRow] {
        &self.rows
    }

    pub fn read_at(&self) -> Option<DateTime<Utc>> {
        self.read_at
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn is_starting(&self) -> bool {
        self.starting
    }

    pub fn selected_path(&self) -> Option<&str> {
        self.rows.get(self.cursor).map(|row| row.path.as_str())
    }

    pub fn is_unfolded(&self, path: &str) -> bool {
        self.rows
            .iter()
            .position(|row| row.path == path)
            .is_some_and(|index| self.open(index))
    }

    pub fn needs_read(&self, now: Instant) -> bool {
        if self.wanted {
            return true;
        }
        self.starting
            && self
                .answered_at
                .is_none_or(|at| now.saturating_duration_since(at) >= READER_RETRY)
    }

    pub fn request_read(&mut self) {
        self.wanted = true;
        self.requested_while_reading = self.reading;
    }

    pub fn begin_read(&mut self) {
        self.reading = true;
    }

    pub fn apply(&mut self, now: Instant, result: Result<ExecutionStateResponse, String>) {
        self.wanted = std::mem::take(&mut self.requested_while_reading);
        self.reading = false;
        self.answered_at = Some(now);
        self.starting = false;
        match result {
            Ok(ExecutionStateResponse::Available { state, read_at, .. }) => {
                self.replace_rows(state_rows(&state));
                self.read_at = Some(read_at);
                self.note = None;
                self.failure = None;
            }
            Ok(ExecutionStateResponse::Unavailable {
                reason: ConversationUnavailableReason::ReaderStarting,
                ..
            }) => {
                self.starting = true;
                self.failure = None;
            }
            Ok(ExecutionStateResponse::Unavailable { message, .. }) => {
                self.replace_rows(Vec::new());
                self.read_at = None;
                self.note = Some(message);
                self.failure = None;
            }
            Err(reason) => self.failure = Some(reason),
        }
    }

    fn replace_rows(&mut self, rows: Vec<StateRow>) {
        let rows: Vec<StateRow> = rows
            .into_iter()
            .map(|row| {
                let value = sanitize(&row.value);
                StateRow {
                    path: one_line(&row.path),
                    lines: value.split('\n').count(),
                    value,
                }
            })
            .collect();
        let position = |path: &str| rows.iter().position(|row| row.path == path);
        let cursor = if self.rows.is_empty() {
            0
        } else {
            self.rows
                .iter()
                .skip(self.cursor)
                .find_map(|row| position(&row.path))
                .unwrap_or(rows.len().saturating_sub(1))
        };
        self.unfolded
            .retain(|path| rows.iter().any(|row| row.path == *path));
        self.rows = rows;
        self.cursor = cursor;
        self.keep_cursor_in_view();
    }

    pub fn set_view(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.scroll.set_viewport(height);
        self.keep_cursor_in_view();
    }

    pub fn on_action(&mut self, action: Action) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let spans = self.spans();
        let total = spans[last].1;
        let (start, end) = spans[self.cursor];
        let (offset, viewport) = (self.scroll.offset(), self.scroll.viewport());
        let below = end > offset + viewport;
        let above = start < offset;
        match action {
            Action::Down if below => self.scroll.set_offset(offset + 1, total),
            Action::Down if self.cursor < last => self.select(self.cursor + 1),
            Action::Up if above => self.scroll.set_offset(offset - 1, total),
            Action::Up if self.cursor > 0 => self.select(self.cursor - 1),
            Action::PageDown if below => self
                .scroll
                .set_offset((offset + viewport).min(end - viewport), total),
            Action::PageDown => {
                let target = row_at(&spans, (start + viewport).min(total - 1));
                self.select(target.max(self.cursor + 1).min(last));
            }
            Action::PageUp if above => self
                .scroll
                .set_offset(offset.saturating_sub(viewport).max(start), total),
            Action::PageUp => {
                let target = row_at(&spans, start.saturating_sub(viewport));
                self.select(target.min(self.cursor.saturating_sub(1)));
            }
            Action::Top => {
                self.cursor = 0;
                self.scroll.to_top();
            }
            Action::Bottom => {
                self.cursor = last;
                self.scroll.to_bottom(total);
            }
            Action::Expand => {
                self.toggle_fold();
                self.keep_cursor_in_view();
            }
            _ => {}
        }
    }

    fn select(&mut self, index: usize) {
        self.cursor = index;
        let spans = self.spans();
        let total = spans.last().map_or(0, |span| span.1);
        let (start, end) = spans[index];
        let (offset, viewport) = (self.scroll.offset(), self.scroll.viewport());
        if start < offset {
            self.scroll
                .set_offset(start.max(end.saturating_sub(viewport)), total);
        } else if end > offset + viewport {
            self.scroll
                .set_offset(start.min(end.saturating_sub(viewport)), total);
        }
    }

    fn toggle_fold(&mut self) {
        if !self.foldable(&self.rows[self.cursor]) {
            return;
        }
        let path = self.rows[self.cursor].path.clone();
        if !self.unfolded.remove(&path) {
            self.unfolded.insert(path);
        }
    }

    fn foldable(&self, row: &StateRow) -> bool {
        row.lines > 1
            || cells(&format!("{UNFOLDABLE_MARKER} {}: {}", row.path, row.value)) > self.width
    }

    fn open(&self, index: usize) -> bool {
        let row = &self.rows[index];
        self.unfolded.contains(&row.path) && self.foldable(row)
    }

    fn value_lines(&self, row: &StateRow) -> Vec<String> {
        let width = self.width.saturating_sub(VALUE_INDENT.len()).max(1);
        row.value
            .split('\n')
            .flat_map(|line| wrap_cells(line, width))
            .map(|chunk| format!("{VALUE_INDENT}{chunk}"))
            .collect()
    }

    fn row_lines(&self, index: usize) -> Vec<String> {
        let row = &self.rows[index];
        if self.open(index) {
            let mut lines = wrap_cells(&format!("{UNFOLDED_MARKER} {}:", row.path), self.width);
            lines.extend(self.value_lines(row));
            lines
        } else if self.foldable(row) {
            let first = row.value.split('\n').next().unwrap_or_default();
            vec![fit_cells(
                &format!("{FOLDED_MARKER} {}: {first}", row.path),
                self.width,
            )]
        } else {
            vec![format!("{UNFOLDABLE_MARKER} {}: {}", row.path, row.value)]
        }
    }

    fn row_height(&self, index: usize) -> usize {
        self.row_lines(index).len()
    }

    fn spans(&self) -> Vec<(usize, usize)> {
        let mut start = 0;
        (0..self.rows.len())
            .map(|index| {
                let end = start + self.row_height(index);
                let span = (start, end);
                start = end;
                span
            })
            .collect()
    }

    fn keep_cursor_in_view(&mut self) {
        let spans = self.spans();
        let total = spans.last().map_or(0, |span| span.1);
        self.scroll.clamp(total);
        let Some(&(start, end)) = spans.get(self.cursor) else {
            return;
        };
        let (offset, viewport) = (self.scroll.offset(), self.scroll.viewport());
        if end <= offset {
            self.scroll.set_offset(start, total);
        } else if start >= offset + viewport {
            self.scroll
                .set_offset(start.min(end.saturating_sub(viewport)), total);
        }
    }

    pub fn visible_lines(&self) -> Vec<StateLine> {
        let (offset, viewport) = (self.scroll.offset(), self.scroll.viewport());
        let mut shown = Vec::new();
        let mut start = 0;
        for index in 0..self.rows.len() {
            let height = self.row_height(index);
            let end = start + height;
            if end > offset && start < offset + viewport {
                let skip = offset.saturating_sub(start);
                let take = (offset + viewport).min(end) - start - skip;
                shown.extend(
                    self.row_lines(index)
                        .into_iter()
                        .skip(skip)
                        .take(take)
                        .map(|text| StateLine {
                            text,
                            selected: index == self.cursor,
                        }),
                );
            }
            if end >= offset + viewport {
                break;
            }
            start = end;
        }
        shown
    }
}

fn row_at(spans: &[(usize, usize)], line: usize) -> usize {
    spans
        .iter()
        .position(|&(start, end)| (start..end).contains(&line))
        .unwrap_or(spans.len().saturating_sub(1))
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    state: &mut State,
    stale: bool,
    focused: bool,
    glyph: &str,
) {
    let grey = |role: Role| if stale { Role::Done } else { role };
    let refresh = key_of(Context::Details, Action::RereadState)
        .map(|key| format!(" · {key} odśwież"))
        .unwrap_or_default();
    let mut top = Vec::new();
    let has_rows = !state.rows.is_empty();
    if has_rows {
        let mut spans = vec![Span::styled(
            format!(
                "stan z {}",
                state
                    .read_at
                    .map(|at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_default()
            ),
            styles.of(grey(Role::Calm)),
        )];
        if state.reading || state.starting {
            spans.push(Span::styled(
                format!(" · {glyph} odczytuję stan…"),
                styles.of(Role::Done),
            ));
        }
        spans.push(Span::styled(refresh, styles.of(Role::Done)));
        top.push(Line::from(spans));
        if let Some(reason) = &state.failure {
            top.push(Line::styled(
                format!(
                    "✗ nie udało się odświeżyć stanu ({}) — spróbuj ponownie",
                    one_line(reason)
                ),
                styles.of(grey(Role::Problem)),
            ));
        }
    } else if let Some(reason) = &state.failure {
        top.push(Line::from(vec![
            Span::styled(
                format!(
                    "✗ nie udało się odczytać stanu ({}) — spróbuj ponownie",
                    one_line(reason)
                ),
                styles.of(grey(Role::Problem)),
            ),
            Span::styled(refresh, styles.of(Role::Done)),
        ]));
    } else if state.starting {
        top.push(Line::styled(
            format!("{glyph} odczytuję stan… zadanie jest zatrzymane, czytelnik się uruchamia"),
            styles.of(Role::Attention),
        ));
    } else if let Some(note) = &state.note {
        top.push(Line::from(vec![
            Span::styled(
                format!("stan niedostępny: {}", one_line(note)),
                styles.of(grey(Role::Attention)),
            ),
            Span::styled(refresh, styles.of(Role::Done)),
        ]));
    } else {
        top.push(Line::styled(
            format!("{glyph} odczytuję stan…"),
            styles.of(Role::Done),
        ));
    }
    let top_rows = u16::try_from(top.len()).unwrap_or(0).min(area.height);
    frame.render_widget(
        Paragraph::new(top),
        Rect::new(area.x, area.y, area.width, top_rows),
    );
    if !has_rows {
        return;
    }
    let list = Rect::new(
        area.x,
        area.y + top_rows,
        area.width,
        area.height - top_rows,
    );
    state.set_view(
        usize::from(list.width.saturating_sub(PREFIX_WIDTH)),
        usize::from(list.height),
    );
    let lines: Vec<Line> = state
        .visible_lines()
        .into_iter()
        .map(|line| {
            Line::from(vec![
                bar_prefix(styles, line.selected, focused),
                Span::styled(line.text, styles.of(grey(Role::Calm))),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
}

/// Wraps `line` into chunks of at most `width` display cells.
fn wrap_cells(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = vec![String::new()];
    let mut used = 0;
    for ch in line.chars() {
        let w = cells(ch.encode_utf8(&mut [0; 4]));
        if used + w > width && used > 0 {
            chunks.push(String::new());
            used = 0;
        }
        chunks.last_mut().expect("one chunk").push(ch);
        used += w;
    }
    chunks
}
