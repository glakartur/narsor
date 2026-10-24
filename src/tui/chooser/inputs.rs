//! The layout of a form of inputs: a strip with the entry's name, a bar of the
//! short fields, and the multi-line fields under them. The one with the focus
//! (the main one when the focus is in the bar) fills the screen; the others
//! fold to what they hold.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{Chooser, ChooserField, ChooserFocus, ChooserItem, FieldKind};
use crate::tui::form::field::{cursor_spans, value_lines};
use crate::tui::form::layout::{window_start, wrap, PROMPT_WIDTH};
use crate::tui::scroll::{one_line, sanitize};
use crate::tui::style::{Role, Styles};

/// Rows a folded multi-line field shows of its text, at most.
const FOLDED_ROWS: usize = 3;

/// The main field needs at least this many rows of text, or the others fold
/// to their headers.
const MIN_MAIN_ROWS: usize = 4;

/// The rows above the multi-line fields besides the strip: the bar and the
/// status line.
const BAR_ROWS: u16 = 2;

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
    let top = (strip_rows + BAR_ROWS).min(area.height);
    let [head, rest] = Layout::vertical([Constraint::Length(top), Constraint::Min(0)]).areas(area);
    let [strip, bar, status] = Layout::vertical([
        Constraint::Length(strip_rows.min(head.height)),
        Constraint::Length(1.min(head.height.saturating_sub(strip_rows))),
        Constraint::Length(1.min(head.height.saturating_sub(strip_rows + 1))),
    ])
    .areas(head);
    render_strip(frame, strip, item, &summary_rows, styles, footer);
    let focused = (owner_focused && chooser.focus == ChooserFocus::Fields).then_some(chooser.field);
    render_bar(frame, bar, item, focused, styles);
    render_status(frame, status, item, focused, styles);
    render_long_fields(frame, rest, chooser, item, focused, styles);
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

