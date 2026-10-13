//! A list with a bracket around the selected item (R7–R14).
//!
//! Lists in the interface — the tasks, the history, the step axis, the
//! choosers — do not mark the selection with reverse video, which swallows the
//! colours of the row (R13). Their items stack directly, one under another,
//! and the selected item is bracketed: a horizontal rule in a row of its own
//! directly above it, another directly below it, and a vertical bar beside
//! each of its lines (R8). The rules span the whole width of the list from
//! column 0; the bar sits in column 0 only on the item's own lines, so the
//! two never meet — there are no corners. Every line keeps a two-column
//! prefix (the bar and a space, or two spaces), so the content columns are
//! the same for every item.
//!
//! ```text
//!   ! ✗  Fix login              <- an item
//!        #3 · claude-code
//! ────────────────────────────  <- the rule above
//! │ ! ●  Refactor parser        <- the bar beside every line
//! │      w toku · implement
//! ────────────────────────────  <- the rule below
//!   ? ◆  Add docs
//! ```
//!
//! Only the selected item takes the two rule rows, so moving the selection
//! shifts its neighbours by a row; without a selection there are no rules.
//!
//! The marks are always thin: in the focus role where the keys are, in the
//! background role elsewhere ([`Styles::selection_marks`]). They do not grey
//! out when the data is stale: they say where the keys go, and the grey
//! content already says the connection is gone.
//!
//! Transcripts and *Zdarzenia* share only the left bar ([`bar_prefix`]),
//! without rules (R15, R16).

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::style::Styles;

/// How many columns the bar takes on the left of every line: the bar and a
/// space. Callers lay their lines out in the area's width minus this.
pub const PREFIX_WIDTH: u16 = 2;

/// The rows a bracketed list of `count` one-line items, one of them
/// selected, takes whole: a row per item and the two rules around the
/// selected one. One item takes three, the least that shows a selection with
/// its bracket; no items take none.
pub const fn bracketed_list_rows(count: usize) -> u16 {
    if count == 0 {
        0
    } else if count >= (u16::MAX - 2) as usize {
        u16::MAX
    } else {
        count as u16 + 2
    }
}

/// The two columns on the left of a line: `"│ "` when the line belongs to
/// the selected element (in the focus role when `focused`), two spaces when
/// it does not. Shared by the bracketed lists and by the transcripts, which mark
/// their selected entry with the bar alone.
pub fn bar_prefix(styles: &Styles, selected: bool, focused: bool) -> Span<'static> {
    if !selected {
        return Span::raw("  ");
    }
    let marks = styles.selection_marks(focused);
    Span::styled(format!("{} ", marks.bar), marks.style)
}

/// Draws `items` into `area` one under another, bracketing `selected`.
///
/// * `items` — each item's lines, laid out by the caller in
///   `area.width - PREFIX_WIDTH` columns and styled as the content wants to
///   look; the list adds no style of its own to them (R13). Items may differ
///   in height.
/// * `selected` — the index of the bracketed item, or `None` for a list
///   without a selection; an index past the end counts as `None`.
/// * `focused` — whether the keys are on this list: marks in the focus role,
///   in the background role otherwise (R12).
/// * `top` — the index of the first item in view, kept by the caller between
///   frames so a refresh never moves the list. It is moved only as far as it
///   takes to show the selected item with both of its rules (R11), and back
///   when the list got shorter and the end of it fits.
///
/// An area lower than the selected item and its two rules shows that item
/// alone, with the bar and without the rules. Any area, however small, is
/// safe.
pub fn render_bracketed_list(
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
            row += 1;
        }
        for line in item {
            if row >= height {
                break;
            }
            draw_line(frame, area, row, styles, line, is_selected, focused);
            row += 1;
        }
        if is_selected && row < height {
            draw_rule(frame, area, row, marks.rule, marks.style);
            row += 1;
        }
    }
}

/// Moves `top` as little as it takes for the selected item and the rules
/// around it to be in a view `height` rows high.
fn keep_in_view(top: &mut usize, selected: Option<usize>, heights: &[usize], height: usize) {
    *top = (*top).min(heights.len().saturating_sub(1));
    if let Some(selected) = selected {
        if selected < *top {
            *top = selected;
        }
        while *top < selected && rows_through(heights, *top, selected, Some(selected)) > height {
            *top += 1;
        }
    }
    while *top > 0 && rows_through(heights, *top - 1, heights.len() - 1, selected) <= height {
        *top -= 1;
    }
}

/// The rows items `first` through `last` take, with the two rules of the
/// selected item when it is among them.
fn rows_through(heights: &[usize], first: usize, last: usize, selected: Option<usize>) -> usize {
    let rules = match selected {
        Some(index) if (first..=last).contains(&index) => 2,
        _ => 0,
    };
    heights[first..=last].iter().sum::<usize>() + rules
}

/// The horizontal rule in row `row` of `area`, across its whole width.
fn draw_rule(frame: &mut Frame, area: Rect, row: usize, rule: &str, style: ratatui::style::Style) {
    let rect = Rect::new(area.x, area.y + row as u16, area.width, 1);
    frame.render_widget(
        Line::styled(rule.repeat(usize::from(area.width)), style),
        rect,
    );
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
    if selected {
        // A low contrast fill makes the selected row easier to track while
        // leaving each label's semantic foreground colour intact.
        let background = Style::default().bg(Color::Indexed(236));
        for x in area.x..area.x + area.width {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_style(background);
            }
        }
    }
}
