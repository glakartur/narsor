---
title: TUI Mouse Wheel and Click Selection - Plan
type: feat
date: 2026-11-26
topic: tui-mouse-scroll-and-click
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# TUI Mouse Wheel and Click Selection - Plan

## Goal Capsule

- **Objective:** An operator who holds a mouse can move through the lists and the conversations of the `narsor` TUI with the wheel, and can put the focus on a panel and a list item with one click, without first learning the keys.
- **Means:** Mouse reporting in the terminal (crossterm), routed to the panel under the pointer. The keyboard stays the primary input and keeps all its behavior.
- **Product authority:** This plan covers the `narsor` TUI only (`src/tui/`). The Web UI, `narsor-cli`, the server and the protocol are not active scope. Drag, double-click, right-click, hover and clicks on tabs, links and form fields are not active scope (see Scope Boundaries).
- **Open blockers:** Q1 in "Pytania otwarte dla użytkownika" (mouse capture takes native text selection from the terminal). The document uses assumption Z1 until the person answers.

---

## Product Contract

### Summary

The wheel moves the selection in lists and scrolls conversations, in the panel under the pointer. A left click on a panel gives it the focus; a left click on a list item also selects that item, as `j`/`k` would. All keys work as before.

### Problem Frame

The TUI is keyboard-only today: `src/tui/mod.rs` says "nothing depends on a mouse", and the loop in `run` handles only key and paste events. An operator who comes from a mouse-driven terminal tool reaches for the wheel first. Today the wheel does nothing in the alternate screen, or the terminal turns it into arrow keys, which moves the wrong thing. The focus model (panel focus, `Focus::List` / `Focus::Details`, *Przebieg* two levels, a bracketed or barred selected item, see `src/tui/selection.rs`) is already explicit, so a click can map onto it.

### Key Decisions

- **The pointer picks the target, not the focus.** The wheel acts on the panel under the pointer, so the operator can scroll a panel that does not have the keys. Governs R3, R5.
- **The wheel over a list moves the selection; it does not scroll a viewport past the selection.** This matches "jump through list options" and keeps one cursor per list. Governs R3.
- **A click reuses the existing selection and focus transitions.** A click does what the equivalent keys would do (focus change, select item), so side effects (details load, follow mode, timeline freeze) stay the same. Governs R6, R7, R8.
- **The keyboard stays complete.** No action becomes mouse-only. Governs R11.

### Requirements

**Mouse reporting**

- R1. The TUI turns on terminal mouse reporting when it takes the screen, and turns it off on every exit path, including the panic path, together with the other terminal modes (`restore_terminal`).
- R2. The TUI ignores mouse events that this plan does not use (move, drag, right and middle button, release) without any visible effect.

**Wheel**

- R3. Over a list (the task list, the history list, the step axis on *Przebieg*, the chooser lists: the menu, the answer panel, the new task, the repositories, the queues), one wheel notch moves the selection by one item, up or down, and stops at the first and the last item.
- R4. Over a conversation or another read-only content window (*Rozmowa*, the conversation of a step on *Przebieg*, *Zdarzenia*, *Dyrygent*, *Stan*, *Info*, the event detail), one wheel notch scrolls the content by a fixed small number of lines and stops at the top and the bottom, with the same limits as the line keys of that window.
- R5. The wheel changes the panel under the pointer. It does not move the keyboard focus to that panel.
- R6. Wheel scrolling up in a conversation that follows the newest entry stops the follow, as scrolling up with the keys does; scrolling back to the bottom behaves as the keys do there.

**Click**

- R7. A left click inside a panel gives that panel the keyboard focus (for example the task list or the details), with the same visible focus frame as a key change.
- R8. A left click on an item of a list also selects that item: it gets the selection bracket or bar, and every effect of a key selection follows (for example the details of the clicked task open).
- R9. A left click on a step on *Przebieg* puts the focus on the step axis and selects that step; a left click in the conversation of the selected step puts the focus in that conversation (the second focus level). A click on an entry in a conversation selects that entry where the keys `j`/`k` can select entries.
- R10. A click on empty space in a panel, on a frame, on the header or on the shortcut bar changes only the panel focus, or nothing where there is no panel. It never runs an action.

**Compatibility**

- R11. All keys keep their current behavior; the mouse adds input and removes none.
- R12. In the narrow layout (`Screen::Full`, under 100 columns) the mouse acts only on the one panel on screen. On the "terminal too small" screen the mouse does nothing.
- R13. While a modal form or chooser covers part of the screen, a click or a wheel notch outside it does not reach the panels under it.
- R14. `docs/user/tui.md` describes the mouse behavior, and the module comment in `src/tui/mod.rs` no longer says that nothing uses the mouse.

