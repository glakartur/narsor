---
title: "feat: Wyróżnienie wpisów „WYBÓR WORKFLOW” tłem w zakładce Rozmowa"
type: feat
status: completed
date: 2026-10-30
origin: docs/brainstorms/2026-10-30-tui-wyroznienie-wyborow-workflow-requirements.md
---

# feat: Wyróżnienie wpisów „WYBÓR WORKFLOW” tłem w zakładce Rozmowa

## Summary

Każdy wpis „WYBÓR WORKFLOW” w *Rozmowie* dostaje tło na całej szerokości treści panelu. Bez koloru (`NO_COLOR`) wpis dostaje inwersję. Adapter rozmowy oznacza wpis wyboru jawną flagą w modelu transkryptu. Renderer nie rozpoznaje wpisu po tekście nagłówka. Flaga przechodzi z `TranscriptEntry` do `TranscriptLine` i do zawiniętych wierszy. `render_transcript` maluje komórki tych wierszy po narysowaniu tekstu, tak jak `selection.rs` maluje `selection_fill`. Treść, nagłówek, identyfikator i kolejność wpisu są bez zmian (see origin: `docs/brainstorms/2026-10-30-tui-wyroznienie-wyborow-workflow-requirements.md`).

---

## Problem Frame

Dziś wpis wyboru różni się od innych wpisów tylko kolorem nagłówka (`chat_header` dla `Agent`, `Conductor`, `Operator`). W długiej *Rozmowie* wpis ginie. Operator nie zatrzymał workflow, gdy agent kroku `brainstorm` wybrał `requirements_drafted` z uzasadnieniem „Dokumentu nie zapisano” (see origin).

Fakty z kodu:

- Wpis wyboru buduje `workflow_choice_entry` w `src/tui/task_app/tabs/conversation.rs` ze zdarzeń `DecisionMade`, `ConductorDecision` (`ChooseOutput`), `HumanInputReceived` (operator, z etykietą) i `HumanTransitionSelected`. Wpis ma id `workflow-choice/<sequence>`, pusty `raw` i brak `detail_lines`.
- `TranscriptPresentation` wybiera kolor aktora. Tej samej wartości używają zwykłe wpisy agenta, dyrygenta i operatora. Sama prezentacja nie odróżnia wyboru od wiadomości.
- `Paragraph` w ratatui rysuje tylko grafemy linii. Styl linii nie wypełnia pustej części wiersza (potwierdzone w planie `docs/plans/2026-10-24-001-fix-tui-tlo-aktywnego-pola-plan.md`). Tło do prawej krawędzi wymaga malowania komórek bufora.
- `render_transcript` jest wspólny dla *Rozmowy* (`conversation::render`), *Przebiegu* (`progress.rs`) i *Dyrygenta* (`conductor.rs`). Szczegóły aktywnego zadania i Historia używają tego samego `Conversation` i `conversation::render` (`detail.rs`, `history.rs`).

---

## Requirements Trace

| Wymaganie | Jednostka |
|---|---|
| R1, R2 (tło nagłówka i treści do prawej krawędzi) | U1, U3 |
| R3 (inwersja bez koloru) | U2, U3 |
| R4 (kolor różny od innych teł) | U2 |
| R5 (jeden wygląd dla wszystkich aktorów) | U3 |
| R6 (pasek zaznaczenia widoczny) | U3 |
| R7 (tylko wpisy ze zdarzeń wyboru) | U1, U3 |
| R8 (szczegóły i Historia) | U3 (wspólna ścieżka rysowania) |
| R9, R10 (bez zmian treści i zachowania) | U1, U3 (testy regresji) |
| AE1–AE6 | U3 (AE1–AE5), U3 przez wspólną ścieżkę (AE6) |

---

## Scope Boundaries

- Poza zakresem: zmiana treści, kolejności lub formatu wpisu wyboru.
- Poza zakresem: wyróżnienia w zakładkach *Przebieg*, *Zdarzenia*, *Dyrygent* i na liście zadań. `render_transcript` jest wspólny, ale wpisy tych zakładek nie mają flagi, więc wygląd tych zakładek się nie zmienia.
- Poza zakresem: zmiany w silniku (kontrola istnienia pliku artefaktu, odpowiedź „krok nie został wykonany”). To osobne tematy (see origin).
- Poza zakresem: wykrywanie, czy uzasadnienie agenta zgadza się z wyborem.

