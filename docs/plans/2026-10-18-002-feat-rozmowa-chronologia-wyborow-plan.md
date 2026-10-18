---
title: "feat: Chronologiczne miejsce wyborów workflow w zakładce Rozmowa"
type: feat
status: active
date: 2026-10-18
origin: docs/brainstorms/2026-10-18-rozmowa-chronologia-wyborow-workflow-requirements.md
---

# feat: Chronologiczne miejsce wyborów workflow w zakładce Rozmowa

## Summary

`Conversation::rebuild_with_events` wstawia dziś wszystkie wpisy „WYBÓR WORKFLOW” na koniec transkryptu. Plan zmienia to tak, że każdy wybór trafia po rozmowie wejścia w krok, po którym nastąpił, i przed nagłówkiem następnego kroku. Zmiana dotyczy jednej funkcji budującej transkrypt. Szczegóły zadania i Historia już wywołują tę samą funkcję (R6), więc nie wymagają zmian.

Głębokość planu: lekka (jedna jednostka produkcyjna, jedna jednostka testów).

---

## Problem Frame

Zob. origin: `docs/brainstorms/2026-10-18-rozmowa-chronologia-wyborow-workflow-requirements.md`. Operator nie widzi decyzji po kroku `code_review` obok rozmowy tego kroku. Musi dopasować decyzje z dołu transkryptu do kroków według godziny.

---

## Requirements

Wymagania R1–R7 i przykłady AE1–AE5 pochodzą z dokumentu origin i są stałe. Plan je tylko realizuje.

- R1, R1a, R2, R3, R4 — miejsce wyboru: U1, testy U2.
- R5 — treść wpisu bez zmian: U1 nie dotyka `workflow_choice_entry`.
- R6 — te same wywołania w `src/tui/task_app/detail.rs` i `src/tui/task_app/history.rs`: bez zmian w wywołaniach.
- R7 — kursor, rozwinięcia i tryb „podąża”: bez zmian w `TranscriptState`; test AE4 w U2.

---

## Scope Boundaries

- Nie zmieniamy treści, nagłówka, koloru ani identyfikatora wpisu wyboru (`workflow-choice/<sequence>`).
- Nie dodajemy nowych typów zdarzeń. Nie zmieniamy silnika, protokołu ani osi czasu (`crates/narsor-engine/src/engine/timeline.rs`).
- Nie wstawiamy wyboru w środek rozmowy kroku.
- Nie zmieniamy zakładek *Przebieg*, *Zdarzenia* i *Dyrygent*.

---

## Context & Research

### Ustalenia z kodu

- `rebuild_with_events` (`src/tui/task_app/tabs/conversation.rs`) przechodzi po `positions` i pomija pozycje bez rozmowy (`ConversationState::Recorded`) oraz powtórzone `action_execution_id`. Potem dopisuje wszystkie wybory na końcu.
- `Position` (`crates/narsor-protocol/src/api/timeline.rs`) ma `state_id` i `visit`. `visit` liczy się w `build_timeline` przy każdym zdarzeniu `StateEntered` danego stanu. Próby tego samego wejścia (retry, reprompt, wznowienie) mają ten sam `(state_id, visit)`. To jest dokładnie granica „wejścia w krok” z origin i ta sama reguła co licznik `max_entries` (`count_entry` w `crates/narsor-engine/src/engine/runner.rs` liczy przejścia między krokami).
- Zdarzenie `StateEntered` jest emitowane raz na wejście (`enter_state` w `runner.rs`; także po ręcznym przejściu i cofnięciu decyzji w `runner/operations.rs`). Wybory (`DecisionMade`, `ConductorDecision`, `HumanInputReceived`, `HumanTransitionSelected`) są zapisywane przed `StateEntered` następnego kroku (kolejność w nagłówku `runner.rs`: `DecisionMade → TransitionTaken`, potem `enter_state`).
- Dane wejściowe: `Event.sequence` jest całkowicie uporządkowany i ma go każde zdarzenie, które TUI już czyta (`view.events.events()`).

