---
title: "TUI focus stays in the task details after a confirmed action"
date: 2026-10-20
category: design-patterns
module: tui/task_app
problem_type: design_pattern
component: frontend
severity: low
applies_when:
  - "A TUI action is confirmed from the answer panel or from a confirmation window"
  - "A success path changes the focus after sending a request"
  - "A new action path is added that ends in answer()"
tags:
  - tui
  - focus
  - task-details
  - answer-panel
  - waiting
---

# TUI focus stays in the task details after a confirmed action

## Context

After a successful action from the answer panel, `answer()` in `src/tui/task_app/waiting.rs` set `Focus::List`. The operator lost the task details and had to press a key to return. The fix is one line, but the rule is easy to break again, so it is written down here.

## Guidance

On success, `answer()` sets `Focus::Details`, clears `panel_focus` and calls `back_to_axis()`. The focus stays on the timeline of the same task, and the list selection does not change. On a refusal (`Err`), focus and panel state stay as they were and the typed draft is kept.

- Every confirmed action goes through `answer()`: `Submit::Input`, `Submit::Operator`, `Submit::Owner` and `Confirmed::Answer` from the confirmation window. One change covers them all.
- Actions started from the list (pause, cancel) do not change focus. They need no change.
- `Esc` in the details still returns to the list. `Tab` is unchanged.
- Other `self.focus = ...` assignments (`history.rs`, `new_task.rs`, `repositories.rs`, `mod.rs`) belong to other views. Do not change them for this rule.

## Why This Matters

The operator answers a task and normally wants to see what the task does next. Moving the focus to the list forces an extra key press after every action. A test that assumed `Focus::List` after sending hid this: it pressed `Enter` to reopen the details. After the fix that `Enter` would enter the step conversation, so such tests must assert the new focus instead.

## When to Apply

- When you add an action path that ends in `answer()`.
- When you change what happens after a successful send.
- Automatic jump to the next waiting task (`NextWaiting`) is out of scope. It is a separate decision.

## Examples

```rust
// src/tui/task_app/waiting.rs, answer(), Ok(()) branch
self.settle_answer(&task_id, &waiting_id, Ok(()));
self.focus = super::Focus::Details;   // was Focus::List
self.panel_focus = false;
self.back_to_axis();
```

Tests in `tests/tui_state.rs` (`task_app_waiting`) check four cases:

- Enter-send keeps `Focus::Details`, `panel_focused()` is false and the list selection is unchanged.
- A send from the step conversation keeps the details on the axis, and `Esc` then returns to the list.
- An action confirmed in the window with `y` ends with the same focus.
- A refusal keeps the focus and the draft (`detail().has_drafts()`).

## Related

- [TUI modal confirmation keyboard contract](tui-modal-confirmation-keyboard-contract.md)
- `docs/plans/2026-10-19-002-fix-focus-po-zatwierdzeniu-akcji-plan.md`
- `docs/brainstorms/2026-10-19-focus-po-zatwierdzeniu-akcji-requirements.md`
