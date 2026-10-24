//! The layout of a form of inputs: a strip with the entry's name and the
//! fields in one column in the order they are declared. Every field is drawn
//! as in the other forms: its header, its value under it, its complaint under
//! the value. The main multi-line field fills the rest of the screen; the
//! other multi-line fields fold to what they hold.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{Chooser, ChooserField, ChooserFocus, ChooserItem};
use crate::tui::form::field::{clip, fill_padding, header_line, value_lines, value_rows};
use crate::tui::form::layout::{text_width, window_start, wrap};
use crate::tui::scroll::{one_line, sanitize};
use crate::tui::style::{Role, Styles};

/// Rows a folded multi-line field shows of its text, at most.
const FOLDED_ROWS: usize = 3;

/// The main field needs at least this many rows of text, or the others fold
/// to their headers and the column scrolls.
const MIN_MAIN_ROWS: usize = 4;

/// The blank twin of the prompt, on the rows of a field after the first.
const CONTINUATION: &str = "  ";

/// What the area says when there is no multi-line field.
const NO_TEXT_FIELD: &str = "brak pola tekstowego w tym workflow";

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    chooser: &Chooser,
    styles: &Styles,
    owner_focused: bool,
    footer: bool,
) {
    let Some(item) = chooser.selected_item() else {
        return;
    };
    // The description wraps in its frame, whole, up to a third of the area.
    let summary = strip_summary(item);
    let text_width = usize::from(area.width).saturating_sub(2);
    let mut summary_rows = wrap(&summary, None, text_width).rows;
    let cap = (usize::from(area.height) / 3).max(1);
    if summary_rows.len() > cap {
        summary_rows.truncate(cap);
        if let Some(last) = summary_rows.last_mut() {
            *last = clip(&format!("{last}…"), text_width.max(1));
        }
    }
    let frame_rows: u16 = if footer { 1 } else { 2 };
    let strip_rows = frame_rows + summary_rows.len() as u16;
    let [strip, rest] = Layout::vertical([
        Constraint::Length(strip_rows.min(area.height)),
        Constraint::Min(0),
    ])
    .areas(area);
    render_strip(frame, strip, item, &summary_rows, styles, footer);
    let focused = (owner_focused && chooser.focus == ChooserFocus::Fields).then_some(chooser.field);
    render_column(frame, rest, chooser, item, focused, styles);
}

/// The first paragraph of the entry's description, on one line.
fn strip_summary(item: &ChooserItem) -> String {
    item.description
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| one_line(&sanitize(line)))
        .unwrap_or_default()
}

