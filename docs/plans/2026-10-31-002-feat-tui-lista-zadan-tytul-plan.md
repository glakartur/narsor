---
title: "feat: Lista zadań w TUI pokazuje tytuł zadania zamiast początku opisu"
type: feat
status: active
date: 2026-10-31
origin: docs/brainstorms/2026-10-31-tui-lista-zadan-tytul-zamiast-opisu-requirements.md
---

# feat: Lista zadań w TUI pokazuje tytuł zadania zamiast początku opisu

## Summary

Druga linia wiersza na liście zadań pokazuje `task.title` (tytuł operatora albo tytuł dyrygenta). Gdy tytułu brak, linia pokazuje to, co dziś: `display.title`, a gdy go brak, identyfikator zadania. Zmiana dotyczy tylko wiersza listy i filtra listy. Funkcja `title_of` i jej inni użytkownicy (komunikaty akcji, historia, panel, `mod.rs`) zostają bez zmian (see origin: `docs/brainstorms/2026-10-31-tui-lista-zadan-tytul-zamiast-opisu-requirements.md`).

---

## Problem Frame

Ramka szczegółów pokazuje `task.title`. Wiersz listy pokazuje `display.title`, który często jest początkiem opisu. Operator nie może łatwo powiązać wiersza ze szczegółami (see origin).

Stan kodu:
- `src/tui/task_app/list.rs`: `title_of` zwraca `one_line(display.title)` albo `task_id`. `row_text_with` wpisuje wynik `title_of` do `RowText.title`. `matches_task` przekazuje `title_of` do `matches_filter`.
- `src/tui/task_app/detail.rs`: `edge_layout` czyta `task.title` przez `one_line` i odrzuca tekst pusty po przycięciu. Gdy brak ticketu i tytułu, używa `title_of`.
- Renderer wiersza już skraca tekst do szerokości panelu (`fit`). Wiersz ma trzy linie.

---

## Requirements

- R1. Linia tytułu pokazuje `task.title`, gdy jest ustawiony i niepusty po przycięciu.
- R2. Bez tytułu linia pokazuje `display.title`, a gdy go brak, identyfikator zadania.
- R3. Tytuł dyrygenta pojawia się po następnym odświeżeniu listy, bez akcji operatora.
- R4. Tytuł przechodzi przez `one_line`.
- R5. Długi tytuł jest skrócony tak jak dziś. Jedna linia, wiersz ma trzy linie.
- R6. Filtr `/` dopasowuje tytuł zadania, ticket i pełny identyfikator.
- R7. Wiersz listy i ramka szczegółów pokazują ten sam tytuł.

**Origin acceptance examples:** AE1 (R1), AE2 (R2, R3), AE3 (R5), AE4 (R6).

---

## Scope Boundaries

- Brak zmian w formacie wiersza (ticket, status, czas aktywności, etykieta).
- Brak zawijania tytułu.
- Brak zmian w generowaniu tytułu przez dyrygenta i w szablonach `display`.
- Brak zmian w CLI (`src/cli/task_text.rs`).
- Brak zmian w `title_of` i jego innych użytkownikach (komunikaty akcji w `actions.rs` i `mod.rs`, komunikaty akcji w `list.rs` poza wierszem i filtrem, historia w `history.rs`, panel w `panel/state.rs`, ramka szczegółów).

### Deferred to Follow-Up Work

- Q1 z origin: czy komunikaty akcji, historia i panel mają pokazywać tytuł zadania zamiast `display.title`. Gdy operator to potwierdzi, następna zmiana przenosi logikę nowej funkcji do `title_of`.

---

## Assumptions

Pytania Q1 i Q2 z origin są otwarte. Ten plan przyjmuje założenia z origin (A1–A5). Operator musi je potwierdzić przy przeglądzie.

- A1. „Tytuł” to `task.title` (tytuł efektywny z serwera). Nie jest to `display.title`.
- A2. Bez tytułu wiersz pokazuje to, co dziś.
- A3. Skracanie bez zmian.
- A4 / Q1. Plan zmienia tylko wiersz listy i filtr. Powód: origin ogranicza zakres do R1–R7, a `title_of` ma wielu użytkowników poza listą. Węższa zmiana nie zmienia komunikatów, historii i panelu bez decyzji operatora.
- A5 / Q2. Filtr dopasowuje także `display.title` (przez `title_of`), gdy zadanie ma tytuł. Operator dalej znajduje zadanie po słowach z opisu.

---

## Key Technical Decisions

- **Nowa funkcja tytułu wiersza w `list.rs`, obok `title_of`.** Funkcja zwraca `one_line(task.title)`, gdy wynik jest niepusty po przycięciu, a w innym przypadku `title_of(task)`. Dla zadań bez ticketu to ta sama reguła co w `edge_layout` (`plain` + fallback do `title_of`), więc R7 jest spełnione z konstrukcji. Gdy zadanie ma ticket i nie ma tytułu, ramka pokazuje tylko ticket, a wiersz pokazuje `display.title` (R2). Nazwę funkcji wybiera implementer (na przykład `row_title_of`).
- **`row_text_with` używa nowej funkcji.** Skracanie zostaje w rendererze (`fit`), więc R5 nie wymaga zmian.
- **Filtr dopasowuje dwa teksty.** `matches_task` sprawdza nowy tytuł wiersza oraz `title_of` (A5). Implementer może rozszerzyć `matches_filter` o drugi tekst albo wywołać go dwa razy. `matches_pending` zostaje bez zmian, bo `PendingRow` nie ma `TaskSummary`.
- **R3 nie wymaga kodu.** Lista dostaje nowy `TaskSummary` przy odświeżeniu. Wiersz czyta tytuł z niego.

