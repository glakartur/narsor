---
title: "feat: Jedna ikona stanu na liście zadań TUI"
type: feat
status: completed
date: 2026-11-10
origin: docs/brainstorms/2026-11-10-tui-ikony-stanu-zadan-requirements.md
---

# feat: Jedna ikona stanu na liście zadań TUI

## Summary

Lista zadań dostaje jedną kolumnę z ikoną, wyprowadzoną po stronie klienta z pól, które serwer już wysyła. Serwer i protokół się nie zmieniają. Liczniki w nagłówku i skok „następny czekający" liczą z tej samej ikony, więc zgadzają się z tym, co widać na liście. Logika `attention_of` zostaje dla CLI i jako jedno ze źródeł ikony. Kolumna `?`/`!` znika. Trzecia linia wiersza traci słowo stanu i pokazuje krok oraz czas.

---

## Problem Frame

Wiersz zadania ma dziś dwie kolumny znaczników i trzecią linię z angielskim słowem stanu, które powtarza ikonę (patrz origin: `docs/brainstorms/2026-11-10-tui-ikony-stanu-zadan-requirements.md`). Plan opisuje, jak zrealizować wymagania R1–R13 z tego dokumentu.

---

## Requirements

Wymagania R1–R13, R12a, R12b i przykłady AE1–AE8 pochodzą z dokumentu origin i obowiązują bez zmian. Plan dodaje cztery decyzje, które zapadły podczas planowania i zmieniają wygląd z origin:

- R2a. Zestaw znaków jest ustalony w tabeli „Tabela ikon" w sekcji „Key Technical Decisions". Znak `?` oznacza „czeka na reakcję", a `!` oznacza „ma problem". Kolumna `?`/`!` znika, ale znaki wracają jako ikony stanu.
- R4a. Stany „czeka na reakcję" i „ma problem" są głośne: znak w pogrubieniu i kolorze (żółty, czerwony), tytuł zadania w pogrubieniu. Pozostałe ikony są spokojne.
- R13a. Liczniki w nagłówku i skok `Space` liczą z ikony wiersza: licznik `?` to liczba zadań z ikoną „czeka na reakcję", a licznik `!` to liczba zadań z ikoną „ma problem". Zadanie zarchiwizowane z resztką uwagi nie liczy się. Zadanie ze stanem błędu bez elementu uwagi liczy się jako problem.
- R14a. Nagłówek szczegółów pokazuje ikonę i słowo stanu wyprowadzone z tej samej ikony, więc obie części zawsze się zgadzają. Słowo w nagłówku uczy operatora znaczenia ikon. Legendy w aplikacji nie ma.

**Origin acceptance examples:** AE1 (R1, R6), AE2 (R5), AE3 (R9, R10), AE4 (R10), AE5 (R2), AE6 (R2, R12, R12a), AE7 (R12a), AE8 (R12b).

---

## Scope Boundaries

- Stan „nowy" i akcja „skieruj do procesowania". Plan rezerwuje tylko ikonę `○`. Żadne zadanie jej nie pokazuje.
- Zmiana znaczenia czasu (łączny czas, czas kroku). Czas zostaje „w bieżącym stanie".
- Zmiana kolejności i filtrowania listy (poza regułą `z`, patrz U4).
- Nowe stany po stronie serwera i zmiany w `crates/narsor-protocol`.
- `narsor task list` (CLI) oraz `status_badge` w `src/cli/task_text.rs`. Wiersz CLI nadal pokazuje znak `?`/`!` z `attention_of`.
- Tekst `bez slotu` w nagłówku szczegółów (`src/tui/task_app/detail.rs`) i w zakładce Info. Zmiana z R12 dotyczy tylko trzeciej linii wiersza listy.

### Deferred to Follow-Up Work

- Stan „nowy": nowy stan w modelu zadań, akcja skierowania do kolejki i podłączenie ikony `○`.

---

## Context & Research

### Relevant Code and Patterns

