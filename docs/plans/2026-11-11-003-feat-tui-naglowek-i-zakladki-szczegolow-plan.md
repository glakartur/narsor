---
title: "feat: Nagłówek i belka zakładek w szczegółach taska"
type: feat
status: active
date: 2026-11-11
origin: docs/brainstorms/2026-11-11-tui-naglowek-i-zakladki-szczegolow-taska-requirements.md
---

# feat: Nagłówek i belka zakładek w szczegółach taska

## Summary

Zmieniamy tylko rysowanie szczegółów taska w TUI. Nagłówek i belka zakładek tracą granatowe tło. Pusty wiersz nad nagłówkiem znika. Kolejność wierszy to: tytuł (gdy ramka go skraca), workspace'y, status, koszt, powiadomienie. Wiersze workspace'ów dostają separatory ` · ` i tracą `—`. Status traci `bez slotu`. Belka zakładek stoi między dwiema liniami w kolorze ramki, z łącznikami `├`/`┤` na bokach ramki. *Rozmowa* jest pierwszą zakładką. Znaczniki śledzenia przechodzą z belki na linie nad listą kroków. Tekst w pigułkach na ramce ma kolor RGB `(0, 0, 0)`.

Pochodzenie: `docs/brainstorms/2026-11-11-tui-naglowek-i-zakladki-szczegolow-taska-requirements.md` (see origin).

---

## Problem Frame

Opis problemu jest w dokumencie wymagań (sekcja *Problem Frame*). Krótko: tło nagłówka odcina go od ramki, pusty wiersz zajmuje miejsce, kolejność wierszy nie pasuje do sposobu czytania, wiersz workspace'u nie ma separatorów, znaczniki śledzenia stoją w belce wspólnej dla wszystkich zakładek, a *Rozmowa* nie jest pierwsza.

---

## Requirements

Plan realizuje wszystkie wymagania R1–R31 z dokumentu wymagań. Grupy:

- Tło: R1, R2, R3 → U1, U3
- Pigułki na ramce: R4, R5 → U1
- Kolejność i wiersze nagłówka: R6–R10 → U3
- Wiersze workspace'ów: R11–R16, R30 → U2
- Status: R17 → U3
- Belka zakładek: R18–R24, R29 → U4 (R18 także U5)
- Znacznik w *Rozmowie*: R25–R28 → U6
- Znacznik w *Przebiegu*: R31 → U6 (z otwartym pytaniem P1)
- Przykłady akceptacji AE1–AE8 mają testy w U2, U3, U4, U6.

---

## Scope Boundaries

- Treść zakładek *Przebieg*, *Zdarzenia*, *Dyrygent*, *Info* bez zmian (poza miejscem znacznika *Przebiegu*).
- Lista tasków, panel odpowiedzi (`answer_panel_background`), formularze bez zmian.
- `bez slotu` zostaje w liście tasków i w zakładce *Info* (`src/tui/task_app/tabs/info.rs`). Zmiana dotyczy tylko wiersza statusu w nagłówku.
- Logika śledzenia i klawisz `f` bez zmian w obu zakładkach. Teksty pomocy klawiszy w `src/tui/keymap.rs` bez zmian.
- Domyślna zakładka bez zmian (Z7): `DetailState::new` już otwiera *Rozmowę*, historia (`history.rs`) otwiera *Przebieg*.
- Historia (`src/tui/task_app/history.rs`) dostaje zmiany przez wspólne `render_view` i `render_panel_edge`. Wymaga tylko podania nowego pola `ViewOptions::frame_focused` (U4).

### Deferred to Follow-Up Work

- Brak.

---

## Context & Research

### Relevant Code and Patterns

- `src/tui/task_app/detail.rs`:
  - `render_view` — rysuje nagłówek (`header`), wstawia pusty wiersz (`lines.insert(0, Line::default())`), dodaje czarną linię (`task_tabs_divider`), maluje tło (`task_header_background`) w obszarze nagłówka i belki, rysuje `tab_bar`, zeruje tło wiersza podkreślenia.
  - `areas` — dzieli panel: nagłówek (`header_lines`), belka (2 wiersze), treść, panel odpowiedzi.
  - Stałe `QUIET_ROW` i `SEPARATOR_ROW`; `link_spots` dodaje `QUIET_ROW` do wiersza linku.
  - `header` — kolejność: tytuł, status, workspace'y, koszt.
  - `status_parts` — dodaje `bez slotu` przez `without_slot(task)`.
  - `tab_bar` — nazwy zakładek, znaczniki śledzenia na końcu wiersza nazw, drugi wiersz z podkreśleniem aktywnej zakładki.
  - `workspace_line` — mapuje `Part` na rolę i liczy kolumnę linku PR.
  - Komentarz modułu (linie 1–18) pokazuje stary układ.
