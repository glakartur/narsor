---
title: "TUI: header rule overwrote the last header row in low frames and kept stale cell content"
date: 2026-11-11
category: ui-bugs
module: tui
problem_type: ui_bug
component: tooling
symptoms:
  - "In a low frame the rule under the task header covered the last visible header row"
  - "A forced link width stayed in a cell of the rule line after the rule was drawn"
root_cause: logic_error
resolution_type: code_fix
severity: low
tags: [tui, ratatui, buffer, rule, header, task-details, low-frame, render-test]
---

# TUI: header rule overwrote the last header row in low frames and kept stale cell content

## Problem
The task details header and tab bar (PR #95) sit between two rules in the frame colour. `render_view` in `src/tui/task_app/detail.rs` drew the header rule on the last row of `parts.header`. When the frame was too low for all header rows, that row held header text, and the rule covered it. `draw_rule` also wrote symbols into cells without clearing them first, so a forced width from a link (set earlier in the same cell) stayed in the rule.

## What Didn't Work
Nothing was tried first; render tests for low frames found both defects.

## Solution
- Draw the header rule only when the header has all its rows: `parts.header.height == base_lines`.
- Call `cell.reset()` in `draw_rule` before `set_symbol` and `set_style`.

```rust
if parts.header.height > 0 && parts.header.height == base_lines {
    let y = parts.header.y + parts.header.height - 1;
    draw_rule(frame.buffer_mut(), area, y, &rule_line(...), rule_style);
}
// in draw_rule:
cell.reset(); // drops what the cell held before, e.g. a link's forced width
cell.set_symbol(&ch.to_string());
```

## Why This Works
The rule row is reserved only when layout gave the header its full height. In a low frame the header text keeps the row. `reset()` removes all old state of a cell, so the rule shows only its own symbol and style.

## Prevention
- A decoration line that takes a row from content must check that the row is spare, not assume it.
- A helper that paints over cells that other code may have written must reset them first.
- Keep render tests for low frames (see `tests/integration/tui_render.rs`) for every new header or rule row.
- Related: `docs/solutions/design-patterns/tui-row-background-from-semantic-flag-after-render-2026-10-30.md`.
