---
title: "TUI modal confirmation keyboard contract"
date: 2026-10-15
category: design-patterns
module: tui/task_app
problem_type: design_pattern
component: frontend
severity: low
applies_when:
  - "A keyboard-driven TUI adds a confirmation modal over existing views or overlays"
  - "A modal consumes input while layout can take early-return paths, such as a too-small terminal"
  - "A destructive action can discard unsent user input"
tags:
  - tui
  - modal
  - keyboard
  - confirmation
  - quit
---

# TUI modal confirmation keyboard contract

## Context

A quit confirmation spans input handling, overlay rendering, layout, and the shortcut bar. Reviewing this flow exposed cases where the app could consume keys while the dialog was hidden, show shortcuts for the underlying view, stack two dialogs, or leave a draft-loss warning out of the prompt.

## Guidance

Treat a keyboard modal as the active interaction across the whole frame. While it is open, route input only to the modal's accepted keys, show only those keys in the shortcut bar, and give the modal precedence over other overlays. Render it on every layout path that can return early, including the too-small-terminal path. If confirming can discard user input, say so in the dialog.

Keep cancellation local to the modal: closing it should reveal the prior view or overlay without losing its state. Test both the input contract and the rendered result, including ignored keys, modified keys, small terminals, and an underlying overlay.

## Why This Matters

When input handling and rendering are split across the app, layout, and overlay code, a modal can be logically active without being visible. Stale hints or a second dialog make the next keypress ambiguous, while an unexplained destructive exit can silently lose user work.

## When to Apply

- A confirmation dialog consumes input from multiple TUI views.
- The terminal has alternate render paths or minimum-size handling.
- A confirmation can interrupt another overlay or discard a draft.

## Examples

For a quit flow, the first `q` opens the dialog; another unmodified `q` confirms, and `Esc` cancels. While it is open, unrelated and modified keys leave it open, the bar advertises only `q` and `Esc`, and a draft produces a loss warning. The same dialog remains visible when the main layout is replaced by the too-small message.

## Related

- [Task detail action footer visual hierarchy](task-detail-action-footer-visual-hierarchy.md)
- `src/tui/task_app/mod.rs`
- `src/tui/task_app/actions.rs`
- `src/tui/task_app/layout.rs`