- `src/tui/task_app/workspace_rows.rs` — `Entry`, `Columns::fitted`, `Columns::overflow`, `Columns::row`; stałe `GAP` i `NO_PR`; `Part::NoPr`.
- `src/tui/task_app/mod.rs` — `DetailTab::ALL` (kolejność zakładek), `DetailTab::shifted` (przechodzenie klawiszami używa `ALL`).
- `src/tui/style.rs` — `task_header_background`, `task_tabs_divider`, `edge_pill` (tekst `Color::Black`), `render_panel_edge` (rola ramki: `Focus` przy fokusie, w innym przypadku `Done`; pigułki `Done` przy `stale`), `selected_tab`, `FRAME = BorderType::Rounded`.
- `src/tui/task_app/tabs/conversation.rs` — `render_step_strip` rysuje linię, wiersz kroków, linię (`STEP_STRIP_ROWS = 3`); `Conversation::follows()`.
- `src/tui/task_app/tabs/progress.rs` — `render` rysuje oś kroków (`render_bracketed_list`) od pierwszego wiersza treści. **Nad osią nie ma dziś osobnej linii.** Patrz P1.
- `src/tui/task_app/history.rs` — drugi wywołujący `render_panel_edge` i `render_view` (`ViewOptions`).

### Tests that describe the current layout

- `tests/integration/tui_render.rs`:
  - `the_task_header_uses_a_muted_navy_background_and_rule_above_tabs` — opisuje tło, pusty wiersz i czarną linię. Do zastąpienia.
  - `the_header_of_a_running_task_shows_the_status_the_step_and_each_workspace` (asercja `tabs.contains("śledzi")` ok. linii 2263) — do zmiany.
  - `the_pill_text_is_dark_on_the_frame_colour` — asercja `Color::Black`. Do zmiany na `Color::Rgb(0, 0, 0)`.
  - Testy workspace'ów: `a_narrower_terminal_cuts_the_workspace_row_in_a_fixed_order`, `the_header_has_a_row_for_each_workspace`, `the_workspace_row_has_coloured_changes_and_a_pull_request_link`, `the_columns_of_two_workspaces_line_up`, `a_queued_task_shows_its_workspaces_without_changes_or_a_pull_request`.
  - Testy tytułu i kosztu: `a_title_the_edge_cuts_is_said_in_full_on_the_first_header_row`, `the_title_row_appears_only_when_the_edge_cuts_the_title`, `the_header_shows_what_the_task_used_in_a_row_under_the_workspaces`, `a_default_harness_the_task_cannot_use_is_one_line_in_the_header_and_a_row_in_info`, `wide_the_header_has_everything`.
  - Testy belki i rozmowy: `the_conversation_tab_has_the_steps_newest_first_between_two_rules`, `the_conversation_tab_bar_follows_the_focus`, `with_focus_in_the_panel_backgrounds_and_markers_replace_separator_rules`.
  - Testy, które szukają wiersza po `"Przebieg"` i liczą przesunięcia (`row_of(&buffer, "Przebieg")`) — przesunięcia zmienią się o −1 wiersz.
- `tests/integration/tui_render.rs` (ok. linii 4983, test panelu odpowiedzi) — asercja `fg == Color::Black` na dzielniku w wierszu nad `row_of(&buffer, "Przebieg")`. Do zmiany na asercję roli linii (U4).
- `tests/integration/tui_form.rs` — `the_field_in_edit_is_a_navy_darker_than_the_task_header` porównuje z `task_header_background`.
- `tests/integration/tui_state.rs` — `a_workflow_choice_is_a_background_of_its_own_or_reversed` wymienia `task_header_background`.

### Institutional Learnings

- Poprzednie plany tego obszaru: `docs/plans/2026-11-08-0211-feat-tui-ramka-wiersze-i-zuzycie-plan.md`, `docs/plans/2026-11-06-1200-feat-tui-repo-galaz-w-wierszu-i-rozmowa-kroku-plan.md`, `docs/plans/2026-11-07-0518-refactor-naglowek-zadania-bez-display-plan.md`. Wzorzec: testy renderowania w `tests/integration/tui_render.rs` na buforze `ratatui`, asercje na tekście wierszy i na `fg`/`bg` komórek.

### External References

- Nie są potrzebne. Repozytorium ma silne lokalne wzorce (`ratatui`, testy na buforze).

---

## Key Technical Decisions

