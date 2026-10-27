---
title: Agent bez --json-schema - Plan
type: feat
date: 2026-10-27
topic: agent-bez-json-schema
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Agent bez --json-schema - Plan

## Goal Capsule

- **Objective:** Agent `claude` pracuje w jednej sesji aż do odpowiedzi końcowej. Komentarz agenta nie kosztuje wznowienia sesji. Wznowienie następuje tylko przy błędzie formatu odpowiedzi końcowej (R7) i nie powtarza wykonanej pracy.
- **Means:** Krok agenta nie przekazuje `--json-schema`. Odpowiedź końcowa przechodzi do zwykłego tekstu (KTD1).
- **Product authority:** Artur (decyzje z dialogu: rezygnacja ze schematu, wznowienie przy błędzie formatu, usunięcie `progress`).
- **Stop conditions:** Test z prawdziwym `claude` bez schematu i z pracą w tle pokazuje, że sesja kończy się przedwcześnie. Wtedy U2 wstrzymaj i wróć do brainstormu.
- **Open blockers:** Brak.

---

## Product Contract

Product Contract preservation: restructured, no scope change. R1 dotyczy kroku agenta (conductor zachowuje własny schemat). R7 podaje istniejący limit: jedno wznowienie, potem conductor. Stare pytania o limit i o conductora zamknięto.

### Summary

Agent nie dostaje schematu `--json-schema`. Pracuje normalnie i pisze komentarze zwykłym tekstem. Komentarz nie kończy tury. Odpowiedź końcową (`choice` albo `awaiting_input`) podaje jako obiekt JSON w ostatniej wiadomości. Silnik sam sprawdza format. Odpowiedź `progress` znika.

### Problem Frame

Przy `--json-schema` wywołanie `StructuredOutput` kończy turę, także gdy niesie `progress`. Gdy nic nie działa w tle, `claude` wychodzi sam z kodem 0. Agent traci resztę zaplanowanej pracy. Silnik ratuje krok przez wznowienie sesji (`--resume`).

Eksperyment z prawdziwym CLI (`claude` 2.1.311, `haiku`, pusty katalog) potwierdził oba przypadki:

- Ze schematem: `progress`, potem `Bash echo AFTER > after.txt`. Proces wyszedł po ok. 8 s. Plik `after.txt` nie powstał.
- Bez schematu: komentarz tekstowy, potem to samo `Bash`, potem JSON w ostatniej wiadomości. Jedna sesja, plik powstał, JSON był w linii `result`.

Przed odpowiedzią `progress` ten błąd był niewidoczny. Każde `choice` kończyło krok, więc zamknięcie sesji nic nie psuło. Wznowienie kosztuje czas, a praca w tle agenta może się powtórzyć lub przepaść.

### Requirements

**Kontrakt wyjścia**

- R1. Krok agenta nie przekazuje `--json-schema` do `claude`.
- R2. Komentarz tekstowy agenta w trakcie pracy nie kończy tury ani kroku.
- R3. Odpowiedź końcowa to obiekt JSON w ostatniej wiadomości agenta: `choice`, albo `awaiting_input` z kompletnym pytaniem.
- R4. Odpowiedź `progress` znika z kontraktu, z promptu i z notatki o kontrakcie. Agent nie dostaje już polecenia, by wysyłać notatki.

**Kończenie kroku**

- R5. Krok kończy się od razu po poprawnej odpowiedzi końcowej (R3). Okres łaski i ubicie grupy procesów po odpowiedzi działają jak dziś.
- R6. Linia `result` bez poprawnej odpowiedzi końcowej nie kończy kroku, gdy proces nadal działa (praca w tle).
- R7. Gdy proces wyjdzie bez poprawnej odpowiedzi końcowej, silnik raz wznawia sesję i prosi o odpowiedź w poprawnym formacie. Gdy to nie da odpowiedzi, krok przechodzi do conductora.

**Zgodność**

