---
title: TUI Wybór użytkownika i krok bez rozmowy - Plan
type: fix
date: 2026-11-01
topic: tui-wybor-uzytkownika-i-krok-bez-rozmowy
origin: docs/brainstorms/2026-11-01-tui-wybor-uzytkownika-i-krok-bez-rozmowy-requirements.md
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
---

# TUI Wybór użytkownika i krok bez rozmowy - Plan

## Goal Capsule

- **Objective:** Operator czyta *Rozmowę* i widzi każdy wybór człowieka dokładnie raz, jako wybór użytkownika, pod nagłówkiem kroku, w którym wybór padł.
- **Means:** Zmiana wyłącznie w budowaniu *Rozmowy* w TUI (`build_conversation` i projekcja wyborów); silnik i format zdarzeń bez zmian.
- **Authority:** Dokument wymagań `docs/brainstorms/2026-11-01-tui-wybor-uzytkownika-i-krok-bez-rozmowy-requirements.md` (R1–R12, AE1–AE7).
- **Stop conditions:** AE1–AE7 mają testy, istniejące testy *Rozmowy* i *Przebiegu* przechodzą, `cargo fmt --check` i `cargo clippy` czyste.

---

## Product Contract

Źródłem produktowym jest dokument wymagań z `origin`; jego R1–R12, AE1–AE7, zakres i założenia obowiązują bez zmian. Plan ich nie powtarza, tylko odwołuje się po ID. (see origin: docs/brainstorms/2026-11-01-tui-wybor-uzytkownika-i-krok-bez-rozmowy-requirements.md)

Skrót: wybór użytkownika ma jeden wpis „użytkownik” (R1–R3, R5), wybory agenta i dyrygenta bez zmian (R4), każde wejście w krok bez własnej rozmowy ma nagłówek „── <krok> #n ──” w widoku całej *Rozmowy* (R6–R8), *Przebieg* nagłówków nie dostaje (R9), treść, styl i kolejność wpisów bez zmian (R10, R11), tylko TUI (R12).

Pytanie otwarte z dokumentu (duplikaty wyborów dyrygenta) pozostaje poza zakresem.

---

## Planning Contract

**Product Contract preservation:** Product Contract unchanged. Jedno doprecyzowanie R3 (KTD2) zawęża, kiedy `DecisionMade` od dostawcy `human` jest pomijany, żeby nie zgubić wyboru, który nie ma drugiego wpisu.

### Verified facts about existing code

- `src/tui/task_app/tabs/conversation.rs`: `build_conversation(scope, positions, attempts, events)` składa *Rozmowę* (`Scope::Whole`) i widok kroku (`Scope::Step`, *Przebieg*, wołany z `src/tui/task_app/tabs/progress.rs`). Nagłówek „── etykieta ──” tworzy wyłącznie `Builder::push_position`, i to tylko gdy próba ma niepuste wpisy.
- `step_entries(events)` daje wejścia w krok (`StateEntered`) z numerem `visit` liczonym jak w osi czasu; `workflow_choices(events)` zwraca wybory z numerem sekwencji, a `workflow_choice_entry` mapuje `DecisionMade` → „agent · <dostawca>”, `ConductorDecision`, `HumanInputReceived` (operator, z etykietą) i `HumanTransitionSelected` → „użytkownik”.
- Kolejność zdarzeń po odpowiedzi użytkownika (silnik): `HumanInputReceived` (`crates/narsor-engine/src/engine/actions/human.rs`) → `DecisionMade` (`apply_decision` w `crates/narsor-engine/src/engine/runner.rs`) → `StateExited`/`TransitionTaken` → `StateEntered` następnego kroku. Ręczne przejście (`crates/narsor-engine/src/engine/runner/operations.rs`) emituje po kolei `DecisionMade` (dostawca `human` albo `human_override`), `HumanTransitionSelected`, `StateExited`, `TransitionTaken`.
- `HumanInputReceived.output_label` jest `None` dla wznowienia agenta i zatwierdzenia wyniku (`ResumeAgent`, `ApproveOutput`), a `DecisionMade` z dostawcą `human` powstaje także tam (`runner.rs`, wywołania `apply_decision` z `DecisionProviderType::Human`). Takiego wyboru nie da się sparować po etykiecie.
- Testy: `tests/integration/tui_state.rs`, moduł `step_conversation` (helper `order`, `tags`, `recorded`, `attempts`) oraz `tests/integration/task_app_support/mod.rs` (`entered`, `agent_chose`, `conductor_chose`, `human_chose`). Brakuje helperów dla `HumanInputReceived` z etykietą i dla `DecisionMade` z dowolnym dostawcą.