- **Górna linia belki jest ostatnim wierszem obszaru nagłówka; dolna linia jest drugim wierszem obszaru belki.** `SEPARATOR_ROW` zostaje (teraz to górna linia), `QUIET_ROW` znika. `areas` zachowuje belkę o wysokości 2 (nazwy + dolna linia). Dzięki temu tryb `deferred` (który rysuje tylko nagłówek) nadal ma linię pod nagłówkiem, a układ obszarów zmienia się minimalnie. Suma wierszy belki to 3 (R19).
- **Linie belki rysujemy bezpośrednio w buforze przez całą szerokość wnętrza panelu, a łączniki na komórkach ramki** (`area.x − 1` i `area.right()`), tylko gdy komórka leży w buforze i zawiera `│` ramki. `render_view` dostaje obszar wewnątrz ramki w obu wywołaniach (`detail::render`, `history::render`), więc boki ramki są dokładnie obok.
- **Rola linii pochodzi z jawnego pola `frame_focused` w `ViewOptions`.** Oba wywołujące podają tę samą wartość, którą dają `render_panel_edge`. Rola: `Focus`, gdy `frame_focused && !stale`; w innym przypadku `Done` (R21).
- **Odcinek pod aktywną zakładką to zawsze `━`** z pogrubieniem: rola `Focus`, gdy treść ma klawiaturę (`focused`, jak dziś dla podkreślenia), w innym przypadku `Done` z `BOLD` (R22, AE6). Dziś bez fokusu jest `─`; wymaganie R22 mówi `━` w obu przypadkach.
- **Pozycję odcinka liczy `tab_bar`** (już liczy `selected_start`/`selected_width`). `tab_bar` zwraca wiersz nazw i pozycję odcinka; `render_view` rysuje linie.
- **Separatory w wierszu workspace'u to osobne elementy `Piece` z rolą `Done`**, wstawiane w `Columns::row` w miejsce `GAP` między repo–gałąź, (gałąź/baza)–statystyki i statystyki–PR. Między nazwą a repo zostaje spacja (Z3). Separator stoi przed kolumną tylko wtedy, gdy kolumna w tym wierszu ma tekst. Gdy kolumna ma tekst w innym wierszu, a w tym nie, wiersz dostaje spacje o szerokości separatora i kolumny, aby kolumny dalej były wyrównane (R16). Wiersz kończy się tam, gdzie kończy się tekst (końcowe puste kolumny i ich separatory znikają, R15). `Columns::overflow` liczy szerokość separatora (3 komórki) zamiast `GAP` dla tych trzech połączeń, więc kolejność skracania zostaje ta sama (R16).
- **`NO_PR`, `Part::NoPr` i gałąź `(None, None) if branch_known` znikają** (R11).
- **Link PR**: `workspace_line` liczy kolumnę z szerokości wszystkich elementów, także separatorów, więc OSC 8 trafia w tekst PR bez osobnej zmiany (R30). `link_spots` przestaje dodawać `QUIET_ROW`.
- **`bez slotu`**: usuwamy tylko gałąź `without_slot` w `status_parts` (R17). Funkcja `without_slot` zostaje, gdy inne miejsca jej używają; gdy nie ma innych użyć, usuwamy ją.
- **Tło**: usuwamy oba pętle malowania tła i zerowanie tła wiersza podkreślenia w `render_view`. Usuwamy `Styles::task_header_background` i `Styles::task_tabs_divider`, bo nie mają innych użyć w kodzie produkcyjnym. Testy w `tui_form.rs` i `tui_state.rs`, które ich używają, zmieniamy (U1). Komentarz `field_editing` przestaje odwoływać się do nagłówka.
- **Czarny tekst pigułki**: `edge_pill` używa `Color::Rgb(0, 0, 0)` zamiast `Color::Black` (Z1). Tryb bez koloru bez zmian (R5). `edge_pill` ma wywołania tylko w `render_panel_edge`, więc zmiana dotyczy tylko pigułek na ramce (Z2).
- **Kolejność zakładek**: zmieniamy tylko `DetailTab::ALL` (R18). `shifted` używa `ALL`, więc klawisze dziedziczą kolejność.
- **Znacznik w linii**: wspólna mała funkcja pomocnicza rysuje linię `─` z tekstem przy prawym końcu (`───── ↧ śledzi [f] ─`), w kolorze linii, z tekstem znacznika w swojej roli. Gdy linia jest za krótka na znacznik ze spacją z każdej strony i jedną komórką linii z każdej strony (szerokość tekstu + 4), rysuje samą linię (R27). Funkcję tworzy U3 w `src/tui/task_app/detail.rs` (jako `pub(super)`); `tabs/conversation.rs` już importuje z `detail`, więc U4 i U6 tylko ją wywołują. Teksty: *Rozmowa* `↧ śledzi [f]` (`Live`) / `⏸ ręcznie [f]` (`Attention`); *Przebieg* `↧ podąża [f]` (`Live`) / `· ręcznie [f]` (`Done`, jak dziś).

---

## Assumptions

