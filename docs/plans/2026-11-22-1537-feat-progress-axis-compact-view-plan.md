---
title: "Przebieg: one-line and multi-line step list, toggled with v - Plan"
type: feat
date: 2026-11-22
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Przebieg: one-line and multi-line step list, toggled with v - Plan

## Goal Capsule

- **Objective:** On *Przebieg*, the operator sees one row per step by default, so more steps fit on the screen. When they need the agent details, they press `v` to show the full rows.
- **Means:** One session-wide view flag in the TUI detail state, read by the axis layout, flipped by a new keymap action bound to `v` on the step list (KTD1, KTD2, KTD3).
- **Product authority:** The request from the operator (2026-11-22). The Product Contract below wins on behaviour. Assumptions taken without the operator are listed in "Założenia do potwierdzenia" and under Planning Contract › Assumptions.
- **Open blockers:** None. One non-blocking question for the operator is in "Pytania otwarte dla użytkownika"; the plan uses its recommended option until the operator answers.
- **Execution profile:** One PR. TUI-only change in the `narsor` binary (`src/tui/`), plus user documentation.
- **Stop conditions:** Stop and ask if the change needs a new config key, a persisted setting, a protocol change, or a change to the first-row columns. Those are out of scope.
- **Who finishes:** The implementer (`ce-work` or a person) builds, tests and opens the PR. The operator answers the open question before merge if they want the other option.

---

## Product Contract

Product Contract preservation: Product Contract unchanged (R1–R8, AE1–AE3, A1–A5 kept as written). Added A6 as the working assumption for the 2026-11-22 review finding on axis width; Q1 keeps the choice open for the operator.

### Summary

The step list (the axis) on *Przebieg* gets two views. The one-line view shows only the first row of each step: glyph, name, start, duration and usage. The multi-line view is what the axis shows today: it also shows the second row with model, effort and harness. `v` switches between the two views while the keys are on the step list. The one-line view is the default.

### Problem Frame

Today each step with a model, effort or harness takes two rows on the axis. On a long run, half of the axis height goes to the agent details, so the operator sees fewer steps and scrolls more. The agent details are useful only some of the time.

### Requirements

**Views**

- R1. In the one-line view, each step takes exactly one row on the axis: the row with the step name, as it is rendered today.
- R2. In the multi-line view, the axis shows each step exactly as it does today, including the model / effort / harness row when the step has one.
- R3. The one-line view is the default when the TUI starts.

**Toggle**

- R4. When the keys are on the step list (`Context::Axis` and `Context::HistoryAxis`), `v` switches between the one-line view and the multi-line view.
- R5. The key help shows the `v` binding in those contexts with a short label (for example `widok`), in the same way as the other keymap entries.
- R6. A switch keeps the selected step, the follow mode and the scroll position on that step. The selected step stays visible after the switch.
- R7. The chosen view stays in effect when the operator selects another task, moves to another tab or comes back, and across timeline refreshes, until they press `v` again or quit the TUI.

**Unchanged**

- R8. `v` in the step conversation (`Context::StepConversation`) keeps its current meaning (next conversation view). The step header on the right pane does not change.

### Acceptance Examples

- AE1 (Covers R1, R3). Given a task with three steps that have a model and a harness, when the operator opens *Przebieg*, then the axis shows three rows, one per step, without the model / effort / harness rows.
- AE2 (Covers R2, R4, R6). Given the one-line view with the second step selected, when the operator presses `v` on the axis, then each step with a model, effort or harness shows its second row, and the second step is still selected and visible. A second `v` returns to AE1.
- AE3 (Covers R8). Given the keys are in the step conversation, when the operator presses `v`, then the conversation view changes and the axis view does not.

### Scope Boundaries

- No new config option or persisted setting for the default view (deferred).
- No change to which columns the first row shows or how they shrink on a narrow tab.
- No change to `narsor steps` or other CLI output.
- No change to the web UI (`crates/narsor-web`).

