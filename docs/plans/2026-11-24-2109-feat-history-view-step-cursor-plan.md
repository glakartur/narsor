---
title: History View - Step Cursor and Reduced Tabs - Plan
type: feat
date: 2026-11-24
topic: history-view-step-cursor
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# History View - Step Cursor and Reduced Tabs - Plan

## Goal Capsule

- **Objective:** An operator who reads an archived task in the history sees only tabs that can hold content, and can move along every step of the archived run and read each step, however long the run and whatever the terminal height.
- **Product authority:** This plan covers the TUI history (`H`) and the matching check of the Web UI history and `narsor-cli`. The live task details are not active scope.
- **Open blockers:** None. Two questions in "Pytania otwarte dla użytkownika" have a recommended default that planning can use.

---

## Product Contract

### Summary

The TUI history drops the *Rozmowa*, *Zdarzenia* and *Dyrygent* tabs, because archiving deletes the conversations, the events and the conductor data. It keeps only *Przebieg*, *Stan* and *Info*.
*Przebieg* in the history gets a step cursor: `j`/`k`, arrows, `g`/`G` move it, the axis scrolls to keep it in view in both the one-row and the full (`v`) layout, and the right side shows the facts of the selected step.
The Web UI and the CLI are checked for the same two points and change only where they show the same gap.

### Problem Frame

An archived task keeps only the step header rows of its run (see `docs/plans/2026-11-20-002-feat-archive-task-run-snapshot-plan.md`).
The conversation, the events and the conductor go away with the storage.
The TUI history still shows the *Rozmowa*, *Zdarzenia* and *Dyrygent* tabs, and each can only say "storage deleted".
The earlier plan also made *Przebieg* in the history a list without a cursor ("Step rows render without a selection cursor").
The axis draws from the first step down and has nothing to scroll by.
When the run has more steps than the tab has rows, the operator cannot see the later steps.
The full layout (`v`) uses two rows per step, so the problem shows sooner there.

### Key Decisions

- **The history keeps the cursor model of the live *Przebieg*, without the parts that need a live run.** No follow (`f`), no step conversation, no `Enter`/`→` into a step. Governs R3, R4, R6.
- **The history drops *Rozmowa*, *Zdarzenia* and *Dyrygent*.** Decision Q1: the person chose to remove all three, because each shows only "storage deleted". Governs R1.
- **The CLI does not change.** `narsor-cli task show` and `task conversation` for an archived task print one availability line ("the task is archived"), not a conversation view. Governs R9.

### Requirements

**TUI: tabs in the history**

- R1. The tab strip of the history details does not show *Rozmowa*, *Zdarzenia* or *Dyrygent*, and `[` / `]` skip them.
- R2. The tabs of the live task details do not change.

**TUI: the step cursor on *Przebieg* in the history**

