---
title: "feat: Oś kroków w Przebiegu bez tła, z czasem przy nazwie i bez słowa stanu"
type: feat
status: active
date: 2026-11-12
origin: docs/brainstorms/2026-11-12-tui-przebieg-os-krokow-requirements.md
---

# feat: Oś kroków w Przebiegu bez tła, z czasem przy nazwie i bez słowa stanu

## Summary

Plan zmienia trzy rzeczy na osi kroków w zakładce *Przebieg*. Oś nie rysuje tła zaznaczonego kroku (klamra zostaje). Czas startu i czas trwania stoją w pierwszym wierszu kroku, w kolumnach za nazwą. Słowo stanu znika z osi i z nagłówka wybranego kroku. Plan ma 3 jednostki. Rozmiar: Standard (jedna zmiana układu osi dotyka dwóch dziś osobnych ścieżek rysowania i kilku testów integracyjnych).

---

## Problem Frame

Oś rysuje `src/tui/task_app/tabs/progress.rs` przez `render_bracketed_list` z `src/tui/selection.rs`. Wspólna funkcja `draw_line` wypełnia tło zaznaczonego wiersza (`Styles::selection_fill`). Czas startu i czas trwania stoją w drugim wierszu (`AxisLayout::add_details`). Słowo stanu stoi za nazwą (`axis_line` i kolumna `word` w `AxisLayout::table`) oraz w nagłówku kroku (`step_lines`) (see origin: `docs/brainstorms/2026-11-12-tui-przebieg-os-krokow-requirements.md`). Plan realizuje R1–R12 i AE1–AE5 z origin.

---

## Requirements

- R1, R2, R3. Oś *Przebiegu* nie ma tła zaznaczenia. Klamra zostaje. *Historia* i choosery zostają bez zmian. (U1)
- R4, R5, R6, R7. Pierwszy wiersz: ikona, nazwa, czas startu, czas trwania. Drugi wiersz: model, wysiłek, harness. Brak `started_at` daje puste pole bez separatora. (U2)
- R8, R9. Kolumny czasu są wyrównane i stoją przed kolumnami zużycia. Przy wąskiej osi: najpierw nazwa do `MIN_LABEL`, potem znika czas trwania, potem czas startu. (U2)
- R10, R11, R12. Brak słowa stanu na osi i w nagłówku. Ikona, wiersz błędu i noty zostają. (U2, U3)

Origin: Acceptance Examples AE1 (U1), AE2–AE4 (U2), AE5 (U2, U3).

---

## Scope Boundaries

- Bez zmian: *Historia* (`src/tui/task_app/history.rs`), choosery (`src/tui/chooser.rs`), lista tasków, `render_barred_list`, zawartość po prawej stronie osi poza słowem stanu.
- Bez nowych danych i klawiszy. Formaty czasu (`clock`) i trwania (`short_age`) zostają.

### Deferred to Follow-Up Work

- Osobna ikona dla `Interrupted` (dziś `✗` jak `Failed`). Origin, założenie 3: poza zakresem.

---

## Key Technical Decisions

