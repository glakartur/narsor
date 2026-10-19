---
title: "feat: Zaokrąglone ramki paneli w TUI"
type: feat
status: active
date: 2026-10-20
origin: docs/brainstorms/2026-10-20-tui-zaokraglone-ramki-requirements.md
---

# feat: Zaokrąglone ramki paneli w TUI

## Summary

Wszystkie ramki paneli dostają narożniki `╭ ╮ ╰ ╯` (`BorderType::Rounded`). Focus przestaje być pokazywany grubością linii. Pokazują go kolor roli *Focus*, pogrubiony tytuł i znacznik `▸` przed tytułem. Plan ma dwie jednostki: zmiana `Styles::panel` z testami oraz zmiana czterech ręcznych ramek z dokumentacją.

## Problem Frame

Dziś `Styles::panel` w `src/tui/style.rs` daje panelowi z focusem `BorderType::Thick`, a pozostałym `BorderType::Plain`. Rounded nie ma wersji grubej. Cztery miejsca budują ramkę ręcznie z `Borders::ALL`. (see origin: `docs/brainstorms/2026-10-20-tui-zaokraglone-ramki-requirements.md`)

## Requirements

- R1. Wszystkie ramki paneli mają zaokrąglone narożniki, także cztery ręczne.
- R2. Focus jest czytelny bez koloru. Sygnał nie opiera się na grubości linii.
- R3. Znaki zaznaczenia `─` / `│` bez zmian.
- R4. Układ bez przesunięć. Narożnik zajmuje tę samą komórkę.
- R5. Układ, skróty i zachowanie TUI bez zmian.

## Scope Boundaries

- Brak zmiany kolorów ról i znaków zaznaczenia.
- Brak motywów i opcji konfiguracji ramek.
- Brak zmian w układzie paneli.

### Deferred to Follow-Up Work

- Brak.

## Założenia (do potwierdzenia przez Artura)

Brainstorm zostawił jedno pytanie przed planowaniem: znacznik w tytule czy sam kolor i pogrubienie. Ta sesja jest nieinteraktywna. Plan przyjmuje **znacznik `▸`**. Jest to zgodne z sekcją *Key Decisions* w dokumencie źródłowym. Plan przyjmuje też, że utrata grubego obrysu jest akceptowana. Jeśli Artur woli sam kolor i pogrubienie, usuń znacznik z U1. Reszta planu nie zmienia się.

## Context & Research

- `src/tui/style.rs`: `Styles::panel` jest jedynym miejscem wyboru `BorderType` dla paneli. `render_panel` go używa. Wywołują go `list.rs`, `detail.rs`, `history.rs`, `new_task.rs`.
- Tytuły są przekazywane z odstępami, np. `" Zadania "`. `detail.rs` skraca tytuł funkcją `panel_title(.., area.width)`.
- `▸` jest już użyte w `src/tui/task_app/waiting.rs` (nagłówek w panelu odpowiedzi). To inny kontekst, ale implementator ma to wiedzieć.
- Cztery ręczne ramki: `src/tui/chooser/inputs.rs` (karta, rola Done), `src/tui/chooser.rs` (`pane`, Focus lub Done), `src/tui/task_app/actions.rs` (`framed`, rola z argumentu), `src/tui/keymap.rs` (okno Pomoc, rola Focus).
- Testy: `tests/tui_render.rs` ma stałe `THICK` i `THIN`, helper `assert_frame` i kilka asercji na `┏ ┓ ┃ ┌ ┐`.
- Komentarze z rysunkami ASCII: `src/tui/task_app/layout.rs`, `src/tui/chooser.rs`, `src/tui/task_app/new_task.rs`, `src/tui/task_app/repositories.rs`. Rysunek jest też w `README.md` (ok. linii 486).

## Key Technical Decisions