- R3. When the archived timeline has steps, one step is selected; when the history entry first shows its timeline, the selection is on the first step.
- R4. While the keys are on the axis, `j` / `↓` and `k` / `↑` move the selection one step, and `g` / `G` move it to the first / last step; the selection stops at the ends.
- R5. The axis scrolls so that the selected step is always in view, in the one-row layout and in the full (`v`) layout, and when the operator switches the layout or the terminal changes size.
- R6. The selected step uses the same bracket as on the live axis: in the focus role while the keys are on the axis, in the background role when the focus is on the history list.
- R7. The right side of the tab shows the facts of the selected step that the live *Przebieg* shows above the conversation (name, decision by, duration, error, notes, the gate's scripts for the gate step), and below them one fixed line that the conversation was deleted at archiving. It never starts a conversation read.
- R8. Each history entry remembers its own selection and scroll position while the history is open, so going to another entry and back keeps them.
- R9. The key help and the footer of the history axis list the new keys.

**Web UI and CLI**

- R10. The Web UI history keeps its current form (a dialog without a *Rozmowa* tab, with all step rows and their facts in a page that the browser scrolls); it does not change (decision Q2: the person chose no step selection in the Web UI).
- R11. The CLI does not change (per Key Decisions).

**Documentation**

- R12. The user documentation of the history (`docs/user/tui.md`, and `docs/user/web.md` if Q2 changes the Web UI) says which tabs the history has and how to move along the steps.

### Acceptance Examples

- AE1. **Covers R3, R4, R5.** **Given** an archived run with 30 steps and a tab 12 rows high in the full layout, **when** the operator presses `G`, **then** the last step is selected and in view; **when** the operator then presses `g`, **then** the first step is selected and in view.
- AE2. **Covers R5.** **Given** the selected step is the 10th in the one-row layout and in view, **when** the operator presses `v`, **then** the 10th step stays selected and is still in view.
- AE3. **Covers R7.** **Given** a selected step that failed, **when** the right side draws, **then** it shows the error line and the "conversation deleted at archiving" line, and no conversation read starts.
- AE4. **Covers R3.** **Given** the read of the archived timeline failed or returned no timeline, **when** *Przebieg* draws, **then** it shows the existing error or "storage deleted" text and there is no cursor.
- AE5. **Covers R1.** **Given** the history details are on *Przebieg*, **when** the operator presses `[` repeatedly, **then** the tabs go round *Info*, *Stan*, *Przebieg*, and never show *Rozmowa*, *Zdarzenia* or *Dyrygent*.

### Scope Boundaries

- Not in scope: a change to what the archive keeps (the snapshot stays step header rows only).
- Not in scope: keyboard selection of steps in the Web UI history (decision Q2).
- Not in scope: any change to the live task details or the live *Przebieg*.

### Założenia do potwierdzenia

- A1. *Rozmowa*, *Zdarzenia* and *Dyrygent* are removed from the TUI history. They all show only the "storage deleted" text (decision Q1).
- A2. The Web UI history already has no *Rozmowa* tab: it is a dialog with the *Przebieg* step list, the header and *Info* (`crates/narsor-web/ui/js/ui/settings.js`, `ArchivedTask`). Its step rows already show all step facts, and the browser scrolls the dialog. So the Web UI needs no change for this request.
- A3. The CLI has no *Rozmowa* equivalent for archived tasks. `task show` prints the step lines and one conversation availability line; `task conversation` reports "the task is archived". These lines tell the truth and stay.
- A4. The first step is the initial selection, because a closed run has no running step to follow.
- A5. The right side of a selected step shows only facts that the archive kept; it does not try to read the conversation, because the storage is gone.
- A6. The step keys are the same as on the live axis (`j`/`k`, arrows, `g`/`G`), so the operator does not learn new keys.

### Pytania otwarte dla użytkownika

- None. Q1 (remove *Zdarzenia* and *Dyrygent* too) and Q2 (no Web UI change) were answered by the person; the plan follows both.

### Outstanding Questions

**Resolve Before Planning**

- None.

**Deferred to Planning**

- How the history keeps the selection when the archived timeline is read again (`drop_selection` in `src/tui/task_app/tabs/progress.rs` now clears it after each read).
- Whether the existing `Context::HistoryAxis` key layer gets the move keys, or the history reuses the bindings of `Context::Axis` without `f` and `Enter`.

### Sources / Research

- `src/tui/task_app/history.rs` — the history view; `on_history_timeline` calls `progress.drop_selection()`, so the axis has no cursor.
- `src/tui/task_app/tabs/progress.rs` — `Progress::drop_selection`, `step`/`first`/`last`, `AxisRows` (`v`), and the axis drawn with `render_bracketed_list_plain`, which scrolls only to keep a selected item in view.
- `src/tui/keymap.rs` — `Context::HistoryAxis` binds only `Esc` and `v`.
- `src/tui/task_app/mod.rs` — `DetailTab::ALL` and `shifted`, shared by the live details and the history.
- `crates/narsor-web/ui/js/ui/settings.js` (`ArchivedTask`) and `crates/narsor-web/ui/js/ui/tabs/progress.js` (`readOnly`) — the Web UI history.
- `src/cli/task_text.rs` (`unavailable_reason`) and `src/cli/remote.rs` — the CLI for archived tasks.
- `docs/plans/2026-11-20-002-feat-archive-task-run-snapshot-plan.md` — the earlier decision that history steps have no cursor; this plan reverses that part.