- R8. Wznowienie sesji po notatce `progress` znika razem z `progress`.
- R9. Rozmowa i TUI pokazują komentarze agenta jako zwykły tekst agenta.
- R10. Starsze zapisy z `progress`, `agent_session_failed` i `output_reprompted` nadal się wczytują.

### Key Decisions

- **Rezygnacja z `--json-schema`.** Governs R1, R2. (session-settled: user-approved — chosen over keeping the schema and `progress`: `StructuredOutput` kończy turę, więc każda notatka kosztuje wznowienie sesji.)
- **Zła lub brakująca odpowiedź końcowa: wznowienie sesji z poprawą, potem conductor.** Governs R7. (session-settled: user-approved — chosen over "od razu do conductora" i "traktuj tekst jako progress": koszt wznowienia tylko przy błędzie formatu, bez pętli bez końca.)
- **`progress` usunięty w całości.** Governs R4, R8. (session-settled: user-approved — chosen over "zostaw jako opcjonalny": dwa sposoby na to samo to więcej kodu.)
- **Odrzucone: narzędzie lub polecenie narsora do zgłaszania odpowiedzi.** Cięższe i bez przewagi nad JSON w tekście.

### Key Flows

- F1. Praca z komentarzami i zadaniem w tle
  - **Trigger:** Agent zaczyna krok i uruchamia zadanie w tle.
  - **Steps:** Agent pisze komentarze tekstowe. Proces żyje, bo działa zadanie w tle. Linie `result` bez odpowiedzi końcowej nie kończą kroku. Agent wysyła JSON z `choice`. Silnik kończy krok.
  - **Outcome:** Jedna sesja, zero wznowień, praca w tle nie jest ubita przed odpowiedzią.
  - **Covered by:** R2, R5, R6

- F2. Agent kończy bez poprawnej odpowiedzi
  - **Trigger:** Proces wychodzi, a ostatnia wiadomość nie ma poprawnego JSON.
  - **Steps:** Silnik raz wznawia sesję z prośbą o odpowiedź w formacie. Agent odpowiada. Gdy odpowiedzi nadal brak, krok przechodzi do conductora.
  - **Outcome:** Błąd formatu kosztuje wznowienie, komentarz nie kosztuje nic.
  - **Covered by:** R3, R7

### Acceptance Examples

- AE1. **Covers R2, R5.** Gdy agent pisze komentarz, uruchamia `Bash`, a potem wysyła JSON z `choice`, krok kończy się w jednej sesji, a wynik `Bash` istnieje.
- AE2. **Covers R6.** Gdy w procesie działa zadanie w tle, a agent wysłał tekst bez JSON, krok trwa dalej.
- AE3. **Covers R7.** Gdy proces wyszedł z tekstem bez JSON, silnik wznawia sesję z prośbą o poprawny format.
- AE4. **Covers R7.** Gdy wznowienie nie daje poprawnej odpowiedzi, krok przechodzi do conductora.
- AE5. **Covers R10.** Zapis zadania `task-fdb9e1ab73af4fa496da23c1c4e11d1d` z `progress` nadal się wczytuje.

### Success Criteria

- Agent, który komentuje pracę i czeka na zadanie w tle, kończy krok w jednej sesji, bez wznowień.
- Nagrania prawdziwego `claude` bez schematu (z pracą w tle i bez niej) są w fixturach i testach.

### Scope Boundaries

**Deferred for later**

- Zabijanie pracy w tle po `choice` (okres łaski 15 s zostaje bez zmian).
- Limit czasu dla kroku bez `timeout_secs`.

**Poza zakresem**

- Conductor (`crates/narsor-engine/src/conductor/`). Ma własny schemat odpowiedzi i krótkie wywołania.
- Adaptery `opencode` i `codex`. Nie używają schematu: ich `HarnessRun` ma `structured_output: None`.

### Dependencies / Assumptions

