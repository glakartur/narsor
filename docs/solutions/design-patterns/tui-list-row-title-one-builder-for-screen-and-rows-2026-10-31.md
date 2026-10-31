---
title: "TUI: list row shows the task title; one row builder for the screen and rows()"
date: 2026-10-31
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: low
applies_when:
  - "A TUI list row must show a different text than a shared helper (for example title_of) gives to other views"
  - "The list has a state accessor (rows()) for tests and a separate draw path"
  - "Two views (list row and detail frame) must show the same text for one item"
  - "A text filter must find an item by the new text and by the old text"
tags: [tui, task-list, row-title, title-of, row-text-with, filter, edge-layout, one-line, render-test]
---

# TUI: list row shows the task title; one row builder for the screen and rows()

## Context
The task list showed `display.title` on the second line of a row. This text is often the start of the task description. The detail frame (`edge_layout` in `src/tui/task_app/detail.rs`) shows `task.title` (the operator title or the conductor title). The operator could not easily connect a row with its details.

Three facts made the change less simple than "replace one string":
- `title_of` (`src/tui/task_app/list.rs`) has many users: action messages (`actions.rs`, `mod.rs`), history (`history.rs`), the panel (`panel/state.rs`). A change in `title_of` changes all of them without an operator decision.
- The draw path of the list built `RowText` inline with its own copy of the fields. `rows()` used `row_text_with`. A test on `rows()` would pass while the screen still showed the old text.
- The filter `/` used `title_of`. Operators find tasks by words of the description.

## Guidance
1. **Add a narrow helper next to the shared one. Do not change the shared one.** `row_title_of` returns `task.title` on one line when it is not blank, otherwise `title_of(task)`. Only `row_text_with` and `matches_task` call it. Other users of `title_of` stay the same until the operator decides (deferred question Q1 in the plan).

```rust
pub fn row_title_of(task: &TaskSummary) -> String {
    non_blank_line(task.title.as_deref()).unwrap_or_else(|| title_of(task))
}

/// `text` on one line, or `None` when it is missing or blank.
fn non_blank_line(text: Option<&str>) -> Option<String> {
    text.map(one_line).filter(|text| !text.trim().is_empty())
}
```

2. **Use the same rule as the other view.** `edge_layout` uses `one_line` and drops text that is blank after `trim`, then falls back to `title_of`. `row_title_of` uses the same rule, so the row and the frame agree for a task without a ticket. (A task with a ticket and no title: the frame shows only the ticket, the row falls back to `display.title`.) The test `the_list_row_and_the_edge_show_the_same_title` in `tests/tui_render.rs` locks this.

3. **One row builder for the screen and the state accessor.** The draw path now calls `self.row_text_with(index, &positions)` and then changes only the move label. Before, it built `RowText` inline. With one builder, a test on `rows()` checks the same text that the screen draws.

4. **Filter on both texts.** `matches_task` calls `matches_filter` with `row_title_of` and again with `title_of`. A task with a title is still found by words of its description.

5. **Shared normalisation in one function.** The ticket and the title both need "one line, `None` when blank". `non_blank_line` holds this rule for both.

## Why This Matters
- A change in a shared helper changes views outside the scope of the task (messages, history, panel). A narrow helper keeps the change inside the scope.
- Two copies of row construction drift. The first change to one copy (here, the title) makes the screen and `rows()` differ, and state tests do not see it.
- If the list and the detail frame use different blank and new-line rules, the same task shows two different titles.

## When to Apply
- When a list row must show a new field, check the draw path first. If it builds the row inline, change it to the shared builder before the field change.
- When a field shows in two views, copy the normalisation rule of the existing view (`one_line` + blank check) and add a test that compares both views.
- When the text that a filter matches changes, keep the old text in the filter if users search by it.

## Examples
Tests that cover the change (`tests/tui_state.rs`, `tests/tui_render.rs`):
- `the_row_shows_the_task_title_instead_of_the_description`
- `without_a_title_the_row_shows_the_description_until_a_refresh_brings_one`
- `a_blank_title_falls_back_to_the_description`
- `without_a_title_and_a_description_the_row_shows_the_task_id`
- `a_title_with_a_new_line_is_shown_on_one_line`
- `the_filter_matches_the_task_title_and_still_the_description`
- `a_long_task_title_is_cut_on_the_second_line_of_a_three_line_row`
- `the_list_row_and_the_edge_show_the_same_title`

Test helper note: `titled` in `tests/task_app_support/mod.rs` sets `display.title`, not `task.title`. Tests for the task title set `task.title = Some(..)` directly.

## Related
- Plan: `docs/plans/2026-10-31-002-feat-tui-lista-zadan-tytul-plan.md`
- Requirements: `docs/brainstorms/2026-10-31-tui-lista-zadan-tytul-zamiast-opisu-requirements.md`
- `docs/solutions/design-patterns/tui-row-background-from-semantic-flag-after-render-2026-10-30.md` (render tests that check the real screen, not only state)
