---
title: "feat: Etykieta nad polem we wszystkich formularzach TUI"
type: feat
status: active
date: 2026-10-25
origin: docs/brainstorms/2026-10-24-formularz-nowego-zadania-etykieta-nad-polem-requirements.md
---

# feat: Etykieta nad polem we wszystkich formularzach TUI

## Summary

Formularz nowego zadania przestaje rysować pola po swojemu. Każde pole ma dwa wiersze: etykieta z opisem i pod nią wartość z `> `. Rysuje je jeden wspólny kod. Pola Ticket i Tytuł znikają, gdy focus jest na liście workflow. Wejścia `target_repo` i `description` mają polskie etykiety. Wejście `feature_description` (i `bug_description`, `task_brief`) zmienia nazwę na `description`. Pliki formularza trafiają do jednego folderu.

---

## Problem Frame

Dziś dwa kody rysują pola. `field_lines` w `src/tui/chooser.rs` używa `value_lines` z `src/tui/form/field.rs` (norma). `src/tui/chooser/inputs.rs` ma własne `short_line`, `header_line` i `prefixed` (odstępstwo, tylko formularz nowego zadania). Różnice w wyglądzie wynikają z tego odstępstwa. Szczegóły: (see origin: `docs/brainstorms/2026-10-24-formularz-nowego-zadania-etykieta-nad-polem-requirements.md`).

---

## Requirements

Plan realizuje R0–R19 i AE1–AE8 z dokumentu źródłowego bez zmian zakresu. Mapa na jednostki:

| Wymagania | Jednostka |
|---|---|
| R8, R9, R11 (nagłówek, przycinanie opisu) | U1 |
| R16, R17, R17b, R18 (etykiety) | U2 |
| R0–R7, R10, R15 (wspólne rysowanie, układ) | U3 |
| R12–R14 (lista workflow bez pól) | U4 |
| R17a, R17c (zmiana nazw wejść) | U5 |
| R19 (folder) | U6 |

---

## Scope Boundaries

- Bez zmian: kolejność pól, nawigacja, kolory, limity (15 i 50), lista workflow i jej opisy, nazwa `target_repo`.
- Bez aliasu `feature_description` → `description` i bez migracji zapisanych zadań (R17b).
- Wejście `ticket_id` w `workflows/open-mercato-sdlc.yaml` zostaje.

### Deferred to Follow-Up Work

- Brak.

---

## Context & Research

- `src/tui/form/field.rs`: `value_lines` rysuje `> `, tło edycji i wypełnienie do `text_width(width)`. `text_width` odejmuje `PROMPT_WIDTH` i `FIELD_RIGHT_MARGIN` (`src/tui/form/layout.rs`). Wolna kolumna po prawej (R5, R6) już działa, więc U3 jej nie buduje, tylko zaczyna ją używać w formularzu nowego zadania.
- `src/tui/chooser.rs`: `field_lines` buduje nagłówek (etykieta, `(wymagane)`, `◆`, ` · opis`), wartość przez `value_lines` i wiersz błędu pod wartością. `render_with_style` przełącza na `inputs::render`, gdy `inputs_layout` i focus na polach. `render_fields` woła `field_lines` bezpośrednio (kompozytory).
- `src/tui/chooser/inputs.rs`: układ pionowy (pasek, status, kolumna z przewijaniem, pole główne na resztę wysokości). `render_column` liczy wiersze: pole krótkie = 1, długie = nagłówek + wiersz błędu + tekst.
- `src/tui/input_form.rs`: `workflow_input_fields` tworzy pola z `input.name`. Nazwa pola jest też kluczem wartości (`workflow_input_values`, `TICKET_FIELD`/`TITLE_FIELD` w `new_task.rs`), więc etykiety nie można wpisać do `name`.
- Testy: `tests/tui_render.rs` (m.in. pętle `inputs_layout` false/true przy ok. linii 384–440 i 6424), `tests/tui_form.rs` (`with_inputs_layout`, linie 46, 169, 508, 568).
- `feature_description` / `bug_description` / `task_brief` występują w: `workflows/*.yaml` (4 pliki), `README.md`, `WORKFLOW_SCHEMA.md`, `workflows/README.md`, `crates/narsor-engine/src/workflow/validator.rs` (test, linia ok. 838) i w `tests/` (m.in. `actions_agent.rs`, `api.rs`, `engine_execution.rs`, `persistence.rs`, `schema_loading.rs`, `template.rs`, `validation.rs`, `workflows.rs`, `fixtures/implement_feature.yaml`, `fixtures/invalid/*.yaml`).
- Brak notatek w `docs/solutions/` dla tego obszaru; badania zewnętrzne pominięte (wzorce są w repozytorium).