- `src/tui/task_app/list.rs`: `Attention`, `StateIcon`, `attention_of`, `state_icon`, `label_of`, `activity_since`, `RowText`, `row_text`, `history_row_text`, `PendingRow::text`, `TaskList::visible`, `TaskList::hidden_count`, `TaskList::counters`, `ROW_PREFIX`.
- `src/tui/style.rs`: `Role` i `Styles::of`. `Role::Attention` jest pogrubiony i żółty, a `Role::Problem` czerwony bez pogrubienia. Reguła z nagłówka modułu: kolor nigdy nie niesie informacji sam.
- `crates/narsor-protocol/src/display.rs`: `loss_state` i `LossState::AutoResume` rozpoznają automatyczne wznawianie z pól `RunnerState` i `ResumeState`. Lista już z nich korzysta w `attention_of` (precedens wyprowadzania po stronie klienta).
- `crates/narsor-engine/src/orchestration/model.rs`: `derive_task_state` opisuje, jak serwer zwija `Provisioning`, `PauseRequested` i `Interrupted` do `TaskState`. Plan nie zmienia tej funkcji, ale klient musi znać zasadę, bo `Provisioning` bez wykonania daje `Queued`, a z zapisem stanu `Running`.
- `src/tui/task_app/detail.rs`: nagłówek szczegółów rysuje `state_icon(task)` i `status_word(task)`.
- `src/tui/task_app/history.rs`: `row()` buduje `RowText` dla archiwum i dla starego formatu, a `legacy_icon` mapuje `ExecutionStatus` na `StateIcon`.
- Testy: `tests/integration/tui_state.rs` (moduł `task_app_list`, funkcja pomocnicza `row`), `tests/integration/tui_render.rs` (asercje na znaki i układ wiersza), `tests/integration/task_app_support/mod.rs` (`served_state`, `task`, `with_controller`, `with_runner`, `waiting`, `queue_view`).

### Institutional Learnings

- `docs/solutions/design-patterns/tui-list-row-title-one-builder-for-screen-and-rows-2026-10-31.md`: ekran i `rows()` muszą korzystać z jednego konstruktora wiersza (`row_text_with`), a asercje robić na `rows()` i na rysunku. Plan to zachowuje: nowe pola trafiają do `RowText`, a nie do osobnej ścieżki rysowania.

### Odpowiedź na pytanie z origin („Needs research")

`TaskSummary` zawiera `runner_state`, `resume`, `queue`, `controller` i `attention`. Wystarcza to do rozróżnienia uruchamiania, wstrzymywania, wznawiania automatycznego i kolejki zablokowanej bez zmiany protokołu.

---

## Key Technical Decisions

- **Wyprowadzanie po stronie klienta.** Dokument `TaskState` mówi, że klienci „nie liczą stanu sami". Lista już liczy `attention_of`, `controller_offline` i `loss_state` z tych samych pól, więc plan podąża za tym precedensem. Alternatywa (nowe wartości `TaskState` na serwerze) dotknęłaby protokołu, CLI i innych klientów. Nie jest potrzebna do celu zmiany.
- **Jedna funkcja wyprowadza ikonę** w kolejności pierwszeństwa z tabeli niżej. Liczniki, skok „następny czekający" (`Space`) i słowo stanu w nagłówku szczegółów czytają tę samą ikonę (R13a, R14a). `attention_of` zostaje dla CLI i jako źródło ikony.
- **Bez legendy w aplikacji.** Znaczenie ikon uczy słowo stanu w nagłówku szczegółów zadania, a znaki zostają takie, jakie pokazano w makiecie. Rozróżnienie podobnych znaków (`◷`, `⟳`, `↺`, `↻`) oceniamy wizualnie przy implementacji.
- **`RowText` traci pole `attention` i `status`.** Ikona niesie wszystko, a słowo stanu znika z trzeciej linii. Wiersze historii, wiersze oczekujące i formularz nowego zadania budują `RowText` bez tych pól.
- **Wyróżnienie bez zmiany `Role::Problem`.** `Role::Problem` ma około 35 użyć w całym TUI, więc nie zmieniam go globalnie. Wiersz listy używa istniejącego `Styles::strong(role)`, który dokłada pogrubienie do roli. Nowy pomocnik nie jest potrzebny.
- **Reguła `z` liczy stan, nie ikonę.** Dziś `hide_done` porównuje ikonę z `StateIcon::Done`. Po rozdzieleniu zakończonych i zarchiwizowanych porównanie ikon byłoby kruche, więc reguła czyta `TaskState`.