### Decyzje planistyczne (rozstrzygają pytania z origin)

**Kotwica: numer sekwencji `StateEntered`, a nie czas.** Origin podejrzewał kotwicę czasową i zostawił do weryfikacji założenie o wspólnym zegarze. Plan wybiera kotwicę sekwencyjną:
- nie wymaga założenia o zegarze (`Event.timestamp` i `Position.started_at` mogą mieć remisy lub różne źródła),
- nie ma problemu z pozycją bez `started_at`,
- daje wejście w krok wprost: n-te `StateEntered` stanu S to wejście `(S, visit = n)`, czyli ten sam klucz co `Position.visit`,
- kroki bez rozmowy (brama `human`, skrypt) mają własne `StateEntered`, więc R4 wynika z tego samego mechanizmu bez osobnego przypadku.

Wybór należy do wejścia o największym numerze sekwencji `StateEntered` mniejszym niż numer sekwencji wyboru. Wybór bez takiego wejścia stoi przed pierwszym nagłówkiem (R3). Wybór po ostatnim wejściu stoi na końcu (R3, AE4).

**Pozycja bez `StateEntered` (stare logi lub niepełna strona zdarzeń).** Pozycje bez dopasowanego wejścia nadal się wyświetlają jak dziś. Jeśli w ogóle nie ma zdarzeń `StateEntered`, wszystkie wybory zostają na końcu (dzisiejsze zachowanie). To jest zamierzone, bezpieczne zachowanie zapasowe.

---

## Key Technical Decisions

- Zmiana jest lokalna w `rebuild_with_events`. Funkcja pomocnicza grupuje wybory według wejścia i zwraca je w kolejności wejść. `workflow_choice_entry` zostaje bez zmian.
- Kolejność wyjścia: najpierw wybory przed pierwszym wejściem, potem dla każdego wejścia w kolejności `StateEntered`: nagłówek i rozmowa (o ile wejście ma nagłówek i rozmowę), potem wybory tego wejścia w kolejności `sequence` (R2). Wybory ostatniego wejścia kończą transkrypt.
- Wejście bez rozmowy nie wypisuje nic oprócz swoich wyborów (R4).
- Wejście z kilkoma próbami (R1a, AE5) wypisuje rozmowy wszystkich prób, a wybory dopiero po ostatniej z nich. Wymusza to grupowanie pozycji po `(state_id, visit)`, nie po pojedynczej pozycji.

---

## Implementation Units

### U1. Umieszczenie wyborów po wejściu w krok

**Goal:** Zastąpić dopisywanie wyborów na końcu transkryptu wstawianiem ich po rozmowie wejścia, do którego należą.

**Requirements:** R1, R1a, R2, R3, R4, R5, R6.

**Dependencies:** brak.

**Files:**
- Modify: `src/tui/task_app/tabs/conversation.rs`
- Test: `tests/tui_state.rs` (zob. U2)

