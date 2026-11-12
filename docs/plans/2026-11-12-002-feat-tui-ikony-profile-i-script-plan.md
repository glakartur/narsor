---
title: "feat: Ikony kroków profile (⚖️) i script (💻) w TUI"
type: feat
status: active
date: 2026-11-12
origin: docs/brainstorms/2026-11-12-tui-ikony-typow-kroku-profile-i-script-requirements.md
---

# feat: Ikony kroków profile (⚖️) i script (💻) w TUI

## Summary

Funkcja `step_icon` dostaje ikonę `⚖️` dla kroku `profile`. Ikona kroku `script` zmienia się z `📜` na `💻`. Widoki nie wymagają zmian w logice, bo biorą ikonę tylko z `step_icon`. Zmiana obejmuje testy i zapisy `📜` w dokumentach. Jedno ryzyko to szerokość `⚖️` (znak z selektorem wariantu) w terminalu. Plan ma 2 jednostki. Rozmiar: Lightweight.

---

## Problem Frame

Krok `profile` nie ma ikony, więc w Przebiegu i w Rozmowie wygląda jak krok nieznanego rodzaju. Ikona `📜` kroku `script` nie mówi, że krok uruchamia polecenie w shellu (see origin: `docs/brainstorms/2026-11-12-tui-ikony-typow-kroku-profile-i-script-requirements.md`).

---

## Requirements

- R1. Krok `profile` ma ikonę `⚖️` we wszystkich miejscach, w których widać ikonę kroku.
- R2. Krok `script` ma ikonę `💻` zamiast `📜`.
- R3. Ikony `✨`, `✋`, `⚡🔀`, `🧑🔀`, `🔀` nie zmieniają się.
- R4. Kroki `human`, `none`, stan końcowy i nieznany rodzaj nie mają ikony.
- R5. `step_icon` jest jedynym źródłem mapowania.
- R6. Po zmianie `📜` nie zostaje jako ikona kroku `script` w kodzie, testach ani w aktualnych dokumentach.

Kryteria sukcesu z origin: operator widzi `⚖️ profile #1` i `💻 test #1`; wiersze mieszczą się w szerokości terminala; testy ikon i TUI oczekują nowych ikon.

---

## Scope Boundaries

- Nie zmieniamy statusu, numeracji ani nazw kroków.
- Nie dodajemy klucza ikon ani trybu bez emoji.
- Nie dodajemy typu kroku `shell`.

### Deferred to Follow-Up Work

- Klucz ikon i tryb bez emoji zostają w pozycjach otwartych planu `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md`.

---

## Context & Research

- `crates/narsor-protocol/src/api/timeline.rs`: `step_icon` to jedyne mapowanie. Dziś `("script", _)` daje `📜`, a `profile` wpada w `_ => None`.
- Typ `profile` dociera do widoków: silnik emituje `ActionKind::of(action).name()` (`crates/narsor-engine/src/engine/runner/step/action.rs`), a `ActionConfig::type_name` zwraca `"profile"` (`crates/narsor-engine/src/domain/workflow_definition.rs`). `Position.action_type` i `ExecutionCursor.current_action_type` przenoszą go bez filtra. To zamyka pytanie z origin o filtrowanie. Żaden widok nie filtruje typu: grep po `src/tui` nie znajduje wyjątków dla typów kroku.
- Wszystkie widoki używają `step_icon` przez `Position::icon` i `ExecutionCursor::icon` (`crates/narsor-protocol/src/controller/runner/state.rs`).
- Szerokość: pasek kroków Rozmowy liczy `cells(&step_name(icon, label))` w `src/tui/task_app/tabs/conversation.rs` (`chip_widths`). `cells` to `Span::raw(text).width()` w `src/tui/task_app/detail.rs`, czyli szerokość z biblioteki ratatui 0.30 (unicode-width). `cell_width` w `src/tui/form/layout.rs` zwraca 1, ale dotyczy pól formularza, nie ikon kroków.
- Testy z `📜`: `crates/narsor-protocol/src/api.rs` (`step_icon_names_who_runs_the_step_and_what_it_is`), `tests/integration/tui_render.rs` (oś), `tests/integration/tui_state.rs` (nagłówek w Rozmowie).
- Dokumenty z `📜`: `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md` (KTD3, AE1, AE2, R2, Z2 i inne). Brak zapisu w `README.md`.

Zewnętrzne badania: pominięte. Wzorzec jest lokalny i ma wiele przykładów.

---

## Key Technical Decisions

- KTD1. Zmiana mieści się w `step_icon`. Widoki nie dostają wyjątków (R5).
- KTD2. Ikona `⚖️` jest zapisana ze selektorem wariantu U+FE0F, tak jak podał autor (A3). Szerokość mierzy test, nie założenie (patrz U1).
- KTD3. Plan ikon z 2026-11-07 dostaje adnotację „zastąpione planem 2026-11-12-002 w zakresie 📜”, a jego zapisy `📜` zostają jako historia. Nie przepisujemy planu, który opisuje stan z czasu jego powstania (R6 dla dokumentów: oznaczenie wystarcza).