- A1. Linie belki przy `stale` mają rolę `Done`, także gdy ramka jest w roli `Focus` (`render_panel_edge` dziś nie szarzeje ramki przy `stale`). Plan stosuje R21 dosłownie.
- A2. Przy tasku bez gałęzi i bez statystyk wiersz ma tylko `name: repo` (jak dziś bez `—`).
- A3. Krótka forma znacznika *Przebiegu* to `↧ podąża [f]` i `· ręcznie [f]`, w rolach jak dziś (`Live`, `Done`). R31 mówi „kolory jak w R26”, ale R26 opisuje role *Rozmowy*; plan zostawia role *Przebiegu* bez zmian, bo zakres mówi, że logika i wygląd *Przebiegu* zmieniają się tylko w miejscu i tekście.
- A4. Znacznik *Przebiegu* jest widoczny tylko wtedy, gdy oś ma kroki (`!progress.positions().is_empty()`), jak dziś.

---

## Pytania otwarte dla użytkownika

- **P1 (R31, sekcja „Znacznik śledzenia w *Przebiegu*”).** W zakładce *Przebieg* nad osią kroków nie ma dziś osobnej linii: oś zaczyna się w pierwszym wierszu pod belką. Bezpośrednio nad osią stoi dolna linia belki zakładek. Gdzie ma stać znacznik `↧ podąża [f]` / `· ręcznie [f]`?
  - (a) **Zalecane:** na dolnej linii belki zakładek, przy jej prawym końcu, tylko gdy aktywna jest zakładka *Przebieg*. Brak zmiany wysokości treści *Przebiegu*.
  - (b) Na nowej linii `─` w pierwszym wierszu treści *Przebiegu*, nad osią (jak górna linia paska kroków w *Rozmowie*). Oś traci jeden wiersz.
  - (c) Na nowej linii tylko nad kolumną osi (do szerokości osi), nie nad szczegółami kroku. Oś traci jeden wiersz.

  Plan (U6) zakłada opcję (a). Gdy decyzja jest inna, zmienia się tylko część *Przebiegu* w U6.

---

## High-Level Technical Design

> *Ten szkic pokazuje zamierzony układ i jest wskazówką do przeglądu, nie specyfikacją implementacji.*

```text
╭▸ NAR-12 ─ Długi tytuł…──────────────────────── a1b2c3 ─╮
│  Długi tytuł zadania w całości                          │  ← tylko gdy title_cut (R7)
│  code: orders-api · ⎇ feat/nar-12 → main · +120 −34 · #42 ↗
│  docs: orders-docs · ⎇ main                             │  ← bez „—” (R11)
│  ● running · krok 3 · implement · code-review v3 · 14m  │  ← bez „bez slotu” (R17)
│  razem $68.30 · …                                       │  ← tylko gdy usage (R9)
│  (powiadomienie)                                        │  ← tylko gdy jest (R10)
├─────────────────────────────────────────────────────────┤  ← górna linia = ostatni wiersz obszaru nagłówka
│   Rozmowa   Przebieg   Zdarzenia   Dyrygent   Info      │  ← obszar belki, wiersz 1
├──━━━━━━━────────────────────────────────────────────────┤  ← obszar belki, wiersz 2 (P1 (a): tu znacznik Przebiegu)
──────────────────────────────────── ↧ śledzi [f] ─         ← górna linia paska kroków Rozmowy (R25–R27)
   ✓ plan · ● review_api
─────────────────────────────────────────────────────────
```

---

## Implementation Units

### U1. Styl: bez tła nagłówka, czarny tekst pigułki

**Goal:** Usunąć style tła nagłówka i czarnej linii; pigułki z tekstem RGB `(0, 0, 0)`.

**Requirements:** R1, R2 (część stylu), R3, R4, R5

**Dependencies:** brak

**Files:**
- Modify: `src/tui/style.rs`
- Test: `tests/integration/tui_render.rs`, `tests/integration/tui_form.rs`, `tests/integration/tui_state.rs`

**Approach:**
- `edge_pill`: tekst `fg(Color::Rgb(0, 0, 0))`. Gałąź `no_color` bez zmian.
- W tej jednostce zmienić tylko testy, które używają `task_header_background` i `task_tabs_divider`, tak aby nie zależały od tych metod. Usunięcie metod należy do U3 (U3 usuwa ich ostatnie wywołania w `render_view`).
- Komentarz `field_editing` opisuje sam granat, bez porównania z nagłówkiem.