---

## Implementation Units

### U1. Tytuł zadania w wierszu listy i w filtrze

**Goal:** Wiersz listy pokazuje `task.title` z fallbackiem. Filtr dopasowuje tytuł.

**Requirements:** R1, R2, R3, R4, R5, R6, R7. AE1–AE4.

**Dependencies:** brak.

**Files:**
- Modify: `src/tui/task_app/list.rs` (nowa funkcja tytułu wiersza, `row_text_with`, `matches_task`, komentarz modułu o filtrze)
- Test: `tests/tui_state.rs` (wiersze i filtr, moduł z `slash_filters_by_ticket_title_and_full_id_without_reordering`)
- Test: `tests/tui_render.rs` (skracanie długiego tytułu, zgodność z `edge_layout`)

**Approach:**
- Dodaj funkcję tytułu wiersza obok `title_of`. Dodaj komentarz doc, który mówi, że reguła jest taka sama jak w `edge_layout` dla zadań bez ticketu. Gdy zadanie ma ticket i nie ma tytułu, ramka pokazuje tylko ticket.
- `row_text_with` wpisuje wynik nowej funkcji do `RowText.title`.
- `matches_task` dopasowuje ticket, pełny identyfikator, nowy tytuł wiersza i `title_of`.
- Nie zmieniaj `title_of` i jego wywołań poza `row_text_with` i `matches_task`.

**Patterns to follow:**
- `plain` w `edge_layout` (`src/tui/task_app/detail.rs`): `one_line` + odrzucenie tekstu pustego po `trim`.
- Pomocniki testowe `titled`, `queued`, `listing` z `tests/task_app_support/mod.rs` oraz `titles` z `tests/tui_state.rs`. Uwaga: `titled` ustawia `display.title`, nie `task.title`. Testy R1, AE1 i AE4 ustawiają `task.title = Some(..)` bezpośrednio, tak jak `labelled_app` w `tests/tui_render.rs`. Nie zmieniaj `titled`, bo używają go inne pliki testów.

**Test scenarios:**
- Covers AE1. Zadanie z `title = "Zmiana API zamówień"` i `display.title = "Proszę przerobić endpoint…"`: `rows()` daje wiersz z tytułem „Zmiana API zamówień”.
- Covers AE2. Zadanie z `title = None` i `display.title = "Opis"`: wiersz pokazuje „Opis”. Po `load` z tym samym zadaniem i `title = Some("Tytuł dyrygenta")` wiersz pokazuje „Tytuł dyrygenta”.
- Edge: `title = Some("   ")` daje fallback do `display.title`.
- Edge: `title = None` i pusty `display.title` daje pełny identyfikator zadania.
- Edge (R4): tytuł z nową linią (`"Linia1\nLinia2"`) daje tekst w jednej linii, tak jak `one_line`.
- Covers AE3. Render listy w wąskim panelu z tytułem dłuższym niż szerokość: druga linia wiersza kończy się znakiem skrócenia, wiersz ma trzy linie.
- Covers AE4. Zadanie z tytułem „Zmiana API zamówień”: filtr „api zam” zostawia zadanie na liście.
- A5: zadanie z tytułem i z `display.title = "Parser cleanup"`: filtr „parser” zostawia zadanie na liście.
- Filtr: ticket i pełny identyfikator dalej działają (istniejący test bez zmian).
- R7: dla zadania z tytułem i bez ticketu tytuł w `rows()` jest równy tekstowi tytułu, który `edge_layout` umieszcza w `edge.left` przy dużej szerokości.
- Regresja: komunikaty akcji i historia dalej używają `display.title` (istniejące testy przechodzą bez zmian).

**Verification:**
- Nowe testy przechodzą. Istniejące testy `tests/tui_state.rs` i `tests/tui_render.rs` przechodzą. Gdy istniejący test ustawia `title` i sprawdza `display.title` w wierszu listy, implementer aktualizuje oczekiwanie do tytułu zadania i podaje to w opisie zmiany.
- `cargo fmt` i `cargo clippy` bez nowych ostrzeżeń.

---

## Risks

- **Istniejące testy z ustawionym `title`.** Testy, które ustawiają `task.title` i sprawdzają `row.title`, zmienią wynik. Implementer sprawdza je i aktualizuje tylko oczekiwania listy.
- **Rozbieżność listy i komunikatów.** Komunikat akcji (na przykład „zaznaczono …”) dalej pokazuje `display.title`, a wiersz pokazuje tytuł. To jest świadomy wybór do decyzji operatora (Q1, Deferred).