### Tabela ikon

| Stan | Ikona | Rola | Skąd klient go bierze |
|---|---|---|---|
| nowy | `○` | spokojna | zarezerwowany, nieużywany |
| w kolejce | `◷` | spokojna | `TaskState::Queued`, bez blokady |
| kolejka zablokowana | `⊘` | spokojna | `Queued`, gdy przyczyna kolejki to `ControllerOffline` lub `BackendUnknown`; `Running` i wznawianie automatyczne, gdy `controller_offline(task)` |
| uruchamianie | `⟳` | żywa | `runner_state == Provisioning`, bez blokady |
| w toku | `●` | żywa | `TaskState::Running`, bez innych warunków |
| wznawianie automatyczne | `↺` | spokojna | `loss_state` zwraca `AutoResume` |
| czeka na reakcję | `?` | uwaga, głośna | `TaskState::HumanNeeded` lub `attention_of == Question` |
| ma problem | `!` | problem, głośna | `attention_of == Problem` lub `TaskState::Error` |
| wstrzymywanie | `◑` | spokojna | `state == Paused` i `runner_state == PauseRequested` |
| zatrzymany ręcznie | `‖` | spokojna | `state == Paused` i `runner_state` inny niż `PauseRequested` (m.in. `Stopped`) |
| zakończony | `✓` | przygaszona | `TaskState::Finished` |
| zarchiwizowany | `▪` | przygaszona | `TaskState::Archived` |
| nieznany | `·` | przygaszona | `TaskState::Unrecognized` |
| wysyłanie (formularz) | `↻` | spokojna | wiersz oczekujący, bez zmian |
| nie wysłano (formularz) | `!` | problem, głośna | wiersz oczekujący po nieudanej próbie |

Role w tabeli: „żywa" to istniejący `Role::Live` (zielony), „spokojna" to `Role::Calm`, „przygaszona" to `Role::Done`, „uwaga" to `Role::Attention`, „problem" to `Role::Problem`.

Pauza z polityki zwalniania slotu jest dla serwera stanem `Running` (`pause_by_policy`), a `TaskSummary` nie niesie tego pola. Klient pokazuje ją jako „w toku", zgodnie z `state`, i nie próbuje jej odtwarzać.

Kolejność pierwszeństwa (pierwsza pasująca wygrywa): zarchiwizowany, problem, reakcja, kolejka zablokowana, zakończony, wstrzymywanie, uruchamianie, wznawianie automatyczne, zatrzymany ręcznie, w kolejce, w toku, nieznany. Problem wygrywa nad reakcją i nad stanem „w toku" (R6). Problem i reakcja mają też pierwszeństwo przed „kolejką zablokowaną" (R12a dotyczy zadań bez problemu i bez pytania). Wznawianie automatyczne przy kontrolerze offline pokazuje `⊘`.

*Ta tabela ilustruje zamierzone zachowanie i jest wskazówką do przeglądu, nie specyfikacją kodu.*

---

## Open Questions

### Resolved During Planning

- Skąd klient bierze rozróżnienie uruchamiania, wstrzymywania i wznawiania: z pól `TaskSummary` (patrz wyżej). Zmiana protokołu jest zbędna.
- Jaką ikonę ma zadanie w toku z kontrolerem offline: „kolejka zablokowana" (R12a). Zapadło w brainstormie.
- Czy zmieniać `Role::Problem`: nie, użyć istniejącego `Styles::strong` (patrz Key Technical Decisions).

### Deferred to Implementation

- Dokładna nazwa wariantów `StateIcon` i pomocnika w `Styles`. Należy zachować nazewnictwo z modułu.
- Zachowanie wiersza przy bardzo wąskiej liście: najpierw ucinany jest krok, a czas ma pierwszeństwo. Implementacja potwierdza to testem szerokości.
- Czy znak `⊘`, `↺`, `◑`, `▪` i `⟳` rysują się w jednej komórce w terminalach używanych przez zespół. Sprawdza się na etapie przeglądu wizualnego.