- **Wariant dekoracji zamiast flagi w wywołaniu.** `SelectionDecoration` dostaje wariant `BracketedPlain`: reguły i pasek jak `Bracketed`, bez wypełnienia. `selection.rs` dodaje publiczną `render_bracketed_list_plain`. `Bracketed` zostaje dla *Historii* i chooserów. Powód: `has_rules()` i `keep_in_view` zależą od dekoracji, więc wariant zachowuje układ wierszy. Rozstrzyga to pytanie z origin o R1.
- **Jedna ścieżka układu osi.** Dziś `AxisLayout::usage` ma dwie ścieżki: prosty wiersz (`axis_line`) bez zużycia i tabelę (`table`) ze zużyciem. Po zmianie obie potrzebują kolumn czasu, więc zostaje tabela, a kolumny zużycia mogą być puste (kolumna o szerokości 0 nie zajmuje miejsca, jak dziś). `axis_line` znika. Powód: jedno miejsce liczy szerokość i skracanie (R8, R9).
- **Kolejność kolumn:** ikona, nazwa (dopełniona), czas startu, czas trwania (do prawej), potem kolumny zużycia (R8). Kolumna `word` znika.
- **Skracanie (R9):** funkcja `shrink` skraca nazwę do `MIN_LABEL`. Gdy to nie wystarcza, zeruje szerokość czasu trwania, potem czasu startu. Zużycie zostaje, jak dziś. Kolejność zera wynika z założenia 4 w origin.
- **Szerokość osi** rośnie do `axis_cap` (70%), jak dziś. Szerokość `today` (`area/3`, 18..`AXIS_MAX_WIDTH`) zostaje dolną granicą.
- **`status_look` zwraca parę (ikona, rola).** Trzeci element (słowo) traci jedyne użycia: `axis_line`, `table`, `step_lines`. `conversation.rs:1406` już ignoruje słowo.
- **Pusty czas startu (R6):** komórki obu kolumn czasu są puste, a ogon wiersza bez treści zostaje bez spacji końcowych, gdy żadna kolumna zużycia nie ma wartości w tym wierszu.
- **`no_color` (R3, pytanie z origin):** pasek `│` i reguły `─` to znaki, więc zaznaczenie nie zależy od koloru ani od tła. Rola „tła" (`Role::Done`) różni się od roli fokusu stylem, nie znakiem. Plan nie dodaje nic nad to. U1 ma test, który potwierdza, że w trybie `no_color` oba stany klamry (fokus, tło) mają znaki `│` i `─` na osi.

---

## Implementation Units

### U1. Klamra bez wypełnienia dla osi Przebiegu

**Goal:** Zaznaczony krok na osi nie ma tła. *Historia* i choosery zachowują tło.

**Requirements:** R1, R2, R3, AE1.

**Dependencies:** brak.

**Files:**
- `src/tui/selection.rs` (wariant `SelectionDecoration::BracketedPlain`, `has_rules`, `draw_line`, nowa `render_bracketed_list_plain`, doc modułu)
- `src/tui/task_app/tabs/progress.rs` (wywołanie `render_bracketed_list_plain` w `render`, doc modułu)
- `tests/integration/tui_render.rs` (testy osi z helperem `no_colour_but_the_selection_fill`, wiersze ok. 552 i 7325; asercje osi ok. 3242–4050)
- `tests/integration/tui_state.rs` (ok. 1643, użycie `selection_fill`)

**Approach:** `draw_line` rysuje wypełnienie tylko dla dekoracji `Bracketed`. Nowy wariant ma `has_rules() == true` i nie wypełnia. Wywołanie w `progress::render` zmienia się na wariant bez wypełnienia.

**Patterns to follow:** `render_barred_list` w `src/tui/selection.rs` (osobna funkcja nad wspólnym `render_list`). `docs/solutions/design-patterns/tui-row-background-from-semantic-flag-after-render-2026-10-30.md`.

**Test scenarios:**
- Covers AE1. Oś z zaznaczonym krokiem: żadna komórka wierszy kroku nie ma `bg` z `selection_fill`. Klamra (`│`, dwie reguły `─`) jest w buforze.
- Covers AE1. Lista *Historia* z zaznaczonym wpisem ma tło jak dziś (istniejące testy zostają zielone).
- Choosery: istniejące testy zostają zielone.
- Oś z fokusem na osi i z fokusem poza osią: znaki klamry są te same, styl różny (rola fokusu i tła).
- `no_color`: oś z zaznaczeniem ma `│` i `─`, bez `REVERSED` (istniejący `no_reversed_on_the_axis` zostaje).
- Skrajny przypadek: oś o wysokości mniejszej niż krok z regułami pokazuje sam krok z paskiem (bez paniki).

**Verification:** testy `tui_render` i `tui_state` dla osi i *Historii* przechodzą. Test osi nie znajduje wypełnienia.

### U2. Czas przy nazwie, bez słowa stanu na osi

**Goal:** Pierwszy wiersz kroku to ikona, nazwa, czas startu, czas trwania (i zużycie). Drugi wiersz to model, wysiłek, harness. Wąska oś skraca w kolejności z R9.

**Requirements:** R4–R10, AE2, AE3, AE4, AE5 (oś).

