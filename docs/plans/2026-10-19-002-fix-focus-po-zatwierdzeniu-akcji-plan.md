---
title: "fix: Focus zostaje w szczegółach taska po zatwierdzeniu akcji"
type: fix
status: active
date: 2026-10-19
origin: docs/brainstorms/2026-10-19-focus-po-zatwierdzeniu-akcji-requirements.md
---

# fix: Focus zostaje w szczegółach taska po zatwierdzeniu akcji

## Summary

Po udanym zatwierdzeniu akcji z panelu odpowiedzi kod ustawia `Focus::List`. Zmiana: ustawić `Focus::Details`. Reszta (`panel_focus = false`, `back_to_axis`) zostaje. Plan ma jedną jednostkę kodu z testami.

## Problem Frame

Operator odpowiada agentowi i traci focus w szczegółach. Musi wrócić klawiszem. (see origin: `docs/brainstorms/2026-10-19-focus-po-zatwierdzeniu-akcji-requirements.md`)

## Requirements

- R1. Po sukcesie focus zostaje w szczegółach tego samego taska.
- R2. Zaznaczenie na liście się nie zmienia.
- R3. Szczegóły wracają na oś czasu (`back_to_axis`, `panel_focus = false`).
- R4. Przy błędzie focus i stan panelu bez zmian (już działa).
- R5. Reguła dotyczy każdej akcji zatwierdzanej w szczegółach.
- R6. `Esc` i `Tab` bez zmian.

## Findings From Planning (answers to deferred questions)

- **R5 — jedna ścieżka.** `Submit::Input`, `Submit::Operator` i `Submit::Owner` przechodzą przez `answer()` w `src/tui/task_app/waiting.rs`. Potwierdzenie w oknie (`Confirmed::Answer` w `src/tui/task_app/actions.rs`) kończy się też w `answer()`. Jedyne miejsce, które po akcji ustawia `Focus::List`, to `waiting.rs` (ok. linii 1556). Inne `self.focus = ...` w `mod.rs`, `history.rs`, `new_task.rs`, `repositories.rs` należą do innych widoków (poza zakresem).
- **Akcje z listy** (np. pauza, anulowanie przez `action_request`) nie zmieniają focusa. Nie ma tam co poprawiać.
- **Testy do zmiany.** Tylko test `an_answer_sent_from_the_steps_conversation_opens_the_details_again_on_the_axis` w `tests/tui_state.rs` (ok. linii 6549) zakłada `Focus::List` po wysłaniu. Dalej naciska `Enter`, żeby otworzyć szczegóły. Po zmianie ten `Enter` wszedłby w rozmowę kroku, więc test trzeba przepisać. Pozostałe asercje `Focus::List` w pliku dotyczą `Esc`, `Tab`, historii i formularza.

## Scope Boundaries

- Bez zmian: `Esc`, `Tab`, skróty nawigacji, historia, formularz nowego zadania, widok repozytoriów.
- Bez automatycznego przejścia do następnego taska (`NextWaiting`).

## Key Technical Decisions

- Focus ląduje na osi czasu (`Focus::Details` + `back_to_axis`), nie w polu odpowiedzi. Założenie z dokumentu źródłowego: Artur potwierdza, że „szczegóły" to oś czasu. Jeśli woli panel, zmienia się tylko `panel_focus` i R3.

---

## Implementation Units

### U1. Zostaw focus w szczegółach po udanej akcji

**Goal:** Po `Ok(())` w `answer()` focus to `Focus::Details`.

**Requirements:** R1, R2, R3, R5, R6

**Dependencies:** brak

**Files:**
- Modify: `src/tui/task_app/waiting.rs` (gałąź `Ok(())` w `answer`; komentarz przy funkcji, jeśli opisuje focus)
- Test: `tests/tui_state.rs`

**Approach:**
- Zamienić `Focus::List` na `Focus::Details` w gałęzi sukcesu. Nie ruszać `settle_answer`, `back_to_axis` ani gałęzi `Err`.
- Zaznaczenie listy nie jest tu zmieniane, więc R2 jest spełnione bez kodu. Test ma to potwierdzić.

**Patterns to follow:** `open_selected` w `src/tui/task_app/waiting.rs` (ustawia `Focus::Details`, `panel_focus = false`, `back_to_axis`).

**Execution note:** Najpierw zmień asercję w istniejącym teście (czerwony), potem kod.

**Test scenarios:**
- Covers AE1. Happy path: task czeka, focus w panelu, `Enter` z odpowiedzią i udane wysłanie → `focus() == Focus::Details`, `step_focus() == StepFocus::Axis`, `panel_focused()` fałsz, `list().selected_id()` bez zmian. Przepisać `an_answer_sent_from_the_steps_conversation_opens_the_details_again_on_the_axis`: zamiast `Enter` po wysłaniu sprawdzić, że szczegóły są już otwarte na osi.
- Covers AE2. Error path: wysłanie kończy się błędem → focus i `panel_focused()` bez zmian, szkic odpowiedzi nadal jest (`detail().has_drafts()`). Sprawdzić, czy istnieje taki test; jeśli nie, dodać.
- Integration (R5): akcja operatora lub właściciela zatwierdzona w oknie potwierdzenia (`Confirmed::Answer`) → po sukcesie `Focus::Details`.
- Covers AE3. `Esc` w szczegółach po akcji → `Focus::List` (istniejące testy `Esc` zostają zielone).

**Verification:** Testy TUI przechodzą (`tests/tui_state.rs`). Po udanej akcji focus jest w szczegółach na osi czasu. Zaznaczenie listy się nie zmienia.

---

## Risks

- Niski. Po sukcesie panel może nadal pokazywać kolejne oczekiwanie tego samego taska. `panel_focus = false` zostaje, więc operator przechodzi do panelu przez `Tab` jak dziś.

## Deferred to Implementation

- Dokładne nazwy pomocników testowych do symulacji błędu wysyłania. Należy użyć tych, które już są w `tests/tui_state.rs`.
