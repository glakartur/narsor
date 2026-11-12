---
date: 2026-11-12
topic: tui-ikony-typow-kroku-profile-i-script
---

# Ikony typów kroku: profiler i krok shellowy

## Summary

Krok `profile` dostaje w TUI ikonę `⚖️`. Krok `script` zmienia ikonę z `📜` na `💻`. Reszta ikon zostaje bez zmian.

---

## Problem Frame

Funkcja `step_icon` (`crates/narsor-protocol/src/api/timeline.rs`) mapuje typ kroku na ikonę. Krok `profile` nie ma dziś ikony (`None`), więc w Przebiegu i w Rozmowie wygląda jak krok nieznanego rodzaju. Krok `script` ma `📜`. Ta ikona nie mówi, że krok uruchamia polecenie w shellu.

## Założenia (przyjęte bez pytania do osoby)

- A1. „Profiler” to krok `type: profile`. Jest to jedyny krok, który ocenia złożoność i wybiera model oraz effort (`docs/brainstorms/2026-11-11-krok-profile-zamiast-profilowania-przez-dyrygenta-requirements.md`).
- A2. „Krok shellowy” to krok `type: script`. W workflow nie ma osobnego typu `shell`. Do weryfikacji przez autora, jeśli powstanie inny typ shellowy.
- A3. Ikona `⚖️` jest jednym symbolem. Zapis z selektorem wariantu (U+FE0F) jest zgodny z tym, jak podał go autor zlecenia. Szerokość ikony w układzie TUI musi być policzona tak jak dla innych emoji.
- A4. Ikona `profile` to jedna ikona, bez drugiej. Zasada „kto + co” z `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md` dotyczy `agent`, `script` i `decision`. Krok `profile` nie ma wykonawcy w tym sensie, jak krok `wait`.

## Requirements

- R1. Krok `type: profile` ma ikonę `⚖️` przed nazwą, w każdym miejscu, w którym dziś widać ikonę kroku: oś Przebiegu, nagłówek kroku, widok Rozmowa (pasek kroków i nagłówki w transkrypcie), lista zadań i nagłówek zadania.
- R2. Krok `type: script` ma ikonę `💻` zamiast `📜`, w tych samych miejscach.
- R3. Ikony `✨` (agent), `✋` (wait), `⚡🔀` i `🧑🔀` i `🔀` (decision) nie zmieniają się.
- R4. Kroki `human`, `none`, stan końcowy i nieznany rodzaj dalej nie mają ikony.
- R5. Jedna funkcja `step_icon` pozostaje jedynym źródłem mapowania. Widoki nie dodają własnych wyjątków.
- R6. Żadna część TUI, test ani dokument nie zostaje z `📜` jako ikoną kroku `script`.

## Success Criteria

- Operator widzi `⚖️ profile #1` i `💻 test #1` w Przebiegu i w Rozmowie.
- Układ osi kroków i nagłówków nie łamie się przez szerokość `⚖️` (wiersze dalej mieszczą się w szerokości terminala).
- Testy ikon (`crates/narsor-protocol/src/api.rs`) i testy TUI (`tests/integration/tui_state.rs`, `tests/integration/tui_render.rs`) oczekują nowych ikon.

## Scope Boundaries

- Nie zmieniamy sposobu pokazywania statusu, numeracji ani nazw kroków.
- Nie dodajemy klucza ikon ani trybu bez emoji. Zostaje to w pozycjach otwartych planu ikon z 2026-11-07.
- Nie dodajemy nowego typu kroku `shell`.

## Dependencies / Assumptions

- Cursor i `Position` przenoszą `action_type = "profile"` (typ kroku `profile` istnieje w `crates/narsor-engine/src/domain/workflow_definition.rs`). Do potwierdzenia w planie: czy wszystkie widoki dostają ten typ, czy któryś filtruje go jak „nieznany”.
- Plan ikon `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md` (KTD3, AE1, R2) opisuje `📜`. Plan zmiany powinien zaktualizować te zapisy albo je oznaczyć jako historyczne.

## Outstanding Questions

### Deferred to Planning

- Jak mierzona jest szerokość `⚖️` (znak z selektorem wariantu) w obliczeniu szerokości paska kroków (`chip_widths`) i czy w popularnych terminalach nie rozjeżdża kolumn.
