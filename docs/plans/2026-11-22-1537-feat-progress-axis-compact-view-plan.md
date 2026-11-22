---
title: "Przebieg: one-line and multi-line step list, toggled with v"
date: 2026-11-22
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Przebieg: one-line and multi-line step list, toggled with `v`

## Goal Capsule

- **Objective:** On *Przebieg*, the operator sees one row per step by default, so more steps fit on the screen. When they need the agent details, they press `v` to show the full rows.
- **Product authority:** The request from the operator (2026-11-22). Assumptions taken without the operator are listed in "Założenia do potwierdzenia".
- **Open blockers:** None.

## Product Contract

### Summary

The step list (the axis) on *Przebieg* gets two views. The one-line view shows only the first row of each step: glyph, name, start, duration and usage. The multi-line view is what the axis shows today: it also shows the second row with model, effort and harness. `v` switches between the two views while the keys are on the step list. The one-line view is the default.

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

### Założenia do potwierdzenia

- A1. "The first line (the one with the name)" means the whole first row of the axis entry (glyph, name, start, duration, usage), not the name alone. Reason: the request names the line, not the field; the usage columns are on that line today.
- A2. The toggle also works on the history axis (`Context::HistoryAxis`) of a closed run, because it renders the same step list. This is the safer reading for consistency.
- A3. The view is kept in TUI memory for the session only (R7). It is not written to disk, and each new TUI start uses the one-line view (R3).
- A4. One view applies to all tasks, not one view per task.
- A5. The label in the key help is Polish and short, like the other TUI labels (`widok`, description for example "jedno- albo wieloliniowa lista kroków").

### Pytania otwarte dla użytkownika

None. Every unclear point has an assumption above.

### Outstanding Questions

**Deferred to Planning**

- Where the view state lives (the shared TUI state next to the `Progress` state, or in `Progress` itself) so that R7 holds across tasks and the history view.

### Sources

- `src/tui/task_app/tabs/progress.rs` — `AxisLayout::new` builds the first row (`table`) and adds the model / effort / harness row (`add_details`). The module doc describes the two rows.
- `src/tui/keymap.rs` — `Context::Axis` and `Context::HistoryAxis` bindings; `v` is free there and bound to `NextConversationView` in `Context::StepConversation`.

## Deferred / Open Questions

These are review items left for the author. They do not block implementation.

### From 2026-11-22 review

- **Axis width on a view switch is not specified** — Product Contract, R1, R2 and R6 (P2, design-lens, confidence 75)

  Today the axis gets wider so that the model / effort / harness row fits. If the one-line view only hides that row, the axis becomes narrower and the step content pane becomes wider. Each press of `v` then moves the pane split, and the document does not say if this is wanted. R6 (keep the selection, follow mode and scroll on a switch) names what stays stable, but not the layout.

  Change: Add a requirement that, in the one-line view, the axis width comes from the first-row columns only, so the step content keeps the freed space. State that the pane split may change on `v`, and that the selected step stays visible across that width change.
