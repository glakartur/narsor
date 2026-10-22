---
title: "TUI form engine: cursor as cell style, wrapped layout, confirmation and events"
date: 2026-10-22
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: medium
applies_when:
  - "A TUI form edits text and every form must behave the same"
  - "A cursor is drawn inside the text and the width of its glyph shifts what follows"
  - "A form sends data and must ask before it does"
  - "A form grows with its content and its owner has to lay out the rest of the screen"
tags:
  - tui
  - ratatui
  - forms
  - cursor
  - wrapping
  - confirmation
---

# TUI form engine: cursor as cell style, wrapped layout, confirmation and events

The rules of a form live in `src/tui/form/`, not in the screens that own forms.
A new form is a list of fields and an area to draw them in.

## Decisions to keep

- **The cursor is a style, not a character.** The cell under the cursor is
  drawn `REVERSED`; at the end of the text it is one reversed space. Nothing is
  inserted into the text, so the width of a glyph cannot shift what follows it.
  A cursor at the end of a full row gets a row of its own (`form::layout::wrap`).
- **One layout function.** `layout::wrap` gives the rows, the cell of the cursor
  and the map from drawn characters to the value (a tab draws as spaces).
  Drawing, scrolling and `↑`/`↓` over the drawn rows all read it. The width of
  the last drawing is kept in the field (`WrapWidth`), so a key, which has no
  area, walks the rows that are on the screen.
- **The edge arrow is two presses.** In a multi-line field the first `↑`/`↓` at
  the first/last row only moves the cursor to the start/end of the text; the
  second one leaves (`FormEngine::take_edge`). Any other key clears the pending
  press. One-line fields, numbers and selects leave on one press; the list of
  actions never leaves.
- **Home/End work on the logical line** (between `\n`), not on the drawn row.
- **The confirmation is the form's own window** with keys of
  `Context::FormConfirm` (`Enter` sends, `Esc` returns). It inherits no global
  key, so `q` and `?` are neither commands nor text, and the bar shows only its
  two keys. It is drawn before the "terminal too small" return. Keys with any
  modifier are not its `Enter`. An entry that is irreversible keeps its own
  window instead of getting a second one.
- **Events are data.** `Chooser::take_events` returns `FormEvent`s
  (`Edited`, `Resized`, `ConfirmRequested`, `Submitted`, `Cancelled`,
  `FocusChanged`); nothing is called back and nothing polls. The owner reads
  them after a key, in its own loop.
- **Growth has a limit given by the owner** (`Growth { direction, limit_rows }`).
  Past it the field scrolls and no `Resized` is reported.

## Gotchas

- A test that says "Enter sends" has to answer the window: the helpers of
  `tests/tui_state.rs` press the second `Enter` when `form_confirming()`.
- The first field in the walk gets the focus when a form opens, not the first
  required multi-line one. Tests that type at once type into that field.