**Dependencies:** U1 (ta sama funkcja `render`; kolejność tylko dla czystych różnic).

**Files:**
- `src/tui/task_app/tabs/progress.rs` (`status_look`, `AxisLayout::{usage,table,add_details}`, usunięcie `axis_line`, doc modułu, testy w `mod tests`)
- `src/tui/task_app/tabs/conversation.rs` (wywołanie `status_look` w ok. 1406)
- `tests/integration/tui_render.rs` (oczekiwane teksty z `w toku` na osi ok. 3242–4050, np. `"● ✨ review #1 w toku"`)

**Approach:**
- `status_look` zwraca `(&'static str, Role)`.
- `usage` zawsze buduje tabelę. Gdy nikt nie ma zużycia, kolumny zużycia mają szerokość 0 i tabela daje oś z samą nazwą i czasem.
- `table` dostaje dwie kolumny czasu zamiast kolumny `word`: start (do lewej) i trwanie (do prawej), zbudowane z `clock` i `duration_of`. Szerokość docelowa: prefiks + 2 + nazwa + kolumny czasu + kolumny zużycia, w granicach `today`..`axis_cap`.
- `shrink` skraca nazwę do `MIN_LABEL`, potem zeruje czas trwania, potem czas startu (R9). Wiersz bez treści w kolumnach końcowych nie ma spacji końcowych (R6).
- `add_details` zostawia w drugim wierszu tylko kolumny `agent`. Wiersz istnieje tylko, gdy krok ma model, wysiłek lub harness (R5). Szerokość osi nadal może rosnąć do `axis_cap` z powodu tego wiersza.
- Czas trwania liczy się do `observed_at`, gdy `ended_at` jest puste (R7): `duration_of` zostaje bez zmian.

**Patterns to follow:** istniejące `Column`, `columns_width`, `fit_step_name`, `fit_cells` w tym samym pliku. `docs/plans/2026-11-04-2316-feat-tui-przebieg-dwa-wiersze-na-krok-plan.md` i `docs/plans/2026-11-02-2300-feat-tui-zuzycie-per-krok-w-przebiegu-plan.md` (poprzednie układy osi).

**Test scenarios:**
- Covers AE2. Krok sprzed 90 s z `sonnet`/`high`/`claude-code`: pierwszy wiersz zaczyna się od `✓ <nazwa>`, zawiera `MM-DD HH:MM:SS` i kończy się `1m`. Drugi wiersz to `   sonnet  high  claude-code` bez czasu.
- Covers AE3. Krok bez `started_at`: pierwszy wiersz po `trim_end` to `<ikona> <nazwa>`, bez separatora na końcu.
- Covers AE4. Oś o malejącej szerokości (pętla szerokości jak w `a_very_narrow_tab_cuts_the_label_and_never_panics`): najpierw nazwa dochodzi do `MIN_LABEL` (zawiera `…`), potem czas trwania znika przy nadal widocznym czasie startu, potem znika czas startu. Nazwa nigdy nie znika. Brak paniki dla szerokości 0..60.
- Covers AE5. Krok `InProgress`: tekst wierszy osi nie zawiera `w toku`, ikona to `●` w roli `Live`. Analogicznie `ok`, `błąd`, `przerwany`, `czeka na decyzję` nie występują jako słowa stanu.
- R8. Trzy kroki o różnych nazwach i czasach: kolumna czasu startu zaczyna się w tej samej kolumnie ekranu w każdym wierszu, a czas trwania kończy się w tej samej kolumnie.
- R8. Oś z zużyciem: kolumny zużycia (`$`, `↑`, `↓`, subagenci) stoją za czasem trwania. Istniejące `metric_columns_are_two_spaces_apart` i `values_of_a_kind_stand_in_one_column` dostosowane do nowej kolejności.
- R7. Krok bez `ended_at`: czas trwania liczy się do `observed_at` (istniejący test wiersza z czasem).
- Oś nie przekracza 70% szerokości (istniejący `the_axis_stays_within_seventy_percent_and_leaves_the_rest_to_the_step`), także z czasami w pierwszym wierszu.
- Krok bez modelu, wysiłku i harnessu: jeden wiersz (istniejący test `without_a_report_the_axis_is_as_it_was` dostosowany).
- Integracja: testy `tui_render` z osią zaznaczają `● ✨ review #1` bez `w toku`.

