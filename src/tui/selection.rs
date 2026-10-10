//! A list with breathing room and a bracket around the selected item (R7–R14).
//!
//! Lists in the interface — the tasks, the history, the step axis, the
//! choosers — do not mark the selection with reverse video, which swallows the
//! colours of the row (R13). They put one blank row between items, above the
//! first and below the last (R10), and bracket the selected item: a horizontal
//! rule in the blank row above it, another in the blank row below it, and a
//! vertical bar beside each of its lines (R8). The rules start where the
//! content starts, two columns in, so they never touch the bar — there are no
//! corners.
//!
//! ```text
//!                               <- gap
//!   ! ✗  Fix login              <- an item
//!        #3 · claude-code
//!                               <- gap
//!     ━━━━━━━━━━━━━━━━━━━━━━━━  <- the rule above
//! ┃ ! ●  Refactor parser        <- the bar beside every line
//! ┃      w toku · implement
//!     ━━━━━━━━━━━━━━━━━━━━━━━━  <- the rule below
//!   ? ◆  Add docs
//! ```
//!
//! The marks are thick and in the focus role where the keys are, thin and in
//! the background role elsewhere ([`Styles::selection_marks`]). They do not
//! grey out when the data is stale: they say where the keys go, and the grey
//! content already says the connection is gone.
//!
//! Transcripts and *Zdarzenia* share only the left bar ([`bar_prefix`]),
//! without rules or gaps (R15, R16).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::style::Styles;

/// How many columns the bar takes on the left of every line: the bar and a
/// space. Callers lay their lines out in the area's width minus this.
pub const PREFIX_WIDTH: u16 = 2;

/// The rows a spaced list of `count` one-line items takes whole: each item
/// with the gap above it, and the gap below the last. One item takes three,
/// the least that shows a selection with its bracket.
pub const fn spaced_list_rows(count: usize) -> u16 {
    if count >= (u16::MAX / 2) as usize {
        u16::MAX
    } else {
        count as u16 * 2 + 1
    }
}

/// The two columns on the left of a line: `"┃ "` or `"│ "` when the line
/// belongs to the selected element (thick when `focused`), two spaces when it
/// does not. Shared by the bracketed lists and by the transcripts, which mark
/// their selected entry with the bar alone.
pub fn bar_prefix(styles: &Styles, selected: bool, focused: bool) -> Span<'static> {
    if !selected {
        return Span::raw("  ");
    }
    let marks = styles.selection_marks(focused);
    Span::styled(format!("{} ", marks.bar), marks.style)
}

/// Draws `items` into `area` as a spaced list, bracketing `selected`.
///
/// * `items` — each item's lines, laid out by the caller in
///   `area.width - PREFIX_WIDTH` columns and styled as the content wants to
///   look; the list adds no style of its own to them (R13). Items may differ
///   in height.
/// * `selected` — the index of the bracketed item, or `None` for a list
///   without a selection; an index past the end counts as `None`.
/// * `focused` — whether the keys are on this list: thick marks in the focus
///   role, thin ones in the background role otherwise (R12).
/// * `top` — the index of the first item in view, kept by the caller between
///   frames so a refresh never moves the list. It is moved only as far as it
///   takes to show the selected item with both of its rules (R11), and back
///   when the list got shorter and the end of it fits.
///
/// An area lower than the selected item and its two rules shows that item
/// alone, with the bar and without the rules. Any area, however small, is
/// safe.
pub fn render_spaced_list(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    items: &[Vec<Line<'_>>],
    selected: Option<usize>,
    focused: bool,
    top: &mut usize,
) {
    let selected = selected.filter(|&index| index < items.len());
    let height = usize::from(area.height);
    let heights: Vec<usize> = items.iter().map(Vec::len).collect();
    keep_in_view(top, selected, &heights, height);
    if area.width == 0 || area.height == 0 || items.is_empty() {
        return;
    }
    let marks = styles.selection_marks(focused);

    if let Some(index) = selected {
        if heights[index] + 2 > height {
            for (row, line) in items[index].iter().take(height).enumerate() {
                draw_line(frame, area, row, styles, line, true, focused);
            }
            return;
        }
    }

    let mut row = 0;
    for (index, item) in items.iter().enumerate().skip(*top) {
        if row >= height {
            break;
        }
        let is_selected = selected == Some(index);
        if is_selected {
            draw_rule(frame, area, row, marks.rule, marks.style);
        }
        row += 1;
        for line in item {
            if row >= height {
                break;
            }
            draw_line(frame, area, row, styles, line, is_selected, focused);
            row += 1;
        }
        if is_selected && row < height {
            draw_rule(frame, area, row, marks.rule, marks.style);
        }
    }
}

/// Moves `top` as little as it takes for the selected item and the gaps
/// around it to be in a view `height` rows high.
fn keep_in_view(top: &mut usize, selected: Option<usize>, heights: &[usize], height: usize) {
    *top = (*top).min(heights.len().saturating_sub(1));
    if let Some(selected) = selected {
        if selected < *top {
            *top = selected;
        }
        while *top < selected && rows_through(heights, *top, selected) > height {
            *top += 1;
        }
    }
    while *top > 0 && rows_through(heights, *top - 1, heights.len() - 1) <= height {
        *top -= 1;
    }
}

/// The rows from the gap above item `first` to the gap below item `last`.
fn rows_through(heights: &[usize], first: usize, last: usize) -> usize {
    heights[first..=last]
        .iter()
        .map(|lines| lines + 1)
        .sum::<usize>()
        + 1
}

/// The horizontal rule in row `row` of `area`, from the content column to the
/// right edge.
fn draw_rule(frame: &mut Frame, area: Rect, row: usize, rule: &str, style: ratatui::style::Style) {
    let width = area.width.saturating_sub(PREFIX_WIDTH);
    if width == 0 {
        return;
    }
    let rect = Rect::new(area.x + PREFIX_WIDTH, area.y + row as u16, width, 1);
    frame.render_widget(Line::styled(rule.repeat(usize::from(width)), style), rect);
}

/// One line of an item in row `row` of `area`: the prefix, then the content.
fn draw_line(
    frame: &mut Frame,
    area: Rect,
    row: usize,
    styles: &Styles,
    line: &Line<'_>,
    selected: bool,
    focused: bool,
) {
    let y = area.y + row as u16;
    let prefix_width = area.width.min(PREFIX_WIDTH);
    frame.render_widget(
        Line::from(bar_prefix(styles, selected, focused)),
        Rect::new(area.x, y, prefix_width, 1),
    );
    let content_width = area.width - prefix_width;
    if content_width > 0 {
        frame.render_widget(
            line.clone(),
            Rect::new(area.x + prefix_width, y, content_width, 1),
        );
    }
}