**Approach:**
- Z `events` zbuduj uporządkowaną listę wejść: dla każdego `StateEntered { state_id }` kolejne `visit` danego stanu (licznik na stan, od 1), z numerem `sequence`.
- Dla każdego wyboru (wynik `workflow_choice_entry`, razem z `event.sequence`) znajdź wejście o największym `sequence` mniejszym niż `sequence` wyboru. Wynik: wybory przed pierwszym wejściem oraz wiadro wyborów na wejście. Wiadro zachowuje kolejność zdarzeń.
- Pętla po `positions` zostaje, ale każda pozycja ma klucz `(state_id, visit)`. Gdy klucz zmienia się względem poprzedniej pozycji (także pozycji bez rozmowy, które dziś są pomijane), zamknij poprzednie wejście: wypisz jego wiadro wyborów. Pętla musi więc śledzić klucz także dla pozycji pomijanych (`NotCreated`, `NoRecord`, bez `action_execution_id`).
- Wejścia z `StateEntered`, dla których nie ma żadnej pozycji (np. brama `human` bez pozycji), też wypisują swoje wybory. Wypisuj je w kolejności `StateEntered`, w miejscu między sąsiednimi wejściami. Prosta realizacja: iteruj po liście wejść z `StateEntered` i dla każdego wejścia pobierz jego pozycje z mapy `(state_id, visit) → Vec<&Position>`, zachowując kolejność pozycji.
- Pozycje, których klucz nie występuje na liście wejść (brak `StateEntered`), wypisz jak dziś, w kolejności `positions`, po ostatnim znanym wejściu, bez wyborów. To zachowuje zapasowe zachowanie.
- Zachowaj obecne reguły: deduplikacja `seen_attempts`, flagi `loading`, `failure`, `record`, nagłówek `── etykieta ──` w pierwszym wystąpieniu, `prefix_source_ids`. Nie zmieniaj ich kolejności względem siebie.
- Zaktualizuj komentarz „Keep choices at the current end…” i opis metody. Zasada „wybory na końcu” nie obowiązuje, ale najnowszy wybór nadal kończy transkrypt, gdy po nim nie ma nowego kroku (R3).

**Patterns to follow:**
- Styl i nazewnictwo w `src/tui/task_app/tabs/conversation.rs` (krótkie komentarze doc, `TranscriptEntry::new(...).with_header(...)`).
- Grupowanie po stanie i wizytach jak w `build_timeline` (`crates/narsor-engine/src/engine/timeline.rs`): licznik wejść na stan rośnie przy `StateEntered`.

**Test scenarios:** zob. U2. Jednostka jest nośnikiem funkcji, testy są w U2.

**Verification:** Dla danych z AE1–AE5 `Conversation::transcript().entries()` ma kolejność z origin. Wywołania w `detail.rs` i `history.rs` działają bez zmian w sygnaturze.

---

### U2. Testy kolejności wyborów

**Goal:** Pokryć scenariusze AE1–AE3 i AE5 testem jednostkowym `rebuild_with_events` (kryterium sukcesu z origin) oraz AE4 i przypadki brzegowe.

**Requirements:** R1, R1a, R2, R3, R4, R7.

**Dependencies:** U1.

**Execution note:** Najpierw napisz test AE1 i zobacz, że nie przechodzi na obecnym kodzie, potem U1.

**Files:**
- Test: `tests/tui_state.rs` (obok istniejących testów `Conversation` / `Attempts`, ok. linii 2978 i 3547, które mają gotowe pomocniki `AttemptRead` i `Attempts::apply`)

**Approach:** Zbuduj ręcznie `Position` (pole `visit`, `state_id`, `action_execution_id`, `ConversationState`), `Attempts` z wpisami rozmów i listę `Event` z `StateEntered`, `DecisionMade`, `ConductorDecision`, `HumanInputReceived`, `HumanTransitionSelected` z rosnącym `sequence`. Czytaj kolejność z nagłówków i identyfikatorów wpisów transkryptu (`workflow-choice/<sequence>`, `<attempt>/step`). Pomocniki zdarzeń: `tests/task_app_support/mod.rs`, jeśli pasują.