- **Rounded wszędzie, jeden typ.** Panel z focusem i bez focusa używa `BorderType::Rounded`. Różnią się rolą (Focus cyan i Done szary), stylem tytułu (pogrubiony) i znacznikiem.
- **Znacznik `▸` tylko przy focusie, przed tytułem, dodany w `Styles::panel`.** Dzięki temu działa w obu trybach, nie tylko bez koloru. Jedno miejsce zmiany, bez edycji wywołań. Tytuł z focusem jest o jedną komórkę szerszy. Blok obcina tytuł, który się nie mieści, więc układ się nie przesuwa (R4).
- **Cztery ręczne ramki: zmiana samego typu, bez przenoszenia do `Styles::panel`.** Mają własne role i tytuły (okno, karta, pole). `Styles::panel` modeluje „panel z focusem lub bez". Przeniesienie zmieniłoby ich wygląd (tytuł, pogrubienie), a to wykracza poza R1. Dla spójności warto dodać w `style.rs` jedną stałą lub małą metodę z typem ramki, żeby `Rounded` nie był wpisany w pięciu miejscach. Wybór formy należy do implementatora.
- **Chooser `pane` bez znacznika.** Dziś pokazuje aktywny panel samym kolorem i nie miał grubości, więc nie ma regresji R2. Znacznik tam to rozszerzenie zakresu.

## Implementation Units

### U1. `Styles::panel`: Rounded i znacznik focusa

**Goal:** Panele z `Styles::panel` mają zaokrąglone narożniki. Focus jest czytelny bez koloru.

**Requirements:** R1, R2, R3, R4, R5. Covers AE1, AE2, AE3.

**Dependencies:** brak.

**Files:**
- `src/tui/style.rs` (zmiana `panel`, jego komentarza i opisu w nagłówku modułu, jeśli wspomina grubość)
- `tests/tui_render.rs` (aktualizacja)

**Approach:**
- `panel` zawsze używa `BorderType::Rounded`.
- Rola i styl tytułu bez zmian (Focus i pogrubiony, Done i zwykły).
- Przy `focused` tytuł dostaje prefiks `▸`. Przy braku focusa tytuł bez zmian.
- Zaktualizować komentarz przy `panel` (dziś mówi o grubości).

**Patterns to follow:** istniejący `panel` i `strong`. Wzorzec znacznika to `▸` w `waiting.rs`.

**Test scenarios:**
- Covers AE1. Happy path: w `busy_app()` 120x30 z kolorem, narożniki listy i szczegółów to `╭ ╮ ╰ ╯`. Lista ma kolor Cyan, szczegóły DarkGray. Zastąpić `THICK` i `THIN` jedną stałą `ROUNDED`.
- Covers AE1. Po `Enter` focus przechodzi na szczegóły. Narożniki szczegółów są Cyan, listy DarkGray, oba zaokrąglone.
- Covers AE1. Brak `┌ ┐ └ ┘ ┏ ┓ ┗ ┛` na ekranie w widoku listy i szczegółów.
- Covers AE2. Bez koloru (`Styles::new(true)`): panel z focusem ma `▸` przed tytułem i pogrubiony tytuł. Panel bez focusa nie ma `▸` i ma tytuł niepogrubiony. Wszystkie komórki mają `Reset`.
- Covers AE2. Przełączenie focusa lista → szczegóły przenosi `▸` z `Zadania` na tytuł szczegółów.
- Covers AE3. Wybrany element: znaki `─` i `│` zaznaczenia bez zmian (`assert_bracketed`, `no_marks`). Panel zajmuje ten sam obszar (te same współrzędne narożników co dziś: `0`, `LIST_WIDTH - 1`, `LIST_WIDTH`, `119`).
- Edge: wąskie układy 80x24 i 60x20 (jeden panel). Narożniki `╭ ╮` w kolumnach `0` i `79` lub `59`. Długi tytuł z `▸` nie zawija się i nie psuje narożnika.
- Edge: asercje „nagłówek nie zawiera znaków ramki" (`['┌','┏','│','┃','─','━']`) zaktualizować o `╭ ╮ ╰ ╯`, żeby nadal sprawdzały brak ramki wokół ekranu.
- Test `the_list_column_is_28_wide_...` oczekuje `┏┓┌┐` w górnym wierszu. Zmienić na cztery narożniki `╭ ╮ ╭ ╮` w tych samych kolumnach.
- Test z `column_of(&buffer, y, 0, "┌")` (okolice linii 5235, okno wyboru harnessu) zmienić na `╭`.
- Asercja `cell_at(&buffer, 0, 15) == "┃"` (lewa krawędź z focusem) zmienić na `│` w kolorze Cyan. Pozostałe krawędzie `│` DarkGray.

