---
title: TUI: Prostsze zaznaczenie listy tasków - Plan
type: feat
date: 2026-10-15
topic: tui-task-list-selection
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: legacy-requirements
origin: docs/brainstorms/2026-10-10-tui-zaznaczenie-i-rozmowa-kroku-requirements.md
execution: code
---

# TUI: Prostsze zaznaczenie listy tasków - Plan

## Goal Capsule

- **Objective:** Operator szybko rozpoznaje wybrany task i czyta listę bez wchodzenia tekstu na prawą krawędź panelu.
- **Means:** Lista tasków zachowuje tylko lewą kreskę zaznaczenia, a renderowanie tekstu rezerwuje jedną kolumnę przy prawej krawędzi (KTD1, KTD2).
- **Product authority:** Wymagania poniżej zawężają starsze reguły zaznaczenia wyłącznie dla listy tasków; pozostałe widoki zachowują obecne zachowanie.
- **Tail ownership:** `ce-work` odpowiada za implementację i jej weryfikację.
- **Open blockers:** Brak.

## Product Contract

### Summary

Zaznaczony task pozostaje oznaczony lewą pionową kreską, bez zmiany tła oraz bez poziomych kresek nad i pod wierszem. Tekst listy pozostawia wolną kolumnę przed prawą krawędzią panelu.

### Problem Frame

Obecna lista wyróżnia zaznaczenie lewą kreską, poziomymi liniami i ciemnym tłem. Linie przesuwają sąsiednie wiersze, a tło nadpisuje tło komórek zaznaczonego taska. Tekst może też dochodzić do samej prawej krawędzi panelu.

### Key Decisions

- **Lewa kreska zostaje jako jedyny znacznik zaznaczenia** (session-settled: user-directed — chosen over removing all selection marks: zachowuje czytelne położenie kursora bez tła i poziomych linii). Governs R1–R3.

### Requirements

**Zaznaczenie taska**

- R1. Wybrany task ma lewą pionową kreskę przy każdej swojej linii; jej styl podąża za istniejącą regułą focusu listy.
- R2. Zaznaczenie nie zmienia tła treści taska i nie dodaje poziomych kresek nad ani pod nim.
- R3. Wybór taska nie zmienia wysokości wierszy listy ani położenia sąsiednich tasków.

**Odstęp od prawej krawędzi**

- R4. Tekst w wierszach listy pozostawia co najmniej jedną pustą kolumnę przed prawą krawędzią panelu, także gdy tytuł lub etykieta wypełnia dostępne miejsce.

### Acceptance Examples

- AE1. **Covers R1–R3.** Gdy lista ma fokus, wybrany task ma lewą kreskę w stylu focusu przy obu liniach; po przeniesieniu focusu poza listę kreska zachowuje dotychczasowy styl nieaktywnego zaznaczenia. W obu stanach jego tło i kolory ról treści pozostają niezmienione, nie ma poziomych linii, a zmiana wyboru nie przesuwa sąsiednich tasków.
- AE2. **Covers R4.** Długi tytuł albo etykieta dochodzące do prawej strony dostępnego miejsca kończą się przed ostatnią kolumną panelu; ta kolumna pozostaje pusta.

### Scope Boundaries

- Zmiana dotyczy wyłącznie listy tasków. Lista historii, oś kroków, choosery, panel „Czeka na Ciebie”, transkrypty i lista zdarzeń zachowują własne znaczniki.
- Paleta ról, szerokość panelu, treść tytułów i etykiet oraz ich dotychczasowe skracanie nie zmieniają się.

### Sources

- `docs/brainstorms/2026-10-10-tui-zaznaczenie-i-rozmowa-kroku-requirements.md` — wcześniejszy kontrakt zaznaczenia list, nadpisany tu tylko w zakresie listy tasków.
- `src/tui/selection.rs` — współdzielone renderowanie klamry i zachowanie kolorów treści.
- `src/tui/task_app/list.rs` — składanie dwuwierszowego tekstu taska i wywołanie renderera listy.
- `tests/tui_render.rs` — istniejące asercje pozycji, kreski i renderowania listy tasków oraz współdzielonej klamry.

## Planning Contract

### Key Technical Decisions