**Test scenarios:**
- Happy path: `the_pill_text_is_dark_on_the_frame_colour` — tekst pigułki ma `fg == Color::Rgb(0, 0, 0)`, tło w kolorze roli (Covers R4).
- Edge case: pigułka bez fokusu i przy `stale` — tekst nadal `Rgb(0, 0, 0)` (R4).
- Edge case: tryb bez koloru — pigułka to `┤ tekst ├`, bez `fg` RGB (R5; istniejący `without_colour_the_pills_are_bracketed_and_with_colour_they_use_the_caps`).
- `tui_form.rs`: test pola w edycji sprawdza tylko, że fill jest granatowy (niebieski dominuje) i nie zmienia `fg`.
- `tui_state.rs`: lista porównania tła wyboru workflow bez `task_header_background`.

**Verification:** Pigułki mają czarny tekst RGB w każdym stanie; testy nie odwołują się do `task_header_background` i `task_tabs_divider`.

---

### U2. Wiersze workspace'ów: separatory i bez `—`

**Goal:** Separatory ` · ` między repo, gałęzią, statystykami i PR; brak pola PR bez PR.

**Requirements:** R11–R16, R30; AE3, AE4; Z3, Z4, Z5

**Dependencies:** brak

**Files:**
- Modify: `src/tui/task_app/workspace_rows.rs`, `src/tui/task_app/detail.rs` (`workspace_line`: rola separatora, usunięcie `Part::NoPr`)
- Test: `tests/integration/tui_render.rs`

**Approach:**
- Nowa stała separatora ` · ` i nowy `Part` dla separatora (rola `Done` w `workspace_line`).
- `Entry::of`: brak PR → `pr: None` (usunąć `NO_PR`, `Part::NoPr`).
- `Columns::row`: zbudować wiersz z kolumn; przed kolumną repo spacja, przed gałęzią, statystykami i PR separator — tylko gdy kolumna ma tekst w tym wierszu i przed nią jest widoczny tekst. Gdy kolumna jest wyrównana (szerokość > 0), a w tym wierszu pusta, wstawić spacje o szerokości `separator + kolumna`, chyba że po niej nie ma już tekstu (wtedy wiersz się kończy).
- `Columns::overflow`: separator liczy 3 komórki dla połączeń repo–gałąź, gałąź–statystyki, statystyki–PR; spacja dla nazwa–repo. Kolejność skracania w `fitted` bez zmian.
- Zaktualizować komentarz modułu (przykład z separatorami, bez `—`).

**Patterns to follow:** istniejące `Columns::piece`, `change_pieces` (pola `joined` dla `+N −M`).

**Test scenarios:**
- Covers AE3. Workspace `orders-api`, gałąź `feat/nar-12` na `main`, `+120 −34`, PR `#42` z adresem → wiersz `code: orders-api · ⎇ feat/nar-12 → main · +120 −34 · #42 ↗`.
- Covers AE4. Workspace `docs` / `orders-docs` na `main`, bez zmian, bez PR → `docs: orders-docs · ⎇ main`, bez `—`, bez końcowego `·`.
- Edge case (Z5): gałąź i PR, bez statystyk, jeden wiersz → `… · ⎇ main · #7` (jeden separator).
- Edge case: dwa wiersze, pierwszy ze statystykami i PR, drugi bez obu → kolumny `⎇` w obu wierszach zaczynają się w tej samej kolumnie; drugi wiersz kończy się na gałęzi.
- Edge case: dwa wiersze, pierwszy ze statystykami, drugi bez statystyk, ale z PR → PR obu wierszy zaczyna się w tej samej kolumnie; w drugim wierszu jeden separator przed PR, a miejsce statystyk to spacje (R15, R16).
- Edge case: wąski terminal (`a_narrower_terminal_cuts_the_workspace_row_in_a_fixed_order`) — kolejno znikają baza, statystyki (z separatorem przed nimi), potem skraca się gałąź, potem repo; nazwa i PR całe; brak podwójnych i końcowych separatorów.
- Integration (R30): z włączonymi linkami komórka OSC 8 zaczyna się dokładnie na pierwszym znaku tekstu PR po separatorze (`the_workspace_row_has_coloured_changes_and_a_pull_request_link`).
- Edge case: task w kolejce bez zmian i bez PR (`a_queued_task_shows_its_workspaces_without_changes_or_a_pull_request`) → brak `—`.
- Separatory mają rolę `Done`; przy `stale` cały wiersz `Done`.

**Verification:** Testy wierszy workspace'ów przechodzą z nowym tekstem; link PR działa.

---

### U3. Nagłówek: kolejność, bez pustego wiersza, bez tła, bez `bez slotu`

**Goal:** Nowa kolejność wierszy nagłówka, brak wiersza odstępu i tła, czysty wiersz statusu.

**Requirements:** R1, R2 (malowanie tła), R6–R10, R17; AE1, AE2, AE5, AE8

**Dependencies:** U1 (testy nie używają już usuwanych stylów), U2 (nie jest konieczne, ale testy nagłówka używają nowego wiersza)