### Założenia do potwierdzenia

- A1. "The first line (the one with the name)" means the whole first row of the axis entry (glyph, name, start, duration, usage), not the name alone. Reason: the request names the line, not the field; the usage columns are on that line today.
- A2. The toggle also works on the history axis (`Context::HistoryAxis`) of a closed run, because it renders the same step list. This is the safer reading for consistency.
- A3. The view is kept in TUI memory for the session only (R7). It is not written to disk, and each new TUI start uses the one-line view (R3).
- A4. One view applies to all tasks, not one view per task.
- A5. The label in the key help is Polish and short, like the other TUI labels (`widok`, description for example "jedno- albo wieloliniowa lista kroków").
- A6. In the one-line view, the axis width comes from the first-row columns only. The step content gets the space that the second row needed, so the split between the axis and the step content can move when the operator presses `v`. The selected step stays visible across that width change (R6). Reason: the one-line view exists to save space; a width kept for a hidden row saves nothing. See question Q1.

### Pytania otwarte dla użytkownika

- Q1. Section: Product Contract › Założenia do potwierdzenia, A6 (axis width). Question: In the one-line view, must the axis become narrower (the split between the axis and the step content moves on each `v`), or must the axis keep the multi-line width in both views (the split never moves)? Options:
  1. (Recommended) The axis becomes narrower in the one-line view; the step content gets the free columns. The split moves on `v`, and the step content wraps again to the new width.
  2. The axis keeps the multi-line width in both views; the split stays the same on `v`, and the one-line view has empty space on the right of the axis.

  Not blocking: the plan implements option 1. Option 2 changes KTD4, the U1 width test scenario and the A6 manual check in the Verification Contract.

### Outstanding Questions

**Resolved during planning**

- Where the view state lives so that R7 holds across tasks and the history view: in `DetailState` (`src/tui/task_app/mod.rs`), not in `Progress`. See KTD1.

### Sources

- `src/tui/task_app/tabs/progress.rs` — `AxisLayout::new` builds the first row (`table`) and adds the model / effort / harness row (`add_details`). The module doc describes the two rows.
- `src/tui/keymap.rs` — `Context::Axis` and `Context::HistoryAxis` bindings; `v` is free there and bound to `NextConversationView` in `Context::StepConversation`.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **The view flag lives in `DetailState`, not in `Progress`.** `Progress` is per task (one per `TaskDetail`), and the history keeps its own `TaskDetail` map inside `History`, which is created again each time `H` opens. `DetailState` is owned by `TaskApp` for the whole session and already carries a session-wide display flag (`hyperlinks`). So R7 and A4 hold without copying state between tasks, and a timeline refresh (`Progress::apply`) cannot touch the flag.
- KTD2. **A small enum, not a bool.** Add an `AxisRows`-style enum with two values (one-line, full) next to `AxisLayout` in `src/tui/task_app/tabs/progress.rs`, with the one-line value as `Default` (R3). An enum reads clearly at the call site of `progress::render` and in tests. The exact name is an implementation choice.
- KTD3. **A new keymap action, bound to `v` in `Context::Axis` and `Context::HistoryAxis`.** Do not reuse `Action::NextConversationView`: that action means "next conversation view" in `StepConversation` and `AgentConversation`, and handlers match on actions, not keys. A new action (for example `Action::ToggleAxisRows`) keeps R8 true by construction, because `StepConversation` is a different layer and keeps its own `v` binding. Both key handlers flip the flag on `TaskApp.detail` directly: `TaskApp::on_key` in `src/tui/task_app/mod.rs` and `TaskApp::on_history_key` in `src/tui/task_app/history.rs` (before its catch-all `other` arm, which forwards to the per-entry `TaskDetail`).
- KTD4. **The one-line view skips `AxisLayout::add_details`.** `AxisLayout::new` takes the view and calls `add_details` only in the full view. Because `add_details` is also the only code that widens the axis for the second row, the one-line axis width is the first-row width (A6). If the operator picks option 2 of Q1, still compute the width in `add_details` but push no detail lines, change the U1 width test to assert the same width in both views, and change the A6 manual check to "the split does not move on `v`".
- KTD5. **Selection and scroll need no new code.** The selected step is a label in `Progress`, which the toggle does not touch. `render_bracketed_list_plain` calls `keep_in_view` on each frame with the current item heights, so the selected step stays visible after the heights change (R6). Tests prove this; the plan adds no scroll logic.

