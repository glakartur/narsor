---
title: "TUI step axis: one gap constant per layout path, and a width breakpoint for the axis share"
date: 2026-11-12
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: low
applies_when:
  - "A spacing constant is shared by two layout paths and only one path must change"
  - "A pane split (axis vs content) must grow for wide panes but stay safe on narrow ones"
  - "A width cap is written as two linked terms (content minimum and axis maximum)"
tags: [tui, ratatui, przebieg, axis, column-gap, layout, narrow-terminal, constants]
---

# TUI step axis: gap constants per path and a width breakpoint

## Context
PR #102 changed the *Przebieg* axis (`src/tui/task_app/tabs/progress.rs`): the gap between data columns went from 2 to 3 spaces and the axis share from 70% to 75% of the tab. Requirements: `docs/brainstorms/2026-11-12-tui-przebieg-odstepy-i-szersza-os-requirements.md`. Plan: `docs/plans/2026-11-12-003-feat-tui-przebieg-odstepy-i-szersza-os-plan.md`.

## Guidance
1. **Split a shared constant before you change it.** `COLUMN_GAP` served the axis columns and the detail row (model, effort, harness). R4 kept the detail row as it was. A new `DETAIL_GAP = 2` holds the old value and `COLUMN_GAP = 3` is the new one. When code derives a width from the old constant (`agent_width`), correct it by the difference `(COLUMN_GAP - DETAIL_GAP)` per non-empty column. Do not just replace the name.
2. **A cap with two linked terms needs both terms changed.** `axis_cap` takes the lower of "tab minus content minimum" and "axis maximum". With 70% and 30% only the content term binds. Changing only the maximum to 75% would do nothing. Change both: `area.div_ceil(4)` and `area * 3 / 4`.
3. **Use a named breakpoint for the new share.** `WIDE_TAB = 100`: from this width 75/25, below it 70/30. At 80 columns 25% leaves 20 columns for the step content, which is too little. The number is a start value; the test reads the same constant, so a change needs no test rewrite.

## Why This Matters
- A shared constant changed in place moves a row that the requirement said must stay.
- A one-term change to a two-term cap looks done but changes nothing on screen.
- A pane split without a floor for narrow terminals hurts the content pane first.

## When to Apply
- A layout constant has more than one reader.
- A pane share is changed.

## Examples
Tests in `progress.rs`:
- `the_axis_stays_within_its_share_...` loops over widths 60..=220 and checks both shares on both sides of `WIDE_TAB`, plus `layout(.., 200).width <= 150`;
- `metric_columns_are_three_spaces_apart` checks the 3-space gap at the longest value of each column;
- the detail-row test keeps its 2-space separator and 3-space indent.

## Related
- `docs/solutions/design-patterns/tui-step-axis-decoration-variant-and-one-priority-column-table-2026-11-12.md` — moderate overlap: same axis and column table. That doc covers the column priority and the selection variant; this one covers gap constants and the width share. Both stay.