**Test scenarios:**
- Covers AE1. Kroki `plan → code_review → fix_review_findings`; agent wybrał `revise`, potem konduktor wybrał `fix`. Oczekiwana kolejność: nagłówek `plan`, rozmowa `plan`, nagłówek `code_review`, rozmowa `code_review`, wybór agenta, wybór konduktora, nagłówek `fix_review_findings`, jego rozmowa.
- Covers AE2. `plan`, brama `human` bez rozmowy (wybór użytkownika `approved`), `implement`. Wybór jest między rozmową `plan` a nagłówkiem `implement`. Wariant: brama ma pozycję `NotCreated`, wariant: brama nie ma pozycji w ogóle.
- Covers AE3. Pętla `code_review → fix → code_review`, trzy wejścia, dwa wybory po `code_review`. Każdy wybór jest po rozmowie swojego wejścia (`visit` 1 i 2), a nie po ostatnim.
- Covers AE5. `implement` ma trzy próby (ten sam `visit`), potem `done`, potem `code_review`. Wybór `done` jest po ostatnim wpisie ostatniej próby i przed nagłówkiem `code_review`. Żaden wybór nie stoi między próbami.
- Covers AE4 / R3. Wybór po ostatnim kroku jest ostatnim wpisem. Gdy czytelnik jest w trybie „podąża” i na końcu, po przebudowie z nowym wyborem nadal jest na końcu i ostatni wpis jest widoczny (użyj istniejących akcesorów `TranscriptState`, jak w pobliskich testach).
- R3. Wybór przed pierwszym wejściem z rozmową (np. wybór w pierwszym kroku bez rozmowy) stoi przed pierwszym nagłówkiem.
- R2. Dwa wybory w tym samym odstępie zachowują kolejność `sequence`, także gdy zdarzenia podano w innej kolejności w wektorze.
- Brzeg. Brak zdarzeń `StateEntered`: wybory zostają na końcu (zachowanie zapasowe), kolejność rozmów bez zmian.
- Brzeg. Pusta lista zdarzeń: wynik jak przed zmianą (`rebuild` bez zdarzeń).
- R5. Wpis wyboru ma tę samą treść, nagłówek `WYBÓR WORKFLOW · HH:MM:SS`, prezentację i identyfikator co przed zmianą.
- Integracja. Pozycja z `loading` (brak `Attempt`) nadal ustawia `is_loading()` i nie gubi wyborów.

**Verification:** Nowe testy przechodzą, a istniejące testy `tests/tui_state.rs` i `tests/tui_render.rs` przechodzą bez zmian oczekiwań.

---

## System-Wide Impact

- Dotknięta powierzchnia: tylko zakładka *Rozmowa* w szczegółach zadania i w Historii. Oba miejsca wywołują `rebuild_with_events` z tymi samymi argumentami (`src/tui/task_app/detail.rs`, `src/tui/task_app/history.rs`).
- Przebudowa jest wywoływana przy każdej zmianie rozmów lub zdarzeń. Koszt rośnie liniowo z liczbą zdarzeń i pozycji; nie zmienia się rząd złożoności względem dziś (jedno przejście po zdarzeniach, jedno po pozycjach).
- Brak zmian w API, protokole, zapisanych danych i silniku.

---

## Risks & Dependencies

- **Niepełna strona zdarzeń.** `events.events()` może być niepełna (limit stron). Wejścia bez zdarzeń `StateEntered` dostaną zapasowe zachowanie, a wybory bez wejścia trafią na początek lub koniec. Mitygacja: test brzegowy, zachowanie opisane w U1.
- **Cofnięcie decyzji konduktora (undo) i ręczne przejścia** emitują dodatkowe `StateEntered`. Ten sam mechanizm liczy `visit` w osi czasu, więc klucze pozostają zgodne. Weryfikacja przy implementacji: sprawdzić ręcznie jeden przypadek z cofnięciem na danych testowych.
- **Stare logi bez `visit`.** `Position.visit` domyślnie wynosi 1 przy braku `StateEntered`. Zapasowe zachowanie obejmuje ten przypadek.

---

## Deferred to Implementation

- Dokładny kształt funkcji pomocniczej i struktury danych wiadra (np. `BTreeMap` po indeksie wejścia).
- Czy pomocnik do budowy zdarzeń z `tests/task_app_support/mod.rs` wystarcza, czy test potrzebuje własnych konstruktorów zdarzeń.

### Deferred to Follow-Up Work

Brak.

---

## Sources & References

- Origin: `docs/brainstorms/2026-10-18-rozmowa-chronologia-wyborow-workflow-requirements.md`
- Kod: `src/tui/task_app/tabs/conversation.rs`, `crates/narsor-engine/src/engine/timeline.rs`, `crates/narsor-protocol/src/api/timeline.rs`, `crates/narsor-engine/src/engine/runner.rs`
