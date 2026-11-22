---
title: "Rozmowa: mark the selected step like the open tab - Plan"
type: feat
date: 2026-11-22
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Rozmowa: mark the selected step like the open tab - Plan

## Goal Capsule

- **Objective:** On *Rozmowa*, the operator sees the selected step on the step strip marked in the same way as the open tab in the tab bar directly above it: a changed colour on the name and a heavy segment (`━`) under the name. The underline on the step name goes away.
- **Means:** Reuse the tab bar's marker technique. The strip already has a rule row below the steps; draw the heavy segment in that row under the selected step, and drop the underline from `Styles::selected_step`.
- **Product authority:** The request from the operator (2026-11-22). Assumptions taken without the operator are in "Założenia do potwierdzenia".
- **Open blockers:** None.
- **Execution profile:** One small PR. TUI-only change in the `narsor` binary (`src/tui/`).
- **Stop conditions:** Stop and ask if the change needs a new row on the strip, a change to the tab bar, or a change to which steps the strip shows. Those are out of scope.
- **Who finishes:** The implementer (`ce-work` or a person) builds, tests and opens the PR.

---

## Product Contract

### Summary

The step strip on *Rozmowa* has three rows: a top rule with the follow label (`↧ śledzi [f]` / `⏸ ręcznie [f]`), the steps, and a bottom rule. Today the selected step has the open tab's look (bold, focus colour) plus an underline, because the strip "has no marker row of its own". The bottom rule is that marker row. The selected step gets the tab bar's look: bold coloured name, no underline, and a heavy segment in the bottom rule under the name.

### Problem Frame

The tab bar and the step strip sit one above the other and both show "one item of a row is selected". They use two different cues: a heavy segment under the tab, and an underline under the step. The operator reads them as two different things. One cue for both makes the screen consistent.

### Requirements

- R1. The selected step's name on the strip has the open tab's text style (`Styles::selected_tab`: bold, `Role::Focus` while the strip has the keys, `Role::Calm` otherwise). It has no underline.
- R2. The bottom rule of the strip has a heavy segment (`━`) exactly under the selected step's name: it starts at the name's first cell and is as wide as the name. The rest of the rule stays a light rule (`─`).
- R3. The segment uses the same style rule as the tab bar's segment: `Role::Focus` in bold while the strip has the keys and the data is not stale, `Role::Done` in bold otherwise.
- R4. The segment moves with the selection (`h` / `l` and follow mode), and it stays under the name when the strip scrolls and cuts steps with `…`. The segment never goes past the steps' room, as the tab bar's segment stays inside the names' room.
- R5. When the strip is too low to show the bottom rule (fewer than 3 rows), no segment is drawn; the name keeps the bold coloured look. When the strip has no steps (`brak kroków`), the bottom rule has no segment.
- R6. Without colour (`NO_COLOR`), the cue still works: the name is bold and the segment is `━` against `─`, as in the tab bar.

### Acceptance Examples

- AE1 (Covers R1, R2, R3). Given *Rozmowa* with steps `plan`, `implement`, `review`, `implement` selected and the keys on the strip, when the strip is drawn, then `implement` is bold in the focus colour without an underline, and the bottom rule has `━` in the focus colour under exactly the cells of `implement`.
- AE2 (Covers R4). Given AE1, when the operator selects `review`, then the segment is under `review` and no `━` stays under `implement`.
- AE3 (Covers R4). Given more steps than fit on the strip and the last step selected, when the strip is drawn with `…` at the start, then the segment is under the last step's name.

### Scope Boundaries

- No change to the tab bar, its segment or its rule.
- No change to the top rule or the follow label of the strip.
- No change to which steps the strip shows, their order, the separator or the `…` logic.
- No new row on the strip (`STEP_STRIP_ROWS` stays 3).
- No change to the *Przebieg* axis selection (bracket marks) or the web UI.

### Key Decisions

- KD1. Reuse the existing bottom rule of the strip as the marker row, and reuse `rule_line` with its `segment` argument. This is the tab bar's technique, so the two cues are drawn by the same code and look the same.
- KD2. `Styles::selected_step` stops adding `UNDERLINED`. Either it returns `selected_tab(role)` or the chip calls `selected_tab` directly and `selected_step` is removed; the implementer picks. The doc comment on it must not say the strip has no marker row.

### Założenia do potwierdzenia

- A1. "Pogrubiona ramka pod wybraną pozycją" means the heavy segment (`━`) in the rule row under the tab bar, not a full box around the item. Reason: that is what the tab bar draws today (`rule_line` with a segment in `src/tui/task_app/detail.rs`).
- A2. The segment is under the step name only, not under the status glyph before it. Reason: the tab segment covers exactly the styled tab name, and on the strip the styled part is the name; the glyph keeps its own status colour.
- A3. The segment style follows the tab bar rule (focus colour while the strip has the keys and the data is fresh, grey otherwise), with the strip's own focus flag, not the tab bar's.
- A4. The rest of the bottom rule keeps its current grey (`Role::Done`) style. Only the segment changes colour. Reason: the request is about the selected item's cue, not the frame of the strip.
- A5. The change is not visible enough to need a note in `docs/user/tui.md`; that document does not describe the strip's selection cue today.

### Pytania otwarte dla użytkownika

None. Each unclear point has a safe assumption above.

### Sources

- `src/tui/task_app/detail.rs` — `tab_bar` returns the selected tab's start and width; the caller draws `rule_line(..., Some(segment), label)` under the names. `rule_line` draws the `━` segment.
- `src/tui/task_app/tabs/conversation.rs` — `render_step_strip` draws the three strip rows; `step_strip_line` lays out the steps with `…`; `step_chip` styles the selected name with `Styles::selected_step`.
- `src/tui/style.rs` — `selected_tab` (bold) and `selected_step` (bold plus underline).

---

## Planning Contract

### Notes for planning

- `step_strip_line` must also return where the selected name starts and how wide it is (in cells, relative to the steps' area), like `tab_bar` does. The glyph and its space come before the name inside a chip.
- The steps' area starts 2 cells right of the strip's left edge and is `width - 3` wide; the segment offset in the rule must add the same 2 and be clamped to the room (R4), as in the tab bar code.
- Tests: one test that renders the strip into a test buffer and checks AE1 (the `━` cells match the selected name's cells, the name has no underline), and one for AE3 or the narrow case (R5). Add them where the existing strip or tab bar render tests live.

---

## Definition of Done

- R1–R6 hold, and AE1 plus one edge case (AE3 or R5) have tests.
- No other test was weakened.
- `cargo test --workspace` passes.
