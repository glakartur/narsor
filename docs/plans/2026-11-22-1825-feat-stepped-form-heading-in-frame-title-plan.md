---
title: "Multi-step form: heading in the frame title - Plan"
type: feat
date: 2026-11-22
topic: stepped-form-heading-in-frame-title
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
---

# Multi-step form: heading in the frame title - Plan

## Goal Capsule

- **Objective:** The action panel takes 2 fewer rows. The path and the step indicator (`Akcje › Archiwizuj · krok 2 z 2`) move from the first row inside the frame to the frame title. The operator sees more of what the agent wrote while the panel is open.
- **Product authority:** The request from the operator (2026-11-22). Assumptions taken without the operator are listed in "Założenia do potwierdzenia".
- **Open blockers:** None.

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

## Założenia do potwierdzenia

1. **Task title placement.** The task title stays in the frame title, after the step indicator: `Akcje › Archiwizuj · krok 2 z 2 · <task>`. Reason: this keeps the information that the frame shows today. The operator described the view without a task title, so this is not stated in the request.
2. **Truncation on a narrow terminal.** When the frame title does not fit, it is cut at the right end, as the frame title is cut today. The task title goes first, then the step indicator. The path start (`Akcje › …`) stays visible. No new truncation rule is added.
3. **Style of the frame title.** The frame title uses the frame style (the focus colour), as other frame titles do. The separate styles of the heading today (bold path, dimmed step indicator) are not kept. If the frame title accepts styled parts with low effort, the implementer can keep the dimmed step indicator.
4. **Gap row.** The empty row under the heading is removed together with the heading (R5). The confirmation step keeps its own empty row above the keys.
5. **Minimum height.** `PANEL_MIN_ROWS` decreases by the 2 rows of the heading, so that the minimum still shows two entries and does not keep the old empty space.

## Pytania otwarte dla użytkownika

None.
