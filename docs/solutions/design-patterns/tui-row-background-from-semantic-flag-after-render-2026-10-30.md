---
title: "TUI: row background for a marked transcript entry, painted after render from a semantic flag"
date: 2026-10-30
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: low
applies_when:
  - "A rendered card (all its wrapped rows) must get a background that reaches the right edge of the panel"
  - "The background must not change the text colour of the spans"
  - "Some columns (selection bar, prefix) must keep their own style"
  - "The render area can have an offset (area.x, area.y > 0) or a small width"
tags: [tui, ratatui, buffer, set-style, transcript, workflow-choice, background, render-test, area-offset]
---

# TUI: row background for a marked transcript entry, painted after render from a semantic flag

## Context
The Conversation tab must show each "WYBÓR WORKFLOW" entry as a block with a background (Indexed(58); with `NO_COLOR`: reverse video). The block starts at the content column (`PREFIX_WIDTH`) and ends at the right edge of the panel. The selection bar, the RAW view and other tabs stay as they are. The text keeps its own colour (for example `Role::Done` for a stale entry).

Two risks were found:
- The renderer must not find a choice by its text (the header "WYBÓR WORKFLOW"). Text can change, and RAW views show the same bytes.
- The first version painted cells with a loop over `area.y..area.y + area.height` and `area.x..area.x + area.width`. The tests used `Rect` at (0, 0) with full width only, so an error in the offset or a narrow panel would not fail a test.

## Guidance
1. **Carry an explicit flag through the model, not the text.** `TranscriptEntry::as_workflow_choice()` sets `workflow_choice: bool`. `lines_for` copies it to each `TranscriptLine` of the card (not in `DisplayMode::Raw`, not for an entry toggled to raw). `wrap_visible_lines` copies it to every wrapped fragment.
2. **Render the text first, then paint the background on the buffer.** Collect the screen rows of flagged lines before the lines are moved into the `Paragraph`. After `frame.render_widget`, call `Buffer::set_style` on one `Rect` per row. `set_style` merges only the given attributes (here `bg` or `REVERSED`), so `fg` of the text stays.
3. **Use the `Rect` edge helpers.** `area.top()..area.bottom()` for rows, `area.right()` for the right edge. `Buffer::set_style` clips to the buffer, so no per-cell `cell_mut` loop and no `Option` checks are necessary.

```rust
let visible = wrap_visible_lines(&lines, width, transcript.scroll_offset(), height);
let choice_rows: Vec<u16> = visible
    .iter()
    .zip(area.top()..area.bottom())
    .filter(|(line, _)| line.workflow_choice)
    .map(|(_, y)| y)
    .collect();
// ... render the Paragraph from `visible` ...
let background = styles.workflow_choice_background();
let content_x = area.x.saturating_add(PREFIX_WIDTH);
let content_width = area.right().saturating_sub(content_x);
for y in choice_rows {
    frame
        .buffer_mut()
        .set_style(Rect::new(content_x, y, content_width, 1), background);
}
```

4. **Keep the colour in `Styles`.** `Styles::workflow_choice_background()` returns `bg(Indexed(58))`, or `REVERSED` when `no_color` is set. Backgrounds live in `src/tui/style.rs` and never colour text.

## Why This Matters
- A flag in the model makes the highlight independent of header text and lets RAW views opt out in one place (`lines_for`).
- Painting after render keeps the span styles (role colour, stale colour, selection) untouched. Putting `bg` on the spans would need changes in every span builder and would not cover empty cells (see the related field-fill doc).
- An error in the offset paints the wrong rows or columns only when the panel is not at (0, 0). Tests at (0, 0) cannot find it.

## When to Apply
- A highlight for a whole logical entry in a scrolled, wrapped list (transcript, log, history).
- The highlight must not change text colours and must leave some leading columns alone.

## Examples
Tests in `tests/tui_render.rs` that guard this pattern (commit 20be9f8):
- `a_pane_drawn_with_an_offset_highlights_only_its_own_cells`: render into a sub-area `Rect::new(3, 2, 40, 10)` of a 50×14 buffer. All highlighted rows are in `area.top()..area.bottom()`, and the blocks stay in the area.
- `a_pane_no_wider_than_the_bar_draws_without_a_panic`: for widths `1..=PREFIX_WIDTH + 1` the highlighted columns are exactly `PREFIX_WIDTH..width`.
- `a_choice_scrolled_partly_out_of_view_is_highlighted_only_where_visible`: only the visible rows of a scrolled card are highlighted.
- A stale choice: the text still has the `Role::Done` colour under the background.
- Use `narsor::tui::selection::PREFIX_WIDTH` in tests, not a copy of the constant.
- Shared event builders live in `tests/task_app_support/mod.rs`.

Model tests in `src/tui/transcript.rs`: every card line has the flag; every wrapped fragment keeps it; raw views have no flag.

## Related
- `docs/solutions/ui-bugs/tui-active-field-fill-only-under-text-2026-10-24.md` — moderate overlap: same goal (background to the edge), other method (styled padding spaces inside the `Paragraph`). Use padding when the fill belongs to the widget text; use a post-render `set_style` when the fill must not touch span styles.
- `docs/plans/2026-10-30-001-feat-tui-wyroznienie-wyborow-workflow-plan.md`
