---
title: "TUI step axis: a selection-decoration variant instead of a call flag, and one column table that drops columns in priority order"
date: 2026-11-12
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: low
applies_when:
  - "One view of a shared bracketed list must lose a part of the selection look (the fill) while the other views keep it"
  - "A table row has columns of different value (name, times, usage) and the width of the pane changes"
  - "A text label of a state (a word) is replaced by a glyph, and the word has users in several places"
  - "A row must not end with spaces when its last columns are empty"
tags: [tui, ratatui, selection, bracketed-list, przebieg, axis, column-table, narrow-terminal, status-glyph]
---

# TUI step axis: a decoration variant and one priority column table

## Context
The step axis of the *Przebieg* tab (`src/tui/task_app/tabs/progress.rs`) had three problems (PR #100, requirements `docs/brainstorms/2026-11-12-tui-przebieg-os-krokow-requirements.md`): the selected step had a background fill, the start time and the duration stood in a second row, and a status word (`w toku`) took width in the row and in the step header. The axis shares `render_list` in `src/tui/selection.rs` with *Historia* and the choosers. These must keep their fill.

## Guidance
1. **Add a decoration variant, not a boolean argument.** `SelectionDecoration` got `BracketedPlain`. `has_rules()` is true for both bracketed variants (so the row layout and `keep_in_view` stay the same). A new `fills_row()` is true only for `Bracketed`, and `draw_line` calls it for the fill. A new public `render_bracketed_list_plain` wraps it, like `render_barred_list`. The old callers do not change.
2. **Keep one layout path for a row.** The old axis had two paths: a simple row without usage and a table with usage. Both needed the new time columns, so the simple path (`axis_line`) was removed. An empty column has width 0 and takes no space, so the table also covers the simple case.
3. **Say the column priority in code.** Order: glyph, name, start, duration, usage. On a narrow axis `shrink` cuts the name to `MIN_LABEL`. If it still does not fit, the duration column is set to width 0, then the start column. Usage stays. The order of the checks is the order of value.
4. **Trim the tail of a row.** Collect the cells after the name in a list and find the last non-empty one with `rposition`. Cut the cells after it and `trim_end` it. Then a step without `started_at` has no spaces and no separator after its name.
5. **When a word leaves the UI, change the return type.** `status_look` returned `(glyph, role, word)`. The word had users only in the axis and the step header, so it now returns `(glyph, role)`. The compiler found every user. `Failed` and `Interrupted` now share one match arm (same `✗`); a separate glyph for `Interrupted` is deferred.

## Why This Matters
- A flag on the shared function lets the next view pass a wrong combination. A variant keeps the rules and the fill as one decision in `selection.rs`.
- Two layout paths compute width and shrinking twice and drift apart.
- A state that only a word or a colour carries fails in `NO_COLOR`; the selection here stays readable because the bar `│` and the rules `─` are characters.

## When to Apply
- A shared list widget serves several views and one needs a different selection look.
- A table with optional columns must fit panes of unknown width.

## Examples
Tests in `tests/integration/tui_render.rs` (commit `86f58ba`):
- the axis with a selected step has no cell with the `selection_fill` background, and the bracket is in the buffer;
- *Historia* keeps the fill (existing tests stay green);
- a width loop: name first, then duration, then start disappears;
- a step without `started_at` has no trailing spaces;
- `NO_COLOR`: both bracket states have `│` and `─`, no `REVERSED`.

## Related
- `docs/solutions/design-patterns/tui-row-background-from-semantic-flag-after-render-2026-10-30.md` — low overlap: it adds a background to a transcript card; this doc removes one from an axis.
- `docs/solutions/ui-bugs/tui-active-field-fill-only-under-text-2026-10-24.md`
- `docs/plans/2026-11-12-002-feat-tui-przebieg-os-krokow-plan.md`
