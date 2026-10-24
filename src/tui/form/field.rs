//! Drawing the value of a field. The cursor is a style of the cell it stands
//! on, never a character in the text, so what follows it does not move.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::layout::{cell_width, text_width, wrap, Wrapped};
use crate::tui::scroll::one_line;
use crate::tui::chooser::{ChooserField, FieldKind};
use crate::tui::scroll::sanitize;
use crate::tui::style::{Role, Styles};

/// The mark that opens a field's text, and its blank twin on the rows after.
const PROMPT: &str = "> ";
const CONTINUATION: &str = "  ";

/// `text` as spans with the cell at `cursor` reversed. At the end of the text
/// the cursor is one reversed space after it.
pub fn cursor_spans(
    text: &str,
    cursor: usize,
    base: Style,
    cursor_style: Style,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let under: String = chars.get(cursor).map_or(" ".to_string(), |c| c.to_string());
    let after: String = chars.get(cursor + 1..).unwrap_or_default().iter().collect();
    let mut spans = Vec::new();
    if !before.is_empty() {
        spans.push(Span::styled(before, base));
    }
    spans.push(Span::styled(under, base.patch(cursor_style)));
    if !after.is_empty() {
        spans.push(Span::styled(after, base));
    }
    spans
}

/// The spaces that carry `fill` from the end of a text of `used` cells to the
/// last cell of the text area. `Paragraph` paints only the cells that hold
/// text, so the fill of an empty cell must be a space with that style.
pub fn fill_padding(used: usize, width: usize, fill: Style) -> Span<'static> {
    Span::styled(" ".repeat(text_width(width).saturating_sub(used)), fill)
}

/// How many cells `text` takes.
fn cells(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

/// How a field is laid out for `width` columns, the cursor included when
/// `focused`. Remembers the width, so that `↑`/`↓` walk what is drawn.
pub fn layout_of(field: &ChooserField, width: usize, focused: bool) -> Wrapped {
    let text = text_width(width);
    field.wrap_width().set(text);
    wrap(field.value(), focused.then(|| field.cursor()), text)
}

/// How a field is laid out at the width it was last drawn at, the cursor in
/// it. Used by `↑`/`↓`, which have no area of their own.
pub fn layout_of_current(field: &ChooserField) -> Wrapped {
    wrap(
        field.value(),
        Some(field.cursor()),
        field.wrap_width().get(),
    )
}

/// The lines of a field's value, and the line the cursor is on.
///
/// A field being edited has the editing fill under all of its lines and a
/// reversed cell for the cursor; `select` and `integer` use the same indent
/// and fill as text.
pub fn value_lines(
    field: &ChooserField,
    width: usize,
    focused: bool,
    styles: &Styles,
) -> (Vec<Line<'static>>, usize) {
    let fill = if focused {
        styles.field_editing()
    } else {
        Style::default()
    };
    let prompt_role = if focused { Role::Focus } else { Role::Done };
    if matches!(field.kind, FieldKind::Select(_)) {
        let value = sanitize(field.value());
        let shown = format!("‹ {value} ›");
        let mut spans = vec![
            Span::styled(PROMPT, styles.of(prompt_role)),
            Span::styled(shown.clone(), fill),
        ];
        if focused {
            spans.push(fill_padding(cells(&shown), width, fill));
        }
        return (vec![Line::from(spans)], 0);
    }
    let wrapped = layout_of(field, width, focused);
    let lines = wrapped
        .rows
        .iter()
        .enumerate()
        .map(|(row, text)| {
            let prefix = if row == 0 { PROMPT } else { CONTINUATION };
            let mut spans = vec![Span::styled(prefix, styles.of(prompt_role))];
            if focused && row == wrapped.cursor_row {
                spans.extend(cursor_spans(
                    text,
                    wrapped.cursor_col,
                    fill,
                    styles.cursor(),
                ));
            } else {
                spans.push(Span::styled(text.clone(), fill));
            }
            if focused {
                let used = if row == wrapped.cursor_row {
                    cells(text).max(wrapped.cursor_col + 1)
                } else {
                    cells(text)
                };
                spans.push(fill_padding(used, width, fill));
            }
            Line::from(spans)
        })
        .collect();
    (lines, wrapped.cursor_row)
}

/// How many rows the value of a field takes in `width` columns.
pub fn value_rows(field: &ChooserField, width: usize, focused: bool) -> usize {
    if matches!(field.kind, FieldKind::Select(_)) {
        return 1;
    }
    let text = text_width(width);
    wrap(field.value(), focused.then(|| field.cursor()), text)
        .rows
        .len()
        .max(1)
}

/// The row above a field's value: its label, `(wymagane)`, `◆` and ` · description`.
/// The label and the marks always show; the description takes the rest of
/// `width` and ends in `…` where it is cut.
pub fn header_line(
    field: &ChooserField,
    width: usize,
    focused: bool,
    styles: &Styles,
) -> Line<'static> {
    let label_style = if focused {
        styles.of(Role::Focus)
    } else {
        Style::default()
    };
    let mut spans = vec![Span::styled(one_line(field.label()), label_style)];
    if field.required {
        spans.push(Span::styled(" (wymagane)", styles.of(Role::Done)));
    }
    if field.is_proposed() {
        spans.push(Span::styled(" ◆", styles.of(Role::Conductor)));
    }
    if let Some(description) = &field.description {
        let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
        let text = format!(" · {}", one_line(description));
        spans.push(Span::styled(
            clip(&text, width.saturating_sub(used)),
            styles.of(Role::Done),
        ));
    }
    Line::from(spans)
}

/// `text` cut to `width` columns, with `…` where it was cut.
pub fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}
