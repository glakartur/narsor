---
title: "feat: TUI — formularz nowego zgłoszenia (układ, typ `text`, nowa linia)"
type: feat
status: completed
date: 2026-10-09
origin: docs/brainstorms/2026-10-09-tui-formularz-nowego-zadania-uklad-requirements.md
---

# feat: TUI — formularz nowego zgłoszenia (układ, typ `text`, nowa linia)

## Summary

Po wyborze workflow formularz przełącza się z trybu „lista + opis" na tryb „wejścia": wąski panelik z nazwą workflow, pasek krótkich wejść i pole wieloliniowe `text` z fokusem na całą resztę wysokości (przy kilku polach `text` pozostałe zwijają się do treści). Definicja workflow dostaje nowy typ wejścia `text` (obok `string`), więc autor workflow decyduje, które pole jest dużym promptem. Klawisz nowej linii jest jedną regułą w keymapie: `Ctrl+J` zawsze, `Shift+Enter`/`Alt+Enter` gdy terminal je rozróżnia (wykrycie raz przy starcie, cofane przy wyjściu i w panic hooku). Podpowiedzi, pomoc `?` i README czytają tę samą regułę.

---

## Problem Frame

Wejścia workflow dostają dziś tyle miejsca, ile mają treści, a prompt jest polem jednoliniowym, choć jest sednem zgłoszenia. Lista workflow i opis zajmują ekran także po wyborze. Podpowiedź `Alt+Enter` jako „nowa linia" bywa nieprawdziwa, bo terminal lub multiplekser przekazuje ją jako zwykły Enter. Szczegóły: (see origin: docs/brainstorms/2026-10-09-tui-formularz-nowego-zadania-uklad-requirements.md).

---

## Requirements