---

## Implementation Units

### U1. Mapowanie `step_icon` i testy protokołu

- **Goal:** `profile` → `⚖️`, `script` → `💻`, reszta bez zmian.
- **Requirements:** R1, R2, R3, R4, R5
- **Dependencies:** brak
- **Files:**
  - `crates/narsor-protocol/src/api/timeline.rs` (modify: `step_icon` i komentarz)
  - `crates/narsor-protocol/src/api.rs` (modify: testy `step_icon_*`)
- **Approach:** dodać gałąź `("profile", _)` i zmienić gałąź `("script", _)`. Zaktualizować komentarz funkcji, jeśli wymienia rodzaje bez ikony.
- **Patterns to follow:** istniejące gałęzie `match` w `step_icon`.
- **Test scenarios:**
  - Happy path: `step_icon(Some("profile"), None)` == `Some("⚖️")`. `step_icon(Some("script"), None)` == `Some("💻")`.
  - Edge: `profile` z `decision_by = Some("human")` dalej daje `⚖️` (R5, `decision_by` nie wpływa).
  - Edge: `agent`, `wait` i trzy ikony `decision` dalej daje to samo (R3).
  - Edge: `human`, `none`, `terminal`, `None` dalej dają `None` (R4). `profile` nie trafia do tej listy.
- **Verification:** test `step_icon_*` w `crates/narsor-protocol` przechodzi z nowymi ikonami.

### U2. Testy TUI, szerokość `⚖️` i dokumenty

- **Goal:** widoki pokazują nowe ikony, układ się nie łamie, a nie zostaje `📜` jako ikona `script`.
- **Requirements:** R1, R2, R6; kryteria sukcesu (układ)
- **Dependencies:** U1
- **Files:**
  - `tests/integration/tui_render.rs` (modify: `the_axis_shows_the_icon_of_each_kind_after_the_status`: `💻 test #1`, dodać krok `profile`)
  - `tests/integration/tui_state.rs` (modify: nagłówek `── 💻 test #1 ──`, dodać nagłówek kroku `profile`)
  - `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md` (modify: adnotacja historyczna, KTD3)
- **Approach:** w istniejących testach zmienić oczekiwane ikony i dodać przypadek `profile` obok nich. Pomocnik `of_kind` i `app_with_steps` już przyjmują dowolny typ. Dodać test szerokości paska Rozmowy: pasek z krokiem `⚖️` w wąskim terminalu nie przekracza szerokości, a `chip_widths` zgadza się z tym, co ratatui rysuje. Jeśli ratatui liczy `⚖️` inaczej niż rysuje terminal, kosztem jest jedna kolumna różnicy, a decyzję o zastąpieniu zapisu (np. `⚖` bez selektora) podejmuje się w implementacji i zapisuje w opisie PR.
- **Patterns to follow:** istniejące testy osi i paska Rozmowy w `tests/integration/tui_render.rs`.
- **Test scenarios:**
  - Happy path: oś pokazuje `✓ ⚖️ profile #1` i `✓ 💻 test #1` po znaku statusu.
  - Happy path: pasek kroków i nagłówek transkryptu w Rozmowie pokazują `⚖️ profile #1` i `── 💻 test #1 ──` (nagłówek bez wpisu próby też).
  - Edge: pasek Rozmowy z `⚖️` w wąskim terminalu mieści się w szerokości; wybrany krok jest widoczny (`chip_widths`).
  - Integration: lista zadań i nagłówek zadania pokazują `⚖️` dla kursora z `current_action_type = "profile"` (przez `ExecutionCursor::icon`).
  - Regresja R6: wyszukiwanie `📜` w `src`, `crates`, `tests` nie zwraca trafień.
- **Verification:** testy `tests/integration/tui_render.rs` i `tests/integration/tui_state.rs` przechodzą; `📜` zostaje tylko w historycznych dokumentach z adnotacją.

---

## System-Wide Impact

- Zmiana dotyczy tylko znaku w widokach TUI. API i format zdarzeń się nie zmieniają: `action_type` już niesie `profile`.
- Operator z terminalem bez dobrej obsługi emoji może widzieć `⚖️` węższe lub szersze niż liczy TUI. To znane ograniczenie ikon (tryb bez emoji zostaje odroczony).

## Risks

- Szerokość `⚖️` (U+2696 + U+FE0F) różni się między terminalami. Środek zaradczy: test szerokości w U2 i możliwość zapisu bez selektora.

## Deferred / Open Questions

- Do zweryfikowania w implementacji: jak ratatui 0.30 liczy szerokość `⚖️` i czy kolumny rysują się zgodnie z tą szerokością.
- Autor decyduje, czy pojawi się osobny typ `shell` (A2). Plan go nie zakłada.

## Sources & References

- Origin: `docs/brainstorms/2026-11-12-tui-ikony-typow-kroku-profile-i-script-requirements.md`
- `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md`
- `docs/brainstorms/2026-11-11-krok-profile-zamiast-profilowania-przez-dyrygenta-requirements.md`