---

## Key Technical Decisions

- **Etykieta osobno od nazwy (KTD1).** `ChooserField` dostaje opcjonalną etykietę (`labelled(...)`) i metodę zwracającą etykietę do rysowania, z nazwą jako wartością domyślną. Nazwa dalej jest kluczem wartości. Dotyka to tylko rysowania.
- **Mapa po nazwie, nie po typie (KTD2).** `workflow_input_fields` mapuje `target_repo` → „Repozytorium" i `description` → „Opis". Reguła po typie zmieniłaby etykiety w workflow użytkowników (R17b, Z2). Mapa to jedna stała tablica w `src/tui/input_form.rs`.
- **Błąd pola pod wartością, licznik bez zmian (KTD3, rozstrzyga R7/R10).** Wspólny kod już rysuje błąd pod wartością. Formularz nowego zadania robi tak samo. Błędy znikają z linii statusu. Miejsce licznika znaków zostaje takie, jak jest w kodzie dziś; U3 sprawdza, że nie psuje reguł R4 i R5.
- **Linia statusu znika (KTD4).** Po przeniesieniu opisu i błędu do pola linia statusu jest zawsze pusta. U3 usuwa `render_status` i `STATUS_ROWS`. Jeśli pasek i pierwsze pole wyglądają na ściśnięte, implementator może zostawić jeden pusty wiersz odstępu. Jest to decyzja wizualna, nie funkcjonalna.
- **Jeden nagłówek dla wszystkich (KTD5).** Jedna funkcja nagłówka (z szerokością i przycinaniem opisu, R11) zastępuje kod w `field_lines` i `inputs.rs`. Leży obok `value_lines` w `src/tui/form/field.rs`.
- **Pole główne (Opis) w tym samym układzie (KTD6).** `main_body` dalej okienkuje wiersze z `value_lines`. Puste wiersze pola głównego (`blank_row`) i zwinięte pola długie liczą szerokość przez `text_width`, żeby wolna kolumna (R5, Z5) była taka sama jak w polach krótkich.
- **Pola pod listą (KTD7, rozstrzyga R13/R14).** Formularz nowego zadania nie rysuje pól, gdy focus jest na liście. Flaga `inputs_layout` już odróżnia ten formularz od okien z listą (np. repozytoria); `render_with_style` pomija `field_lines` pod listą, gdy flaga jest ustawiona. Przed zmianą implementator sprawdza, które wywołania `with_inputs_layout()` w `src/tui/task_app/waiting.rs` rysują przez `chooser::render` (według źródła rysują przez `render_fields`, więc ich to nie dotyka).
- **Zmiana nazw bez aliasu (KTD8).** Testy i fixtures zmieniają się razem z workflow (R17a). Test AE8 (nazwa spoza mapy dostaje etykietę z nazwy) pokrywa U2 na polu o dowolnej nazwie, więc nie trzeba zostawiać starej nazwy w fixtures silnika.

---

## Implementation Units

### U1. Wspólny nagłówek pola z przycinaniem opisu

**Goal:** Jeden nagłówek pola: etykieta, `(wymagane)`, `◆`, ` · opis`. Opis jest przycięty z `…` do szerokości wiersza.

**Requirements:** R8, R9, R11; AE6.

**Dependencies:** brak. Do czasu U2 nagłówek używa `field.name`; etykieta wchodzi w U2.

**Files:**
- Modify: `src/tui/form/field.rs` (nowa funkcja nagłówka obok `value_lines`)
- Modify: `src/tui/chooser.rs` (`field_lines` używa nowej funkcji)
- Test: `tests/tui_render.rs`

**Approach:** Przenieś kod nagłówka z `field_lines` do funkcji `header_line(field, width, focused, styles)`. Etykieta, `(wymagane)` i `◆` zawsze się mieszczą; opis dostaje resztę szerokości i jest cięty z `…` (istniejąca logika `clip` w `inputs.rs` jest wzorem — przenieś ją do wspólnego miejsca, jeśli U3 nadal jej potrzebuje). Rysowanie używa etykiety z U2, więc zrób U1 i U2 w jednym commicie albo użyj `field.name` do czasu U2.

