---
title: "feat: Lista zadań w TUI — ticket, tytuł, status z czasem, stała kolejność"
type: feat
status: active
date: 2026-10-27
origin: docs/brainstorms/2026-10-26-tui-lista-zadan-ticket-tytul-status-requirements.md
---

# feat: Lista zadań w TUI — ticket, tytuł, status z czasem, stała kolejność

## Summary

Wiersz listy zadań pokazuje ticket (albo skrócony `task_id`), tytuł oraz status z czasem aktywności. Czas liczy się na żywo w kliencie. Plan naprawia też zmianę kolejności po zatwierdzeniu pracy. Naprawa zaczyna się od odtworzenia błędu, bo przyczyna nie jest zweryfikowana (see origin: A5).

Zmiany dotyczą głównie klienta TUI. Wyjątki: nowe, opcjonalne pole w `TaskSummary` (U2) oraz poprawka po stronie serwera, jeśli U1 wykaże, że przyczyna zmiany kolejności leży po stronie serwera.

---

## Problem Frame

Dziś wiersz ma dwie linie (znaczniki + tytuł, etykieta). Brak w nim ticketu i czasu w statusie. Operator zgłasza, że po zatwierdzeniu zadanie „ucieka" z miejsca na liście (see origin: Problem Frame).

Fakty z kodu (sprawdzone podczas planowania):

- `src/tui/task_app/list.rs`: klient nie sortuje. `TaskList::replace` przyjmuje kolejność serwera. Tylko `Moved` zmienia kolejność w `apply_event`.
- Serwer sortuje w `crates/narsor-engine/src/orchestration/store.rs` (`queue_order`: `rank`, potem `queue_sequence`). Lista jest grupowana po backendzie.
- `TaskSummary` (`crates/narsor-protocol/src/controller.rs`) ma `ticket`, `title`, `display.title`, `created_at`, `state`, `rank`, `queue: Option<QueueView>` (z `since`), `attention: Vec<AttentionItem>` (z `since`). **Nie ma** ogólnego pola „od kiedy w tym stanie".
- `short_age(seconds)` w `src/cli/task_text.rs` daje `45s`/`4m`/`3h`/`2d`. Format w R7 (`2h 10m`) jest inny — patrz U3.
- Skrót `task_id` do 8 znaków już istnieje: `SHORT_ID_CHARS` w `src/tui/task_app/detail.rs`.
- Pętla TUI budzi się co `TICK` (`src/tui/mod.rs`, `wait_until_next_due`). Rysowanie wynika z tego ticka.

---

## Requirements

Przeniesione z origin bez zmian: R1–R11, AE1–AE5. Plan przyjmuje założenia A1–A4, A6, A7 jako decyzje (patrz niżej), bo mają bezpieczne wartości domyślne.

---

## Scope Boundaries

- Poza zakresem: nowe sortowanie lub grupowanie listy; zmiana zasady, że kolejność wyznacza serwer; edycja ticketu z listy; zmiany panelu szczegółów poza wspólnymi funkcjami (see origin: Scope Boundaries).

### Deferred to Follow-Up Work

