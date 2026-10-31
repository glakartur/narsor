---
date: 2026-10-31
topic: tui-lista-zadan-tytul-zamiast-opisu
---

# Lista zadań w TUI: tytuł zadania zamiast początku opisu

## Summary

Druga linia wiersza na liście zadań (lewy panel TUI) pokazuje tytuł zadania. Tytuł podaje operator, albo generuje go dyrygent. Gdy tytuł nie mieści się w szerokości panelu, wiersz pokazuje jego początek. Dziś ta linia pokazuje `display.title` workflow, który często jest początkiem opisu zadania.

---

## Problem Frame

Zadanie ma pole tytułu (zobacz `docs/brainstorms/2026-10-24-task-ticket-i-tytul-requirements.md`). Ramka szczegółów zadania już pokazuje ten tytuł. Lista zadań go nie pokazuje. Operator widzi w liście początek opisu, a w szczegółach tytuł. Przez to trudno mu rozpoznać zadanie na liście i powiązać wiersz ze szczegółami.

Stan kodu (zweryfikowany):
- `src/tui/task_app/list.rs`: `row_text_with` wypełnia linię tytułu przez `title_of(task)`.
- `title_of` zwraca `task.display.title`, a gdy jest pusty, pełny `task_id`. Nie czyta pola `task.title`.
- `display.title` to wynik szablonu `display` workflow. Szablon często używa wejścia z opisem zadania, więc wynik to początek opisu.
- `task.title` w `TaskSummary` to tytuł efektywny: tytuł operatora, a gdy go brak, tytuł wygenerowany przez dyrygenta (`effective_title` w `crates/narsor-engine/src/controller_protocol/service.rs`).
- Ramka szczegółów (`edge_layout` w `src/tui/task_app/detail.rs`) już używa `task.title`. Gdy brak ticketu i tytułu, używa `title_of`.
- Wiersz listy już skraca zbyt długi tekst do szerokości panelu (`fit` w `src/tui/task_app/list.rs`).
- `title_of` ma też innych użytkowników: komunikaty akcji (`actions.rs`), historia (`history.rs`), panel (`panel/state.rs`), `mod.rs`.

---

## Requirements

**Zawartość linii tytułu**

- R1. Linia tytułu w wierszu listy pokazuje tytuł zadania (`task.title`), gdy jest ustawiony i nie jest pusty po przycięciu białych znaków.
- R2. Gdy zadanie nie ma tytułu (na przykład przed zapisem tytułu przez dyrygenta), linia tytułu pokazuje to, co dziś: `display.title`, a gdy go brak, identyfikator zadania.
- R3. Gdy dyrygent zapisze tytuł, wiersz pokazuje go przy następnym odświeżeniu listy. Operator nie musi nic robić.
- R4. Tytuł przechodzi przez `one_line`, tak jak inne teksty z serwera.

**Długi tytuł**

- R5. Gdy tytuł jest dłuższy niż szerokość panelu, wiersz pokazuje jego początek, skrócony tak jak dziś (ten sam znak skrócenia). Tytuł zajmuje jedną linię. Wysokość wiersza (trzy linie) się nie zmienia.

**Filtr**

- R6. Filtr tekstowy (`/`) dopasowuje tytuł zadania. Nadal dopasowuje ticket i pełny identyfikator.

**Spójność**

- R7. Wiersz listy i ramka szczegółów pokazują ten sam tytuł dla tego samego zadania.

---

## Acceptance Examples

- AE1. **Covers R1.** Given zadanie z tytułem „Zmiana API zamówień” i opisem „Proszę przerobić endpoint…”, when lista się rysuje, then druga linia wiersza pokazuje „Zmiana API zamówień”.
- AE2. **Covers R2, R3.** Given zadanie bez tytułu operatora, przed zapisem tytułu przez dyrygenta, when lista się rysuje, then druga linia pokazuje `display.title`. When dyrygent zapisze tytuł, then po odświeżeniu druga linia pokazuje ten tytuł.
- AE3. **Covers R5.** Given tytuł dłuższy niż szerokość panelu, when lista się rysuje, then wiersz pokazuje początek tytułu ze znakiem skrócenia, a wiersz ma trzy linie.
- AE4. **Covers R6.** Given zadanie z tytułem „Zmiana API zamówień”, when operator wpisuje w filtrze „api zam”, then zadanie zostaje na liście.

---

## Success Criteria

- Operator rozpoznaje zadanie na liście po tytule, bez otwierania szczegółów.
- Wiersz listy i ramka szczegółów pokazują ten sam tytuł.
- Zadania bez tytułu wyglądają tak jak dziś.

---

## Scope Boundaries

- Brak zmian w formacie wiersza (ticket, status, czas aktywności, etykieta).
- Brak zawijania tytułu do kilku linii na liście.
- Brak zmian w generowaniu tytułu przez dyrygenta i w szablonach `display` workflow.
- Brak zmian w CLI (`src/cli/task_text.rs`).

---

## Założenia do potwierdzenia

- A1. „Tytuł” to pole tytułu zadania (`task.title`): tytuł operatora albo tytuł dyrygenta. Nie jest to `display.title` workflow.
- A2. Gdy brak tytułu, wiersz pokazuje to, co dziś (`display.title`, potem identyfikator). Alternatywa (pusta linia albo tekst „bez tytułu”) jest mniej bezpieczna, bo wiersz traci informację.
- A3. Skracanie zostaje takie jak dziś (jedna linia, znak skrócenia na końcu). Nie dodajemy zawijania.
- A4. Zmiana obejmuje wiersz listy (R1–R7). Inne miejsca, które używają `title_of` (komunikaty akcji, historia, panel), planowanie może zmienić razem z listą, gdy to jest jedna zmiana w `title_of`. Wtedy wszystkie miejsca pokazują ten sam tytuł. Plan musi zaznaczyć ten wybór.
- A5. Filtr nadal dopasowuje także `display.title`, gdy zadanie ma tytuł. Wtedy operator dalej znajduje zadanie po słowach z opisu.

---

## Pytania otwarte dla użytkownika

- Q1. Czy komunikaty akcji, historia i panel również mają pokazywać tytuł zadania zamiast `display.title` (A4)?
- Q2. Czy filtr ma nadal dopasowywać tekst `display.title` (początek opisu), gdy zadanie ma tytuł (A5)?