---

## Implementation Units

### U1. Zestaw ikon i wyprowadzanie z `TaskSummary`

**Goal:** `StateIcon` ma wszystkie stany z tabeli, a `state_icon` wybiera ikonę według kolejności pierwszeństwa.

**Requirements:** R1, R2, R2a, R3, R5, R6, R7, R12a. Covers AE1, AE2, AE5, AE6, AE7.

**Dependencies:** brak. Przemianowanie wariantów `StateIcon` w tej jednostce obejmuje też `PendingRow::text` i `history.rs`, żeby każdy commit się kompilował.

**Files:**
- Modify: `src/tui/task_app/list.rs`
- Modify: `src/tui/task_app/history.rs` (tylko przemianowania wariantów)
- Test: `tests/integration/tui_state.rs`

**Approach:**
- Rozszerz `StateIcon` o stany: nowy, kolejka zablokowana, uruchamianie, wznawianie, wstrzymywanie, zarchiwizowany. Przemianuj „Waiting" na reakcję, a „Failed" na problem. Znaki i role wg tabeli.
- `state_icon` korzysta z `attention_of`, `controller_offline`, `loss_state` i `runner_state` zgodnie z kolejnością pierwszeństwa. `attention_of` zostaje bez zmian.
- Dodaj metodę, która mówi, czy ikona wymaga operatora (reakcja lub problem). Używa jej wyróżnienie w U2.

**Patterns to follow:** `attention_of` i `label_of` w tym samym pliku (dostęp do `task.runner`, `task.queue`, `task.controller`).

**Test scenarios:**
- Covers AE1. Zadanie w toku z problemem runnera → ikona problemu, nie „w toku".
- Covers AE2. Zadanie z pytaniem i zadanie z propozycją conductora → ta sama ikona reakcji.
- Covers AE5. Zadanie `Finished` i zadanie `Archived` → różne ikony.
- Covers AE6. Zadanie w kolejce z przyczyną offline → ikona kolejki zablokowanej.
- Covers AE7. Zadanie w toku z kontrolerem offline → ikona kolejki zablokowanej.
- Zadanie z `runner_state == Provisioning` i bez wykonania (stan `Queued`) → ikona uruchamiania, nie kolejki.
- Zadanie `Interrupted` z zaplanowaną próbą wznowienia → ikona wznawiania.
- Zadanie `PauseRequested` (stan `Paused`) → wstrzymywanie. Zadanie z `runner_state == Stopped` (stan `Paused`) → zatrzymany ręcznie.
- Zadanie `PauseRequested` z ręcznie ustawionym `state == Running` (pauza z polityki) → w toku.
- Zadanie w zwykłej kolejce (`WaitingForCapacity`) bez kontrolera → w kolejce, nie kolejka zablokowana.
- Zadanie w toku z problemem i kontrolerem offline → problem (pierwszeństwo przed blokadą). Zadanie z wznawianiem automatycznym i kontrolerem offline → kolejka zablokowana.
- Zadanie `Interrupted` z wyczerpanym wznawianiem → problem.
- Zadanie w stanie `Unrecognized` → ikona nieznana.
- Zadanie `Archived` z pozostałym elementem uwagi → ikona zarchiwizowanego (pierwszeństwo).

**Verification:** `row(&app, ...)` w `tui_state.rs` zwraca właściwą ikonę dla każdego stanu z tabeli.

---

### U2. Wyróżnienie wierszy wymagających reakcji

**Goal:** Znak `?` i `!` jest pogrubiony i kolorowy, a tytuł takiego zadania pogrubiony.

**Requirements:** R4, R4a.

**Dependencies:** U1.

**Files:**
- Modify: `src/tui/task_app/list.rs`
- Test: `tests/integration/tui_render.rs`

