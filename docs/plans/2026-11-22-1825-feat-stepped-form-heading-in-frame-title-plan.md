---
title: "Multi-step form: heading in the frame title - Plan"
type: feat
date: 2026-11-22
topic: stepped-form-heading-in-frame-title
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Multi-step form: heading in the frame title - Plan

## Goal Capsule

- **Objective:** The action panel takes 2 fewer rows. The path and the step indicator (`Akcje › Archiwizuj · krok 2 z 2`) move from the first row inside the frame to the frame title. The operator sees more of what the agent wrote while the panel is open.
- **Product authority:** The request from the operator (2026-11-22). Assumptions taken without the operator are listed in "Założenia do potwierdzenia".
- **Means:** The owner of the form builds the frame title from the path and step text of the form. The form stops drawing its own heading (Key Decision, KTD1).
- **Open blockers:** None.
- **Execution profile:** One PR, Lightweight. Two implementation units, U1 then U2.
- **Stop conditions:** Stop and ask when the change needs new keys, a new step or a change to the text of a step (Scope Boundaries), or when a frame other than the action panel must change.
- **Who finishes:** `ce-work` builds the change, verifies it with `cargo test --workspace` and ships the PR.

---

## Product Contract

### Summary

Today the action panel draws a frame titled `Akcje` (or `Akcje · <task>`). Inside the frame, the first row shows the path and the step, and the second row is empty. Then the step content starts. After this change, the frame title shows the path and the step. The step content starts on the first row inside the frame.

Target view (the same confirmation step as in the request):

```
╭ Akcje › Archiwizuj · krok 2 z 2 ─────────────────────────────────────────────╮
│ Archiwizuj                                                                   │
│ Usuwa workload i storage zadania. Zadanie trafia do Historii. Tego nie da    │
│ się cofnąć.                                                                  │
│ ! nieodwracalne                                                              │
│                                                                              │
│  Enter wyślij  Esc wróć                                                      │
╰──────────────────────────────────────────────────────────────────────────────╯
```

### Problem Frame

The panel opens over the detail view of a task. Each row that the panel takes is a row less for the conversation and the agent output. The heading row and the gap row below it repeat information that the frame title can carry. The frame title today shows only `Akcje`, which is also the first part of the path.

### Requirements

**Frame title**

- R1. The frame title of the action panel shows the path of the multi-step form and the step indicator, in the format used today inside the frame: `<path joined by ›> · krok N z M`. Example: `Akcje › Archiwizuj · krok 2 z 2`.
- R2. When the task has a title, the frame title ends with ` · <task title>`, as it does today. `Akcje` is not shown twice.
- R3. The frame title changes with each step and each level of the form: choice, fields and confirmation, and each submenu level.
- R4. When the panel has no entries (the "nothing to choose" or "no data" message), the frame title stays as it is today: `Akcje` or `Akcje · <task title>`. There is no step indicator, because there is no form.

**Content area**

- R5. The heading row and the empty gap row inside the frame are removed. The step content starts on the first row inside the frame.
- R6. The preferred height of the panel decreases by 2 rows for each step. The minimum height still shows at least two entries of a choice step, or the full "nothing to choose" message.
- R7. Each step keeps its content and keys as today: the list and its description pane, the fields with the one-line description, and the confirmation with the name, the description, the risk, the typed values and the keys.

### Acceptance Examples

- AE1. **Covers R1, R5.** On the confirmation step of `Archiwizuj`, the frame title reads `Akcje › Archiwizuj · krok 2 z 2`, and the first row inside the frame is `Archiwizuj`.
- AE2. **Covers R3.** On the first level of a form with a submenu, the title reads `Akcje · krok 1 z 3`. After the operator opens the submenu `Wybierz krok`, it reads `Akcje › Wybierz krok · krok 1 z 3` (or the actual count). After `Esc`, it reads `Akcje · krok 1 z 3` again.
- AE3. **Covers R6.** For the same form and width, the panel is 2 rows lower than before the change, unless the minimum height applies.

### Scope Boundaries

- Only the action panel (`src/tui/task_app/actions.rs`) uses the multi-step form today (`src/tui/stepped.rs`). The change covers that use. Later users of the form (the answer to an agent question, the new-task form) take the same contract when they are built.
- No change to the keys, the step logic, the step count or the text of the steps.
- No change to other frames in the TUI.

### Key Decisions

- The form gives its heading and step text to its owner, and the owner puts them in the frame title. The form no longer draws its own heading row. Reason: the frame belongs to the owner; the form does not know about it.