- Pokazywanie czasu od zakończenia dla zadań w stanie końcowym (A4: dziś „nic").
- Wariant „czas od utworzenia" lub „łączny czas pracy agenta" (A1).

---

## Key Technical Decisions

- **Decyzje domyślne dla pytań otwartych.** Pytania Q1–Q4 z origin nie mają odpowiedzi w dokumencie. Plan przyjmuje wartości z założeń i oznacza je jako do potwierdzenia przez operatora. Zmiana którejkolwiek zmienia tylko U3/U4, nie architekturę:
  - Q1/A1: czas w bieżącym statusie.
  - Q2: zadanie zostaje na miejscu także po zmianie na końcowy. Ukrywanie zakończonych (`z`) działa jak dziś, jako filtr.
  - Q3/A2: skrót `task_id` to 8 znaków (`SHORT_ID_CHARS`). Gdy jest ticket, skrót się nie pokazuje.
  - Q4: szerokość listy bez zmian. Ticket zajmuje część linii tytułu (R5).
- **Źródło czasu wejścia w stan.** Dodać do `TaskSummary` opcjonalne pole `state_since: Option<DateTime<Utc>>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, jak `ticket`). Serwer wypełnia je przy budowaniu summary (`crates/narsor-engine/src/controller_protocol/service.rs`, miejsce `state: record.task_state()`). Starszy serwer nie wyśle pola — wiersz nie pokazuje czasu (A7). Starszy klient ignoruje pole.
  - Wartość: moment ostatniej zmiany `TaskState`. Implementer ustala, czy rekord już ją trzyma (np. czas zdarzenia zmiany stanu). Jeśli nie — dodaje zapis do rekordu zadania z domyślną wartością dla starych rekordów (`None`).
- **Czas na żywo bez serwera.** Klient liczy `teraz − state_since` przy rysowaniu. Zegar bierze z istniejącej pętli (`TICK`); nie dodaje nowego zadania w tle. Dla stanów końcowych (`Finished`, `Error`, `Archived`) czas się nie pokazuje (R8, A4).
- **Przyczyna zmiany kolejności: najpierw ustalić.** U1 odtwarza błąd testem. Plan nie zakłada rozwiązania. Kandydaci z origin: `replace` po odświeżeniu, zmiana `rank` po stronie serwera, zdarzenie `Moved`.

---

## Implementation Units

### U1. Odtworzyć zmianę kolejności po zatwierdzeniu

**Goal:** Ustalić przyczynę zmiany kolejności i opisać ją w plan-notatce przed naprawą.

**Requirements:** R9, R10, R11; AE4, AE5.

**Dependencies:** brak.

**Files:**
- `tests/tui_state.rs` (nowe testy odtwarzające)
- `src/tui/task_app/list.rs` (tylko po ustaleniu przyczyny)
- `crates/narsor-engine/src/orchestration/store.rs`, `crates/narsor-engine/src/controller_protocol/service/reader.rs` (jeśli przyczyna jest po stronie serwera)

**Approach:**
- Napisać test klienta: lista A, B, C; zdarzenia i odświeżenie, jakie powstają po odpowiedzi, zatwierdzeniu akcji i zatwierdzeniu kroku w B. Sprawdzić `visible_ids()` i `selected_id()`.
- Jeśli test klienta przechodzi, sprawdzić serwer: czy zatwierdzenie zmienia `rank` lub `queue_sequence`, albo czy lista jest sortowana inaczej niż `queue_order` (np. po stanie, w czytniku `reader.rs`).
- Naprawić w miejscu przyczyny. Klient nadal nie sortuje. Jeśli przyczyną jest serwer, naprawa nie zmienia zasady „kolejność wyznacza serwer".

**Execution note:** Zacząć od testu, który nie przechodzi (test-first dla błędu).

**Patterns to follow:** `docs/solutions/ui-bugs/tui-confirmation-popup-closed-by-background-refresh-2026-10-26.md`, `docs/solutions/logic-errors/tui-list-read-on-every-runner-report-2026-10-22.md`.

**Test scenarios:**
- Covers AE4. Lista A, B, C. Zatwierdzenie w B (odpowiedź agentowi) → po zdarzeniu i po `replace` kolejność A, B, C, wybrane B.
- Zatwierdzenie akcji zmienia stan B na `Running`: kolejność bez zmian.
- Zatwierdzenie kroku, po którym B przechodzi w stan końcowy: B zostaje na miejscu (Q2, domyślnie); przy `z` znika jako filtr, bez regrupowania.
- Covers AE5. `Moved` przenosi C przed A: kolejność C, A, B.
- Zadanie dodane lub usunięte w kolejce zmienia kolejność tylko o ten wiersz (R10).

**Verification:** Test odtwarzający nie przechodzi przed naprawą i przechodzi po niej. Przyczyna jest opisana w komentarzu testu i w opisie zmiany.

### U2. Pole `state_since` w protokole i na serwerze

**Goal:** Klient dostaje czas wejścia w bieżący `TaskState`.

**Requirements:** R3, R7; A7.

**Dependencies:** brak (równolegle z U1).

**Files:**
- `crates/narsor-protocol/src/controller.rs`
- `crates/narsor-engine/src/controller_protocol/service.rs`
- `crates/narsor-engine/src/orchestration/store.rs` (jeśli rekord musi trzymać czas)
- `src/tui/task_app/history.rs` (`legacy_summary`)
- testy protokołu obok istniejących w `crates/narsor-protocol/src/controller.rs` i `tests/api.rs`

**Approach:** Opcjonalne pole z `serde(default)`. Brak zmiany wersji protokołu. Aktualizować literały `TaskSummary` w testach i w `legacy_summary` (`src/tui/task_app/history.rs`) na `None`.

**Patterns to follow:** pole `ticket` w `TaskSummary`.

**Test scenarios:**
- Odczyt JSON bez pola daje `None`.
- Zapis z `None` nie dodaje klucza.
- Zmiana `TaskState` zadania aktualizuje `state_since`; zmiana, która nie zmienia `TaskState` — nie.
- Stary rekord bez czasu daje `None`, nie błąd.

**Verification:** Serwer wysyła pole; stary JSON nadal się wczytuje.

### U3. Format czasu aktywności

**Goal:** Jedna funkcja formatuje czas jak w R7.

**Requirements:** R7, R8.

**Dependencies:** brak.

**Files:** `src/cli/task_text.rs` (lub moduł TUI obok listy), `tests/cli_task_text.rs`.

**Approach:** `short_age` daje jedną jednostkę (`4m`, `3h`). R7 podaje przykład `2h 10m`. Dodać osobną funkcję formatu dla listy (dwie jednostki: `5m`, `2h 10m`, `3d`) zamiast zmieniać `short_age`, bo używają go CLI i inne widoki. Dla `3d` wystarczy jedna jednostka.

**Test scenarios:**
- 0 s → `0m` lub `<1m` (jedna decyzja, spisana w teście); 59 s → to samo.
- 12 min → `12m`; 130 min → `2h 10m`; 3 dni → `3d`.
- Wartość ujemna (zegar się cofnął) → jak zero.

**Verification:** Testy formatu przechodzą; `short_age` bez zmian.

### U4. Wiersz listy: ticket, tytuł, status i czas

**Goal:** Wiersz pokazuje dane z R1–R6.

**Requirements:** R1–R6, R7, R8; AE1, AE2, AE3.

**Dependencies:** U2, U3.

**Files:**
- `src/tui/task_app/list.rs` (`RowText`, `title_of`, `label_of`, `row_text`, `render`)
- `src/tui/task_app/style.rs` / `src/tui/style.rs` (rola dla skrótu `task_id`, jeśli trzeba)
- `tests/tui_render.rs`, `tests/tui_state.rs`

**Approach:**
- Układ dwuliniowy (A3). Linia 1: znaczniki, ticket, tytuł. Linia 2: status, czas, dotychczasowa etykieta (krok, `#n`, `offline`, `bez slotu`).
- `RowText` dostaje pola `ticket: Option<String>` (albo skrót id + flaga) oraz `since: Option<DateTime<Utc>>`. Czas formatuje się przy rysowaniu, nie w `RowText` (żeby test nie zależał od zegara).
- Ticket i tytuł przechodzą przez `one_line` (R5). Przycinanie: tytuł tracił miejsce pierwszy, ticket ma dolną granicę (jak `edge_layout` w `src/tui/task_app/detail.rs`). Ticket nie wypiera tytułu całkowicie.
- Brak ticketu: skrót `task_id` (8 znaków) innym stylem i z prefiksem (np. `#` nie — koliduje z `#n` kolejki; użyć np. `·` lub stylu `Role::Done`). Implementer wybiera jeden i dodaje test odróżnialności (R6).
- Funkcje wspólne z panelem szczegółów (`title_of`) nie zmieniają zachowania.

**Patterns to follow:** `edge_layout` (cięcie ticketu i tytułu), `row_text` w `list.rs`, `render_barred_list`.

**Test scenarios:**
- Covers AE1. Ticket `NAR-42`, tytuł „Refaktor parsera” → linia 1 zawiera oba.
- Covers AE2. Brak ticketu → skrót 8 znaków, styl różny od ticketu; nigdy nie wygląda jak ticket.
- Covers AE3. `HumanNeeded` od 12 min → linia 2 ma status i `12m`.
- Wąska lista: długi ticket i tytuł → tytuł skrócony pierwszy, ticket nie znika całkiem, obie linie mieszczą się w szerokości.
- Znaki sterujące w tickecie/tytule → `one_line` je usuwa.
- `state_since = None` → brak czasu, brak błędu.
- Stan końcowy (`Finished`, `Error`, `Archived`) → brak czasu (A4).
- Znaczniki `?`/`!` i ikona stanu bez zmian (R4).
- Wiersz tworzony lokalnie (`PendingRow`) nadal działa bez ticketu i czasu.

**Verification:** Zrzuty/testy renderowania zgodne z AE1–AE3; istniejące testy listy przechodzą.

### U5. Czas rośnie na żywo

**Goal:** Czas w wierszu odświeża się bez odczytu z serwera.

**Requirements:** R8; AE3.

**Dependencies:** U4.

**Files:** `src/tui/mod.rs` (`wait_until_next_due`, `TICK`), `src/tui/task_app/mod.rs`, `tests/tui_state.rs`.

**Approach:** Sprawdzić, czy pętla rysuje po każdym tiku. Jeśli tak — wystarczy liczyć czas z zegara przy rysowaniu. Jeśli rysuje tylko po zmianie stanu — dodać wymuszenie przerysowania raz na minutę, tylko gdy na liście jest wiersz z czasem. Zegar podaje się do renderowania z zewnątrz (parametr `now`), żeby testy go sterowały.

**Test scenarios:**
- Covers AE3. Ten sam stan, `now` +1 min → `12m` zmienia się na `13m`, bez `replace` i bez zdarzenia.
- Zadanie w stanie końcowym: czas się nie pokazuje przy żadnym `now`.
- Brak zadań z czasem → brak dodatkowego budzenia pętli.

**Verification:** Test z `now` pokazuje wzrost czasu; brak dodatkowych odczytów listy.

### U6. Dokumentacja i komentarze modułu

**Goal:** Opis wiersza w dokumentacji modułu i README zgadza się z kodem.

**Requirements:** R1–R9.

**Dependencies:** U1–U5.

**Files:** `src/tui/task_app/list.rs` (doc modułu), `README.md` (jeśli opisuje listę), `docs/solutions/` (notatka o przyczynie z U1).

**Test expectation:** none — tylko dokumentacja.

**Verification:** Dokumentacja opisuje dwie linie wiersza, ticket/skrót id, czas i przyczynę z U1.

---

## Dependencies and Sequencing

```mermaid
flowchart TB
  U1[U1 odtworzenie kolejności] --> U6
  U2[U2 pole state_since] --> U4
  U3[U3 format czasu] --> U4
  U4[U4 wiersz listy] --> U5[U5 czas na żywo]
  U5 --> U6[U6 dokumentacja]
```

---

## Risks

- **Przyczyna kolejności nieznana.** Jeśli leży po stronie serwera (np. zmiana `rank` przy zatwierdzeniu), naprawa wychodzi poza klienta. Granica zakresu: nie zmieniać zasady „kolejność wyznacza serwer", tylko usunąć błąd. Jeśli naprawa wymaga zmiany tej zasady, wrócić do wymagań.
- **Stary rekord bez czasu stanu.** Wiersz bez czasu jest poprawny (A7). Nie zgadywać wartości z `created_at`.
- **Szerokość listy.** Ticket + tytuł na wąskim panelu: pilnują tego testy U4.
- **Decyzje do potwierdzenia przez operatora** (Q1–Q4): wartości domyślne opisane w Key Technical Decisions. Wynik wymaga weryfikacji przez człowieka.

---

## Deferred to Implementation

- Czy rekord zadania już trzyma czas ostatniej zmiany `TaskState` (U2).
- Czy pętla TUI przerysowuje po każdym tiku (U5).
- Dokładny styl i prefiks skrótu `task_id` (U4).
- Zapis czasu `<1m` dla 0–59 s (U3).
