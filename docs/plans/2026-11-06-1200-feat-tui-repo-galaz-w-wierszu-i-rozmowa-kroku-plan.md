---
title: TUI - repozytorium z gałęzią w jednym wierszu, Rozmowa ograniczona do kroku - Plan
type: feat
date: 2026-11-06
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
---

# TUI - repozytorium z gałęzią w jednym wierszu, Rozmowa ograniczona do kroku - Plan

## Goal Capsule

- **Objective:** The operator chooses a repository and its branch in one compact control, and reads the conversation of one workflow step at a time with a visible strip of steps.
- **Product authority:** Two TUI-only changes. No protocol, server or CLI change.
- **Open blockers:** None.

---

## Product Contract

### Problem Frame

1. The new-task form shows the repository (`‹ repo ›`) and its branch as two separate fields, each with a header row, and the branch with a suggestion row. It takes much vertical room for one decision.
2. *Rozmowa* shows the whole conversation of all steps, with headings. In a long task the operator scrolls through steps they do not need. *Przebieg* already reads one step, but it shows the timeline next to it.

### Key Decisions

- Repository and branch stand on one row: `> ‹ repo ›   gałąź: [branch          ]`. (session-settled: user-directed — chosen over separate field rows: the user asked for one control.) Governs R1, R2.
- The branch is, for now, a plain typed field: no suggestion row is drawn. (session-settled: user-directed — chosen over a visible suggestion list: the user said "na razie jako pole do wpisania".) Governs R2.
- *Rozmowa* shows only the selected step; `←` / `→` change the step. (session-settled: user-directed — chosen over the whole conversation with headings: the user asked for it.) Governs R3, R4, R5.

### Requirements

- R1. A `repository` select field and the branch field that follows it (`branch_field_name(input)`) draw as one row under one header (the repository's). The two stay two focus stops (`Tab` / `↓` go from repository to branch), so the existing keys keep working.
- R2. The branch box has the editing background (`Styles::field_editing`) always, so it reads as an input; it stretches to the end of the row minus a gap of one cell. With focus it shows the cursor; a text longer than the box scrolls to keep the cursor in view. The label `gałąź:` takes the focus colour while the branch has focus. A complaint of either field is shown under the row.
- R3. *Rozmowa* layout under the tab bar: a full-width separator, a strip of steps, a full-width separator, then the conversation of the selected step.
- R4. The strip lists step entries newest first (`aktualny · krok-1 · krok-2`), separated by a small ` · `. Every step has its status glyph. The selected step has the selected-tab style (focus colour while the details have focus). A strip wider than the area scrolls to keep the selection in view, with `…` where steps are cut.
- R5. `←` selects a newer step (left), `→` an older one (right). The conversation shows only the selected step entry (`Scope::Step`), with its workflow choices.
- R6. The strip follows the newest step while the newest step is selected; a step the operator picked further back stays selected when a new one starts. Selecting the newest step again resumes the following.

### Assumptions

- *Rozmowa* keeps its own selection, apart from *Przebieg*'s, so the two tabs do not move each other.
- The conversation reader keeps its state per step (cursor, expanded entries, following the end) while the operator moves between steps.
- The branch suggestions are still read from the server and `→` at the end of the branch text still completes from them; only the list row is not drawn.

---

## Implementation Units

### U1. Repository and branch on one row

- **Files:** `src/tui/task_app/new_task/layout.rs`, `src/tui/form/field.rs`, `src/tui/input_form.rs`.
- **Approach:** In `render_column`, when field `i` is a select and field `i + 1` is named `branch_field_name(field i)`, draw one header (the repository's) and one value row built by a new `repository_branch_line` in `form/field.rs`. Row counting treats the pair as one header plus one row plus complaints. `value_rows` / `value_lines` stop drawing the suggestion row (the branch is the only `Suggest` field). The branch field description is dropped (it described the suggestion list).
- **Tests:** `tests/integration/tui_render.rs` — the repository and the branch are on the same row; the branch box has the editing background to one cell before the edge; a focused branch shows the cursor and the focused `gałąź:` label; a long branch keeps the cursor in view; no suggestion row is drawn.

### U2. *Rozmowa* of the selected step

- **Files:** `src/tui/task_app/tabs/conversation.rs`, `src/tui/task_app/detail.rs`, `src/tui/keymap.rs`, `src/tui/task_app/history.rs` (unchanged API use).
- **Approach:** `Conversation` keeps the step list (from the positions), the selected label, a follow flag and one `TranscriptState` per step label. `rebuild_with_events` builds `Scope::Step` of the selected step. `transcript()` / `transcript_mut()` return the selected step's reader. New `Conversation::select_newer` / `select_older`. New actions `NewerStep` / `OlderStep` bound to `←` / `→` in `Context::AgentConversation`; `TaskDetail::on_action` moves the selection and rebuilds. `conversation::render` draws separators, the strip, then the notes and the conversation.
- **Tests:** `tests/integration/tui_render.rs` — the strip is between two full-width separators, newest first, with the selected step marked; the content shows only the selected step; a narrow strip keeps the selection in view. `tests/integration/tui_state.rs` — `←` / `→` change the step and the content; a new step is followed only while the newest is selected. Existing *Rozmowa* tests that asserted step headings are adapted to the step scope.

## Verification

`cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`; a manual look at the TUI where possible.
