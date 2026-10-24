---
title: "feat: Ticket, tytuł i numer na krawędzi ramki szczegółów"
type: feat
status: active
date: 2026-10-24
origin: docs/brainstorms/2026-10-24-ticket-i-tytul-w-ramce-szczegolow-requirements.md
---

# feat: Ticket, tytuł i numer na krawędzi ramki szczegółów

## Summary

Ticket, tytuł i numer zadania (8 znaków GUID) przechodzą z bloku `label_block` do górnej krawędzi ramki panelu szczegółów. Każda wartość to „pigułka”: lewe półkole, napis na tle w kolorze ramki, prawe półkole. Blok `label_block` znika razem z jego wierszami. Zmiana dotyczy tylko TUI. Dane, formularz i backend zostają bez zmian.

Głębokość: Standard. Zmiana dotyka dwóch plików źródłowych (`src/tui/style.rs`, `src/tui/task_app/detail.rs`; warunkowo komentarz w `src/tui/task_app/layout.rs`), wywołania w `src/tui/task_app/history.rs` i testów w `tests/tui_render.rs`.

---

## Problem Frame

Dziś `label_block` rysuje trzy ramki wewnątrz panelu. Panel ma dwie warstwy ramek, a blok zabiera 3 wiersze lub więcej. Ramka panelu ma tytuł, ale `panel_title` zwraca jeden `String` z tytułem z `display` (see origin: `docs/brainstorms/2026-10-24-ticket-i-tytul-w-ramce-szczegolow-requirements.md`).

---

## Requirements

Numeracja R-ID i AE-ID jak w dokumencie źródłowym. Wszystkie R1–R13 i AE1–AE4 obowiązują. Plan dodaje tylko decyzje techniczne.

- R1–R4. Ticket, tytuł (lewo) i numer (prawo) na górnej krawędzi. `label_block` usunięty. Wiersze nagłówka bez zmian.
- R5. Pigułki z glifami U+E0B6 / U+E0B4. Bez koloru: `┤ … ├`.
- R6–R7. Numer zawsze cały. Skracamy najpierw tytuł, potem ticket. Tytuł kończy się `…`.
- R8–R10. Brak wartości: pigułka znika. Brak ticketu i tytułu: lewa strona pokazuje tytuł z `display`. Numer jest zawsze.
- R11–R13. Bez zmian w danych. Wskaźnik ładowania zostaje na krawędzi. Tryby `no_color` i `stale` działają jak dziś.

---

## Scope Boundaries

- Bez zmian w liście zadań, w danych, backendzie i formularzu.
- Bez zawijania tytułu. Krawędź ma jeden wiersz.
- Bez wykrywania czcionki operatora (patrz Key Technical Decisions).

### Deferred to Follow-Up Work

- Wykrywanie obsługi glifów Powerline lub ustawienie w konfiguracji, które wymusza wariant `┤ … ├` przy kolorze. Zrobimy to, gdy operator zgłosi puste kwadraty zamiast półkoli.

---

## Context & Research

Pliki i wzorce, które plan wykorzystuje:

- `src/tui/style.rs`: `Styles::panel` i `render_panel` budują `Block` z jednym tytułem. `Styles::of`, `strong`, `key_cap` pokazują wzorzec „czarny napis na tle w kolorze”. Tryb `no_color` zwraca bold, dim lub reverse.
- `src/tui/task_app/detail.rs`: `panel_title`, `render` (ramka), `render_view` (wysokość nagłówka z `label_block`), `areas`, `header` (parametr `labels_shown`), `label_block`, `wrap_cells`, `fit_cells`, `cells`, stałe `MAX_TITLE_ROWS`, `SHORT_ID_CHARS`.
- `src/tui/task_app/history.rs:1022`: ten sam `render_view` i `panel_title` dla wpisu historii. Wpis ma `TaskSummary`, więc ma ticket, tytuł i `task_id`.
- `tests/tui_render.rs` (sekcja „Ticket and title in the header and in Info”, `labelled_app`): testy dla `label_block`.
- Ratatui `Block` przyjmuje kilka tytułów `Line` z wyrównaniem (`left_aligned`, `right_aligned`). Planowanie nie wymaga badań zewnętrznych.

Brak wpisów w `docs/solutions/` o tym obszarze. Badania zewnętrzne pominięte: lokalne wzorce są mocne.

---