**Approach:**
- Wiersz listy używa istniejącego `Styles::strong(role)` dla znaku i dla tytułu, gdy ikona wymaga operatora. W kolorze `Role::Attention` jest już pogrubiony, a `Role::Problem` jest czerwony bez pogrubienia, więc pogrubienie dokłada tu `strong`. Przy `NO_COLOR` obie role są już pogrubione.

**Patterns to follow:** `Styles::of` w `src/tui/style.rs`, wywołania `styles.of(grey(...))` w `row_text`.

**Test scenarios:**
- Znak `?` ma kolor żółty i modyfikator pogrubienia. Tytuł tego wiersza jest pogrubiony.
- Znak `!` ma kolor czerwony i modyfikator pogrubienia. Tytuł tego wiersza jest pogrubiony.
- Wiersz „w toku" i „w kolejce" nie ma pogrubionego tytułu.
- Przy `NO_COLOR` znaki `?` i `!` nadal różnią się od innych ikon kształtem i pogrubieniem.
- Wiersz nieaktualny (`stale`) przygasza kolor, ale nie gubi kształtu znaku.

**Verification:** Test renderu odczytuje styl komórki ze znakiem i z pierwszą literą tytułu.

---

### U3. Układ wiersza: jedna kolumna i trzecia linia

**Goal:** Wiersz ma jedną kolumnę ikony, a trzecia linia pokazuje krok, miejsce w kolejce i czas.

**Requirements:** R1, R8, R9, R10, R11, R12, R13, R13a. Covers AE3, AE4, AE6, AE7.

**Dependencies:** U1.

**Files:**
- Modify: `src/tui/task_app/list.rs`
- Modify: `src/tui/task_app/history.rs`
- Test: `tests/integration/tui_render.rs`
- Test: `tests/integration/tui_state.rs`

**Approach:**
- Usuń kolumnę `attention` z `row_text` i `history_row_text`. Zmniejsz `ROW_PREFIX` do szerokości „ikona i dwie spacje".
- Usuń pola `attention` i `status` z `RowText`. Dostosuj `row_text_with`, `PendingRow::text` i `row()` w `history.rs`.
- `label_of` nie zwraca słowa stanu, `offline` ani `bez slotu`. Dla zadania w toku zwraca nazwę kroku (z ikoną kroku), dla zadania w kolejce `#N`, w pozostałych przypadkach pustą etykietę.
- `row_text` składa trzecią linię jako `etykieta · czas`, `czas` albo pustą linię. Czas liczy `activity_since`, jak dziś. Przy wąskiej liście ucinana jest etykieta, a czas zostaje.
- Etykiety wierszy oczekujących (`wysyłanie…`, `nie wysłano`) i podgląd przenoszenia (`↕ n/m`) zostają.
- Liczniki w nagłówku (`TaskList::counters`) i skok `Space` (kod w `list.rs` szukający następnego zadania z uwagą) liczą z ikony wiersza (R13, R13a): ikona „czeka na reakcję" zwiększa licznik `?`, ikona „ma problem" zwiększa licznik `!`. Liczniki nadal obejmują wszystkie zadania, także ukryte przez `z` lub filtr tekstowy, jak dziś.

**Patterns to follow:** jeden konstruktor wiersza dla ekranu i `rows()` (`row_text_with`), zgodnie z `docs/solutions/design-patterns/tui-list-row-title-one-builder-for-screen-and-rows-2026-10-31.md`.

