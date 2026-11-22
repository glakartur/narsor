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
- **Means:** Reuse the tab bar's marker technique. The strip already has a rule row below the steps; draw the heavy segment in that row under the selected step, and remove `Styles::selected_step` so the name uses `selected_tab` (KTD3).
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

Product Contract preservation: Product Contract unchanged.

### Key Technical Decisions

- KTD1. **`step_strip_line` also returns the selected name's start and width.** It returns them in cells, relative to the steps' area, next to the `Line`, as `tab_bar` in `src/tui/task_app/detail.rs` does. The layout loop already tracks `used`; the name starts at `used + gap + 2` (glyph and its space; `chip_widths` already counts them as 2) and is `chip_widths[selected] - 2` wide. Here "name" is the styled span that `step_name` builds: the step icon when there is one, its space, and the label (for example `✨ review #1`). A `…` prefix is already in `used`. With no steps (`brak kroków`) there is no selection to return, so no segment is drawn (R5). This keeps one layout pass as the only source of positions, so the segment cannot drift from the names (R2, R4).
- KTD2. **`render_step_strip` builds the bottom rule with a segment.** It offsets the start by the same 2 cells as the steps' area and clamps start and end to `2 + room`, where `room` is the steps' area width (R4). It picks the segment style by the R3 rule with its own `focused` and `stale` arguments. The rest of the rule keeps `Role::Done` (A4). The strip's `focused` argument is the same flag the tab bar gets in `render_view`, so the two cues change focus together.
- KTD3. **Remove `Styles::selected_step`; `step_chip` calls `Styles::selected_tab`.** This is the KD2 choice. `selected_step` has one caller (`step_chip`), and without the underline it would only alias `selected_tab`. Removing it also removes the doc comment that says the strip has no marker row.
- KTD4. **The segment clamp and style choice are written again in `conversation.rs`, not shared with the tab bar.** The duplicated part is about five lines. A shared helper would change the tab bar code, and the stop conditions put tab bar changes out of scope.

### Assumptions

- The status glyphs from `progress::status_look` (`✓`, `✗`, `●`, `?`) are one cell wide. The existing layout depends on this already (`2 + cells(name)` per chip). KTD1 depends on it in the same way.

### Scope Boundaries (planning)

- Considered and not built: a shared "marker segment" helper for the tab bar and the strip (KTD4). It would be worth doing if a third marker row appears.

---

## Implementation Units

### U1. Heavy segment under the selected step, no underline

- **Goal:** The strip marks the selected step like the open tab: bold coloured name and `━` under it in the bottom rule.
- **Requirements:** R1–R6, AE1–AE3. KD1, KD2.
- **Dependencies:** None.
- **Files:**
  - `src/tui/task_app/tabs/conversation.rs` — `step_strip_line`, `render_step_strip`, `step_chip`, and the doc comment on `render_step_strip` (it says "bold and underlined").
  - `src/tui/style.rs` — remove `selected_step` (KTD3).
  - `tests/integration/tui_render.rs` — strip render tests.
- **Approach:**
  1. In `step_strip_line`, return the selected name's start and width with the line (KTD1).
  2. In `render_step_strip`, build the third row with `rule_line(area.width, done, Some(segment), None)` when there is a selection (KTD2). The rows loop stops at `area.height`, so a strip lower than 3 rows draws no rule and no segment (R5). Keep the steps' row drawn after the rules.
  3. In `step_chip`, use `styles.selected_tab(role)` for the selected name (R1, KTD3). Remove `Styles::selected_step`.
  4. Update the `render_step_strip` doc comment to describe the new cue.
- **Patterns to follow:** The tab bar marker in `render_view` (`src/tui/task_app/detail.rs`): `tab_bar` returns start and width, and the caller clamps them to the names' room and builds the segment style.
- **Test scenarios** (in `tests/integration/tui_render.rs`, beside the existing strip tests. The direct `render_step_strip` tests catch layout and style on the strip. The full-screen tests catch the wiring in `render_view`. Neither catches what a real terminal does with wide emoji.):
  - Covers AE1. Update `the_conversation_tab_has_the_steps_newest_first_between_two_rules`: the selected `✨ review #1` name is bold and not underlined. The row under the steps has `━` on exactly the buffer columns of `✨ review #1` (icon included, KTD1), with the focus foreground. No `━` is under the glyph or under `plan #1`. The existing "full rule" check on that row must accept `━`, so change it to allow `━` only in the selected name's cells. Do not remove it.
  - Covers AE2. Update `right_shows_the_older_step_and_left_the_newer_one`: after `Right`, `━` is under `✨ plan #1` and not under `✨ review #1`. This replaces its underline assertion.
  - Covers AE3. New test with 12 steps in a 40-cell strip, built like `a_narrow_strip_keeps_the_selected_step_in_view`, with the oldest step selected so the line starts with `…`: the `━` cells in row 2 are exactly the buffer columns of `✨ step0 #1` in row 1, and no `━` goes past column `2 + 37`.
  - Edge (R5). In the same new test, as a table: a strip of height 2 draws no `━`, and a strip with no steps (`brak kroków`) has a bottom rule of `─` only.
  - R3 and R6. In the same table: with `focused` false the segment is `━` in bold `Role::Done`. With `Styles::new(true)` (no colour) the name is bold and the segment is still `━` against `─`.
- **Verification:** On *Rozmowa* the selected step has no underline and a `━` segment under its name that follows `h` / `l` and follow mode. The tests above pass.

---

## Verification Contract

| Check | Command or step | Proves |
|---|---|---|
| Workspace tests | `cargo test --workspace` | R1–R6 through U1's tests. No other test broke. |
| Build | `cargo build --workspace --bins` | `selected_step` has no other caller left. |
| Manual look (optional) | Run `narsor`, open a task with several steps on *Rozmowa*, press `h` / `l`, `f`, and move the keys to the list | The segment follows the selection and turns grey when the strip loses the keys. Run it once with `NO_COLOR=1` for R6. |

---

## Definition of Done

- R1–R6 hold, and AE1, AE2, AE3, R3 (grey segment), R5 and R6 have tests (U1).
- The two existing tests that asserted the underline now assert the new cue. No assertion was removed without a replacement, and no other test was weakened.
- `Styles::selected_step` is gone, and no doc comment says the strip has an underline or no marker row.
- No code from abandoned attempts stays in the diff.
- `cargo test --workspace` passes.
