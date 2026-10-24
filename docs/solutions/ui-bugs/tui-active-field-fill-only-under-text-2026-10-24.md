---
title: "TUI: active form field background showed only under the typed text"
date: 2026-10-24
category: ui-bugs
module: tui
problem_type: ui_bug
component: tooling
symptoms:
  - "The active field background (field_editing) covers only the cells that hold text"
  - "An empty active field shows no background at all"
  - "A new line added with Ctrl+J has no background behind it"
root_cause: wrong_api
resolution_type: code_fix
severity: low
tags: [tui, ratatui, paragraph, line-style, field-editing, form, render-test]
---

# TUI: active form field background showed only under the typed text

## Problem
`value_lines` in `src/tui/form/field.rs` set the `field_editing` style with `Line::style(fill)`. The operator saw the background only under the typed text. The field looked like highlighted text, not like a field (#24).

## Symptoms
- The empty part of a line after the text has the standard background.
- An empty active field has no visible background.
- The short-field bar in `src/tui/chooser/inputs.rs` has the same gap.

## What Didn't Work
- Style on `Line` (`Line::from(spans).style(fill)`). `Paragraph` paints only the cells that hold text, so a cell with no text keeps the standard background.
- Plain `Span::raw` for the text. It carries no style of its own, so the text cells depend on the line style only.

## Solution
Put `fill` on every text span. Then add one padding span of spaces with `fill` from the end of the text to the last cell of the text area. Add it only when the field is focused.

```rust
/// The spaces that carry `fill` from the end of a text of `used` cells to the
/// last cell of the text area.
pub fn fill_padding(used: usize, width: usize, fill: Style) -> Span<'static> {
    Span::styled(" ".repeat(text_width(width).saturating_sub(used)), fill)
}
```

- On the row with the cursor, count `max(cells(text), cursor_col + 1)` cells. The cursor cell at the end of the text then does not count twice.
- Count cells with `cell_width`, not `chars().count()`, in the form engine. Wide glyphs take two cells.
- Give the `Select` field (`‹ value ›`) the same padding.
- Pass `fill` as the base style of `cursor_spans`, so the text next to the cursor keeps the fill.
- The prompt `> ` stays outside the fill. The last column (margin) stays outside too.

## Why This Works
A space is a cell with text. `Paragraph` paints it with the span style. The padding turns the empty cells of the line into styled cells, so the background reaches the full text width on every row.

## Prevention
- In ratatui, do not rely on `Line::style` or `Paragraph::style` for a background behind empty cells. Add styled spaces.
- Test the fill on the real `Buffer`: draw the chooser and check the `bg` of each column. Tests in `tests/tui_render.rs` (`mod active_field_fill`) check columns 2..=38 for width 40: empty field, short text, wrap, `Ctrl+J`, cursor at the end, `Select`, and the short-field bar.
- When you add a new field kind, give it the same padding for the focused state.

## Related Issues
- `docs/solutions/design-patterns/tui-form-engine-events-and-cursor-style.md` (cursor as a cell style, wrapped layout). Moderate overlap: same module, other problem.
- `docs/brainstorms/2026-10-24-tlo-aktywnego-pola-formularza-requirements.md`
- `docs/plans/2026-10-24-001-fix-tui-tlo-aktywnego-pola-plan.md`