### Key Technical Decisions

- KTD1. **Duplikat rozpoznaje się po kolejności zdarzeń w obrębie jednego wejścia w krok.** `workflow_choices` przechodzi po zdarzeniach w kolejności sekwencji, trzyma listę etykiet z `HumanInputReceived` (operator, z etykietą) i czyści ją przy każdym `StateEntered`. `DecisionMade` z etykietą z tej listy zużywa jedną pozycję i nie daje wpisu. Nie rozstrzyga o tym nazwa dostawcy (`rules` też bywa wynikiem odpowiedzi użytkownika). Realizuje R1, R2, R5, AE1, AE7.
- KTD2. **`DecisionMade` od dostawcy `human`/`human_override` jest pomijany tylko wtedy, gdy ma partnera.** Partnerem jest `HumanTransitionSelected` w tym samym wejściu w krok (R3, AE2) albo sparowana odpowiedź z KTD1. Bez partnera (zatwierdzenie lub wznowienie bez etykiety) wpis zostaje, ale z aktorem „użytkownik” i prezentacją operatora zamiast „agent · human”, bo inaczej wybór człowieka zniknąłby z *Rozmowy*. Wciąż jeden wpis na wybór (R1); R4 nie jest dotknięte, bo dotyczy dostawców innych niż człowiek. `human_override` traktować tak samo jak `human`, stałe nazw dostawców brać z silnika (`MANUAL_PROVIDER`, `OVERRIDE_PROVIDER`), jeśli są osiągalne z TUI, inaczej z literałów z testem.
- KTD3. **Nagłówek kroku bez rozmowy powstaje w pętli po `step_entries`, nie w `push_position`.** Dla każdego wejścia, które po przejściu po jego pozycjach nie dodało nagłówka ani nie czeka na odczyt rozmowy, `build_conversation` dodaje nagłówek „── <state_id> #<visit> ──” w tym samym stylu (`TranscriptPresentation::System`, `with_clipped_head`). `push_position` zwraca, co zrobił: dodał nagłówek / odczyt trwa (`loading`) / nic. Odczyt w toku nie daje nagłówka zastępczego, żeby po dokończeniu nie było dwóch nagłówków (R8). Pozycja pominięta przez `seen_attempts` (ponowna próba wskazuje ten sam czat) też nie daje nagłówka zastępczego. Realizuje R6, R7, R8, AE4, AE5.
- KTD4. **Nagłówki tylko w `Scope::Whole`.** Gałąź `Scope::Step` nie dostaje zmian poza KTD1/KTD2, które dotyczą `workflow_choices` i działają w obu zakresach (R9, AE6).
- KTD5. **Identyfikator nagłówka zastępczego** jest stabilny i unikalny na wejście w krok, np. `step-visit/<state_id>/<visit>/step`, żeby zachowanie kursora i rozwinięć w transkrypcie (`TranscriptEntryId`) działało tak samo jak dla nagłówków prób. Wzorzec `…/step` używany przez `tags` w testach jest tylko konwencją testów; helper testowy trzeba rozszerzyć.
- KTD6. **Wpisy bez `StateEntered` (stary lub częściowy log)** zachowują obecną gałąź `visits.is_empty()`: bez nagłówków zastępczych, wybory na końcu, z tym samym filtrowaniem duplikatów z KTD1/KTD2 (reset listy etykiet nie występuje, bo nie ma granic wejść, więc parowanie liczy się do pierwszego `DecisionMade` z tą etykietą).