**Test scenarios:**
- Covers AE3. Zadanie w toku w kroku „Review" od 2 h 10 min → trzecia linia zawiera `Review` i `2h 10m`, nie zawiera `running`.
- Covers AE4. Zadanie wraca z reakcji do pracy → czas liczy się od powrotu.
- Covers AE6. Zadanie w kolejce z offline → trzecia linia zawiera `#N` i czas, nie zawiera `offline`.
- Zadanie `Finished`, `Error`, `Archived` → trzecia linia pusta (brak czasu, brak słowa stanu).
- Zadanie wstrzymane i zadanie czekające na reakcję → trzecia linia zawiera tylko czas.
- Zadanie w toku bez nazwy kroku → trzecia linia zawiera tylko czas (bez słowa `running`).
- Wiersz nie zawiera tekstu `bez slotu` dla zadań zakończonych, z błędem, wstrzymanych i czekających.
- Wąska lista: etykieta jest ucięta, czas zostaje widoczny.
- Wiersz formularza nowego zadania: `wysyłanie…` z ikoną `↻`, `nie wysłano` z ikoną problemu.
- Wiersz historii ma jedną ikonę i nie ma kolumny `?`/`!`.
- Covers R13a. Liczba `?` w nagłówku równa się liczbie zadań z ikoną „czeka na reakcję", a liczba `!` liczbie zadań z ikoną „ma problem", dla zestawu zawierającego zadania w każdym stanie z tabeli.
- Zadanie zarchiwizowane z resztką uwagi → nie zwiększa żadnego licznika, a `Space` go pomija.
- Zadanie ze stanem błędu bez elementu uwagi → zwiększa licznik `!`, a `Space` może na nim stanąć.
- Zadanie w toku z kontrolerem offline i z pytaniem → ikona `?` i licznik `?` (reakcja ma pierwszeństwo przed blokadą).