**Files:**
- Modify: `src/tui/style.rs` (usunięcie `Styles::task_header_background` i `Styles::task_tabs_divider`), `src/tui/task_app/detail.rs` (`header`, `render_view`, `status_parts`, `link_spots`, `areas` i jego komentarz, stałe, komentarz modułu, komentarz przy `header` o „quiet row”, nieużywany import `without_slot`)
- Test: `tests/integration/tui_render.rs`

**Approach:**
- `header`: tytuł (gdy `title_cut`), wiersze workspace'ów, status, koszt. Indeks wiersza linku liczony jak dziś z `lines.len()`.
- `render_view`: usunąć `lines.insert(0, …)`, `QUIET_ROW` z `base_lines`, obie pętle tła, zerowanie tła wiersza podkreślenia. Powiadomienie zostaje pod kosztem (już tak jest: `lines.extend(notice)` po `header`). Czarną linię zastępuje górna linia belki: U3 wprowadza funkcję pomocniczą „linia z łącznikami i opcjonalnym tekstem przy prawym końcu” (patrz U4, U6) i rysuje nią górną linię w roli `Done` przed wczesnym `return` w trybie `deferred`, aby ten tryb nadal miał linię pod nagłówkiem. U4 dodaje rolę `Focus` (`frame_focused`) i dolną linię.
- `link_spots`: bez `QUIET_ROW`. Usunąć stałą `QUIET_ROW`.
- `status_parts`: usunąć gałąź `without_slot`.
- Komentarz modułu i `areas`: nowy układ.