**Patterns to follow:** `header_line` w `src/tui/chooser/inputs.rs`; styl `Role::Focus`/`Role::Done`.

**Test scenarios:**
- Happy path: pole z opisem w szerokości 60 daje wiersz `Ticket · numer zadania w Jirze, opcjonalnie`.
- Covers AE6. Edge: opis dłuższy niż wolne miejsce kończy się `…`, etykieta i `(wymagane)` są całe.
- Edge: szerokość mniejsza niż sama etykieta nie powoduje paniki.
- Happy path: pole wymagane z opisem ma `(wymagane)` przed ` · opis`.

**Verification:** Wspólny `field_lines` (kompozytory, panele odpowiedzi) rysuje nagłówek z przycinaniem.

---

### U2. Etykiety wejść niezależne od nazw

**Goal:** `target_repo` pokazuje się jako „Repozytorium", `description` jako „Opis". Inne nazwy zostają bez zmian.

**Requirements:** R16, R17, R17b, R18; AE5, AE8.

**Dependencies:** U1.

**Files:**
- Modify: `src/tui/chooser.rs` (pole `label`, `labelled`, metoda etykiety, użycie w `carry_over`/`rebuilt` jeśli kopiują pola)
- Modify: `src/tui/input_form.rs` (`workflow_input_fields` nakłada etykiety z mapy)
- Modify: `src/tui/form/field.rs` (nagłówek czyta etykietę)
- Test: `tests/tui_form.rs`, `tests/tui_render.rs`

**Approach:** Etykieta ma wpływ tylko na rysowanie nagłówka. Wartości, błędy i `submission` dalej używają `name`. Mapa jest stałą tablicą par (nazwa, etykieta).

**Patterns to follow:** budowniczowie `required()`, `described()` w `ChooserField`.

**Test scenarios:**
- Covers AE5. Happy path: wejścia `description` i `target_repo` rysują „Opis" i „Repozytorium"; `workflow_input_values` zwraca klucze `description` i `target_repo`.
- Covers AE8. Edge: wejście o nazwie spoza mapy (np. `custom_input`) ma etykietę `custom_input` i działa jak dotąd.
- Edge: Ticket i Tytuł zachowują etykiety (R18).

**Verification:** Wysłane zadanie niesie klucze techniczne, nie etykiety.

---

### U3. Formularz nowego zadania na wspólnym rysowaniu pól

**Goal:** `inputs.rs` rysuje układ (pasek, kolumna, pole główne), a każde pole rysuje wspólny kod: nagłówek (U1), wartość z `value_lines`, błąd pod wartością.

**Requirements:** R0–R7, R10, R15; AE1, AE2, AE3, AE7.

**Dependencies:** U1, U2.

**Files:**
- Modify: `src/tui/chooser/inputs.rs` (usuń `short_line`, `header_line`, `window_around_cursor`; usuń `render_status` i `STATUS_ROWS`; `render_column` liczy 2 wiersze + błąd na pole krótkie)
- Modify: `src/tui/form/field.rs` (jeśli potrzebny wspólny `prefixed` dla zwiniętych pól)
- Test: `tests/tui_render.rs`, `tests/tui_form.rs`

**Approach:**
- Pole krótkie i wybór: nagłówek, `value_lines`, wiersz błędu. `value_lines` już daje `> `, tło po `> ` do wolnej kolumny i `‹ wartość ›` dla wyboru (R3–R6). Pole jednowierszowe zawija się jak Opis; to zgodne z wspólnym kodem i z limitami 15 i 50 znaków na standardowej szerokości.
- `render_column`: `rows_of` dla pola krótkiego = nagłówek + wiersze `value_rows` + wiersz błędu. Pola długie niegłówne dalej się zwijają (`folded_rows` z szerokością `text_width`).
- Przewijanie: `focus_line` wskazuje wiersz kursora w wartości (jak w `field_lines`), żeby fokus był widoczny.
- Podział wysokości (pole główne na resztę, `MIN_MAIN_ROWS`, zwijanie) zostaje, ale liczy nowe wysokości pól.
- Pasek workflow: bez zmian.
- Sprawdź, że licznik znaków (np. `7/15`) nadal się pokazuje i nie psuje `> ` ani wolnej kolumny (R7).

**Technical design:** *To jest kierunek, nie specyfikacja.* Wiersze pola: `[nagłówek][wartość…][błąd?]`. Pole główne dostaje wysokość `area − Σ(inne pola) − nagłówek`.