- Założenie, niezweryfikowane: bez schematu, z pracą w tle i komentarzem tekstowym, `claude` nie kończy sesji przedwcześnie. Eksperyment objął tylko przypadek bez pracy w tle. U4 nagrywa ten przypadek, a U2 zależy od wyniku.
- Założenie: ostatnia wiadomość agenta trafia do tekstu linii `result` i zawiera JSON odpowiedzi.
- Tekstowy kontrakt wyjścia jest już w prompcie (`output_contract`). Odczyt odpowiedzi z tekstu istnieje (`extract_response`), a D60 opisuje go jako zapas.

### Sources / Research

- `docs/solutions/runtime-errors/progress-note-recorded-as-failed-session-2026-10-27.md` (tabela eksperymentów, otwarty punkt o rezygnacji ze schematu).
- `docs/brainstorms/2026-10-26-zamykanie-kroku-agenta-i-odpowiedz-progress-requirements.md` (R1, R5–R9).
- `ARCHITECTURE_DECISIONS.md`, D60.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **Krok agenta wywołuje harness bez `output_schema`.** Pole `HarnessRequest.output_schema` zostaje dla conductora (`crates/narsor-engine/src/conductor/invoke.rs`). Funkcja `output_schema` w `agent.rs` znika, bo jej jedynym wywołującym jest krok agenta. Governs R1.
- KTD2. **Odpowiedź końcowa to ostatni obiekt JSON w tekście z linii `result`.** Parser Claude Code czyta ten tekst tą samą regułą co `extract_response`. Do wczesnego zamknięcia kroku wymaga dodatkowo, by obiekt kończył wiadomość (kontrakt mówi „nic po nim"). Cytowany przykład JSON w środku wiadomości nie ubija procesu. Odczyt po wyjściu procesu zostaje łagodny, jak dziś. Governs R3, R5, R6.
- KTD3. **Linia `result` bez odpowiedzi nigdy nie czyści odpowiedzi zapisanej wcześniej.** To dzisiejsza reguła dla powrotu subagenta po odpowiedzi. Bez schematu odpowiedź nie ma osobnego nośnika, więc parser zapamiętuje sam obiekt odpowiedzi (w `structured_output`, które `read_response` czyta jako pierwsze). Ani ten obiekt, ani tekst użyty do odczytu nie są nadpisywane przez późniejszą linię bez odpowiedzi. Governs R5, R6.
- KTD4. **Wznowienie przy błędzie formatu to istniejące jedno wznowienie silnika (`reprompt`) i conductor.** Nie dodajemy stałej limitu ani nowej ścieżki. Wznowienie jest tylko do odczytu i prosi o sam format. Governs R7.
- KTD5. **Znika cała ścieżka `progress`:** `run_until_answered`, `PROGRESS_CONTINUATION`, `PROGRESS_ONLY_DETAIL`, `progress_session`, `mark_progress_only`, `is_progress_note` i gałąź `progress` w parserze. `PromptPurpose::Continuation` zostaje w `narsor-protocol`, bo stare zapisy go zawierają (R10). Governs R4, R8, R10.
- KTD6. **Notatka kontraktu Claude Code (`CONTRACT_NOTE`) traci opis `StructuredOutput` i `progress`.** Zostaje tylko to, co jest potrzebne agentowi, albo notatka znika. Testy nie sprawdzają treści promptu, tylko strukturę (zasada z pamięci projektu: brak asercji na frazy promptu). Governs R4.

### Assumptions

- Warunki wyjścia procesu `claude` bez schematu są takie jak w eksperymencie: po ostatniej turze bez wywołań narzędzi proces wychodzi sam.

### Risks & Dependencies

- Agent, który skończy turę tekstem bez JSON, kosztuje wznowienie tylko do odczytu. Jeśli nie skończył pracy, nie dostanie zgody na jej kontynuację. Zakładamy, że to rzadkie, i obserwujemy w zadaniach po wdrożeniu.
- Brak walidacji w CLI: literówka w kluczu `choice` to błąd formatu, nie ponowna próba modelu w tej samej turze.
- Kolejność: najpierw nagranie z pracą w tle (U4), potem U1–U3. Dzięki temu nieudane założenie nie marnuje pracy z U1 i U3.
- Krok bez `timeout_secs` może wisieć, gdy zadanie w tle się zawiesi, a agent zakończył turę tekstem bez JSON (R6 nie kończy kroku, gdy proces żyje). Limit czasu jest odroczony (Scope Boundaries). Ryzyko jest znane.
- Odpowiedź musi kończyć wiadomość (KTD2). Zdanie po JSON przy działającym zadaniu w tle nie zamknie kroku wcześnie. Po wyjściu procesu odczyt łagodny nadal działa.

---

## Implementation Units

### U1. Krok agenta bez schematu i bez `progress` w kontrakcie

- **Goal:** Krok agenta nie przekazuje schematu, a kontrakt nie wspomina o `progress`.
- **Requirements:** R1, R4. KTD1, KTD6.
- **Dependencies:** część nagraniowa U4 z pracą w tle (spike, wykonaj ją jako pierwszą). Jeśli nagranie pokazuje przedwczesne zamknięcie sesji, zatrzymaj plan (Stop conditions), zanim U1 usunie schemat.
- **Files:**
  - `crates/narsor-engine/src/engine/actions/agent.rs` (`run`, `output_schema`, `AWAITING_INPUT`/`PROGRESS`)
  - `crates/narsor-engine/src/engine/actions/harness/claude_code.rs` (`CONTRACT_NOTE`, `contract_note`, nagłówek modułu)
  - `crates/narsor-engine/src/conductor/prompt.rs` (doc `answer_schema` odsyła do `agent::output_schema`)
  - `tests/actions_agent.rs`
- **Approach:** W `run` przekaż `output_schema: None`. Usuń `output_schema` i stałą `PROGRESS`. Zostaw `AWAITING_INPUT`, bo czyta ją odczyt z tekstu. Skróć `CONTRACT_NOTE` zgodnie z KTD6. Przepisz doc-odnośniki do usuniętej funkcji (`claude_code.rs`, `conductor/prompt.rs`), tak by nie wskazywały jej i nie opisywały schematu kroku agenta.
- **Patterns to follow:** sposób, w jakim `codex.rs` i `opencode.rs` przechodzą bez `output_schema`.
- **Test scenarios:**
  - Wywołanie `claude` przez krok agenta nie zawiera argumentu `--json-schema`. Wywołanie conductora nadal go zawiera.
  - Kontrakt wyjścia nie zawiera opisu odpowiedzi `progress` (asercja strukturalna: brak statusu `progress` w parsowanych odpowiedziach, nie sprawdzanie fraz promptu).
- **Verification:** `cargo test -p narsor-engine`, `cargo test --test actions_agent`.

### U2. Odpowiedź końcowa z tekstu linii `result`

- **Goal:** Parser Claude Code wykrywa odpowiedź końcową w tekście i kończy krok od razu po niej.
- **Requirements:** R2, R3, R5, R6, R9. KTD2, KTD3.
- **Dependencies:** U1. Nagranie z U4 dla przypadku z pracą w tle.
- **Files:**
  - `crates/narsor-engine/src/engine/actions/harness/claude_code.rs` (`ClaudeCodeStreamParser::accumulate`, `final_answer`)
  - `crates/narsor-engine/src/engine/actions/agent.rs` (`final_answer_kind`, `read_response`, `extract_response`)
  - `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs` (`ResultLine.text`)
  - `tests/claude_code_classification.rs`, `tests/actions_agent.rs`
- **Approach:** W `accumulate` wywołaj odczyt odpowiedzi z `result.text` dla linii bez błędu. Gdy to poprawna odpowiedź i obiekt kończy tekst, ustaw `final_answer`. Zapamiętaj obiekt odpowiedzi w `structured_output` i nie nadpisuj go ani `final_answer` linią bez odpowiedzi (KTD3). Odpowiedź w bloku ```json z zamykającym fence'em też liczy się jako kończąca wiadomość: użyj `final_contract_start` (`agent.rs`). Ścieżka `structured_output` zostaje też dla conductora. Usuń gałąź `is_progress_note`. R9 jest spełniony przez istniejący element „tekst agenta" w rozmowie, a jego test jest w scenariuszach poniżej.
- **Patterns to follow:** istniejący odczyt `final_answer_kind` i test fake harnessu w `tests/actions_agent.rs`. Skopiuj kształt `a_progress_note_does_not_end_the_step` do nowego testu przed jego usunięciem w U3.
- **Test scenarios:**
  - Komentarz, wywołanie `Bash`, JSON z `choice` w ostatniej wiadomości: krok kończy się, jedna sesja (AE1).
  - Linia `result` z tekstem bez JSON, proces nadal żyje: krok nie kończy się (AE2).
  - Wiadomość z przykładem JSON w środku i tekstem po nim: wczesne zamknięcie nie następuje. Po wyjściu procesu odczyt łagodny znajduje odpowiedź, jeśli jest.
  - JSON z `awaiting_input` i kompletnym pytaniem kończy krok zgodnie z dzisiejszą regułą `KILL_AFTER_QUESTION`.
  - Odpowiedź w pierwszej linii `result`, proza w drugiej: po wyjściu procesu krok kończy się tą odpowiedzią, bez repromptu (KTD3).
  - JSON w bloku ```json na końcu wiadomości kończy krok.
  - Komentarz tekstowy agenta trafia do rozmowy jako element „tekst agenta" (R9).
- **Verification:** `cargo test --test claude_code_classification --test actions_agent`.

### U3. Usunięcie ścieżki `progress` z wykonawcy

- **Goal:** Brak wznowienia po notatce. Błąd formatu idzie istniejącą ścieżką reprompt, potem conductor.
- **Requirements:** R7, R8, R10. KTD4, KTD5.
- **Dependencies:** U2.
- **Files:**
  - `crates/narsor-engine/src/engine/actions/agent.rs` (`execute`, `run_until_answered`, `PROGRESS_CONTINUATION`, `PROGRESS_ONLY_DETAIL`, `progress_session`, `mark_progress_only`, `progress_continuation_config`)
  - `crates/narsor-protocol/src/chat.rs` (`PromptPurpose::Continuation`)
  - `crates/narsor-engine/src/persistence/conversation.rs`
  - `tests/actions_agent.rs`
- **Approach:** `continuation_config` zostaje: używa jej `runner.rs` przy odpowiedzi operatora na `awaiting_input`. Usuwamy tylko `progress_continuation_config`. `execute` wywołuje `run` bezpośrednio. Przenieś do `run` raportowanie limitu czasu kroku, jeśli było tylko w `run_until_answered`. Usuń gałąź `complete_session` dla notatki, tak by każdy przebieg bez odpowiedzi kończył się `fail_session` i `MISSING_OUTPUT_LABEL`. Wariant `Continuation` zostaje z komentarzem, że czytają go stare zapisy. Numeracja wpisów w `ConversationWriter::create` zostaje bez zmian.
- **Patterns to follow:** istniejąca ścieżka `Engine::reprompt` w `crates/narsor-engine/src/engine/runner.rs`.
- **Test scenarios:**
  - Proces wychodzi z tekstem bez JSON: `agent_session_failed`, `action_output_rejected`, `output_reprompted`, jedno wznowienie tylko do odczytu (AE3).
  - Wznowienie nie daje odpowiedzi: krok przechodzi do conductora (AE4).
  - Zapis z `progress` i `agent_session_failed` z zadania `task-fdb9…` wczytuje się bez błędu (AE5).
  - Testy `a_progress_note_*` (także `a_progress_note_does_not_end_the_step` i `a_progress_note_leaves_no_rejection_in_the_log`), `a_long_series_of_progress_notes_is_not_limited` i `the_continuation_after_a_progress_note_*` są usunięte, a `a_resumed_run_continues_the_numbering_of_the_conversation` i `the_timeout_of_the_step_counts_from_its_first_run` zachowane, jeśli nadal dotyczą istniejącego kodu.
- **Verification:** `cargo test --workspace`.

### U4. Nagrania `claude` bez schematu

- **Goal:** Fixtury i testy klasyfikacji opisują prawdziwy kształt strumienia bez schematu.
- **Requirements:** R2, R3, R6. Success Criteria.
- **Dependencies:** brak. Wykonaj jako pierwszą (spike): nagranie z pracą w tle sprawdza założenie, od którego zależą U1 i U2. Testy klasyfikacji z tej jednostki robisz po U2.
- **Files:**
  - `tests/fixtures/claude-code/` (nowe `noschema_*.ndjson`, usunięcie `schema_progress.ndjson` i `schema_progress_only.ndjson`)
  - `tests/fixtures/claude-code/README.md`
  - `tests/claude_code_classification.rs` (usuń testy czytające `schema_progress*`, razem z fixturami)
- **Approach:** Nagraj z prawdziwego `claude` dwa scenariusze: komentarz, `Bash`, JSON na końcu; to samo z zadaniem w tle (`sleep` w tle) i komentarzem w trakcie. Zapis w stylu istniejącego README (wersja CLI, flagi, wnioski). Przypadek „tekst bez JSON" pokrywa pierwsza linia `result` nagrania z pracą w tle i test fake harnessu z U3. Nagrania `schema_*` zostają dla conductora, poza `schema_progress*`. Usuń z nagrań nazwy wtyczek i serwerów użytkownika (jak robi to `--strict-mcp-config` w istniejących).
- **Test scenarios:**
  - Klasyfikacja linii `result` z JSON w tekście daje odpowiedź końcową.
  - Klasyfikacja linii `result` z samym tekstem nie daje odpowiedzi końcowej.
  - W nagraniu z pracą w tle: dwie linie `result`, odpowiedź końcowa dopiero w drugiej. Jeśli nagranie pokazuje przedwczesne zamknięcie sesji, zatrzymaj U2 (Stop conditions).
- **Verification:** `cargo test --test claude_code_classification`.

### U5. Dokumentacja decyzji

- **Goal:** Dokumenty opisują nowy kontrakt, a nie schemat.
- **Requirements:** R1, R4.
- **Dependencies:** U1–U4.
- **Files:**
  - `ARCHITECTURE_DECISIONS.md` (nowy wpis D69 zawężający D60: schemat tylko u conductora)
  - `workflows/README.md` (akapit o `contract_note` i `StructuredOutput`)
  - `docs/solutions/runtime-errors/progress-note-recorded-as-failed-session-2026-10-27.md` (punkt o rezygnacji ze schematu jest rozwiązany)
- **Approach:** Wpis w tabeli w formacie istniejących decyzji (status, uzasadnienie). Nie zmieniaj D60, tylko wskaż D69 jako zawężenie.
- **Test expectation:** none -- zmiana tylko w dokumentacji.
- **Verification:** przegląd różnic.

---

## Verification Contract

| Komenda | Kiedy |
|---|---|
| `cargo test --workspace` | po każdej jednostce, przed scaleniem. Tak jest zapisane w rozwiązaniu `docs/solutions/runtime-errors/progress-note-recorded-as-failed-session-2026-10-27.md` (po #39 `tests/actions_agent.rs` nie kompilował się na `main`). |
| `cargo test --test claude_code_classification --test actions_agent` | U2–U4 |
| Eksperyment z README rozwiązania, bez `--json-schema`, z zadaniem w tle | to samo co spike z U4, powtórzone raz po U2 na gotowym kodzie, na prawdziwym `claude` |

Testy workflow nie sprawdzają fraz promptu, tylko strukturę.

## Definition of Done

- Wszystkie R1–R10 i AE1–AE5 mają działający test lub opisany eksperyment.
- `cargo test --workspace` przechodzi.
- Krok agenta nie przekazuje `--json-schema`. Conductor nadal go przekazuje.
- W kodzie nie ma `PROGRESS`, `PROGRESS_CONTINUATION`, `run_until_answered` ani `progress_session`. `PromptPurpose::Continuation` został z komentarzem.
- Kod z porzuconych prób jest usunięty z różnicy.