### Assumptions

- Założenia z dokumentu wymagań są przyjęte: „widok szczegółów” to zakładka *Rozmowa* szczegółów zadania, kroki bez wpisów inne niż dla człowieka (np. skrypt) też dostają nagłówek, numer „#n” to numer wejścia w krok.
- Kroki, których jedyną „rozmową” jest wybór (bramka `human` bez próby agenta), mają pozycję osi czasu bez `ConversationState::Recorded`; to je odróżnia od kroków z rozmową. Implementator potwierdza to na `Position` w testach jednostkowych, nie zakłada.
- Zakończenie przepływu (stan końcowy) też jest `StateEntered`; dostaje nagłówek jak inne kroki, o ile nie ma rozmowy. Jeśli okaże się, że powstaje to zbyt hałaśliwie (np. `done`), odnotować w PR i zapytać; wymagania tego nie wykluczają (R6 mówi „każde wejście w krok”).

### Sequencing

U1 (wybory) i U2 (nagłówki) zmieniają niezależne funkcje; U2 zależy od U1 tylko w teście AE4, który sprawdza oba razem. Wykonać U1, potem U2.

---

## Implementation Units

### U1. Jeden wpis na wybór użytkownika

- **Goal:** `workflow_choices` nie daje „agent · …” dla wyboru, który podjął człowiek, i nie podwaja wpisów.
- **Requirements:** R1, R2, R3, R4, R5, R10, R11; AE1, AE2, AE3, AE7.
- **Dependencies:** None.
- **Files:** `src/tui/task_app/tabs/conversation.rs`, `tests/integration/task_app_support/mod.rs`, `tests/integration/tui_state.rs`.
- **Approach:**
  1. Zmienić `workflow_choices` z mapowania zdarzenie → wpis na przejście po zdarzeniach w kolejności sekwencji z małym stanem: etykiety odpowiedzi użytkownika w bieżącym wejściu w krok (czyszczone przy `StateEntered`).
  2. `DecisionMade`: pominąć, gdy etykieta jest na liście (KTD1) albo gdy dostawca to człowiek i w tym samym wejściu w krok jest `HumanTransitionSelected` (KTD2, podgląd naprzód do następnego `StateEntered`). Gdy dostawca to człowiek bez partnera, zbudować wpis z aktorem „użytkownik” i prezentacją operatora; powód (`reasoning`) zachować jak dziś.
  3. Pozostałe zdarzenia (`ConductorDecision`, `HumanInputReceived`, `HumanTransitionSelected`) mapować jak dotąd; `HumanInputReceived` bez etykiety nadal nie daje wpisu (R5). Treść, nagłówek czasu, id `workflow-choice/<sequence>` i `as_workflow_choice()` bez zmian (R10).
  4. Wydzielić z `workflow_choice_entry` budowę wpisu (aktor, etykieta, prezentacja, powód), żeby obie ścieżki jej używały bez duplikacji.