Product Contract preservation: Product Contract unchanged. Assumption 6 clarifies how AE2 reads with the indicator logic that exists today.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **The owner builds the frame title from the public `SteppedForm::heading()` and `SteppedForm::step_text()`.** Both methods are public already and give the text of R1: `heading()` joins the form title and the path with ` › `, and `step_text()` gives `krok N z M`. The form needs no new API. `stepped::render` stops drawing the heading row. `SteppedForm::preferred_height` stops adding `HEADING_ROWS`, and the constant is removed. Implements the Key Decision; governs R1, R5, R6.
- KTD2. **`render_panel` in `src/tui/task_app/actions.rs` selects the title from `panel.content`.** For `PanelContent::Entries`, the title is `<heading> · <step text>`, followed by ` · <task title>` when the task title is not empty. For `Nothing` and `NoData`, the title stays `Akcje` or `Akcje · <task title>`. The form title is `Akcje` (`SteppedForm::new("Akcje", …)`), so the heading starts with `Akcje` already and the owner does not add it again. Governs R1, R2, R4.
- KTD3. **The title stays one plain string through the existing `framed` helper.** `framed` applies `one_line` to the full title and draws it in the frame style, as Assumption 3 states. The ratatui `Block` title is cut at the right end, so truncation needs no new code (Assumption 2). This change does not keep the dimmed step indicator (see Scope notes).
- KTD4. **`PANEL_MIN_ROWS` changes from 6 to 4.** The choice step draws its list with `chooser::render_footer`. Two entries take 2 rows plus `LIST_FRAME_ROWS` (2 rows), which is 4 rows. The "nothing to choose" message takes 3 rows, so 4 rows still show all of it (R6, Assumption 5). `PANEL_FRAME_ROWS` and the `width - 4` that goes to `preferred_height` do not change.

### Research notes

- The panel is drawn in two places. In the details, `src/tui/task_app/detail.rs` calls `actions::render_panel(frame, place, "", panel, styles)` with an empty task title, so that frame title has no task part. As a popup, `render_panel_popup` gives the task title. Both go through `render_panel`, so one change covers both places.
- `detail.rs` gets the panel height from `actions::panel_height`. The 2 rows that the panel gives back go to the content area of the details without a change in `detail.rs`.
- Related learning: `docs/solutions/design-patterns/tui-panel-actions-from-server-offer-2026-11-13.md` (the server offer and the multi-step form). It has no rule about the heading.

### Scope notes

- Considered and not built: a styled title (bold path, dimmed step indicator). `framed` takes a string today, and Assumption 3 accepts the frame style. This decision changes if the operator asks for the dimmed indicator again.

---

## Implementation Units

### U1. The form stops drawing its heading

- **Goal:** `stepped::render` draws the step content from the first row of its area, and `preferred_height` gives 2 rows less.
- **Requirements:** R5, R6, R7. KTD1.
- **Dependencies:** None.
- **Files:** `src/tui/stepped.rs` (`render`, `preferred_height`, `HEADING_ROWS`, the module and function doc comments).
- **Approach:**
  1. Remove the heading paragraph and the `HEADING_ROWS` offset from `render`. The body area is the full area.
  2. Remove `HEADING_ROWS` from `preferred_height` and delete the constant.
  3. Update the module doc (the drawing at the top) and the doc of `render`: the owner shows the path and the step, for example in its frame title. Keep `heading()`, `step_text()` and `step_position()` as they are.
  4. Remove imports that become unused. `confirm_lines` still uses `Modifier`.
- **Patterns to follow:** the existing `Stage` match in `render`. The step bodies do not change (R7).
- **Test scenarios:** Test expectation: none -- the existing logic tests in `src/tui/stepped.rs` cover `heading()` and `step_text()` and must pass without change. The form has no render test of its own; U2 proves the drawn rows through the panel, and a second render test would check the same rows.
- **Verification:** The form draws no heading row. The step content starts at the top of its area. The build shows no unused-item warnings for this file.

### U2. The action panel shows the path and the step in its frame title

- **Goal:** While the form is shown, the frame title of the action panel is `<heading> · krok N z M`, followed by ` · <task>` when there is a task title. The panel is 2 rows lower.
- **Requirements:** R1, R2, R3, R4, R6. KTD2, KTD3, KTD4. Covers AE1, AE2, AE3.
- **Dependencies:** U1.
- **Files:** `src/tui/task_app/actions.rs` (`render_panel`, `PANEL_MIN_ROWS` and its doc comment); tests in `tests/integration/tui_render.rs` and `tests/integration/tui_state.rs`.
- **Approach:**
  1. In `render_panel`, build the title from `panel.content` as KTD2 states, before the call to `framed`.
  2. Set `PANEL_MIN_ROWS` to 4. Update its doc comment: "two entries, or the message that it has none".
  3. Adjust the existing integration tests that use the row of the old heading only where the new layout moves the expected row, and keep their intent. Examples: the row checks in `wide_the_panel_shows_context_actions_description_and_fields` and `panel_place_on` in `tests/integration/tui_render.rs`.