- KTD1. **Ogranicz zmianę trybu zaznaczenia do listy tasków.** Zachowaj aktualne renderowanie współdzielonej klamry dla historii, chooserów i pozostałych widoków; lista tasków ma użyć wariantu oznaczonego tylko lewą kreską. Zapobiega to zmianie zakresu wbrew R1–R3.
- KTD2. **Zarezerwuj prawą kolumnę w obliczeniu szerokości tekstu listy.** Dopasuj tytuł i etykietę do szerokości wewnętrznej pomniejszonej o jedną komórkę, zachowując dotychczasowe skracanie tekstu.

### Assumptions

- „Jedna pusta kolumna” oznacza jedną komórkę terminala wewnątrz panelu, przed jego obramowaniem.

## Implementation Units

### U1. Renderuj zaznaczenie taska samą lewą kreską

**Goal:** Zaznaczenie listy tasków nie rysuje tła ani poziomych kresek, a jego pionowa kreska nie zmienia położenia wierszy.

**Requirements:** R1–R3; covers AE1.

**Dependencies:** None.

**Files:** `src/tui/selection.rs`, `src/tui/task_app/list.rs`, `tests/tui_render.rs`.

**Approach:** Dodaj wariant renderowania oznaczenia, który zachowuje lewą kreskę i pomija poziome reguły oraz nakładanie tła. Użyj go wyłącznie w `TaskList::render`; nie zmieniaj domyślnego zachowania pozostałych klientów wspólnego renderera. Zachowaj istniejącą rolę kreski dla listy z fokusem i bez niego oraz kolory semantyczne treści.

**Patterns to follow:** `render_bracketed_list`, `bar_prefix` i `draw_line` w `src/tui/selection.rs`; testy `a_task_takes_two_rows_and_only_the_selected_one_has_rules` i `the_selected_content_keeps_its_own_colours` w `tests/tui_render.rs`.

**Test Scenarios:**

- Covers AE1. Przy zaznaczonym tasku asercje `TestBackend` potwierdzają lewą kreskę przy obu liniach, brak kresek poziomych oraz brak zmiany tła komórek.
- Covers AE1. Po `j` zaznaczenie przechodzi na kolejny task, kreska podąża za nim, a pozycje obu tasków pozostają takie same jak przy niezaznaczonej liście.
- Covers AE1. Przy fokusie i bez niego kreska zachowuje odpowiednie dotychczasowe style, a kolory markerów uwagi, ikony stanu i etykiety pozostają nienaruszone.
- Renderowanie listy historii lub bezpośredniego klienta współdzielonej klamry nadal pokazuje dotychczasowe kreski poziome i tło zaznaczenia.

**Verification:** Testy renderowania potwierdzają zachowanie listy tasków oraz niezmienione renderowanie innych klientów wspólnej klamry.

### U2. Zostaw wolną kolumnę przy prawej krawędzi

**Goal:** Tytuł i etykieta taska nie dochodzą do ostatniej kolumny panelu.

**Requirements:** R4; covers AE2.

**Dependencies:** U1.

**Files:** `src/tui/task_app/list.rs`, `tests/tui_render.rs`.

**Approach:** Odejmij jedną komórkę od szerokości przekazywanej do układu tekstu taska. Zachowaj bieżące skracanie długich ciągów i układ dwóch linii.

**Patterns to follow:** `row_text` i `fit` w `src/tui/task_app/list.rs`; testy listy tasków w `tests/tui_render.rs`.

**Test Scenarios:**

- Covers AE2. Przy długim tytule test renderowania potwierdza, że ostatnia kolumna wewnątrz panelu pozostaje pusta.
- Covers AE2. Przy długiej etykiecie druga linia również pozostawia pustą ostatnią kolumnę.
- Krótki tytuł i krótka etykieta zachowują dotychczasowe treści oraz wyrównanie.

**Verification:** Test renderowania sprawdza prawą krawędź obu linii dla długich treści i potwierdza brak zmian przy krótkich treściach.

## Verification Contract

- `cargo test --test tui_render` — weryfikuje renderowanie tasków, role treści, geometrię listy oraz niezmienione zachowanie pozostałych list korzystających ze wspólnego renderera.

## Definition of Done

- Lista tasków oznacza wybór wyłącznie lewą kreską; nie zmienia tła i nie rysuje poziomych kresek.
- Zmiana zaznaczenia nie przesuwa wierszy tasków.
- Długie tytuły i etykiety zostawiają wolną kolumnę przy prawej krawędzi panelu.
- Inne listy zachowują dotychczasowy wygląd zaznaczenia.
- `cargo test --test tui_render` przechodzi.