### Assumptions

- The help bar may drop hints from the end on a narrow screen. The `v` hint goes after `f` in `Context::Axis`, so `Esc` (first) stays visible, as the keymap comment requires.
- In the history axis no step is selected (`Progress::drop_selection`), so R6 there means only that the scroll stays valid.
- `docs/user/tui.md` is the user documentation for TUI keys and the *Przebieg* rows; it gets updated in the same PR.

### Sequencing

U1 → U2 → U3. U2 needs the enum and the new `render` parameter from U1. U3 documents the result.

---

## Implementation Units

### U1. Axis layout renders one row or full rows per step

- **Goal:** `progress::render` draws the axis in the view it is given: one row per step, or the current two-row layout.
- **Requirements:** R1, R2, A1, A6; covers AE1 at layout level.
- **Dependencies:** None.
- **Files:**
  - `src/tui/task_app/tabs/progress.rs` (enum, `AxisLayout::new`, `render`, module doc, unit tests)
- **Approach:**
  1. Add the view enum (KTD2) with the one-line value as default.
  2. Give `AxisLayout::new` the view; call `add_details` only for the full view (KTD4).
  3. Add a view parameter to `progress::render` and pass it to `AxisLayout::new`. Callers are updated in U2.
  4. Update the module doc: the second row shows only in the multi-line view.
- **Patterns to follow:** The existing test helper `layout(positions, width)` and `timed_steps()` in the same file; the column-table rules in `docs/solutions/design-patterns/tui-step-axis-decoration-variant-and-one-priority-column-table-2026-11-12.md` (do not change the first-row column order).
- **Test scenarios:** (unit tests in `progress.rs`; they catch layout regressions but not key routing)
  - Covers AE1. Given three steps with model, effort and harness, the one-line layout has exactly one line per step, and no line contains the model or harness text.
  - Given the same steps, the full layout has two lines per step, and the second line has the model, effort and harness (the existing test `the_first_row_has_the_times_and_the_second_the_agent` keeps passing with the full view).
  - Given steps whose detail row is wider than the first row and a wide tab, the one-line axis is narrower than the full axis, and its width equals what the first row needs.
  - Given steps with no model, effort or harness, both views give the same lines and the same width.
- **Verification:** The new and the existing `progress.rs` tests pass. The first row of each step is the same in both views.

### U2. `v` toggles the view on the task axis and the history axis

- **Goal:** The operator presses `v` on the step list and the axis switches views; the choice holds for the whole session.
- **Requirements:** R3, R4, R5, R6, R7, R8, A2, A3, A4, A5; covers AE1, AE2, AE3.
- **Dependencies:** U1.
- **Files:**
  - `src/tui/keymap.rs` (new action, bindings in `Context::Axis` and `Context::HistoryAxis`)
  - `src/tui/task_app/mod.rs` (`DetailState` field, accessor and toggle; `on_key` handling)
  - `src/tui/task_app/history.rs` (`on_history_key` handling; `DetailsLook` carries the view to `render_details`)
  - `src/tui/task_app/detail.rs` (`ViewOptions` carries the view to `progress::render`)
  - `src/tui/task_app/layout.rs` (reads the view from `app.detail` for both the task details and the history details)
  - `tests/integration/tui_state.rs` (keymap and app-state tests)
