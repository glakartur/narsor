//! The layout of a form of inputs: a strip with the entry's name, a status
//! line, and the fields in one column in the order they are declared. The
//! main multi-line field fills the rest of the screen; the other multi-line
//! fields fold to what they hold.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{Chooser, ChooserField, ChooserFocus, ChooserItem, FieldKind};
use crate::tui::form::field::{cursor_spans, fill_padding, value_lines};
use crate::tui::form::layout::{window_start, wrap, PROMPT_WIDTH};
use crate::tui::scroll::{one_line, sanitize};
use crate::tui::style::{Role, Styles};

/// Rows a folded multi-line field shows of its text, at most.
const FOLDED_ROWS: usize = 3;

/// The main field needs at least this many rows of text, or the others fold
/// to their headers.
const MIN_MAIN_ROWS: usize = 4;

/// The rows above the fields besides the strip: the status line.
const STATUS_ROWS: u16 = 1;

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
    let top = (strip_rows + STATUS_ROWS).min(area.height);
    let [head, rest] = Layout::vertical([Constraint::Length(top), Constraint::Min(0)]).areas(area);
    let [strip, status] = Layout::vertical([
        Constraint::Length(strip_rows.min(head.height)),
        Constraint::Length(1.min(head.height.saturating_sub(strip_rows))),
    ])
    .areas(head);
    render_strip(frame, strip, item, &summary_rows, styles, footer);
    let focused = (owner_focused && chooser.focus == ChooserFocus::Fields).then_some(chooser.field);
    render_status(frame, status, item, focused, styles);
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

/// A short field, one line: `name: value`. The one being edited has the
/// editing fill to the last cell of the row.
fn short_line(field: &ChooserField, width: usize, focused: bool, styles: &Styles) -> Line<'static> {
    let name = format!("{}: ", one_line(&field.name));
    let (value, cursor) = if matches!(field.kind, FieldKind::Select(_)) {
        (format!("‹ {} ›", one_line(&sanitize(field.value()))), 0)
    } else {
        let byte = crate::tui::form::buffer::char_byte_index(field.value(), field.cursor());
        let before = one_line(&sanitize(&field.value()[..byte])).chars().count();
        (one_line(&sanitize(field.value())), before)
    };
    if !focused {
        let style = if field.error.is_some() {
            styles.of(Role::Problem)
        } else {
            Style::default()
        };
        return Line::from(vec![Span::styled(
            clip(&format!("{name}{value}"), width),
            style,
        )]);
    }
    let fill = styles.field_editing();
    let base = styles.of(Role::Focus).patch(fill);
    let name_width = name.chars().count();
    // The value keeps the room the name leaves, with the cursor in view.
    let room = width.saturating_sub(name_width).max(1);
    let (shown, cursor) = window_around_cursor(&value, cursor, room);
    let mut spans = vec![Span::styled(name, base)];
    spans.extend(cursor_spans(&shown, cursor, base, styles.cursor()));
    let used = name_width + shown.chars().count().max(cursor + 1);
    spans.push(fill_padding(used, width + PROMPT_WIDTH, fill));
    Line::from(spans)
}

/// `text` cut to `width` columns, with `…` where it was cut.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// `text` cut to `width` columns so that the cell at `cursor` shows, and the
/// cursor's place in the cut.
fn window_around_cursor(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < width.max(1) {
        return (text.to_string(), cursor);
    }
    let width = width.max(1);
    let start = (cursor + 1).saturating_sub(width);
    let mut shown: Vec<char> =
        chars[start.min(chars.len())..(start + width).min(chars.len())].to_vec();
    if start > 0 && !shown.is_empty() {
        shown[0] = '…';
    }
    (shown.into_iter().collect(), cursor - start)
}

/// What the focused short field is, or the complaint about one.
fn render_status(
    frame: &mut Frame,
    area: Rect,
    item: &ChooserItem,
    focused: Option<usize>,
    styles: &Styles,
) {
    if area.height == 0 {
        return;
    }
    let complaint = item
        .fields
        .iter()
        .filter(|field| !field.is_long())
        .find_map(|field| field.error.as_ref());
    let line = if let Some(error) = complaint {
        Line::styled(format!("✗ {}", one_line(error)), styles.of(Role::Problem))
    } else if let Some(field) = focused
        .map(|index| &item.fields[index])
        .filter(|field| !field.is_long())
    {
        let mut spans = vec![Span::styled(one_line(&field.name), styles.of(Role::Focus))];
        if field.required {
            spans.push(Span::styled(" (wymagane)", styles.of(Role::Done)));
        }
        if let Some(description) = &field.description {
            spans.push(Span::styled(
                format!(" · {}", one_line(description)),
                styles.of(Role::Done),
            ));
        }
        Line::from(spans)
    } else {
        Line::raw("")
    };
    frame.render_widget(Paragraph::new(line), area);
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
    let order: Vec<usize> = (0..item.fields.len()).collect();
    let has_long = order.iter().any(|&index| item.fields[index].is_long());
    let main = focused
        .filter(|index| item.fields[*index].is_long())
        .unwrap_or_else(|| chooser.main_field());
    let main = has_long.then_some(main);
    let width = usize::from(area.width).saturating_sub(PROMPT_WIDTH);
    let header_rows = |index: usize| 1 + usize::from(item.fields[index].error.is_some());
    let rows_of = |index: usize, with_text: bool| {
        let field = &item.fields[index];
        if !field.is_long() {
            1
        } else if with_text {
            header_rows(index) + folded_rows(field, width).len()
        } else {
            header_rows(index)
        }
    };
    let others = |with_text: bool| -> usize {
        order
            .iter()
            .filter(|&&index| Some(index) != main)
            .map(|&index| rows_of(index, with_text))
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
    let main_height = main_rows(with_text).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focus_line = 0;
    for &index in &order {
        let field = &item.fields[index];
        let is_focused = focused == Some(index);
        if is_focused {
            focus_line = lines.len();
        }
        if !field.is_long() {
            lines.push(short_line(
                field,
                usize::from(area.width),
                is_focused,
                styles,
            ));
            continue;
        }
        lines.push(header_line(field, is_focused, styles));
        if let Some(error) = &field.error {
            lines.push(Line::styled(
                format!("✗ {}", one_line(error)),
                styles.of(Role::Problem),
            ));
        }
        if Some(index) == main {
            let (mut rows, cursor_row) = main_body(
                field,
                usize::from(area.width),
                is_focused,
                main_height,
                styles,
            );
            // The main field keeps its room, and the room is all of its fill.
            let blank = blank_row(usize::from(area.width), is_focused, styles);
            rows.resize(main_height, blank);
            if is_focused {
                focus_line = lines.len() + cursor_row;
            }
            lines.extend(rows);
        } else if with_text {
            lines.extend(prefixed(folded_rows(field, width), styles));
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

fn header_line(field: &ChooserField, focused: bool, styles: &Styles) -> Line<'static> {
    let mut header = vec![Span::styled(
        one_line(&field.name),
        if focused {
            styles.of(Role::Focus)
        } else {
            Style::default()
        },
    )];
    if field.required {
        header.push(Span::styled(" (wymagane)", styles.of(Role::Done)));
    }
    if field.proposed {
        header.push(Span::styled(" ◆", styles.of(Role::Conductor)));
    }
    if let Some(description) = &field.description {
        header.push(Span::styled(
            format!(" · {}", one_line(description)),
            styles.of(Role::Done),
        ));
    }
    Line::from(header)
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