**Verification:** Istniejące asercje na stare znaki i prefiks są zaktualizowane do nowego układu. Lista miejsc (do odróżnienia od komunikatów, w których `✗` nie jest ikoną wiersza): `tests/integration/tui_render.rs` (zbiór dozwolonych znaków ok. linii 1031, początki wierszy `! ✗  id:c`, `  ◷  id:d`, `"  ✗  id:task-"`, `? ●`, `◐  NAR-42`, wiersz „nie wysłano"), `tests/integration/tui_state.rs` (funkcja `row` zwraca dziś krotkę z `Attention`, ok. linii 10574, 10645, 10676) i `tests/integration/tui_panel.rs` (asercje na `✗` tylko tam, gdzie dotyczą ikony wiersza).

Przyczyna stanu „czeka na reakcję" (pytanie, zatwierdzenie, propozycja) jest widoczna w panelu odpowiedzi i w szczegółach zadania (R5). Trzecia linia wiersza pokazuje dla tego stanu tylko czas.

---

### U4. Pozostałe miejsca korzystające z ikony

**Goal:** Reguła `z`, nagłówek szczegółów i historia używają nowego zestawu spójnie.

**Requirements:** R2, R12b, R14a. Covers AE5, AE8.

**Dependencies:** U1, U3.

**Files:**
- Modify: `src/tui/task_app/list.rs`
- Modify: `src/tui/task_app/detail.rs`
- Modify: `src/tui/task_app/history.rs`
- Test: `tests/integration/tui_state.rs`
- Test: `tests/integration/tui_render.rs`

**Approach:**
- `TaskList::visible` i `hidden_count` sprawdzają `TaskState::Finished | TaskState::Archived`, nie `StateIcon::Done`.
- Nagłówek szczegółów rysuje `state_icon(task)`, a słowo stanu (`status_word`) wyprowadza z tej samej ikony, nie z `task_state_name(task.state)` (R14a). Istniejące słowa zostają dla istniejących stanów: queued, running, awaiting input, paused, done, failed, archived, unknown. Nowe słowa dla nowych ikon: new, blocked, starting, resuming, pausing. Ikona „ma problem" daje `failed` dla stanu błędu, a `problem` dla pozostałych przypadków. Dokładne brzmienie ustala implementacja w języku istniejących słów nagłówka.
- Sprawdź, że rola ikony pasuje do tła nagłówka. Tekst `bez slotu` zostaje w nagłówku bez zmian (patrz Scope Boundaries).
- `history.rs`: wiersz archiwum dostaje ikonę zarchiwizowanego. `legacy_icon` zachowuje dzisiejsze mapowanie, tylko z nowymi nazwami wariantów: `Completed` na zakończony, `Failed`, `Cancelled` i `Unknown` na problem, `Paused`, `InputNeeded` i `WaitingForEvent` na zatrzymany, `Running` na w toku.

**Test scenarios:**
- Covers AE8. Lista z zadaniem zakończonym i zarchiwizowanym, `z` → oba ukryte, `hidden_count` zwraca 2.
- Covers R14a. Nagłówek szczegółów zadania w toku pokazuje `●` i `running`, a zadania czekającego na reakcję `?` i `awaiting input`.
- Covers R14a. Zadanie w toku z kontrolerem offline → `⊘` i `blocked`, nie `running`. Zadanie z wznawianiem automatycznym → `↺` i `resuming`.
- Dla każdej ikony z tabeli para (ikona, słowo) w nagłówku jest zgodna: żadna ikona nie ma słowa z innego stanu.
- Wiersz archiwum w historii ma ikonę zarchiwizowanego.
- Wiersz historii ze starego formatu z błędem ma ikonę problemu.

**Verification:** Test `tui_render.rs` dla nagłówka (tabela stanów `TaskState`, ok. linii 1381) pokazuje nowe znaki.

---

### U5. Dokumentacja

**Goal:** Dokumentacja opisuje nowy wiersz i zestaw ikon.

**Requirements:** R1, R2, R8–R12.

**Dependencies:** U1–U4.

**Files:**
- Modify: `docs/user/tui.md`
- Modify: `src/tui/style.rs` (tylko komentarze modułu z przykładami ikon)
- Modify: `src/tui/task_app/list.rs` (komentarz modułu z przykładem wiersza)

**Approach:**
- Opis listy w `docs/user/tui.md` (akapit „The list is the queue") opisuje jedną kolumnę ikony, tabelę ikon i trzecią linię (krok, `#N`, czas). Obecny tekst mówi o dwóch liniach wiersza i kolumnie uwagi, co jest już nieaktualne, więc popraw cały akapit.
- Zaktualizuj makietę w tym dokumencie (przykładowe wiersze z `?` i `!`).
- `docs/user/remote.md` opisuje wiersz CLI (`?`/`!`). Zostaje bez zmian.

**Test expectation:** none -- zmiany w dokumentacji i komentarzach.

**Verification:** Opis w `docs/user/tui.md` zgadza się z tabelą ikon z tego planu.

---

## System-Wide Impact

- **Powierzchnie:** lista zadań, lista historii, nagłówek szczegółów, formularz nowego zadania (wiersz oczekujący). CLI bez zmian.
- **Interakcje:** liczniki w nagłówku i skok `Space` czytają ikonę wiersza (test w U3). `narsor task list` nadal korzysta z `attention_of` i nie zmienia się.
- **Zgodność:** brak zmiany protokołu. Nowszy klient działa ze starszym serwerem, bo korzysta z pól, które już istnieją. Starszy serwer bez pola `state` daje `Unrecognized`, czyli neutralną ikonę, jak dziś.

---

## Risks & Dependencies

- **Testy renderu zakładają stare znaki i prefiks.** Dużo asercji w `tests/integration/tui_render.rs` i `tui_state.rs` odwołuje się do `✗`, `◐`, `! ✗` i `ROW_PREFIX`. Należy je zaktualizować w tych samych commitach, co kod.
- **Szerokość znaków.** `⊘`, `↺`, `◑`, `▪` i `⟳` mogą rysować się szerzej w niektórych fontach. Plan zakłada jedną komórkę. Przegląd wizualny w terminalu zespołu potwierdza to przed scaleniem.
- **Podobne kształty.** `◷` (kolejka) i `⟳` (uruchamianie) są podobne. Jeśli przegląd wizualny je myli, zamiana jednego znaku nie zmienia planu. To samo dotyczy `↺` i `↻`, trzech strzałek obrotu. Legendy w aplikacji nie dodajemy, a znaczenie ikon uczy nagłówek szczegółów (R14a).

---

## Sources & References

- **Origin:** `docs/brainstorms/2026-11-10-tui-ikony-stanu-zadan-requirements.md`
- Rozwiązanie pokrewne: `docs/solutions/design-patterns/tui-list-row-title-one-builder-for-screen-and-rows-2026-10-31.md`
- Kod: `src/tui/task_app/list.rs`, `src/tui/style.rs`, `crates/narsor-engine/src/orchestration/model.rs`, `crates/narsor-protocol/src/display.rs`