## Key Technical Decisions

- **Czysta funkcja układu krawędzi.** `edge_layout(task, glyph, width, …)` zwraca dane (teksty po skróceniu i marker ładowania), bez stylu i bez `Frame`. Dzięki temu skracanie da się testować bez rysowania. Styl i glify dodaje `Styles` (see origin: R5, R6).
- **Nowy wariant ramki w `Styles`.** `Styles::panel` i `render_panel` zostają dla innych paneli. Dodajemy `render_panel_edge`, które bierze listę pigułek po lewej, opcjonalną pigułkę po prawej i marker fokusu. Nie zmieniamy sygnatury `render_panel` (6 wywołań w innych widokach).
- **Kolor pigułki.** Półkola w kolorze ramki (`Role::Focus` lub `Role::Done`) na tle terminala. Napis czarny na tle w kolorze ramki, jak `key_cap`. Pod `stale` rola to `Role::Done`, jak ramka dziś. Pod `no_color`: `┤ napis ├` w stylu ramki, tytuł pogrubiony.
- **Glify.** U+E0B6 i U+E0B4 przy kolorze, `┤` i `├` bez koloru. Czcionki operatora nie wykrywamy: brak wiarygodnego sposobu w terminalu. Ryzyko opisane niżej.
- **Marker fokusu `▸`.** Stoi na lewym końcu krawędzi, przed pierwszą pigułką. Zajmuje 1 komórkę. Bez koloru marker i pogrubienie nadal pokazują fokus.
- **Odstęp.** Pigułki po lewej dzieli jedna komórka linii ramki. Między grupą lewą a numerem musi zostać co najmniej jedna komórka linii.
- **Wskaźnik ładowania (R12).** Doklejony do ostatniego członu lewego jako ` ⠋` poza pigułką: jest zawsze zachowany i nie jest skracany. Dla fallbacku `display` doklejony do tekstu jak dziś, ale też liczony w budżecie i nie skracany osobno.
- **Kolejność skracania (R6).** Budżet = szerokość ramki − 2 rogi − numer − odstępy − marker − wskaźnik ładowania (2 komórki, gdy aktywny). Najpierw tytuł zmniejszamy do 1 znaku plus `…`. Potem ticket do 3 komórek (znak plus `…`). Gdy nadal brakuje miejsca, najpierw znika pigułka tytułu, potem ticketu. Numer nigdy nie jest skracany. Poniżej szerokości numeru plus rogi nic nie gwarantujemy.
- **Fallback R9.** Bez ticketu i tytułu lewa strona to ` tytuł z display ` jak dziś (zwykły napis, nie pigułka), skrócony do budżetu po odjęciu numeru.
- **Nagłówek wewnątrz panelu.** Linia 1 `header` wraca do zachowania „bez bloku”: tytuł z `display` i pełny GUID. Parametr `labels_shown` znika. Własny tytuł jest tylko na krawędzi i w Info (R10). To jedyna zmiana w treści nagłówka, wynika ze zniknięcia bloku.
- **Historia (pytanie z dokumentu).** Widok historii używa tego samego `render_view` i dostaje tę samą krawędź. Wpis historii ma `TaskSummary` z ticketem i tytułem, więc oba panele wyglądają tak samo.
- **Wysokość.** `render_view` liczy `header_lines = HEADER_LINES + notice`. Zakładki i treść dostają odzyskane wiersze przez istniejące `areas` bez dodatkowej logiki.

---

## Implementation Units

### U1. Pigułki i ramka z krawędzią w `Styles`

**Goal:** Narysować ramkę panelu z pigułkami po lewej i numerem po prawej.

**Requirements:** R1, R4, R5, R13

**Dependencies:** brak

**Files:**
- Zmiana: `src/tui/style.rs`
- Test: `src/tui/style.rs` (moduł testów, jeśli istnieje; inaczej `tests/tui_render.rs`)

**Approach:**
- Typ opisujący krawędź: lista napisów po lewej (każdy: tekst, czy pogrubiony), opcjonalny napis po prawej, opcjonalny zwykły napis ładowania. Skracanie już zrobione przed wywołaniem.
- `Styles::edge_pill(text, focused)` zwraca `Vec<Span>`: półkole, napis, półkole (kolor) lub `┤ … ├` (bez koloru).
- Nowa metoda rysuje `Block` z tytułami: lewe `Line` (marker, pigułki rozdzielone `─`) i prawe `Line` wyrównane do prawej. Zwraca `inner`, jak `render_panel`.
- Marker `▸` i rola ramki jak w `Styles::panel`.