- **Approach:**
  1. Add the action (KTD3) and two bindings: `v`, label `widok`, description per A5. Put the `Context::Axis` binding after `f`.
  2. Add the view field to `DetailState` with a getter and a toggle (KTD1). `DetailState::new` uses the default (one-line).
  3. In `TaskApp::on_key`, handle the new action by flipping the flag. In `TaskApp::on_history_key`, handle it before the `other` arm.
  4. Pass the flag from `layout.rs` into `ViewOptions` (task details) and `DetailsLook` (history details), and from `render_view` into `progress::render`.
- **Patterns to follow:** How `hyperlinks` goes from `DetailState` through `layout.rs` into `ViewOptions` and `DetailsLook`; how `Action::Follow` is bound in `Context::Axis`.
- **Test scenarios:** (integration tests in `tests/integration/tui_state.rs`; they drive real keys through `TaskApp`, so they catch routing and state, not pixel layout)
  - Keymap: `v` in the `Axis`/`Details` stack and in the `HistoryAxis`/`HistoryDetails` stack gives the new action, and its help label is `widok`; `v` in the `StepConversation` stack still gives `NextConversationView`. Update the existing assertion in `the_axis_bar_enters_the_step_and_does_not_expand` that `v` does nothing on the axis. The keymap consistency check (`check_consistency`) still passes.
  - Covers AE1. A new `TaskApp` starts in the one-line view.
  - Covers AE2. With the keys on the axis and the second step selected, `v` switches to the full view; the selected step label and the follow mode are the same as before; a second `v` returns to the one-line view.
  - R7. After `v`, selecting another task, switching tabs with `]` / `[`, and applying a new timeline read keep the full view.
  - A2. With the history open and the keys on its *Przebieg*, `v` switches the view, and the view is the same one the task details use after the history closes.
  - Covers AE3. With the keys in a step conversation, `v` changes the conversation view and does not change the axis view.
- **Verification:** All tests in `tests/integration/tui_state.rs` pass. A manual run of `narsor` shows one row per step on *Przebieg*, `v` shows the agent rows, and the help (`?`) lists `v` on the axis.

### U3. User documentation for the two views

- **Goal:** `docs/user/tui.md` describes the one-line default and the `v` key.
- **Requirements:** R1–R5.
- **Dependencies:** U2.
- **Files:**
  - `docs/user/tui.md` (the *Przebieg* paragraph and the key table rows for the steps and for the history steps)
- **Approach:** State that the step list shows one row per step by default and that `v` on the steps shows the second row (model, effort, harness). Add `v` to both key-table rows for *Przebieg* steps.
- **Test scenarios:** Test expectation: none -- documentation only.
- **Verification:** The doc describes the default and the key, and does not contradict R1–R8.

---

## Verification Contract

| Check | Command or action | Proves |
| --- | --- | --- |
| Build | `cargo build --workspace --bins` | The new parameter reaches every caller of `progress::render` and `render_view`. |
| Tests | `cargo test --workspace` (CI runs it with `--locked`) | U1 layout tests and U2 keymap and state tests; no regression in other TUI tests. |
| Lints (advisory, not a CI gate) | `cargo clippy --workspace --all-targets` and `cargo fmt --check` | No new warnings, for example from the extra `render` argument. |
| Manual | Run `narsor` against a task with agent steps, press `v` on *Przebieg* and in the history | AE1–AE3 by eye, and A6 (the split moves on `v`). |

`#[ignore]`d tests are not needed for this change.

---

## Definition of Done

- R1–R8 hold, and AE1–AE3 have tests in U1 or U2.
- `cargo test --workspace` passes.
- The keymap test that said `v` does nothing on the axis now asserts the new action, and no other test was weakened.
- `docs/user/tui.md` matches the behaviour.
- No dead code from abandoned approaches remains in the diff.
- Q1 is answered by the operator, or the PR description says that option 1 is in place and what to change for option 2 (KTD4).