- **Patterns to follow:** the screen-text helpers `draw_app` and `screen` in `tests/integration/tui_render.rs`, and `row_of` in `tests/integration/tui_state.rs`.
- **Test scenarios:** These tests read the drawn screen. They find where the text is (frame row or content row). They do not check colours.
  - Covers AE1. Extend `the_confirmation_step_says_what_will_happen_and_what_enter_and_esc_do` in `tests/integration/tui_render.rs`: on the confirmation step of `Stop`, the frame row of the panel (the row with `╭` and `Akcje`) contains `Akcje › Stop · krok 2 z 2`, and the next row contains `Stop` as the first content row. When this fixture draws the panel as a popup, the frame row also ends with ` · ` and the task title (R2); the implementer checks which place the fixture uses.
  - Covers AE2. Extend `the_steps_are_a_submenu_in_the_same_panel` in `tests/integration/tui_state.rs`: after the submenu `Wybierz krok` opens, the frame row contains `Akcje › Wybierz krok · krok 1 z`. After `Esc`, the frame row contains `Akcje · krok 1`, and no row contains `Akcje › Wybierz krok`.
  - Covers AE3. For a running task with the panel in the details, `actions::panel_height` is 2 rows less than before the change. Write the expected height as a fixed number for that fixture and width, not as a formula over internals.
  - R4 edge case. When the panel has no entries (`PanelContent::Nothing`), the frame row of the panel (the row with `╭` and `Akcje`) contains `Akcje` and does not contain `krok`. The check is limited to the frame row because the task header can contain `krok` (for example `krok 3 · review`).
- **Verification:** The four scenarios pass. The existing panel tests pass with only row-position adjustments.

---

## Verification Contract

| Gate | Command | Proves |
|---|---|---|
| Build | `cargo build --workspace --bins` | No compile errors and no new warnings in `src/tui/stepped.rs` and `src/tui/task_app/actions.rs`. |
| Tests | `cargo test --workspace` | The U1 logic tests pass without change; the U2 scenarios and the adjusted panel tests pass. CI runs the same command with `--locked`. |

No `#[ignore]`d test is necessary: the change does not start processes, call `claude` or use a cluster.

## Definition of Done

- The frame title of the action panel follows R1–R4 in the popup and in the details.
- The panel has no heading row and no gap row inside the frame. `HEADING_ROWS` is removed. `PANEL_MIN_ROWS` is 4.
- The U2 test scenarios exist and pass. `cargo test --workspace` passes.
- The doc comments in `src/tui/stepped.rs` and on `PANEL_MIN_ROWS` describe the new behavior.
- The diff contains no dead code and no code from abandoned attempts.

---

## Założenia do potwierdzenia

1. **Task title placement.** The task title stays in the frame title, after the step indicator: `Akcje › Archiwizuj · krok 2 z 2 · <task>`. Reason: this keeps the information that the frame shows today. The operator described the view without a task title, so this is not stated in the request.
2. **Truncation on a narrow terminal.** When the frame title does not fit, it is cut at the right end, as the frame title is cut today. The task title goes first, then the step indicator. The path start (`Akcje › …`) stays visible. No new truncation rule is added.
3. **Style of the frame title.** The frame title uses the frame style (the focus colour), as other frame titles do. The separate styles of the heading today (bold path, dimmed step indicator) are not kept. If the frame title accepts styled parts with low effort, the implementer can keep the dimmed step indicator.
4. **Gap row.** The empty row under the heading is removed together with the heading (R5). The confirmation step keeps its own empty row above the keys.
5. **Minimum height.** `PANEL_MIN_ROWS` decreases by the 2 rows of the heading, so that the minimum still shows two entries and does not keep the old empty space.
6. **Step indicator text.** The indicator is the text that the form gives today (`step_text()`). When a submenu entry is highlighted, the total is not known, so the indicator is `krok 1` without `z M`. R1 ("the format used today") and Scope Boundaries ("no change to the step logic, the step count") govern this; the counts in AE2 are examples. So on the first level with `Wybierz krok` highlighted, also after `Esc` from the submenu, the title is `Akcje · krok 1`. When a leaf is highlighted, the title shows its total, for example `Akcje · krok 1 z 3`.

## Pytania otwarte dla użytkownika

None.