**Patterns to follow:** `Styles::panel`, `Styles::key_cap`, `Styles::render_panel`.

**Test scenarios:**
- Kolor, fokus: napis pigułki ma tło cyan i czarny napis. Półkola mają kolor cyan i brak tła.
- Kolor, bez fokusu: tło `DarkGray`.
- `no_color`: wynik zawiera `┤ NAR-12 ├` i nie zawiera glifów U+E0B6 i U+E0B4. Brak kolorów w stylach.
- Numer jest wyrównany do prawego rogu: ostatnia komórka przed `╮` to prawe półkole.
- Pusta lista lewa: tylko numer, brak błędu.

**Verification:** Test renderu ramki z bufora pokazuje oczekiwane komórki i style w obu trybach.

### U2. Układ krawędzi z skracaniem

**Goal:** Z `TaskSummary`, szerokości i markera ładowania wyliczyć, co stoi na krawędzi.

**Requirements:** R4, R6, R7, R8, R9, R10, R12

**Dependencies:** U1 (typ opisujący krawędź)

**Files:**
- Zmiana: `src/tui/task_app/detail.rs`
- Test: `tests/tui_render.rs` oraz testy jednostkowe w `src/tui/task_app/detail.rs`, jeśli plik ma moduł testów

**Approach:**
- Nowa funkcja czysta (`edge_layout`; nazwę ustala implementacja), która:
  - normalizuje ticket i tytuł (`one_line`, pusty = brak), numer = pierwsze `SHORT_ID_CHARS` znaków `task_id`;
  - liczy budżet i skraca wg Key Technical Decisions, używając `fit_cells` i `cells`;
  - przy braku ticketu i tytułu zwraca fallback z `title_of(task)`.
- `render` i `history.rs` wołają ją zamiast `panel_title` dla panelu zadania. `panel_title` zostaje dla panelu zadania oczekującego (`pending`).
- Szerokość w komórkach (`cells`), nie w znakach: tytuły szerokie (CJK, emoji) muszą trzymać układ.

**Patterns to follow:** `label_block` (wyliczanie `id`, `fit_cells`), `panel_title`.

**Test scenarios:**
- Covers AE1. Ticket `NAR-12`, tytuł „Zmiana API zamówień”, GUID `6a7ceacf…`, szerokość 100: lewo `NAR-12`, `Zmiana API zamówień`, prawo `6a7ceacf`, bez `6a7ceacfd`.
- Covers AE2. Tytuł 70 znaków, szerokość 60: tytuł kończy się `…`, numer cały, ticket cały.
- Szerokość bardzo mała (np. 30): tytuł do `…`, potem ticket skrócony, numer cały.
- Szerokość tak mała, że nie mieści ticketu i numeru: pigułka tytułu znika, potem ticketu. Numer zostaje.
- Covers AE3. Brak ticketu i tytułu: lewo to tytuł z `display`, prawo numer, brak pustych pigułek.
- Covers AE4. Sam ticket: brak pigułki tytułu. Sam tytuł: brak pigułki ticketu.
- Ticket lub tytuł z samych spacji traktujemy jak brak.
- Tytuł z szerokimi znakami (`変更 …🚀`) nie przekracza budżetu w komórkach.
- Ładowanie: glif widoczny po ostatnim członie lewym także przy skróconym tytule.
- Tytuł z nową linią zamienia się na jedną linię (`one_line`).

**Verification:** Testy układu przechodzą dla szerokości 20, 30, 60, 100 i 200.

### U3. Podpięcie krawędzi i usunięcie `label_block`

**Goal:** Panel zadania i panel historii rysują nową krawędź. Blok nagłówkowy znika.

**Requirements:** R1, R2, R3, R10, R11, R12, R13

**Dependencies:** U1, U2

**Files:**
- Zmiana: `src/tui/task_app/detail.rs` (`render`, `render_view`, `areas`, `header`, usunięcie `label_block`, `MAX_TITLE_ROWS`, `wrap_cells` jeśli nie ma innych użyć; poprawa komentarzy o „trzech liniach”)
- Zmiana: `src/tui/task_app/history.rs` (wywołanie ramki)
- Zmiana: `tests/tui_render.rs` (migracja testów)