- R1. Wybór workflow nadal pokazuje listę z opisem po prawej (bez zmian).
- R2. Po wyborze workflow lista i opis zwijają się do paneliku na górze (nazwa, krótkie streszczenie).
- R3. Pod panelikiem: wąski pasek z repo i krótkimi wejściami; pod nim pole `text` z fokusem rozciągnięte na resztę wysokości, a pozostałe pola `text` zwinięte do treści (1–3 linie).
- R4. Głównym polem jest wieloliniowe wejście tekstowe workflow; gdy jest ich kilka, pierwsze wymagane. *(Doprecyzowane tutaj: „wieloliniowe wejście" to nowy typ `text` w definicji workflow; patrz Key Technical Decisions.)*
- R5. Wejścia zachowują wymagane/opcjonalne, walidację, wartości domyślne i opisy.
- R6. `Esc` / `Shift+Tab` z pól wraca do wyboru workflow; wpisany prompt i repo zostają. *(Doprecyzowane w dialogu planowania: powrót działa z pól, nie z paneliku; origin mówi o fokusie na panelik.)*
- R7. `Ctrl+J` wstawia nową linię w każdym terminalu.
- R8. Gdy terminal rozróżnia modyfikatory, `Shift+Enter` i `Alt+Enter` też wstawiają nową linię i nie zatwierdzają.
- R9. Pasek podpowiedzi pokazuje tylko klawisze nowej linii działające w bieżącym terminalu.
- R10. Zasada dotyczy wszystkich pól tekstowych TUI; `Enter` nadal wysyła.
- R11. README, pomoc i podpowiedzi nie opisują `Alt+Enter` jako jedynego klawisza nowej linii.

**Origin acceptance examples:** AE1 (R2, R3, R4), AE2 (R3, R4), AE3 (R6), AE4 (R7, R10), AE5 (R8, R9).

**Decyzje z dialogu planowania:** nowy typ `text` w schemacie (zamiast znacznika/heurystyki); workflow bez wejścia `text` → tylko pasek, reszta ekranu pusta (bez awaryjnego promowania `string`); kilka pól `text` → pole z fokusem rozciąga się, pozostałe zwijają się do treści (1–3 linie); `Esc` w polach wraca do listy workflow.

---

## Scope Boundaries

- Dwukrokowy pełnoekranowy wybór (osobno repo, osobno prompt) — odrzucony.
- Repo jako lista z filtrowaniem — poza zakresem (pasek, jak dziś przełącznik).
- Zmiana semantyki `Enter` — nadal wysyła.
- Zmiany w przepływie wyboru workflow i opisu po prawej (R1).
- Brak awaryjnego promowania pola `string` do dużego pola: workflow bez `text` ma pół pustego formularza (świadoma decyzja).
- Nawigacja pionowa wewnątrz pola wieloliniowego, Home/End, Delete — nie istnieją dziś i nie są częścią tej pracy.

### Deferred to Follow-Up Work

- Migracja workflow spoza repo na typ `text` — po stronie ich autorów (dokumentacja opisuje typ).

---

## Context & Research

### Relevant Code and Patterns

- Edytor: `ChooserField` w `src/tui/chooser.rs` (`FieldKind::Text{multiline}`, `edit(event, newline)`); prymitywy edycji w `src/tui/input_form.rs` (`text_insert`, `text_backspace`, `text_step`). Każda powierzchnia tekstowa (nowe zadanie, panel odpowiedzi, zakładka Dyrygent, formularz porażki) idzie przez `Chooser`, więc jedna zmiana pokrywa wszystkie pola.
- `Chooser::on_fields_key` rozwiązuje klawisze przez `lookup(Context::Panel, …)`, a podpowiedzi przy pisaniu pochodzą z `Context::Form` — obie tablice w `src/tui/keymap.rs` wiążą `Action::Newline` osobno i muszą zmieniać się razem.
- `Key::matches` w `src/tui/keymap.rs` odrzuca SHIFT po obu stronach — `Shift+Enter` jest dziś nierozróżnialne od `Enter` (Submit). `Key::display()` nie ma formy „Shift+Enter".
- Cykl życia terminala: `TerminalGuard::enter()` / `restore_terminal()` / `install_panic_hook()` w `src/tui/mod.rs`; `restore_terminal` jest idempotentne i ma licznik `restore_count()` do testów. Wątek `read_terminal_events` przetwarza tylko `KeyEventKind::Press`.
- Brak jakiegokolwiek uzbrajania rozszerzonych klawiszy (`PushKeyboardEnhancementFlags`) w repo; wykrywanie środowiskowe istnieje tylko dla OSC 8 (`src/tui/task_app/detail.rs::hyperlinks_supported`).
- Mapowanie wejść: `workflow_input_fields` (`src/tui/input_form.rs`) — `String` → `ChooserField::text` (jednoliniowe), `Boolean` → select, `Repository` → select/tekst, `Integer` → `ChooserField::integer`. `workflow_item()` w `src/tui/task_app/new_task.rs` przypina te pola do `ChooserItem`; wartości żyją w `item.fields`, więc przeżywają zmianę fokusu.
- Model wejść: `InputParameter`/`ParamType` w `src/domain/workflow_definition.rs`, widok `WorkflowInputView` w `src/controller_protocol/wire.rs`; użycia `ParamType` także w `src/workflow/schema.rs`, `src/workflow/loader.rs`, `src/cli/commands.rs`, `src/cli/control_text.rs`, `src/api/routes/control.rs`.
- Układ: `new_task::render` → `chooser::render` (lista + opis u góry, pola pod spodem). `MIN_WIDTH=60`, `MIN_HEIGHT=20` w `src/tui/task_app/layout.rs`; poniżej tylko komunikat „powiększ okno".
- Testy: `tests/tui_render.rs` (TestBackend, asercje `contains` na spłaszczonym buforze), `tests/tui_state.rs` (klawisze → stan), `tests/task_app_support/` (helpery, m.in. `workflow_input`), `tests/schema_loading.rs`, `tests/cli_control_text.rs`. Test w `tests/tui_render.rs` pilnuje, że kolory są tylko w `style.rs` — nowy kod używa `Role`.

### Institutional Learnings

- `docs/solutions/` nie istnieje. Decyzje wiążące z poprzedniego planu (`docs/plans/2026-10-09-001-feat-tui-przeprojektowanie-plan.md`): jeden keymap jest źródłem obsługi, podpowiedzi i `?` (R16/D-T10), żaden klawisz nie ma dwóch znaczeń w jednym kontekście; poniżej ~60×20 tylko komunikat; odświeżenie nie zmienia zaznaczenia, fokusu, kursorów ani szkiców; stała kolejność listy.
- Notatki robocze: testy potrafią zostawiać procesy `narsor daemon` — jeśli nowe testy je uruchamiają, sprzątać w `Drop`.

### External References

- Brak. Zachowanie `crossterm` 0.29 (`supports_keyboard_enhancement`, flagi `DISAMBIGUATE_ESCAPE_CODES`) ma być zweryfikowane w kodzie przy implementacji (zob. Deferred to Implementation).

---

## Key Technical Decisions

- **Nowy typ wejścia `text`** (wieloliniowy string) zamiast znacznika lub heurystyki: autor workflow wskazuje, które wejścia są dużymi polami, schemat sam opisuje wysokość pola. `string` zostaje krótkim polem jednoliniowym. Dla CLI/API `text` jest zwykłym tekstem (bez zmiany semantyki wartości); różni się tylko prezentacją w TUI.
- **Dołączone workflow migrują** opisowe wejścia (np. `task_brief`, `feature_description`) na `text`; wejścia identyfikatorów (`ticket_id`) zostają `string`.
- **Brak awaryjnego promowania:** bez `text` w definicji formularz ma tylko pasek (decyzja Artura) — prostsza reguła, bez zgadywania.
- **Duże pole = pierwszy wymagany `text`**, w razie braku wymaganych — pierwszy `text`. Fokus startowy na nim.
- **Kilka pól `text`:** pole z fokusem zajmuje całą pozostałą wysokość; pozostałe zwijają się do swojej treści (min. 1, max. 3 linie). Kolejność pól się nie zmienia, zmienia się tylko wysokość (akordeon).
- **Dwa tryby jednego rendera:** `Chooser` dostaje tryb „wejścia" (fokus w polach) obok trybu „lista + opis". Wartości zostają w `item.fields`, więc R6/AE3 nie wymaga dodatkowego przechowywania.
- **`Esc` w polach wraca do listy** (nie zamyka formularza); zamknięcie to `Esc` z listy. `Shift+Tab` robi to samo z pól. Dotyczy tylko formularza nowego zadania; w pozostałych użyciach `Chooser` (panel odpowiedzi, Dyrygent) `Esc` zachowuje obecne znaczenie.
- **Nowa linia = jedna funkcja decyzyjna** w keymapie, zasilana flagą „terminal rozróżnia modyfikatory". `Ctrl+J` (`Char('j')`+CONTROL) zawsze; `Shift+Enter` i `Alt+Enter` zawsze nie wysyłają formularza, gdy zdarzenie niesie modyfikator (tam, gdzie terminal go nie raportuje, to po prostu `Enter` i wysyła — tego nie da się rozróżnić).
- **Wykrycie wsparcia raz przy starcie** w `TerminalGuard::enter()` (zapytanie `crossterm`); gdy wsparte — uzbrojenie `DISAMBIGUATE_ESCAPE_CODES`, cofnięcie w `restore_terminal()` (a więc też w panic hooku, błędy ignorowane). Wynik udostępniony podpowiedziom (np. flaga procesowa lub pole przekazane do `hint_line`/`render_help`/`key_line`), bo `keymap::hints(context)` jest dziś czystą funkcją kontekstu.
- **Wejścia zdarzeń Release/Repeat** nie są żądane (bez `REPORT_EVENT_TYPES`); wątek zdarzeń nadal przetwarza tylko `Press`.

---

## Open Questions

### Resolved During Planning

- Walidacja i wartości domyślne `text`: identyczne jak `string`; `workflow_input_values` nie wymaga zmian poza traktowaniem `Text` jak `String` w `match`.
- Tytuł zadania (`title_for` w `src/tui/task_app/new_task.rs`) dziś bierze pierwsze wejście `String`/`Repository`. Po migracji opisowych pól na `text` tytuł ma używać pierwszej linii wartości `text` (przyciętej), gdy żadne `String`/`Repository` nie daje tytułu.

- Jak wskazać „prompt": typ `text` w definicji workflow (pierwszy wymagany).
- Zachowanie bez pola `text`: tylko pasek, pusta reszta.
- Kilka pól `text`: akordeon z fokusem na maksymalną wysokość.
- Powrót z pól: `Esc` i `Shift+Tab` wracają do listy; zamknięcie z listy.

### Deferred to Implementation

- Dokładna zawartość paneliku (nazwa + pierwsza linia opisu vs krótsze streszczenie); wysokość paneliku przyjęta w budżecie U4 (3 wiersze) — korekta tylko jeśli pole `text` przy 60×20 zostaje poniżej 4 wierszy.
- Priorytet hintów przy 60 kolumnach, gdy terminal z rozszerzonymi klawiszami dodaje `Shift+Enter`/`Alt+Enter`: dotychczasowa reguła `hint_line` (odrzucanie od końca) — newline wymieniony wcześniej niż mniej ważne klawisze; potwierdzić w teście 60×20.
- Mechanizm przekazania flagi wsparcia do hintów (flaga procesowa vs. pole stanu aplikacji) — wybrać zgodnie z tym, jak `TaskApp` zasila `render_bar`.
- Zachowanie w multiplekserach (tmux/screen): czy `supports_keyboard_enhancement` zwraca prawdę i czy `Alt+Enter` dochodzi z modyfikatorem — sprawdzić ręcznie; w razie braku wsparcia zostaje `Ctrl+J` (zgodnie z wymaganiem).
- Czy `DISAMBIGUATE_ESCAPE_CODES` samo wystarcza, by `Shift+Enter` docierało z modyfikatorem (protokół kitty traktuje Enter/Tab/Backspace osobno; może być potrzebna dodatkowa flaga, np. raportowanie wszystkich klawiszy jako sekwencji): zweryfikować na kitty/wezterm/foot przed zaufaniem hintom. Wymaganie: hint nigdy nie obiecuje klawisza, który nie działa, więc przy wątpliwości `Shift+Enter` nie trafia do podpowiedzi.
- `Alt+Enter` w terminalach legacy dochodzi jako ESC+CR i `crossterm` raportuje go jako Enter+ALT bez flag; reguła newline'a traktuje to jako newline zawsze, a hint wymienia `Alt+Enter` tylko po wykryciu wsparcia — świadomie ostrożne (podpowiedź nie obiecuje więcej, niż pewne).
- Pierwszy test w U2 potwierdza, że `crossterm` w trybie raw dostarcza `0x0A` jako `Ctrl+J`, a nie Enter; `supports_keyboard_enhancement` odpytuje terminal i może blokować — wymagany limit czasu/obsługa braku odpowiedzi (U3).

---

## High-Level Technical Design

> *To ilustruje zamierzone podejście i jest wskazówką do przeglądu, nie specyfikacją implementacji. Implementujący traktuje to jako kontekst, nie kod do odtworzenia.*

Układ w trybie „wejścia" (wysokość 20, szerokość ≥ 60):

```text
┌ nazwa workflow · krótkie streszczenie ───────┐  panelik (2–3 w.)
├ repo ◀ api ▶ │ ticket_id: ABC-1 │ dry_run: ✓ ┤  pasek: string/integer/boolean/repository
├ task_brief (wymagane) ───────────────────────┤
│ …pole z fokusem: cała reszta wysokości…      │  text (fokus)
├ notatki: pierwsza linia treści… (zwinięte)   ┤  text bez fokusu: 1–3 linie
└ podpowiedzi: Enter wyślij · Ctrl+J nowa linia ┘
```

Kolejność pól stała; zmienia się wysokość: `focused text` = reszta, `inne text` = min(3, linie treści), min 1.

Reguła nowej linii (decyzja w jednym miejscu):

```text
is_newline(event, enhanced):
  Ctrl+J                      -> true (zawsze)
  Enter + (SHIFT|ALT)         -> true (zdarzenie niesie modyfikator, więc terminal go rozróżnia)
  Enter bez modyfikatora      -> false (Submit)
hint_keys(enhanced) = ["Ctrl+J"] + (enhanced ? ["Shift+Enter","Alt+Enter"] : [])
```

---

## Implementation Units

### U1. Typ wejścia `text` w definicji workflow

**Goal:** `text` jest pełnoprawnym typem wejścia: parsowanie, walidacja, widok w protokole, dokumentacja, dołączone workflow.

**Requirements:** R4, R5.

**Dependencies:** Brak.

**Files:**
- Modify: `src/domain/workflow_definition.rs`
- Modify: `src/workflow/schema.rs`
- Modify: `src/workflow/loader.rs`
- Modify: `src/controller_protocol/wire.rs`
- Modify: `src/api/routes/control.rs`
- Modify: `src/cli/commands.rs`
- Modify: `src/cli/control_text.rs`
- Modify: `src/tui/input_form.rs` (`match` na `ParamType`: parsowanie wartości, domyślne — `Text` jak `String`)
- Modify: `src/tui/task_app/new_task.rs` (`title_for`: pierwsza linia `text`, gdy brak tytułu ze `String`/`Repository`)
- Modify: `WORKFLOW_SCHEMA.md`
- Modify: `workflows/README.md`
- Modify: `workflows/open-mercato-sdlc.yaml`, `workflows/compound-engineering.yaml`
- Test: `tests/schema_loading.rs`, `tests/cli_control_text.rs`

**Approach:**
- Dodać wariant `Text` do `ParamType` obok `String`; semantyka wartości (tekst, wartość domyślna, `required`) identyczna jak dla `String`.
- Przejrzeć wszystkie `match` na `ParamType` (CLI, widok tekstowy, API, TUI) tak, by `Text` zachowywał się jak `String` tam, gdzie liczy się tylko wartość (CLI `--input`, walidacja wartości, `control_text` — pokazanie typu).
- Zaktualizować `WORKFLOW_SCHEMA.md` (lista typów, opis „wieloliniowy; w TUI duże pole") i `workflows/README.md`; zmigrować opisowe wejścia dołączonych workflow.

**Patterns to follow:**
- Sposób, w jaki wariant `Repository` został dodany do `ParamType` i obsłużony w loaderze oraz CLI.

**Test scenarios:**
- Happy path: workflow z wejściem `type: text` ładuje się i wystawia `input_type == Text` w widoku.
- Happy path: wejście `text` z `default` i `required: true` zachowuje wartość domyślną i walidację jak `string`.
- Edge case: nieznany typ (`type: textarea`) nadal jest odrzucany z czytelnym komunikatem.
- Integration: dołączone workflow (`open-mercato-sdlc`, `compound-engineering`) ładują się po migracji, a `ticket_id` pozostaje `string`.
- Happy path: zadanie z workflow, którego jedynym opisowym wejściem jest `text`, dostaje tytuł z pierwszej linii (przyciętej), a nie wielolinijkowy ani sam nazwę workflow.
- CLI: `task input`/`create` z wartością wieloliniową dla pola `text` przekazuje ją bez zmian (test w `tests/cli_control_text.rs`).

**Verification:**
- Workflow z typem `text` przechodzi ładowanie i walidację; wszystkie istniejące testy schematu i CLI przechodzą; dokumentacja wymienia typ.

---

### U2. Jedna reguła nowej linii: keymap, hinty, pomoc

**Goal:** `Ctrl+J` zawsze, `Shift+Enter`/`Alt+Enter` gdy zdarzenie niesie modyfikator, nigdy Submit; podpowiedzi pokazują tylko klawisze działające w bieżącym terminalu.

**Requirements:** R7, R8, R9, R10, R11 (kod).

**Dependencies:** U3 (flaga wsparcia, z której hinty budują podpowiedź newline'a); reguła newline'a na zdarzeniu (`Ctrl+J`, Enter+SHIFT/ALT) nie zależy od U3, więc można ją zbudować wcześniej, a hinty podłączyć po U3. Niezależne od U1.

**Files:**
- Modify: `src/tui/keymap.rs`
- Modify: `src/tui/chooser.rs`
- Modify: `src/tui/task_app/layout.rs` (`render_bar`)
- Modify: `src/tui/task_app/tabs/conductor.rs` (`key_line`, komentarz modułu)
- Test: `tests/tui_state.rs`, `tests/tui_render.rs`

**Approach:**
- Zastąpić dwa osobne wiązania `Alt+Enter` (Panel, Form) jedną funkcją decyzyjną newline'a używaną przez `Chooser::on_fields_key` oraz przez każdy kontekst; obie tablice (Panel i Form) zmieniają się razem.
- Rozwiązać problem `Key::matches` ignorującego SHIFT: sprawdzać newline na surowym `KeyEvent` przed `lookup`, tak by `Shift+Enter` nie wpadało w Submit.
- Dodać formę wyświetlania „Shift+Enter" w `Key::display()`; podpowiedź newline'a składa się dynamicznie z flagi wsparcia (zawsze `Ctrl+J`, plus `Shift+Enter`/`Alt+Enter` gdy wykryto).
- Zaktualizować teksty: pomoc Compose („Enter wysyła, Ctrl+J nowa linia…"), komentarze o `Alt+Enter` w `src/tui/chooser.rs`, `src/tui/keymap.rs`, `conductor.rs`.
- Sprawdzić brak kolizji `Ctrl+J` z istniejącymi wiązaniami (m.in. `Ctrl+v` w konwersacji, zwykłe `j` w Details).

**Patterns to follow:**
- Istniejący kontrakt „jeden keymap = obsługa + hinty + `?`" (D-T10); stałe `Role` zamiast kolorów.

**Test scenarios:**
- Covers AE4. Happy path: `Ctrl+J` w polu `text` wstawia `\n` i nie wysyła formularza (flaga wsparcia wyłączona).
- Covers AE5. Happy path: z flagą wsparcia `Shift+Enter` wstawia `\n`, a pasek podpowiedzi zawiera `Ctrl+J` i `Shift+Enter`.
- Happy path: `Alt+Enter` (ALT) wstawia nową linię i nie wysyła.
- Edge case: zwykły `Enter` nadal wysyła formularz w każdym kontekście (panel odpowiedzi, Dyrygent, nowe zadanie, formularz porażki).
- Edge case: bez wsparcia pasek podpowiedzi NIE wymienia `Shift+Enter`/`Alt+Enter`, wymienia `Ctrl+J`.
- Edge case: `Ctrl+J` w polu jednoliniowym (`string`) nie wstawia znaku i nie wysyła.
- Error path: `Ctrl+J` w polu `select`/integer nie zmienia wartości.
- Integration: te same reguły w panelu odpowiedzi, zakładce Dyrygent i formularzu porażki (wspólny `Chooser`).
- Regresja: istniejące testy asercji `Alt+Enter` (`tests/tui_render.rs`, `tests/tui_state.rs`) zaktualizowane do nowych podpowiedzi.

**Verification:**
- Żadna powierzchnia tekstowa nie opisuje `Alt+Enter` jako jedynego klawisza; `Shift+Enter` z modyfikatorem nigdy nie wysyła formularza.

---

### U3. Wykrywanie rozszerzonych klawiszy w cyklu życia terminala

**Goal:** Aplikacja wie, czy terminal rozróżnia modyfikatory, uzbraja raportowanie tylko gdy jest wsparte i zawsze je cofa.

**Requirements:** R8, R9.

**Dependencies:** Brak (U3 jest producentem flagi; konsumuje ją U2). Test hintów z obiema flagami to test integracyjny uruchamiany po U2 i U3.

**Files:**
- Modify: `src/tui/mod.rs`
- Modify: `src/tui/keymap.rs` (źródło flagi dla hintów)
- Test: `tests/tui_state.rs` (licznik `restore_count`, flaga w hintach)

**Approach:**
- W `TerminalGuard::enter()` zapytać terminal o wsparcie; przy wsparciu uzbroić `DISAMBIGUATE_ESCAPE_CODES`, a wynik zapisać w stanie dostępnym dla hintów.
- W `restore_terminal()` (wspólne dla `Drop` i panic hooka) cofnąć flagi, best-effort, błędy ignorowane; zachować idempotencję i licznik.
- Zapytanie musi tolerować terminale bez wsparcia (brak odpowiedzi/błąd → brak flagi, tylko `Ctrl+J`), nie blokować startu i nie psuć nieinteraktywnych uruchomień.
- Pętla zdarzeń bez zmian (tylko `Press`).

**Patterns to follow:**
- Dwie niezależne ścieżki przywracania (`TerminalGuard` + `install_panic_hook`) i test przez `restore_count()`; wzorzec detekcji środowiska z `hyperlinks_supported`.

**Test scenarios:**
- Happy path: przy symulowanym wsparciu flaga jest ustawiona, a hinty zawierają `Shift+Enter`/`Alt+Enter`.
- Error path: gdy zapytanie o wsparcie zwraca błąd/brak, flaga jest wyłączona, a start TUI się nie przerywa.
- Edge case: wielokrotne `restore_terminal()` (Drop + panic hook) jest idempotentne i pop flag nie wykonuje się dwa razy w sposób szkodliwy.
- Integration: ścieżka panic hooka cofa raportowanie (licznik przywróceń rośnie), tak jak dla trybu raw i ekranu alternatywnego.
- Test expectation: realne zachowanie w tmux/screen weryfikowane ręcznie (poza automatem).

**Verification:**
- Po normalnym wyjściu i po panice terminal wraca do stanu wyjściowego bez rozszerzonego raportowania.

---

### U4. Formularz nowego zadania: tryb „wejścia" (panelik + pasek + pola `text`)

**Goal:** Po wyborze workflow ekran pokazuje panelik, pasek krótkich wejść i pola `text` z akordeonową wysokością; `Esc`/`Shift+Tab` wraca do listy bez utraty wartości.

**Requirements:** R1, R2, R3, R4, R5, R6.

**Dependencies:** U1 (typ `text`), U2 (nowa linia w polach).

**Files:**
- Modify: `src/tui/input_form.rs` (`workflow_input_fields`: `Text` → `ChooserField::multiline`)
- Modify: `src/tui/chooser.rs` (tryb „wejścia", układ akordeonu, wysokości pól)
- Modify: `src/tui/task_app/new_task.rs` (`render`, wejście w tryb i powrót, fokus startowy)
- Modify: `src/tui/keymap.rs` (`Esc`/`Shift+Tab` z pól formularza nowego zadania → powrót do listy)
- Test: `tests/tui_render.rs`, `tests/tui_state.rs`, `tests/task_app_support/mod.rs` (helper wejścia `text`)

**Approach:**
- Mapowanie: `ParamType::Text` → `ChooserField::multiline`; `String`/`Integer`/`Boolean`/`Repository` bez zmian i trafiają do paska.
- Render trybu „wejścia": panelik (nazwa + skrót opisu) → pasek (wejścia nie-`text`, zawijane/przycinane do szerokości) → pola `text` w kolejności definicji. Wysokość: pole z fokusem = reszta (po odjęciu panelika, paska, nagłówków pól i paska podpowiedzi), pozostałe `text` = min(3, linie treści), min 1. Przy braku pól `text` — sam pasek, reszta pusta.
- Fokus startowy na pierwszym wymaganym `text` (w razie braku wymaganych — pierwszym `text`; bez `text` — na pierwszym wejściu paska). Nawigacja między polami jak dziś (`Tab`/strzałki), z przeniesieniem akordeonu za fokusem.
- Powrót do listy: `Esc` i `Shift+Tab` z pól formularza nowego zadania przełączają fokus na listę (nie zamykają). `Esc` z listy zamyka formularz i zapisuje szkic do `new_task_memory` jak dziś. W innych użyciach `Chooser` (panel odpowiedzi, Dyrygent) `Esc` bez zmian.
- **Kolejność fokusu i klawisze (domyślne reguły planu):** fokus idzie po polach paska w kolejności definicji, potem po polach `text` w kolejności definicji; `Tab` do przodu, `Shift+Tab` do tyłu, a `Shift+Tab` z pierwszego pola wraca do listy. W polu `text` `Left`/`Right` poruszają kursorem, a `Up`/`Down` przechodzą do poprzedniego/następnego pola (jak dziś — brak ruchu pionowego w polu); w polach paska typu select `Left`/`Right` przełączają wartość. Ponowne wejście do formularza po powrocie z listy zawsze stosuje regułę fokusu startowego. Wejście z listy do pól: `Enter` na wybranym workflow.
- **Pasek:** pola przycinane z wielokropkiem do szerokości (min. szerokość pola dobrana tak, by mieściła nazwę i kilka znaków wartości); pole `string` z fokusem przewija się poziomo, by kursor był widoczny. Marker „wymagane", błąd walidacji i opis aktywnego pola widnieją w jednowierszowym wierszu statusu pod paskiem, bo pasek nie ma nagłówków pól.
- **Błąd wysyłki:** przy nieudanym `Enter` fokus przechodzi na pierwsze niepoprawne pole (z paska lub `text`), a błąd jest widoczny w wierszu statusu lub w nagłówku pola.
- **Budżet wysokości przy 20 wierszach:** ramka 2 + pasek podpowiedzi 1 + nagłówek formularza 1 + panelik 3 + pasek 1 + wiersz statusu 1 = 8, więc ≥ 12 wierszy zostaje na pola `text` (nagłówek pola + treść). Zwinięte pola `text` zajmują nagłówek + 1–3 linie; gdy ich suma zostawiałaby polu z fokusem mniej niż 4 wiersze, zwinięte pola redukują się do samego nagłówka.
- **Stany puste:** zwinięte puste pole `text` pokazuje nagłówek i `…`; treść dłuższa niż 3 linie kończy się `…`; workflow bez `text` pokazuje w pustym obszarze jednolinijkową wskazówkę (np. „brak pola tekstowego w tym workflow").
- Zachować R5: wymagane/opcjonalne, błędy walidacji, wartości domyślne, opisy (`input_description`) w nagłówkach pól; kursor `▏` i przewijanie jak w istniejących helperach `field_value_rows`.
- Zachować stałą kolejność listy i niezmienianie stanu przy odświeżeniu (`Chooser::rebuilt` niesie wartości po etykiecie wpisu + nazwie pola).
- Niskie terminale: układ musi działać od 60×20 (poniżej — dotychczasowy komunikat); dobrać wysokości tak, by duże pole miało co najmniej kilka wierszy przy 20 liniach.

**Technical design:** zob. High-Level Technical Design (układ i reguła wysokości).

**Patterns to follow:**
- `chooser::render` i `render_fields` jako wzorzec rysowania pól; `Role` do stylów; testy `TestBackend` przy szerokim i wąskim rozmiarze.

**Test scenarios:**
- Covers AE1. Happy path: workflow z `repository` i `text` `prompt` — po wyborze lista/opis znikają, widać panelik, pasek z repo i pole `prompt` zajmujące resztę wysokości (render 120×30 i 60×20).
- Covers AE2. Happy path: workflow z `ticket_id` (string), `dry_run` (boolean), `repository` i `prompt` (text) — pierwsze trzy w pasku, `prompt` w dużym polu.
- Covers AE3. Integration: wpisz prompt i wybierz repo, `Esc` do listy, wybierz ten sam workflow — prompt i repo zachowane.
- Happy path: `Shift+Tab` z pól wraca do listy; `Esc` z listy zamyka formularz i szkic jest zapamiętany.
- Edge case: dwa pola `text` — fokus na pierwszym wymaganym rozciąga je, drugie zwinięte do treści (1–3 linie); `Tab` przenosi rozciągnięcie na drugie, a pierwsze zwija się do ≤3 linii.
- Edge case: pole `text` z treścią dłuższą niż 3 linie po utracie fokusu pokazuje tylko pierwsze 3 linie.
- Edge case: workflow bez żadnego `text` — widoczny tylko pasek, brak błędu renderu, reszta pusta; `Enter` wysyła jak dotąd.
- Edge case: bardzo wąski (60 kolumn) pasek wejść przycina/zawija bez paniki; poniżej 60×20 komunikat „powiększ okno".
- Error path: pusty wymagany `repository`/`integer` na pasku → `Enter` nie wysyła, fokus przechodzi na to pole, błąd widoczny w wierszu statusu.
- Edge case: kolejność fokusu `Tab`: pola paska, potem `text`; `Shift+Tab` z pierwszego pola wraca do listy; `Up`/`Down` w polu `text` zmieniają pole.
- Edge case: przy 60×20 i trzech polach `text` pole z fokusem ma ≥ 4 wiersze (zwinięte spadają do samych nagłówków).
- Edge case: długa wartość `string` na pasku przycięta z `…`, kursor widoczny przy edycji.
- Error path: brak wartości w wymaganym `text` → błąd walidacji widoczny w nagłówku pola i formularz niewysłany.
- Integration: odświeżenie listy workflow w trakcie wpisywania nie zmienia fokusu, kursora ani treści pola (zgodnie z D-T6).
- Regresja: `Esc` w panelu odpowiedzi i zakładce Dyrygent zachowuje dotychczasowe zachowanie.

**Verification:**
- Wielowierszowy prompt wpisuje się w dużym polu bez przewijania; po wyborze workflow lista nie zajmuje ekranu; `Esc`/`Shift+Tab` wracają do listy bez utraty wartości.

---

### U5. Dokumentacja, pomoc i sprzątanie testów

**Goal:** README i pomoc opisują nowy układ i klawisze nowej linii; testy odzwierciedlają nowe podpowiedzi.

**Requirements:** R11.

**Dependencies:** U2, U4.

**Files:**
- Modify: `README.md` (tabela klawiszy: Panel/Formularze; sekcja „Starting one from the interface")
- Test: `tests/tui_render.rs`, `tests/tui_state.rs` (aktualizacja asercji `Alt+Enter`)

**Approach:**
- README: wiersz Panel „`Enter` send, `Ctrl+J` new line (also `Shift+Enter`/`Alt+Enter` where the terminal reports modifiers)"; wiersz Forms z klawiszem nowej linii; opis nowego układu formularza (panelik, pasek, pole `text`, `Esc` wraca do listy) i typu `text`.
- Dokumenty historyczne (`docs/plans/…`, `docs/brainstorms/2026-10-08-…`) zostają bez zmian.
- Zaktualizować istniejące testy z asercjami `Alt+Enter` (tests/tui_render.rs: panel i Dyrygent; tests/tui_state.rs: pasek klawiszy i helper `alt_enter()`).

**Test scenarios:**
- Test expectation: none dla treści README. Zaktualizowane asercje testów z U2/U4 pokrywają zmiany tekstów w UI.

**Verification:**
- Wyszukanie `Alt+Enter` w `README.md`, `src/tui/` i testach nie zwraca opisów „jedynego klawisza nowej linii".

---

## System-Wide Impact

- **Interaction graph:** nowa linia i `Esc` przechodzą przez wspólny `Chooser` — dotyka panelu odpowiedzi, Dyrygenta, formularza porażki i nowego zadania; reguła nowej linii wspólna, zmiana `Esc` ograniczona do formularza nowego zadania.
- **Error propagation:** błędy detekcji/uzbrojenia terminala są połykane (best-effort); walidacja pól jak dziś.
- **State lifecycle risks:** wartości pól w `item.fields` (powrót do listy, odświeżenie, szkic `DRAFT_KEY`, `new_task_memory`) — nie wolno ich zerować przy zmianie trybu.
- **API surface parity:** typ `text` widoczny w API/CLI (`WorkflowInputView`, `control_text`); każda akcja TUI ma odpowiednik CLI (D-T11) — nowych akcji nie dodajemy, więc parytet nie wymaga nowych komend, ale CLI musi poprawnie obsłużyć typ.
- **Integration coverage:** panic hook cofający raportowanie klawiszy; zachowanie w tmux/screen (ręcznie).
- **Unchanged invariants:** semantyka `Enter`; wybór workflow i opis po prawej; stała kolejność listy; próg 60×20; styl kolorów przez `Role`.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Terminal/multiplekser nie raportuje modyfikatorów, więc `Shift/Alt+Enter` = zwykły Enter | `Ctrl+J` zawsze działa; hinty wymieniają `Shift/Alt+Enter` tylko po wykryciu wsparcia |
| Zapytanie o wsparcie klawiszy opóźnia lub psuje start w niektórych terminalach | Tolerancja błędu/braku odpowiedzi → tylko `Ctrl+J`; test ścieżki błędu |
| Rozszerzone flagi zostają w terminalu po panice | Cofnięcie w idempotentnym `restore_terminal()` wspólnym dla Drop i panic hooka + test licznika |
| `Shift+Enter` wpada w Submit przez `Key::matches` (ignoruje SHIFT) | Decyzja newline'a na surowym `KeyEvent` przed `lookup` + test |
| Dwie tablice keymapy (Panel/Form) rozjeżdżają się | Jedna funkcja newline'a; testy sprawdzają obie ścieżki |
| Zbyt niski ekran (20 w.) zostawia za mało miejsca na prompt | Wysokość paneliku 2–3 w.; test układu przy 60×20; poniżej — komunikat |
| Workflow spoza repo ze stringami zamiast `text` dają pusty formularz | Świadoma decyzja; dokumentacja typu `text` w `WORKFLOW_SCHEMA.md` |
| Testy zostawiają procesy `narsor daemon` | Sprzątanie w `Drop` w nowych testach, jeśli uruchamiają daemon |

---

## Documentation / Operational Notes

- `WORKFLOW_SCHEMA.md` i `workflows/README.md` opisują typ `text` (autorzy workflow mogą migrować opisowe pola).
- README opisuje nowy układ formularza i klawisze nowej linii.
- Po wdrożeniu: ręczna weryfikacja w kitty/wezterm/foot (wsparcie) oraz tmux/zwykły emulator (tylko `Ctrl+J`).

---

## Sources & References

- **Origin document:** [docs/brainstorms/2026-10-09-tui-formularz-nowego-zadania-uklad-requirements.md](docs/brainstorms/2026-10-09-tui-formularz-nowego-zadania-uklad-requirements.md)
- Poprzedni plan TUI: `docs/plans/2026-10-09-001-feat-tui-przeprojektowanie-plan.md`
- Projekt TUI: `docs/brainstorms/2026-10-08-tui-przeprojektowanie-design.md`
- Kod: `src/tui/chooser.rs`, `src/tui/input_form.rs`, `src/tui/keymap.rs`, `src/tui/mod.rs`, `src/tui/task_app/new_task.rs`, `src/domain/workflow_definition.rs`