**Test scenarios:**
- Covers AE1. Krótki tytuł mieszczący się na ramce → pierwszy wiersz wnętrza ramki to wiersz workspace'u `code:`; nad nim brak pustego wiersza.
- Covers AE2. Długi tytuł skrócony `…` → wiersze: pełny tytuł, workspace'y, status, koszt (w tej kolejności).
- Covers AE5. Zakończony task bez slotu → wiersz statusu nie zawiera `bez slotu`; lista tasków bez zmian (`a_finished_task_without_a_slot_is_marked_and_not_counted` nadal przechodzi).
- Covers AE8. Tryb kolorowy → żadna komórka obszaru nagłówka i belki nie ma `bg == Rgb(34, 48, 72)`; zastępuje `the_task_header_uses_a_muted_navy_background_and_rule_above_tabs`.
- Edge case (R9): task bez zużycia → brak wiersza kosztu, belka wyżej o jeden wiersz (`a_task_without_a_usage_report_keeps_the_header_as_it_was`).
- Edge case (R10): niedostępny harness → powiadomienie pod kosztem, nad górną linią belki.
- Edge case: tryb `deferred` → nagłówek, linia pod nim i komunikat „więcej danych…” od razu pod linią.
- Integration: historia (`history.rs`) — ten sam układ nagłówka (jeden test renderowania historii z workspace'em).

**Verification:** Gdy ramka mieści tytuł, nagłówek jest niższy o jeden wiersz niż dziś.

---

### U4. Belka zakładek między liniami w kolorze ramki

**Goal:** Trzy wiersze belki: górna linia, nazwy, dolna linia z odcinkiem `━`; łączniki `├`/`┤` na ramce; brak znaczników w wierszu nazw.

**Requirements:** R2, R19–R24, R29; AE6, część AE7

**Dependencies:** U3

**Files:**
- Modify: `src/tui/task_app/detail.rs` (`ViewOptions`, `render`, `render_view`, `tab_bar`), `src/tui/task_app/history.rs` (`ViewOptions { frame_focused, … }`)
- Test: `tests/integration/tui_render.rs`

**Approach:**
- `ViewOptions.frame_focused`: w `detail::render` wartość `frame_focused`, w `history.rs` wartość podana do `render_panel_edge`.
- Rola linii: `Focus`, gdy `frame_focused && !stale`, w innym przypadku `Done`.
- Górna linia: ostatni wiersz obszaru nagłówka, `─` przez całą szerokość `area`, łączniki na `area.x − 1` (`├`) i `area.right()` (`┤`), gdy komórki są w buforze.
- `tab_bar`: tylko wiersz nazw (bez znaczników śledzenia, R24) plus pozycja i szerokość odcinka aktywnej zakładki. Parametry `progress` i `conversation_follows` znikają.
- Dolna linia: drugi wiersz obszaru belki, `─` przez całą szerokość, odcinek `━` pod aktywną zakładką (przesunięcie o margines nazw: `+2`), styl odcinka `Focus` + `BOLD` przy fokusie treści, w innym przypadku `Done` + `BOLD`; łączniki jak w górnej linii.
- Odcinek `━` jest przycinany do szerokości obszaru nazw, tak jak nazwy w `tab_bar`. Gdy aktywna nazwa nie mieści się w całości, rysuje się tylko jej widoczna część.
- Wysokość obszaru belki w `areas` pozostaje 2; przy bardzo niskim panelu (`tabs.height < 2`) dolna linia nie jest rysowana.
- Bez koloru: linie i łączniki w stylu roli (modyfikatory), jak ramka (R29).

**Technical design:** U4 używa wspólnej funkcji pomocniczej „linia z łącznikami i opcjonalnym tekstem przy prawym końcu” z U3, aby linie belki i pasków kroków nie powtarzały kodu.

**Test scenarios:**
- Covers AE6. Fokus na treści, aktywna *Rozmowa* → wiersz nad nazwami to `├─…─┤`, wiersz pod nazwami to `├──━━━━━━━──…─┤`; wszystkie komórki linii i łączników mają `fg` roli `Focus`; komórki `━` mają `BOLD` i leżą dokładnie pod `Rozmowa`.
- Happy path: klawiatura na liście → linie i łączniki w roli `Done`; odcinek `━` w `Done` z `BOLD`.
- Edge case: klawiatura w panelu odpowiedzi (`with_focus_in_the_panel_backgrounds_and_markers_replace_separator_rules`) → linie w roli `Focus` (ramka ma fokus), odcinek w `Done` z `BOLD`.
- Edge case (R21, A1): `stale` przy fokusie → linie i łączniki w roli `Done`.
- Edge case (R24, AE7 część): aktywna *Rozmowa* i aktywny *Przebieg* → wiersz nazw nie zawiera `śledzi`, `ręcznie`, `podąża`.
- Edge case (R29): bez koloru → linie i łączniki są rysowane, bez `fg` RGB.
- Edge case: zmiana zakładki klawiszem → odcinek `━` przechodzi pod nową nazwę.
- Integration: historia → belka ma łączniki na ramce panelu historii.
- Edge case (wąski panel): panel węższy niż pięć nazw zakładek, aktywna *Info* → odcinek `━` jest przycięty do szerokości obszaru nazw, nie leży na komórce łącznika `┤` ani poza panelem.
- Zaktualizować test panelu odpowiedzi (`tui_render.rs` ok. linii 4983): dzielnik nad wierszem nazw ma rolę linii (`Focus` przy fokusie ramki, `Done` w innym przypadku), nie `Color::Black`.

**Verification:** Belka łączy się z ramką w szczegółach i w historii; testy `the_conversation_tab_bar_follows_the_focus` i `the_header_of_a_running_task_shows_the_status_the_step_and_each_workspace` zaktualizowane.

---

### U5. Kolejność zakładek: *Rozmowa* pierwsza

**Goal:** Kolejność *Rozmowa*, *Przebieg*, *Zdarzenia*, *Dyrygent*, *Info* na belce i przy przechodzeniu klawiszami.

**Requirements:** R18; Z7

**Dependencies:** brak (rysowanie belki z U4 czyta `DetailTab::ALL`)

**Files:**
- Modify: `src/tui/task_app/mod.rs` (`DetailTab::ALL`, komentarz)
- Test: `tests/integration/tui_render.rs`, `tests/integration/tui_state.rs` (testy przechodzenia, gdy zakładają starą kolejność)

**Approach:** Zmienić tylko kolejność w `ALL`. Domyślne zakładki (`DetailState::new` → *Rozmowa*, historia → *Przebieg*) bez zmian.

**Test scenarios:**
- Happy path: belka pokazuje nazwy w kolejności `Rozmowa   Przebieg   Zdarzenia   Dyrygent   Info`.
- Happy path: z *Rozmowy* `NextTab` → *Przebieg*; z *Rozmowy* `PrevTab` → *Info*; z *Info* `NextTab` → *Rozmowa*.
- Edge case (Z7): po otwarciu szczegółów aktywna zakładka jest taka jak dziś (szczegóły: *Rozmowa*; historia: *Przebieg*).

**Verification:** Testy stanu przechodzenia zakładek przechodzą z nową kolejnością.

---

### U6. Znaczniki śledzenia na liniach nad listą kroków

**Goal:** Krótki znacznik z `[f]` przy prawym końcu linii nad krokami w *Rozmowie* i w *Przebiegu*.

**Requirements:** R25–R28, R31; AE7; Z8, Z9; P1

**Dependencies:** U3 (funkcja pomocnicza linii), U4 (usunięcie znaczników z belki)

**Files:**
- Modify: `src/tui/task_app/tabs/conversation.rs` (`render_step_strip`), `src/tui/task_app/detail.rs` (dolna linia belki dla *Przebiegu* przy P1 (a))
- Test: `tests/integration/tui_render.rs`

**Approach:**
- Funkcja pomocnicza: buduje linię `─` o szerokości `w` z tekstem znacznika przy prawym końcu: `─…─ tekst ─`. Gdy `w < szerokość tekstu + 4` (spacja i jedna komórka linii z każdej strony), zwraca samą linię. Funkcja pochodzi z U3.
- *Rozmowa*: górna linia w `render_step_strip` używa funkcji pomocniczej z `↧ śledzi [f]` (`Live`) albo `⏸ ręcznie [f]` (`Attention`) według `conversation.follows()`. Kolor linii jak dziś (`Done`). Dolna linia paska bez zmian.
- *Przebieg* (P1 (a)): dolna linia belki dostaje przy prawym końcu `↧ podąża [f]` (`Live`) albo `· ręcznie [f]` (`Done`), gdy aktywny jest *Przebieg* i oś ma kroki. Odcinek `━` pod aktywną zakładką i znacznik nie mogą na siebie zachodzić: gdy brak miejsca, znacznika nie ma.
- Przy `stale` znacznik ma rolę `Done` (jak inne elementy, R29).
- Klawisz `f` bez zmian (R28).

**Test scenarios:**
- Covers AE7. *Rozmowa* śledzi → górna linia paska kroków kończy się `↧ śledzi [f] ─`; wiersz nazw zakładek bez znacznika. Po `f` → linia kończy się `⏸ ręcznie [f] ─`.
- Happy path: kolory — `↧ śledzi [f]` w roli `Live`, `⏸ ręcznie [f]` w roli `Attention`; komórki linii w roli `Done`.
- Edge case (R27): wąski panel, w którym linia jest krótsza niż znacznik + 3 → linia cała z `─`, bez znacznika.
- Edge case: dolna linia paska kroków *Rozmowy* nie ma znacznika (Z8).
- Happy path (R31, P1 (a)): *Przebieg* z krokami, śledzenie włączone → dolna linia belki kończy się `↧ podąża [f] ─┤`; po `f` → `· ręcznie [f] ─┤`.
- Edge case: *Przebieg* bez kroków → dolna linia belki bez znacznika.
- Edge case: inne zakładki (*Zdarzenia*, *Dyrygent*, *Info*) → dolna linia belki bez znacznika.
- Zmienić `the_conversation_tab_has_the_steps_newest_first_between_two_rules` i asercję `tabs.contains("śledzi")` w `the_header_of_a_running_task_shows_the_status_the_step_and_each_workspace`.

**Verification:** Operator widzi stan śledzenia nad listą kroków; belka zakładek nie ma znaczników.

---

## System-Wide Impact

- **Interaction graph:** `render_view` i `render_panel_edge` są wspólne dla szczegółów i historii; obie ścieżki dostają zmiany. `ViewOptions` dostaje nowe pole, więc oba miejsca tworzenia muszą je podać (kompilator to wymusi).
- **Unchanged invariants:** logika śledzenia, klawisze, odczyty danych, domyślne zakładki, lista tasków, panel odpowiedzi, teksty `bez slotu` poza nagłówkiem.
- **API surface parity:** brak — zmiana tylko w TUI.

---

## Risks & Dependencies

| Ryzyko | Ograniczenie |
|--------|--------------|
| Wiele testów renderowania liczy wiersze względem `"Przebieg"` lub zakłada pusty wiersz; po zmianie przesunięcia są inne | Szukać wierszy po treści, nie po przesunięciu; zaktualizować testy w tej samej jednostce co zmianę |
| Łączniki rysowane poza obszarem `area` mogą nadpisać inną treść, gdy wywołujący poda obszar bez ramki | Rysować łącznik tylko, gdy komórka leży w buforze i zawiera `│` ramki (jak w Key Technical Decisions) |
| Wyrównanie kolumn z separatorami może zmienić kolejność skracania | `overflow` liczy separatory; test wąskiego terminala sprawdza kolejność |
| Link OSC 8 trafia w złe komórki po zmianie układu | Test linku sprawdza pierwszą komórkę PR po separatorze |

---

## Documentation / Operational Notes

- Zaktualizować komentarze modułów `detail.rs` i `workspace_rows.rs` (przykłady układu).
- Brak zmian w README i dokumentacji użytkownika.

---

## Sources & References

- **Origin document:** `docs/brainstorms/2026-11-11-tui-naglowek-i-zakladki-szczegolow-taska-requirements.md`
- Kod: `src/tui/task_app/detail.rs`, `src/tui/task_app/workspace_rows.rs`, `src/tui/task_app/mod.rs`, `src/tui/style.rs`, `src/tui/task_app/tabs/conversation.rs`, `src/tui/task_app/tabs/progress.rs`, `src/tui/task_app/history.rs`
- Testy: `tests/integration/tui_render.rs`, `tests/integration/tui_state.rs`, `tests/integration/tui_form.rs`