/// The short fields, one line: `name: value │ name: value`.
fn render_bar(
    frame: &mut Frame,
    area: Rect,
    item: &ChooserItem,
    focused: Option<usize>,
    styles: &Styles,
) {
    let short: Vec<usize> = (0..item.fields.len())
        .filter(|&index| !item.fields[index].is_long())
        .collect();
    if short.is_empty() || area.height == 0 {
        return;
    }
    let separator = " │ ";
    let room =
        usize::from(area.width).saturating_sub(separator.chars().count() * (short.len() - 1));
    let texts: Vec<String> = short
        .iter()
        .map(|&index| bar_text(&item.fields[index]).0)
        .collect();
    // The field being typed in keeps what it needs, up to two fifths of the bar.
    let focused_position = short.iter().position(|&index| focused == Some(index));
    let reserved = focused_position.map_or(0, |position| {
        (texts[position].chars().count() + 1).min((room * 2 / 5).max(12))
    });
    let others: Vec<String> = texts
        .iter()
        .enumerate()
        .filter(|(position, _)| Some(*position) != focused_position)
        .map(|(_, text)| text.clone())
        .collect();
    let cap = water_level(&others, room.saturating_sub(reserved));
    let mut spans = Vec::new();
    for (position, (&index, text)) in short.iter().zip(&texts).enumerate() {
        if position > 0 {
            spans.push(Span::styled(separator, styles.of(Role::Done)));
        }
        let field = &item.fields[index];
        if focused == Some(index) {
            let base = styles.of(Role::Focus).patch(styles.field_editing());
            let (text, cursor) = bar_text(field);
            let (shown, cursor) = window_around_cursor(&text, cursor, reserved);
            spans.extend(cursor_spans(&shown, cursor, base, styles.cursor()));
            let used = shown.chars().count().max(cursor + 1);
            spans.push(Span::styled(
                " ".repeat(reserved.saturating_sub(used)),
                styles.field_editing(),
            ));
        } else {
            let style = if field.error.is_some() {
                styles.of(Role::Problem)
            } else {
                Style::default()
            };
            spans.push(Span::styled(clip(text, cap), style));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `name: value`, and where the cursor falls in it.
fn bar_text(field: &ChooserField) -> (String, usize) {
    let name = format!("{}: ", one_line(&field.name));
    if matches!(field.kind, FieldKind::Select(_)) {
        let text = format!("{name}‹ {} ›", one_line(&sanitize(field.value())));
        return (text, 0);
    }
    let value = one_line(&sanitize(field.value()));
    let byte = crate::tui::form::buffer::char_byte_index(field.value(), field.cursor());
    let before = one_line(&sanitize(&field.value()[..byte])).chars().count();
    (format!("{name}{value}"), name.chars().count() + before)
}

/// The widest a segment may be so that all of them fit in `room`: short ones
/// keep what they need, the long ones share the rest.
fn water_level(texts: &[String], room: usize) -> usize {
    let needs: Vec<usize> = texts.iter().map(|text| text.chars().count()).collect();
    let widest = needs.iter().copied().max().unwrap_or(0);
    (0..=widest)
        .rev()
        .find(|&cap| needs.iter().map(|need| (*need).min(cap)).sum::<usize>() <= room)
        .unwrap_or(0)
        .max(8.min(widest))
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

fn render_long_fields(
    frame: &mut Frame,
    area: Rect,
    chooser: &Chooser,
    item: &ChooserItem,
    focused: Option<usize>,
    styles: &Styles,
) {
    let long: Vec<usize> = (0..item.fields.len())
        .filter(|&index| item.fields[index].is_long())
        .collect();
    if long.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(NO_TEXT_FIELD, styles.of(Role::Done))),
            area,
        );
        return;
    }
    let main = focused
        .filter(|index| item.fields[*index].is_long())
        .unwrap_or_else(|| chooser.main_field());
    let width = usize::from(area.width).saturating_sub(PROMPT_WIDTH);
    let header_rows = |index: usize| 1 + usize::from(item.fields[index].error.is_some());
    let folded = |index: usize, with_text: bool| {
        let text = if with_text {
            folded_rows(&item.fields[index], width).len()
        } else {
            0
        };
        header_rows(index) + text
    };
    let others = |with_text: bool| -> usize {
        long.iter()
            .filter(|&&index| index != main)
            .map(|&index| folded(index, with_text))
            .sum()
    };
    let height = usize::from(area.height);
    let mut with_text = true;
    let main_rows = |with_text: bool| {
        height
            .saturating_sub(others(with_text))
            .saturating_sub(header_rows(main))
    };
    if main_rows(true) < MIN_MAIN_ROWS {
        with_text = false;
    }
    let main_height = main_rows(with_text);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for &index in &long {
        let field = &item.fields[index];
        let is_focused = focused == Some(index);
        lines.push(header_line(field, is_focused, styles));
        if let Some(error) = &field.error {
            lines.push(Line::styled(
                format!("✗ {}", one_line(error)),
                styles.of(Role::Problem),
            ));
        }
        if index == main {
            let mut rows = main_body(
                field,
                usize::from(area.width),
                is_focused,
                main_height,
                styles,
            );
            // The main field keeps its room, so the folded ones stay below it.
            rows.resize(main_height, Line::raw(""));
            lines.extend(rows);
        } else if with_text {
            lines.extend(prefixed(folded_rows(field, width), styles));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
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
/// cursor is in view within `height` rows.
fn main_body(
    field: &ChooserField,
    width: usize,
    focused: bool,
    height: usize,
    styles: &Styles,
) -> Vec<Line<'static>> {
    let (rows, cursor_row) = value_lines(field, width, focused, styles);
    let first = if focused {
        window_start(cursor_row, height, field.window_row().get())
    } else {
        0
    };
    field.window_row().set(first);
    rows.into_iter().skip(first).take(height).collect()
}