**Approach:**
- `render` i `history.rs`: `render_panel_edge` zamiast `render_panel(… panel_title …)` dla widoku zadania. Ścieżki `pending` i „brak zaznaczonego zadania” bez zmian.
- `render_view`: usunąć obliczenie `title_rows`, `labels`, `label_lines`. `header_lines = base_lines`. Usunąć `lines.splice` z etykietami.
- `header`: usunąć parametr `labels_shown`. Linia 1 zawsze jak dziś bez bloku (tytuł z `display` plus pełny GUID).
- Sprawdzić `grep` po całym repo, że `wrap_cells` i stałe nie mają innych użyć, zanim się je usunie.
- Zaktualizować komentarz przykładowy w `src/tui/task_app/layout.rs`, jeśli ASCII-art pokazuje stary tytuł ramki (tylko dokumentacja).

**Execution note:** Najpierw przenieś istniejące testy `labelled_app` na nowe asercje (czerwone), potem podmień rysowanie.

**Patterns to follow:** istniejące funkcje `render_at`, `rows`, `screen`, `labelled_app` w `tests/tui_render.rs`.

**Test scenarios:**
- Covers AE1. Szczegóły zadania: wiersz górnej krawędzi (wiersz 0 panelu) zawiera `NAR-12`, tytuł i `6a7ceacf`; ticket stoi przed numerem. Wewnątrz panelu brak `╭` i `╰` ramek etykiet.
- Covers AE2. Przy 60 kolumnach i długim tytule krawędź ma `…`, `6a7ceacf` cały; zakładki `Przebieg` i `Info` widoczne.
- Covers AE3. Brak ticketu i tytułu: krawędź ma `NAR-12 opis` (z `display`) i numer. `IDENTITY_ROW` nadal zawiera tytuł z `display`.
- Covers AE4. Sam ticket: widać ticket i numer.
- Wysokość: przy 60×20 liczba wierszy treści zakładek jest większa o liczbę wierszy dawnego bloku (3 dla jednowierszowego tytułu) niż przed zmianą.
- `no_color`: krawędź zawiera `┤ NAR-12 ├`, brak glifów Powerline.
- `stale`: półkola i napisy w roli `Done`, bez koloru jaskrawego.
- Ładowanie: glif widoczny na krawędzi, gdy szczegóły się ładują.
- Historia: panel historii pokazuje ten sam układ krawędzi dla wpisu z ticketem i tytułem.
- Integracja: przełączenie zadania z ticketem na zadanie bez ticketu nie zostawia starych pigułek na krawędzi (bufor czyszczony przez `Block`).
- Info nadal pokazuje ticket i pełny tytuł (istniejący test `info_lists_the_ticket_and_title_only_when_there_are_any` bez zmian).
- Test `wide_characters_in_the_title_keep_the_frames_aligned`: prawy róg `╮` stoi w tej samej kolumnie co przy ASCII.

**Verification:** `cargo test` dla pakietu z TUI przechodzi. Ręcznie: uruchomić TUI w terminalu z czcionką Powerline i bez niej (`NO_COLOR=1`), sprawdzić wygląd pigułek.

---

## Risks & Dependencies

- **Czcionka bez Powerline.** Glify U+E0B6 i U+E0B4 mogą się pokazać jako puste kwadraty. Wariant `┤ … ├` działa tylko pod `NO_COLOR`. Złagodzenie: odroczone wykrywanie lub ustawienie (patrz Deferred). Do potwierdzenia przez operatora: czy jego czcionka ma te glify (założenie z dokumentu źródłowego).
- **Szerokość znaków.** Glify Powerline mają szerokość 1 w większości terminali. Niektóre terminale liczą je jako 2. Testy używają szerokości 1. Weryfikacja ręczna.
- **Zmiana linii 1 nagłówka.** Przy własnym tytule linia 1 pokazuje już tytuł z `display`, nie własny. Ta zmiana wynika z R2 i R10. Do potwierdzenia przez autora wymagań.

---

## Deferred to Implementation

- Dokładne nazwy typów i funkcji.
- Minimalna szerokość skróconego ticketu (3 komórki to punkt wyjścia).
- Czy `wrap_cells` ma inne użycia (sprawdza `grep` w U3).
- Dokładny kolor napisu w pigułce pod `stale`.