### Deferred to Follow-Up Work

- Zmiany w silniku: kontrola istnienia pliku wymaganego artefaktu i jawna odpowiedź agenta „krok nie został wykonany”. Ten plan ich nie realizuje. Origin zostawia otwarte pytanie, czy te tematy dostaną osobne zgłoszenia. Decyzję podejmuje użytkownik.

---

## Key Technical Decisions

- **Jawna flaga w modelu, nie rozpoznawanie po nagłówku.** `TranscriptEntry` dostaje pole `workflow_choice: bool` i metodę budującą w stylu `with_header` / `with_presentation`. Ustawia je tylko `workflow_choice_entry`. Treść agenta z tekstem `{"choice": …}` nigdy nie dostaje flagi, bo flaga nie zależy od tekstu (R7, AE5). Jest tylko jeden rodzaj wyróżnienia, więc wystarczy `bool`; enum nie jest potrzebny. Rozstrzyga pytanie z origin „Jak oznaczyć wpis wyboru”.
- **Flaga przechodzi do wierszy.** `TranscriptLine` dostaje to samo pole. `lines_for` ustawia je dla nagłówka i każdej linii treści karty. `wrap_visible_lines` kopiuje je do każdego zawiniętego fragmentu (także fragmentów nagłówka po pierwszym). Widok RAW wpisu (globalny `DisplayMode::Raw` i przełącznik RAW jednego wpisu) nie dostaje wyróżnienia: to są surowe bajty źródła, nie karta wyboru. Widok `Chronology` dzieli kartę z `Rendered`, więc dostaje wyróżnienie.
- **Malowanie komórek po narysowaniu tekstu.** `render_transcript` po `Paragraph` przechodzi po widocznych wierszach z flagą i nakłada styl tła na komórki od kolumny `PREFIX_WIDTH` do prawej krawędzi obszaru. Wzorzec: pętla `cell.set_style(background)` w `src/tui/selection.rs` (rysowanie `selection_fill`). `set_style` łata styl, więc kolor tekstu nagłówka (aktor) i treści zostaje (R5). Rozstrzyga pytanie z origin o tło przy zawijaniu i rozwinięciu: wyróżnienie dotyczy wierszy, nie wpisów, więc działa po zawinięciu i po zmianie szerokości.
- **Kolumny paska zaznaczenia bez tła.** Dwie kolumny `bar_prefix` zostają bez wyróżnienia. Pasek zaznaczenia ma więc ten sam wygląd co dziś, w obu trybach (R6, AE4). Blok tła zaczyna się w kolumnie treści i sięga do prawej krawędzi panelu (R2).
- **Kolor tła: `Color::Indexed(58)` (ciemna oliwka, #5f5f00).** Nowa metoda `Styles::workflow_choice_background()` obok `selection_fill`, `task_header_background`, `answer_panel_background`, `field_editing`. Odcień oliwkowy różni się od szarości zaznaczenia (`Indexed(236)`), szarości panelu odpowiedzi (`Rgb(38,40,43)`) i granatów nagłówka zadania i pola edycji (R4). Kolor indeksowany (jak `selection_fill`) działa na terminalach z 256 kolorami i z truecolor. Odcień nie jest odcieniem żadnego aktora (zielony, magenta, cyjan), więc nagłówki zachowują kontrast. Rozstrzyga pytanie z origin o kolor. Na terminalach z 16 kolorami zachowanie zależy od terminala (terminal może przybliżyć kolor albo go nie pokazać), tak samo jak dziś dla `selection_fill` i teł RGB.
- **Bez koloru: `Modifier::REVERSED`.** Metoda w trybie `no_color` zwraca styl z samą inwersją. Inwersja pustych komórek daje pełny blok do prawej krawędzi (R3, AE2). To zgodne z zasadą `style.rs`: bez koloru zostają bold, dim i reverse.
- **Wpis nieaktualny (`stale`) zachowuje tło.** Tekst bierze `Role::Done` jak dziś, a malowanie tła następuje po nim. Przyjęte z założenia origin.
- **Jeden wygląd dla wszystkich aktorów.** Flaga nie zależy od `TranscriptPresentation` (R5, AE3).

---

## Assumptions

- Założenia origin są przyjęte bez zmian: wyróżnienie dotyczy tylko wpisów „WYBÓR WORKFLOW” w *Rozmowie*, wszystkich aktorów, a wpis `stale` zachowuje tło.
- Wybrany kolor `Indexed(58)` jest wartością startową. Implementujący może zmienić odcień po ręcznym sprawdzeniu kontrastu z zielonym, magentą i cyjanem oraz z tekstem wpisu `stale` (`Role::Done`). Warunek: kolor nadal różni się od czterech innych teł (test w U2 to sprawdza).
- Błędy (`TranscriptPresentation::Error`) bez koloru już dziś mają odwrócony nagłówek. To nie jest konflikt z AE2: AE2 dotyczy rozmów kroków, a nagłówki błędów zostają bez zmian.

---

## Implementation Units

### U1. Flaga wyróżnienia w modelu transkryptu

**Goal:** `TranscriptEntry` i `TranscriptLine` niosą informację, że wpis jest wyborem workflow, a zawinięte wiersze ją zachowują.

**Requirements:** R1, R2, R7, R9, R10

**Dependencies:** brak

**Files:**
- Modify: `src/tui/transcript.rs`
- Test: `src/tui/transcript.rs` (moduł testów w pliku)

**Approach:**
- Nowe pole `workflow_choice: bool` w `TranscriptEntry` z wartością `false` w `TranscriptEntry::new`. Metoda budująca w stylu `with_presentation`.
- Nowe publiczne pole `workflow_choice: bool` w `TranscriptLine`. Uzupełnić wszystkie literały `TranscriptLine { … }`: `wrap_visible_lines`, `logical_lines` (linie bez wpisu, `false`), `lines_for`, test w module testów.
- `lines_for`: w gałęzi karty (Rendered/Chronology, wpis nie w RAW) flaga z wpisu dla nagłówka, linii treści i linii szczegółów. W gałęziach RAW `false`.
- `wrap_visible_lines`: kopiuje flagę do każdego fragmentu bez wyjątku (inaczej niż `header` i `selected`).
- Liczenie wierszy, kotwica, kursor, rozwinięcie i tryb „podąża” zostają bez zmian (R10). Pole nie wpływa na `PartialEq` w sposób, który zmienia logikę `replace_entries` / `append` — sprawdzić, że porównania wpisów nadal działają.

**Patterns to follow:** `with_presentation`, `sharing_previous_raw` w `src/tui/transcript.rs`; propagacja `presentation` w `wrap_visible_lines`.

**Test scenarios:**
- Happy path: wpis z flagą i dwiema liniami treści → `logical_lines` daje trzy wiersze (nagłówek + 2) i każdy ma flagę.
- Happy path: wpis bez flagi → żaden wiersz nie ma flagi.
- Edge case: wpis z flagą, nagłówek i treść dłuższe niż szerokość → `wrap_visible_lines` daje kilka fragmentów, każdy fragment ma flagę (także drugi fragment nagłówka, który ma `header: false`).
- Edge case: wpis z flagą w trybie `DisplayMode::Raw` i po `toggle_selected_raw` → wiersz RAW nie ma flagi.
- Edge case: wpis z flagą w trybie `Chronology` → wiersze mają flagę.
- Regresja: liczba wierszy wizualnych (`visual_line_count_for`) i przewijanie są takie same z flagą i bez niej.

**Verification:** Model niesie flagę do każdego widocznego wiersza karty. Istniejące testy transkryptu przechodzą bez zmian.

---

### U2. Styl tła wyboru workflow

**Goal:** `Styles` daje jeden styl wyróżnienia: kolorowe tło albo inwersję bez koloru.

**Requirements:** R3, R4

**Dependencies:** brak

**Files:**
- Modify: `src/tui/style.rs`
- Test: `tests/tui_state.rs` (moduł testów stylów, obok `selection_marks_are_thin_in_the_focus_or_the_background_role`)

**Approach:**
- Nowa metoda `workflow_choice_background()` (nazwa do wyboru, spójna z `selection_fill`, `field_editing`) z komentarzem w stylu sąsiednich metod: co wyróżnia, jaki kolor, co bez koloru.
- Kolor: `Color::Indexed(58)`. Bez koloru: tylko `Modifier::REVERSED`, bez `fg` i `bg`.
- Dopisać jedno zdanie w dokumentacji modułu w punkcie o stałych tłach („the few fixed backgrounds …”): wybór workflow w transkrypcie.
- Nie dodawać metody do `all_styles` w testach: ta funkcja obejmuje style tekstu, a stałe tła są z niej wyłączone (jak `selection_fill`).

**Patterns to follow:** `field_editing`, `answer_panel_background`, `key_cap` w `src/tui/style.rs`.

**Test scenarios:**
- Happy path: tryb koloru → styl ma `bg` i nie ma `fg` ani `REVERSED`.
- Covers R4: tło różni się od `bg` stylów `selection_fill`, `task_header_background`, `answer_panel_background`, `field_editing` w trybie koloru.
- Covers R3: tryb `no_color` → `fg` i `bg` puste, modyfikator zawiera `REVERSED`.

**Verification:** Testy stylów przechodzą, w tym `colour_mode_uses_only_the_sixteen_ansi_colours` i `no_color_mode_leaves_only_bold_dim_underline_and_reverse`.

---

### U3. Oznaczenie wpisu wyboru i rysowanie bloku w Rozmowie

**Goal:** Wpisy z `workflow_choice_entry` mają flagę. `render_transcript` maluje tło na ich wierszach od kolumny treści do prawej krawędzi.

**Requirements:** R1, R2, R3, R5, R6, R7, R8, R9, R10; AE1–AE6

**Dependencies:** U1, U2

**Files:**
- Modify: `src/tui/task_app/tabs/conversation.rs` (`workflow_choice_entry`, `render_transcript`, ewentualnie `transcript_row`)
- Test: `tests/tui_render.rs` (moduł z `conversation_tab` / `draw_conversation_at`), `tests/tui_state.rs` (moduł z `a_choice_keeps_its_text_header_and_id`)
- Uwaga do testów renderu: `conversation_tab` buduje `Conversation` przez `Conversation::rebuild` bez zdarzeń, więc nie daje wpisów `workflow-choice/<n>`. Builderów zdarzeń (`ev`, `agent_chose`, `conductor_chose`) nie ma w `tests/tui_render.rs` — są tylko w `tests/tui_state.rs`. Dodać w `tests/tui_render.rs` pomocnika, który wywołuje `Conversation::rebuild_with_events` ze zdarzeniami wyboru (`DecisionMade`, `ConductorDecision` `ChooseOutput`, `HumanTransitionSelected`), na wzór builderów z `tests/tui_state.rs`.

**Approach:**
- `workflow_choice_entry` wywołuje nową metodę budującą z U1. Nic poza tym się nie zmienia: tekst, nagłówek, id, prezentacja (R9).
- `render_transcript`: zachować listę wierszy po `wrap_visible_lines` (dziś jest od razu mapowana na `Line`). Po `frame.render_widget(Paragraph…)` dla każdego wiersza `i` z flagą nałożyć styl z U2 na komórki `(area.x + PREFIX_WIDTH .. area.x + area.width, area.y + i)`. Kolumny prefiksu zostają bez zmian (R6). Zakres malowania jest ograniczony do obszaru: gdy `area.width <= PREFIX_WIDTH`, zakres jest pusty (użyć `saturating_sub` jak w `render_transcript`). Wiersze poza wysokością obszaru są pomijane.
- Wspólna funkcja dla *Przebiegu* i *Dyrygenta* nie wymaga zmian w wywołaniach: ich wpisy nie mają flagi.
- R8 wynika z tego, że szczegóły i Historia rysują przez `conversation::render`. Nie trzeba zmian w `detail.rs` ani `history.rs`.

**Patterns to follow:** pętla malująca `selection_fill` w `src/tui/selection.rs`; testy wypełnienia `assert_filled` / `without_colour_no_cell_has_a_fill` w `tests/tui_render.rs`.

**Test scenarios:**
- Covers AE1. Rozmowa z wiadomością agenta i wyborem agenta `requirements_drafted` z uzasadnieniem, które zawija się do dwóch wierszy (szerokość 60) → nagłówek wyboru i oba wiersze treści mają tło w każdej komórce od kolumny 2 do ostatniej kolumny obszaru. Wiersze wiadomości agenta nie mają tego tła.
- Covers AE2. `Styles::new(true)`, ten sam układ → komórki wierszy wyboru od kolumny 2 mają `REVERSED`. Komórki wiadomości kroku nie mają `REVERSED` i żadna komórka nie ma koloru.
- Covers AE3. Wybór agenta, dyrygenta i użytkownika w jednej rozmowie → wszystkie trzy bloki mają to samo tło. Pierwszy znak tekstu nagłówka ma `fg` zielony, magenta i cyjan.
- Covers AE4. Zaznaczony wpis wyboru, `focused = true` → kolumna 0 ma `│` w stylu `selection_marks(true)` bez tła wyboru. Komórki treści mają tło wyboru. To samo dla `focused = false`.
- Covers AE5. Wpis agenta z tekstem `{"choice": "x"}` w treści → żaden wiersz nie ma tła wyboru. Test stanu: wpis z `rebuild_with_events` dla zwykłej wiadomości nie ma flagi, wpis `workflow-choice/<n>` ma flagę.
- Covers AE6. Ten sam stan rysowany przez `conversation::render` dla zadania z Historii (np. przez `rebuild_with_events` jak w `history.rs`) → wiersze wyboru mają tło. Wystarczy test, który pokazuje wspólną ścieżkę; nie trzeba osobnej aplikacji Historii, jeśli istniejący pomocnik testów jej nie daje.
- Edge case: `stale = true` → tekst ma `Role::Done`, komórki nadal mają tło wyboru.
- Edge case: szerokość panelu równa `PREFIX_WIDTH` lub mniejsza → render nie panikuje i nie maluje komórek poza obszarem.
- Edge case: wpis wyboru częściowo poza widokiem (przewinięty) → tło mają tylko jego widoczne wiersze, a wiersze sąsiednich wpisów nie.
- Regresja (R9): `a_choice_keeps_its_text_header_and_id` i testy kolejności wyborów przechodzą bez zmian.
- Regresja (R10): w trybie `Raw` rozmowy wiersze nie mają tła wyboru.

**Verification:** W *Rozmowie* każdy wpis wyboru jest blokiem z tłem (albo inwersją) od kolumny treści do prawej krawędzi. Inne zakładki wyglądają jak dziś. Cały zestaw testów TUI przechodzi.

---

## System-Wide Impact

- `TranscriptLine` dostaje nowe publiczne pole. Literały tej struktury są tylko w `src/tui/transcript.rs`, więc zmiana jest lokalna.
- `render_transcript` jest wspólny dla trzech zakładek. Zmiana jest bezpieczna, bo wyróżnienie zależy od flagi, którą ustawia tylko adapter *Rozmowy*.
- Testy renderu w trybie koloru, które sprawdzają brak tła w całej *Rozmowie* z wyborami, mogą wymagać aktualizacji. Takie testy trzeba poprawić tylko dla wierszy wyboru, nie osłabiać ich dla innych wierszy.

---

## Risks

| Ryzyko | Skutek | Ograniczenie |
|---|---|---|
| Kontrast nagłówka zielonego na oliwkowym tle jest słaby w niektórych motywach | Nagłówek agenta trudniej czytać | Nagłówek jest pogrubiony; kolor jest w jednej metodzie i można go zmienić po ręcznym sprawdzeniu; test R4 pilnuje odrębności |
| Terminal z 16 kolorami może nie pokazać koloru indeksowanego (zależy od terminala) | Brak tła w kolorze lub inny odcień | Takie samo zachowanie jak dziś dla `selection_fill`; operator może użyć `NO_COLOR`, który daje inwersję |

---

## Sources & References

- Origin: `docs/brainstorms/2026-10-30-tui-wyroznienie-wyborow-workflow-requirements.md`
- `src/tui/task_app/tabs/conversation.rs` (`workflow_choice_entry`, `transcript_row`, `render_transcript`, `render`)
- `src/tui/transcript.rs` (`TranscriptEntry`, `TranscriptLine`, `lines_for`, `wrap_visible_lines`)
- `src/tui/style.rs` (stałe tła, `chat_header`)
- `src/tui/selection.rs` (`bar_prefix`, malowanie `selection_fill`)
- `docs/plans/2026-10-24-001-fix-tui-tlo-aktywnego-pola-plan.md` (zachowanie `Paragraph` i tło do krawędzi)