**Patterns to follow:** `field_lines` w `src/tui/chooser.rs`; `main_body` w `inputs.rs`.

**Test scenarios:**
- Covers AE1. Happy path: Ticket w szerokości 60: wiersz 1 etykieta + opis, wiersz 2 `> ` + pole + 1 wolna kolumna przed ramką.
- Covers AE2. Happy path: Repozytorium z `acme/web`: wiersz 2 to `> ‹ acme/web ›`.
- Covers AE3. Happy path: aktywny Tytuł ma tło edycji od pola po `> ` do przed ostatnią spacją; etykieta i `> ` bez tła.
- Edge: pole liczbowe i true/false mają ten sam układ.
- Error path: błąd pola (np. wymagane puste) jest pod wartością tego pola, nie w osobnej linii statusu.
- Edge: niski terminal (np. 12 wierszy): kolumna się przewija, aktywne pole jest widoczne.
- Edge: pole główne (Opis) ma `> ` w pierwszym wierszu, wcięcie w kolejnych, wolną kolumnę po prawej (Z4, Z5).
- Covers AE7. Integration: formularz odpowiedzi i kompozytor rozmowy rysują pole tak samo jak formularz nowego zadania (porównanie wiersza etykiety i wartości).
- Integration: istniejące pętle `inputs_layout` false/true w `tests/tui_render.rs` mają zgodny wynik dla pola wieloliniowego i jednowierszowego.

**Verification:** W `inputs.rs` nie ma kodu rysującego pole (tylko układ). Testy renderowania i formularza przechodzą po zaktualizowaniu oczekiwań.

---

### U4. Lista workflow bez pól Ticket i Tytuł

**Goal:** Gdy focus jest na liście workflow, widać listę i panel opisu, bez pól.

**Requirements:** R12, R13, R14, R15; AE4.

**Dependencies:** U3 (mogą iść razem).

**Files:**
- Modify: `src/tui/chooser.rs` (`render_with_style`: przy `inputs_layout` i focusie na liście nie rysuje pól)
- Test: `tests/tui_render.rs`, `tests/tui_form.rs`

**Approach:** Gdy `inputs_layout` jest ustawione, `fields_height` = 0 i lista z opisem dostaje całą wysokość. Chooser bez tej flagi (np. dodawanie repozytorium w `src/tui/task_app/repositories.rs`) zachowuje dziś zachowanie (R14). Nawigacja klawiszami nie zmienia się (R15).

**Test scenarios:**
- Covers AE4. Happy path: focus na liście: widać listę i opis, nie widać „Ticket" ani „Tytuł".
- Happy path: po przejściu do pól pola się pojawiają w kolejności Ticket, Tytuł, Repozytorium, pozostałe, Opis.
- Edge: okno z listą i polami pod nią bez flagi (repozytoria, akcje) nadal rysuje pola pod listą.

**Verification:** Przejście lista ↔ pola nie zmienia kolejności i skrótów.

---

### U5. Zmiana nazw wejść na `description`

**Goal:** `feature_description`, `bug_description`, `task_brief` stają się `description` w dołączonych workflow, szablonach, dokumentacji i testach.

**Requirements:** R17a, R17b, R17c; AE5.

**Dependencies:** brak (niezależne od TUI).

**Files:**
- Modify: `workflows/compound-engineering.yaml`, `workflows/compound-engineering-chat.yaml`, `workflows/compound-engineering-debug.yaml`, `workflows/open-mercato-sdlc.yaml`
- Modify: `README.md`, `WORKFLOW_SCHEMA.md`, `workflows/README.md`
- Modify: `crates/narsor-engine/src/workflow/validator.rs` (test)
- Modify: `tests/actions_agent.rs`, `tests/api.rs`, `tests/engine_execution.rs`, `tests/persistence.rs`, `tests/schema_loading.rs`, `tests/template.rs`, `tests/validation.rs`, `tests/workflows.rs`, `tests/fixtures/implement_feature.yaml`, `tests/fixtures/invalid/five_violations.yaml`, `tests/fixtures/invalid/unknown_input_reference.yaml`
- Test: `tests/builtin_workflows.rs` (sprawdź, czy sprawdza nazwy wejść)

**Approach:** Zmień nazwę wejścia i każde `{{ inputs.… }}`. Opis wejścia w workflow nie mówi o „funkcji". Wejście `ticket_id` w `open-mercato-sdlc.yaml` zostaje. Po zmianie wyszukaj stare nazwy w całym repozytorium (poza `docs/`, gdzie są zapisy historyczne). Fixtures w `tests/fixtures/invalid/` zachowują swój zamiar (np. nieznane wejście w szablonie): zmień tylko nazwę, nie sens testu.