fn render_strip(
    frame: &mut Frame,
    area: Rect,
    item: &ChooserItem,
    summary: &[String],
    styles: &Styles,
    footer: bool,
) {
    if footer {
        let mut lines = vec![Line::styled(
            format!("  {}", one_line(&item.label)),
            styles.strong(Role::Calm),
        )];
        lines.extend(summary.iter().map(|row| Line::raw(format!("  {row}"))));
        frame.render_widget(Paragraph::new(lines), area);
    } else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(Styles::FRAME)
            .border_style(styles.of(Role::Done))
            .title(format!(" {} ", one_line(&item.label)));
        let lines: Vec<Line> = summary.iter().map(|row| Line::raw(row.clone())).collect();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

/// The fields in one column, in the order of focus. The main field takes the
/// room the others leave; when the column is taller than the area it scrolls
/// to keep the focus in view.
fn render_column(
    frame: &mut Frame,
    area: Rect,
    chooser: &Chooser,
    item: &ChooserItem,
    focused: Option<usize>,
    styles: &Styles,
) {
    let order = 0..item.fields.len();
    let has_long = item.fields.iter().any(ChooserField::is_long);
    let main = focused
        .filter(|index| item.fields[*index].is_long())
        .unwrap_or_else(|| chooser.main_field());
    let main = has_long.then_some(main);
    let width = usize::from(area.width);
    let folded_width = text_width(width);
    let complaint_rows = |index: usize| usize::from(item.fields[index].error.is_some());
    let header_rows = |index: usize| 1 + complaint_rows(index);
    let rows_of = |index: usize, with_text: bool| {
        let field = &item.fields[index];
        if !field.is_long() {
            header_rows(index) + value_rows(field, width, focused == Some(index))
        } else if with_text {
            header_rows(index) + folded_rows(field, folded_width).len()
        } else {
            header_rows(index)
        }
    };
    let others = |with_text: bool| -> usize {
        order
            .clone()
            .filter(|&index| Some(index) != main)
            .map(|index| rows_of(index, with_text))
            .sum()
    };
    let height = usize::from(area.height);
    let main_rows = |with_text: bool| {
        main.map_or(0, |main| {
            height
                .saturating_sub(others(with_text))
                .saturating_sub(header_rows(main))
        })
    };
    let with_text = main_rows(true) >= MIN_MAIN_ROWS;
    // A low area scrolls the column rather than squeeze the main field.
    let main_height = main_rows(with_text).max(MIN_MAIN_ROWS);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focus_line = 0;
    for index in order {
        let field = &item.fields[index];
        let is_focused = focused == Some(index);
        lines.push(header_line(field, width, is_focused, styles));
        let value_start = lines.len();
        let mut cursor_row = 0;
        if !field.is_long() {
            let (rows, cursor) = value_lines(field, width, is_focused, styles);
            lines.extend(rows);
            cursor_row = cursor;
        } else if Some(index) == main {
            let (mut rows, cursor) = main_body(field, width, is_focused, main_height, styles);
            // The main field keeps its room, and the room is all of its fill.
            let blank = blank_row(width, is_focused, styles);
            rows.resize(main_height, blank);
            lines.extend(rows);
            cursor_row = cursor;
        } else if with_text {
            lines.extend(prefixed(folded_rows(field, folded_width), styles));
        }
        if is_focused {
            focus_line = value_start + cursor_row;
        }
        if let Some(error) = &field.error {
            lines.push(Line::styled(
                format!("✗ {}", one_line(error)),
                styles.of(Role::Problem),
            ));
        }
    }
    if !has_long {
        lines.push(Line::styled(NO_TEXT_FIELD, styles.of(Role::Done)));
    }
    let offset = (focus_line + 1).saturating_sub(height);
    frame.render_widget(
        Paragraph::new(lines).scroll((offset.min(usize::from(u16::MAX)) as u16, 0)),
        area,
    );
}

/// A row of the main field with no text: the fill, when the field is edited.
fn blank_row(width: usize, focused: bool, styles: &Styles) -> Line<'static> {
    if !focused {
        return Line::raw("");
    }
    Line::from(vec![
        Span::raw(CONTINUATION),
        fill_padding(0, width, styles.field_editing()),
    ])
}

fn prefixed(rows: Vec<String>, styles: &Styles) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(row, text)| {
            let prefix = if row == 0 { "> " } else { "  " };
            Line::from(vec![
                Span::styled(prefix, styles.of(Role::Done)),
                Span::raw(text),
            ])
        })
        .collect()
}

/// A folded field: its first lines, `…` when there is more or nothing.
fn folded_rows(field: &ChooserField, width: usize) -> Vec<String> {
    let value = sanitize(field.value());
    if value.trim().is_empty() {
        return vec!["…".to_string()];
    }
    let lines: Vec<&str> = value.split('\n').collect();
    let mut rows: Vec<String> = lines
        .iter()
        .take(FOLDED_ROWS)
        .map(|line| clip(line, width))
        .collect();
    if lines.len() > FOLDED_ROWS {
        if let Some(last) = rows.last_mut() {
            *last = clip(&format!("{last}…"), width.max(1));
        }
    }
    rows
}

/// The rows of the main field, wrapped to `width` and scrolled so that the
/// cursor is in view within `height` rows, and the row the cursor is on in them.
fn main_body(
    field: &ChooserField,
    width: usize,
    focused: bool,
    height: usize,
    styles: &Styles,
) -> (Vec<Line<'static>>, usize) {
    let (rows, cursor_row) = value_lines(field, width, focused, styles);
    let first = if focused {
        window_start(cursor_row, height, field.window_row().get())
    } else {
        0
    };
    field.window_row().set(first);
    let shown = rows.into_iter().skip(first).take(height).collect();
    (shown, cursor_row.saturating_sub(first))
}