**Verification:** `cargo test --test tui_render` przechodzi. Żaden test nie zawiera `┏ ┓ ┗ ┛ ┃` jako oczekiwanego wyglądu panelu.

### U2. Cztery ręczne ramki i dokumentacja

**Goal:** Okna, karty i panele budowane ręcznie mają narożniki zaokrąglone. Dokumentacja pokazuje nowy wygląd.

**Requirements:** R1, R4, R5. Covers AE1.

**Dependencies:** U1 (wspólna stała lub metoda typu ramki, jeśli powstanie).

**Files:**
- `src/tui/chooser/inputs.rs`, `src/tui/chooser.rs`, `src/tui/task_app/actions.rs`, `src/tui/keymap.rs` (typ ramki)
- `src/tui/task_app/layout.rs`, `src/tui/chooser.rs`, `src/tui/task_app/new_task.rs`, `src/tui/task_app/repositories.rs` (rysunki w komentarzach)
- `README.md` (rysunek ekranu)
- `tests/tui_render.rs` (nowe i zaktualizowane asercje)

**Approach:**
- Dodać `.border_type(BorderType::Rounded)` (lub wspólny pomocnik) do czterech bloków. Role, tytuły i `Clear` bez zmian.
- Zaktualizować rysunki ASCII: `┏┓┗┛┃┌┐└┘` na `╭╮╰╯│`. Focus w rysunku pokazać znacznikiem `▸` przed tytułem. Rysunki są ilustracją, więc nie muszą być co do znaku zgodne z renderem.

**Patterns to follow:** U1.

**Test scenarios:**
- Covers AE1. Okno Pomoc (`?`) ma narożniki `╭ ╮ ╰ ╯` (nowy test, obok istniejącego `at_60x20_...`).
- Covers AE1. Chooser: oba panele (Akcje, Opis) mają `╭ ╮ ╰ ╯`. Aktywny w Cyan, drugi w DarkGray. Użyć istniejącego `draw_chooser`.
- Covers AE1. Okno potwierdzenia lub wyboru z `actions.rs::framed` ma `╭`. Okno wyboru harnessu (test z `" Domyślny harness "`) ma `╭` w kolorze Cyan.
- Covers AE1. Karta w `chooser/inputs.rs` (gałąź bez rozwinięcia, rola Done) ma `╭ ╮ ╰ ╯` w DarkGray.
- Integration (AE1, ogólny): test przechodzący po kilku widokach (lista, szczegóły, chooser, Pomoc, nowe zadanie, historia) i sprawdzający, że bufor nie zawiera `┌ ┐ └ ┘ ┏ ┓ ┗ ┛ ┃ ━`. Uwaga: `│` i `─` zostają, bo są znakami zaznaczenia i krawędzi.
- Covers R3. Istniejące testy zaznaczenia i `no_marks` przechodzą bez zmian logiki. `no_marks` dziś wyklucza `━ ┃`. Po zmianie nie ma ich już nigdzie, więc asercja pozostaje prawdziwa.

**Verification:** `cargo test` (cały workspace) przechodzi. `rg "[┏┓┗┛┃]" src README.md` nie znajduje rysunków starego stylu. Ręczny rzut oka na TUI w terminalu z kolorem i z `NO_COLOR=1`: widać, który panel ma focus.

## Risks & Dependencies

- **Czcionka terminala bez `╭ ╮ ╰ ╯`.** Założenie z dokumentu źródłowego, niesprawdzone u operatorów. Skutek to ubytki w narożnikach, nie błąd działania. Zalecenie: Artur sprawdza w swoim terminalu przed scaleniem.
- **Szerokość tytułu.** `▸` dodaje jedną komórkę. `panel_title` w `detail.rs` skraca tytuł do szerokości obszaru bez znacznika. W skrajnie wąskim panelu tytuł może zostać ucięty o jeden znak. Akceptowalne, bez przesunięcia układu. Implementator sprawdza to w teście 60x20.
- **Rozbieżność z założeniem o znaczniku.** Patrz sekcja *Założenia*.

## Deferred to Implementation

- Dokładna forma wspólnego typu ramki (stała, metoda) i jej miejsce w `style.rs`.
- Czy prefiks `▸` dodawać przez `format!` w `panel`, czy przez osobny `Span` w `Line`. Ważny jest wynik w buforze.