- **Execution note:** Zacząć od testów AE1, AE2, AE7 (zaczerwienione), potem zmiana.
- **Patterns to follow:** istniejące testy `a_choice_keeps_its_text_header_and_id`, `only_a_choice_is_marked_as_a_workflow_choice`, `an_agents_choice_shows_its_reason_without_control_characters` w module `step_conversation`; helpery w `tests/integration/task_app_support/mod.rs`.
- **Test scenarios:**
  - Covers AE1. `HumanInputReceived` (operator, `check_feedback`), potem `DecisionMade` (`rules`, `check_feedback`): jeden wpis „użytkownik wybrał opcję: check_feedback”, brak „agent · rules”.
  - Covers AE2. `DecisionMade` (`human`) i `HumanTransitionSelected`: jeden wpis „użytkownik”; to samo dla `human_override`.
  - Covers AE3. `DecisionMade` (`rules`, `published`) bez odpowiedzi użytkownika: wpis „agent · rules wybrał opcję: published”.
  - Covers AE7. Ta sama etykieta wybrana przez użytkownika w dwóch kolejnych wejściach w krok (para zdarzeń w każdym): dwa wpisy użytkownika, żadnego agenta.
  - Covers R5. `HumanInputReceived` bez etykiety (wznowienie agenta) bez `DecisionMade`: brak wpisu; z następującym `DecisionMade` od dostawcy `human`: jeden wpis „użytkownik” (KTD2).
  - Odpowiedź użytkownika z etykietą A i `DecisionMade` z etykietą B w jednym wejściu: dwa wpisy (parowanie wymaga tej samej etykiety).
  - Odpowiedź operatora z etykietą w wejściu 1 nie tłumi `DecisionMade` o tej samej etykiecie w wejściu 2 (reset na `StateEntered`).
  - Wynik w `Scope::Step` jest zgodny z `Scope::Whole` dla tych samych zdarzeń (jeden wpis użytkownika, R9).
  - Wybór dyrygenta (`ConductorDecision`) i `DecisionMade` z dostawcą `conductor` zachowują się jak dotąd (poza zakresem).
- **Verification:** `cargo test --test tui_state step_conversation` zielone razem z nowymi przypadkami.

### U2. Nagłówek kroku bez rozmowy w widoku całej *Rozmowy*

- **Goal:** Każde wejście w krok bez własnej rozmowy dostaje nagłówek między rozmową poprzedniego kroku a wyborami podjętymi w tym kroku.
- **Requirements:** R6, R7, R8, R9, R10, R11; AE4, AE5, AE6.
- **Dependencies:** U1 (AE4 używa obu zmian).
- **Files:** `src/tui/task_app/tabs/conversation.rs`, `tests/integration/tui_state.rs`, `tests/integration/task_app_support/mod.rs` (jeśli potrzebny helper).
- **Approach:**
  1. Zmienić `Builder::push_position`, aby zwracał wynik: dodano nagłówek, odczyt trwa, nic nie dodano. Wspólną budowę nagłówka (id, `System`, `with_clipped_head`) wydzielić do funkcji używanej przez nagłówek próby i nagłówek zastępczy.
  2. W gałęzi `Scope::Whole` z niepustym `visits` (pętla po wejściach w krok): po przejściu po pozycjach wejścia, jeśli żadna nie dodała nagłówka ani nie ustawiła stanu „odczyt trwa”, dodać nagłówek „── <state_id> #<visit> ──” (KTD3, KTD5), a dopiero potem wybory z `until(...)`. Kolejność: rozmowa poprzedniego kroku, nagłówek tego kroku, wybory w tym kroku, nagłówek następnego (R7, AE4).
  3. Nie zmieniać gałęzi `Scope::Step` ani `visits.is_empty()` oraz pętli „krok, o którego wejściu zdarzenia nie mówią” (pozycje bez `StateEntered` zostają bez zmian).
  4. Przy każdym przebiegu `rebuild_with_events` nagłówki są składane od nowa, więc ich liczba nie rośnie przy odświeżeniu; sprawdzić to testem.
- **Patterns to follow:** istniejące testy `a_choice_of_a_step_without_a_conversation_stands_between_its_neighbours` (zmieni się jego oczekiwana kolejność: dodaje nagłówek bramki), `each_visit_of_a_loop_keeps_its_own_choice`, `without_step_entries_the_choices_stay_at_the_end`.
- **Test scenarios:**
  - Covers AE4. Po `publish_learning #1` następuje `StateEntered` kroku dla człowieka, wybór użytkownika, `check_learning_feedback #1`: kolejność wpisów to wybór „published”, nagłówek kroku dla człowieka, wybór użytkownika, nagłówek `check_learning_feedback #1`.
  - Covers AE5. Krok z rozmową agenta: dokładnie jeden nagłówek, bez zastępczego.
  - Covers AE6. Ten sam krok w `Scope::Step`: brak nagłówków, jeden wpis użytkownika.
  - Krok z pozycją `Recorded`, której odczyt jeszcze trwa (`loading`): brak nagłówka zastępczego, po dołożeniu odczytu jeden nagłówek próby.
  - Dwie próby tego samego kroku wskazujące ten sam czat (`seen_attempts`): jeden nagłówek, bez zastępczego.
  - Krok skryptu bez wpisów: nagłówek „── <krok> #n ──”; pętla odwiedzająca krok dla człowieka dwa razy daje „#1” i „#2”.
  - Pozycja bez `StateEntered` w zdarzeniach: bez nagłówka zastępczego, jak dotąd.
  - Odświeżenie (`rebuild_with_events` dwa razy na tych samych danych) daje te same wpisy.
  - Zmieniony oczekiwany wynik istniejącego testu (bramka między sąsiadami) dopisać świadomie, z komentarzem w opisie PR, nie po cichu.