**Verification:** testy w `progress.rs` i `tests/integration/tui_render.rs` przechodzą. Oś nie ma słowa stanu przy żadnym stanie kroku.

### U3. Nagłówek wybranego kroku bez słowa stanu

**Goal:** Nagłówek kroku po prawej stronie nie pokazuje słowa stanu.

**Requirements:** R11, R12, AE5.

**Dependencies:** U2 (zmiana sygnatury `status_look`).

**Files:**
- `src/tui/task_app/tabs/progress.rs` (`step_lines`)
- `tests/integration/tui_render.rs` (ok. 3167–3186: `"🧑🔀 approval #1 · w toku · decision · human"`, `"✨ work #1 · w toku · agent"`)
- `docs/user/tui.md` (opis *Przebiegu* przy liniach ok. 94 i 116: oś nie ma słowa stanu, czas stoi przy nazwie)

**Approach:** `step_lines` usuwa span ` · {word}`. Reszta nagłówka (` · typ akcji`, ` · decydujący`, ` · czas trwania`) zostaje. Wiersz `✗ <błąd>` i noty bez zmian. Komentarze modułu w `src/tui/selection.rs` (linia ok. 17) i `src/tui/task_app/detail.rs` (ok. 18) zawierają przykłady z `w toku`. Dla `progress.rs` dostosuj tylko komentarze, które opisują oś *Przebiegu*. Przykład w `detail.rs` dotyczy innego widoku: sprawdź, czy opisuje oś, i zmień tylko wtedy.

**Test scenarios:**
- Covers AE5. Nagłówek kroku `InProgress` z akcją `agent`: tekst to `✨ work #1 · agent · <czas>`, bez `w toku`; ikona `●` w roli `Live`.
- Nagłówek kroku `decision` z decydującym: `… · decision · human · <czas>`, bez słowa stanu.
- R12. Krok z błędem: drugi wiersz nagłówka to `✗ <treść>`. Krok z `retry_pending`: nota `ponowienie oczekuje` zostaje.
- Krok bez `started_at`: nagłówek bez czasu trwania i bez osieroconego ` · `.

**Verification:** testy nagłówka w `tui_render` przechodzą. Wyszukiwanie `w toku` w kodzie źródłowym `src/` zwraca tylko miejsca poza osią i nagłówkiem *Przebiegu* (np. listę tasków, jeśli istnieje).

---

## System-Wide Impact

- Dotyczy zakładki *Przebieg* tasku w toku i *Historii* zamkniętego tasku (`Context::HistoryAxis`, ta sama oś). Obie dostają zmianę (origin, zakres).
- Wspólny kod `selection.rs` zyskuje wariant. *Historia* i choosery wołają starą funkcję, więc ich wygląd się nie zmienia.
- Brak zmian w API, kontrakcie danych, silniku i konfiguracji.

---

## Risks

- **Szerokość kolumn przy wąskim terminalu:** czas startu zajmuje 14 kolumn, więc nazwy dłuższe niż `MIN_LABEL` skracają się wcześniej niż dziś, gdy oś jest wąska. Mitygacja: kolejność z R9 (czas trwania i start znikają, nazwa ma pierwszeństwo) i test pętli szerokości.
- **Wiele testów integracyjnych zależy od tekstu `w toku` na osi.** Mitygacja: U2 i U3 zmieniają je razem z kodem.
- **Doc modułu `progress.rs` opisuje stary układ (drugi wiersz z czasem).** Mitygacja: aktualizacja w U2.

---

## Deferred to Implementation

- Dokładna szerokość odstępu między nazwą a czasem startu (`COLUMN_GAP` lub inna stała). Decyzja po zobaczeniu wyniku w testach układu.
- Czy test `no_color` potrzebuje osobnego helpera, czy wystarczy istniejący `no_reversed_on_the_axis`.

---

## Documentation

- `docs/user/tui.md`: opis osi *Przebiegu* (bez słowa stanu, czas startu i trwania w pierwszym wierszu).