**Test scenarios:**
- Happy path: wszystkie dołączone workflow ładują się i walidują (istniejące testy `builtin_workflows`).
- Happy path: szablon `{{ inputs.description }}` renderuje się z wartością z `--input description=...`.
- Error path: fixture `unknown_input_reference` nadal zgłasza nieznane wejście.
- Covers AE5. Integration: `--input target_repo=acme-api --input description="..."` uruchamia workflow w teście CLI/API.

**Verification:** `grep` po starych nazwach poza `docs/` nic nie zwraca. Zespół, który wywołuje dołączone workflow ze skryptów, musi zmienić `--input feature_description=` (Z6; do potwierdzenia przez człowieka).

---

### U6. Przeniesienie plików formularza do jednego folderu

**Goal:** Kod formularza nowego zadania stoi w `src/tui/task_app/new_task/`.

**Requirements:** R19.

**Dependencies:** U3, U4.

**Files:**
- Move: `src/tui/task_app/new_task.rs` → `src/tui/task_app/new_task/mod.rs`
- Move: układ z `src/tui/chooser/inputs.rs` → `src/tui/task_app/new_task/layout.rs`
- Modify: `src/tui/chooser.rs` (usuń `mod inputs`; `render_with_style` nie woła układu)
- Modify: ścieżki `use` w `src/tui/task_app/mod.rs` i innych

**Approach:** Przeniesienie nie zmienia zachowania. Układ z `inputs.rs` zależy od pól prywatnych `Chooser` i `ChooserField` (`is_long`, `focus`, `main_field`, `items`). Udostępnij je jako `pub(crate)`. Formularz nowego zadania sam wybiera: przy focusie na polach woła `layout::render`, w przeciwnym razie `chooser::render` (z U4). `chooser.rs` nie zależy od `task_app`. Wspólne `field_lines`/`value_lines` zostają poza folderem.

**Test expectation:** none — czyste przeniesienie; istniejące testy z U3 i U4 pilnują zachowania.

**Verification:** `cargo build` i testy TUI przechodzą bez zmian oczekiwań po przeniesieniu. `src/tui/chooser/` nie ma już `inputs.rs`.

---

## System-Wide Impact

- Wszystkie formularze używają zmienionego nagłówka (U1): przycinanie opisu i etykiety z U2 widać też w odpowiedziach na pauzę i kompozytorach.
- Pole krótkie zajmuje dwa wiersze: w niskich terminalach formularz nowego zadania przewija się wcześniej.
- Zmiana nazw wejść (U5) łamie skrypty CLI i workflow użytkowników skopiowane z dołączonych plików (R17b, Z6). Wymaga komunikatu dla zespołu.

---

## Risks & Dependencies

| Ryzyko | Mitygacja |
|---|---|
| Wiele snapshotów renderowania w `tests/tui_render.rs` zmienia wygląd | Aktualizuj oczekiwania razem z U3; sprawdź wizualnie każdy zmieniony przypadek, nie akceptuj hurtem |
| Zmiana widoczności pól prywatnych przy U6 rozlewa się po `chooser.rs` | Zrób U6 jako ostatni, osobny commit |
| `with_inputs_layout()` w `waiting.rs` może wejść w ścieżkę U4 | Sprawdź w U4, które wywołania używają `chooser::render` |
| Zmiana nazw wejść psuje zapisane zadania i skrypty | Bez aliasu przez decyzję produktową; zapisane zadania trzymają stare nazwy; zwrócić uwagę w opisie PR |

---

## Deferred to Implementation

- Dokładne nazwy funkcji i modułów w `new_task/` (R19).
- Czy zostawić pusty wiersz odstępu po pasku po usunięciu linii statusu (KTD4).
- Miejsce licznika znaków, jeśli dzisiejsze ustawienie psuje R4 lub R5.
- Czy `tests/builtin_workflows.rs` i inne testy wymagają zmian poza listą w U5.

---

## Sources & References

- Origin: `docs/brainstorms/2026-10-24-formularz-nowego-zadania-etykieta-nad-polem-requirements.md`
- Poprzednie plany: `docs/plans/2026-10-24-005-feat-formularz-nowego-zadania-jedna-kolumna-plan.md`