- **Verification:** `cargo test --test tui_state` zielone; w widoku *Rozmowy* z danych z dokumentu wymagań kolejność pokrywa się z AE4.

---

## System-Wide Impact

- Zmiana dotyczy tylko `src/tui/task_app/tabs/conversation.rs` i jego konsumentów (`Conversation::rebuild_with_events`, `progress.rs`). *Zdarzenia*, *Dyrygent*, historia i lista zadań nie używają `build_conversation`.
- Liczba i identyfikatory wpisów *Rozmowy* się zmieniają (nowe nagłówki, mniej wyborów); kursor i rozwinięcia w `Transcript` zachowują się po `TranscriptEntryId`, więc nowe id nagłówków muszą być stabilne między odświeżeniami (KTD5).
- Testy renderowania w `tests/integration/tui_render.rs` (moduł `workflow_choice_render`) budują własne zdarzenia; sprawdzić, czy któryś nie zakłada starej liczby wpisów.

## Risks & Dependencies

| Ryzyko | Mitygacja |
| --- | --- |
| Pominięcie `DecisionMade` od `human` gubi wybór bez drugiego wpisu (zatwierdzenie, wznowienie) | KTD2: pomijać tylko sparowane; niesparowane pokazać jako „użytkownik”. Test R5. |
| Fałszywe sparowanie przez tę samą etykietę w innym wejściu | Reset listy przy `StateEntered`; zużycie pozycji; test AE7 i test resetu. |
| Dwa nagłówki dla jednego kroku, gdy odczyt jest w toku lub próby dzielą czat | KTD3: wynik trójstanowy z `push_position`; testy loading i `seen_attempts`. |
| Nowe nagłówki dla kroków końcowych/skryptów zmieniają wygląd | Zgodne z R6 i założeniem z dokumentu; odnotować w PR (patrz Assumptions). |
| Etykieta „#n” kroku z próbami różni się od numeru wejścia | Dla kroków bez próby liczby się zgadzają (założenie z dokumentu); kroki z próbami nie dostają nagłówka zastępczego. |

## Verification Contract

| Gate | Evidence |
| --- | --- |
| Wybory | Testy U1 w `tests/integration/tui_state.rs` pokrywają AE1, AE2, AE3, AE7 i przypadki brzegowe. |
| Nagłówki | Testy U2 pokrywają AE4, AE5, AE6 oraz loading, `seen_attempts`, odświeżenie. |
| Regresja TUI | `cargo test --test tui_state` i `cargo test --test tui_render` przechodzą. |
| Jakość statyczna | `cargo fmt --check` i `cargo clippy --all-targets -- -D warnings` bez ostrzeżeń. |

---

## Definition of Done

- U1 i U2 spełniają swoje scenariusze testowe; AE1–AE7 mają odpowiadające testy.
- W *Rozmowie* żaden wybór człowieka nie jest przypisany agentowi i żaden nie występuje dwa razy.
- Krok dla człowieka ma nagłówek między rozmową poprzedniego kroku a swoim wyborem; *Przebieg* nie ma nagłówków.
- Silnik, format zdarzeń i inne zakładki bez zmian w diffie.