### Acceptance Examples

- AE1. **Covers R3, R5.** Given the focus is in the details and the pointer is over the task list, when the operator turns the wheel down two notches, then the selection on the list moves two tasks down, the details show the newly selected task, and the focus frame stays on the details.
- AE2. **Covers R4, R6.** Given *Rozmowa* follows the newest entry, when the operator turns the wheel up over it, then the content scrolls up and the follow stops; new entries do not pull the view down.
- AE3. **Covers R7, R8.** Given the focus is on the list, when the operator clicks the second line of the third task, then the third task is selected and the focus stays on the list.
- AE4. **Covers R9.** Given *Przebieg* is open with the focus on the list, when the operator clicks the step `review_api`, then the focus moves to the details, the step axis has the keys, and `review_api` is selected.
- AE5. **Covers R13.** Given the actions menu (`.`) is open, when the operator clicks a task on the list behind it, then nothing changes behind the menu.
- AE6. **Covers R3.** Given the first task is selected, when the operator turns the wheel up over the list, then the selection stays on the first task.

### Scope Boundaries

Deferred for later:

- Clicks on tab names (`Przebieg`, `Rozmowa`, …) to switch tabs.
- Double-click as `Enter` (open, confirm).
- Clicks on links (`PR !482 ↗`) and on form fields, buttons in forms.
- Drag to select or to move tasks in the queue; drag of the panel border.
- Right-click menus, hover highlight, middle-click paste.
- A setting in the configuration file that turns the mouse on or off (see Q1).

### Założenia do potwierdzenia

- Z1. Mouse capture is on by default every time the TUI starts. Most terminals still allow native text selection with `Shift`+drag (or `Option`/`Fn` on macOS) while capture is on; `docs/user/tui.md` says so. If the person picks another answer to Q1, R1 changes.
- Z2. "Jump through list options" means the wheel moves the selected item, not only the visible window (R3). Where a list has no selection (read-only content), the wheel scrolls the content (R4).
- Z3. The "active element" is the pair the TUI already tracks: the focused panel and the selected item in it. A click sets both (R7, R8); it does not run the item's action.
- Z4. The wheel acts on the panel under the pointer and does not move the focus (R5), as in most terminal tools with panels (for example `lazygit`).
- Z5. One wheel notch moves one list item (R3). For content windows the step is a small fixed number of lines (about 3), chosen in planning.
- Z6. The first release covers every list and content window that has a selection or scroll today. A panel that is hard to hit-test may come later, but the task list, *Przebieg* (steps and conversation) and *Rozmowa* are required.

### Outstanding Questions

Resolve Before Planning:

- Q1. See "Pytania otwarte dla użytkownika".

Deferred to Planning:

- How each panel exposes the screen area of its items for hit-testing (the bracketed selection adds rule rows, and items take several rows), and where the router lives.
- The exact line step of R4 and whether fast wheel bursts need coalescing to keep redraws cheap.
- How to test mouse routing without a terminal (state tests in the style of `tests/tui_state.rs` and render tests in `tests/tui_render.rs`).

### Pytania otwarte dla użytkownika

- Q1. Mouse capture takes text selection away from the terminal: a normal drag no longer selects text for copying (only `Shift`+drag does in most terminals). How should the TUI handle this?
  - (recommended) Mouse on by default; a key toggles it while the TUI runs; `docs/user/tui.md` describes `Shift`+drag.
  - Mouse on by default, no toggle; `Shift`+drag is the only way to copy.
  - Mouse off by default; the operator turns it on with a key or a configuration setting.

### Sources / Research

- `src/tui/mod.rs` — the event loop (`run`) handles `Key` and `Paste` only; terminal modes are set in `TerminalGuard::take_over` and undone in `restore_terminal`.
- `src/tui/selection.rs` — bracketed and barred selection, which changes the rows an item takes.
- `src/tui/scroll.rs` — `ScrollState`, the shared scroll of read-only windows.
- `src/tui/task_app/layout.rs` — screen geometry (`areas`, split and full layouts), the base for hit-testing.
- `src/tui/task_app/mod.rs` — `Focus`, `StepFocus`, `View`.
- `docs/user/tui.md` — the user reference that R14 updates.
- Dependencies: `crossterm 0.29`, `ratatui 0.30` (Cargo.toml) already support mouse events.
