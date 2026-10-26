---
date: 2026-10-26
topic: zamykanie-kroku-agenta-i-odpowiedz-progress
---

# Zamykanie kroku agenta po odpowiedzi końcowej i odpowiedź `progress`

## Summary

Krok `agent` kończy się dziś dopiero, gdy proces `claude` wyjdzie. Agent może zostawić w procesie zadanie w tle, np. pętlę `until … sleep` w `Bash`. Wtedy krok wisi do limitu czasu mimo gotowej odpowiedzi. Zmiana ma trzy części: (1) krok kończy się od razu po odpowiedzi końcowej, a runner zabija drzewo procesów agenta, (2) agent dostaje niefinalną odpowiedź `progress` na komentarze i wyniki częściowe, (3) lista zablokowanych narzędzi obejmuje `Monitor`.

---

## Problem Frame

Zaobserwowano w tasku `task-6aa43b4de1724d0ba575fe9113837462`, krok `doc_review #2`:

- Agent zwrócił `StructuredOutput choice: reviewed` o 11:15:39. Krok trwał dalej bez nowych wpisów do limitu 1800 s.
- Proces `claude` żył dalej. Trzymała go pętla `bash -c 'until … sleep 5'` z wcześniejszej części kroku. Pętla czekała na `"stop_reason":"end_turn"` w plikach subagentów, a tych tam nie ma. Subagenci kończą wywołaniem narzędzia z raportem.
- Runner czeka na `tokio::join!(stdout, stderr, child.wait())` w `spawn_and_wait` (`crates/narsor-engine/src/engine/actions/agent.rs`). Nic nie kończy procesu po odpowiedzi końcowej.
- `--disallowedTools ScheduleWakeup CronCreate` blokuje tylko dwa narzędzia. Komentarz w `claude_code.rs` opisuje ten sam problem (20 minut czekania) naprawiony tylko dla nich. `Bash` z pętlą i `Monitor` zostają otwarte.

Drugi problem: `claude` po prawie każdej odpowiedzi tekstowej prosi agenta o odpowiedź przez `StructuredOutput`. Prompt nie daje agentowi legalnego sposobu, by powiedzieć „jeszcze nie skończyłem”. Wywołanie `StructuredOutput` nie kończy sesji `claude`, więc agent może je wysłać wiele razy. Dziś każdy obiekt bez `choice` i bez `awaiting_input` jest „nieużytecznym wynikiem” (`no_label_error`).

---

## Actors

- A1. Agent: pracuje w procesie `claude`, odpowiada przez `StructuredOutput`.
- A2. Runner/silnik: uruchamia proces, czyta strumień, zamyka krok.
- A3. Operator: czyta rozmowę, widzi komentarze agenta.

---

## Key Flows

- F1. Odpowiedź końcowa z zadaniem w tle
  - **Trigger:** Agent wysyła `StructuredOutput` z `choice`. W procesie działa pętla lub inne zadanie w tle.
  - **Steps:** Silnik rozpoznaje odpowiedź końcową. Czeka krótki okres łaski. Zabija całą grupę procesów agenta. Zamyka krok z odpowiedzią.
  - **Outcome:** Krok kończy się w ciągu okresu łaski, nie po limicie czasu.
  - **Covered by:** R1, R2, R3, R4

- F2. Komentarz lub wynik częściowy
  - **Trigger:** `claude` prosi agenta o `StructuredOutput` po odpowiedzi tekstowej. Agent ma jeszcze pracę (np. czeka na subagenta).
  - **Steps:** Agent wysyła `{"status":"progress","message":"…"}`. Silnik pokazuje `message` w rozmowie. Krok trwa dalej. Agent kończy pracę i wysyła odpowiedź z `choice`.
  - **Covered by:** R5, R6, R7, R8

- F3. Tura zakończona samym `progress`
  - **Trigger:** Ostatni `StructuredOutput` w sesji to `progress`, proces zakończył się lub nie ma już pracy w toku.
  - **Steps:** Silnik wznawia sesję tą samą ścieżką, co dla odpowiedzi bez użytecznego wyniku. Agent dostaje polecenie wysłania odpowiedzi końcowej.
  - **Covered by:** R9

---

## Requirements

**Zamykanie kroku**

- R1. Za odpowiedź końcową uznaje się linię `result`, której `structured_output` zawiera string `choice`, albo `status: awaiting_input` z kompletnym pytaniem. Linia `result` bez takiego obiektu nie kończy kroku. `claude` emituje kilka linii `result` w jednej sesji.
- R2. Po odpowiedzi końcowej silnik czeka okres łaski (stała, domyślnie 5 s), aby proces zdążył wyjść sam i dopisać sesję. Potem kończy proces.
- R3. Silnik uruchamia proces agenta we własnej grupie procesów. Zakończenie obejmuje całą grupę: najpierw SIGTERM, po krótkim czasie SIGKILL. Zabijany jest też Playwright MCP (`npm exec`, `node`) i każdy `bash` z pętlą. `kill_on_drop` zabija dziś tylko sam `claude`.
- R4. Krok zamknięty w ten sposób nie jest błędem. Silnik zapisuje w rozmowie wpis narsora, że proces zakończono po odpowiedzi końcowej, z numerem grupy procesów. Kod wyjścia wymuszonego zakończenia nie zmienia wyniku kroku.

**Odpowiedź `progress`**

- R5. Schemat `--json-schema` (`output_schema`) dopuszcza `status: progress` obok `awaiting_input`, z polem `message` (string). Schemat pozostaje płaskim obiektem, bez `anyOf`/`oneOf`.
- R6. Kontrakt w prompcie (`output_contract` i `CONTRACT_NOTE`) opisuje `progress`: służy do komentarzy, informacji o czekaniu i wyników częściowych. Odpowiedź z `choice` agent wysyła dokładnie raz, gdy wszystkie subagenty oddały wyniki. Prompt ogranicza użycie, np. do jednego komentarza na zakończony etap.
- R7. Wywołanie `progress` pojawia się w rozmowie jako wpis z treścią `message` (rozszerzenie `structured_output_summary` w `tools.rs`). Nie kończy kroku i nie zmienia stanu workflow.
- R8. Parser strumienia zapamiętuje ostatni `structured_output`, który jest odpowiedzią końcową (R1). `progress` go nie zastępuje.
- R9. Gdy sesja kończy się bez odpowiedzi końcowej, a ostatni `structured_output` to `progress`, silnik używa istniejącego wznowienia (`reprompt_config`, `continuation_config` w `agent.rs`). Wznowienie liczy się do istniejącego limitu ponowień.

**Narzędzia**

- R10. `DEFERRING_TOOLS` w `claude_code.rs` zawiera `Monitor`. Komentarz o przyczynie (`claude_code.rs`, sekcja „Why that shape”) opisuje też zadania w tle z `Bash` i wskazuje R1–R4 jako zabezpieczenie.

---

## Success Criteria

- Krok `agent` z odpowiedzią końcową i pętlą w tle kończy się najwyżej okres łaski + czas zabicia po odpowiedzi. Po kroku nie zostaje żaden proces z jego grupy (w tym Playwright MCP).
- Agent czekający na subagentów może wysłać `progress`, nie kończąc kroku. Komentarz widać w rozmowie.
- Odpowiedź końcowa po wcześniejszych `progress` jest czytana poprawnie.
- Istniejące workflow działają bez zmian w definicjach.

---

## Scope Boundaries

- Nie zmieniamy definicji workflow (`workflows/`) ani ich treści promptów poza wspólnym kontraktem z R6.
- Nie dodajemy limitu liczby `progress` w silniku. Ogranicza go prompt (R6). Limit w silniku jest do rozważenia, gdy pojawi się nadużycie.
- Nie zmieniamy limitów czasu kroków (`timeout_secs`).
- Inne harnessy niż `claude-code` nie dostają `progress` przez `--json-schema`. Dla nich zmiana dotyczy tylko tekstu kontraktu, jeśli adapter go używa (do sprawdzenia w planie).
- Nie blokujemy `Bash` ani nie próbujemy wykrywać pętli w poleceniach agenta.

---

## Dependencies / Assumptions

- Zakładamy zachowanie `claude` 2.1.x: `StructuredOutput` nie kończy sesji, a po turze tekstowej CLI prosi o `StructuredOutput`. Dowód: w obserwowanym tasku proces żył po `result` z 11:15:39. Zachowanie CLI trzeba potwierdzić fixturą (`tests/fixtures/claude-code/`).
- Zakładamy, że schemat z `status: progress` jest przyjmowany przez API tak samo jak dziś z `awaiting_input` (płaski obiekt). Do sprawdzenia fixturą.
- Nie sprawdzono, czy brak zabijania po `result` jest świadomą decyzją (np. zapis sesji potrzebnej do `--resume`). Okres łaski z R2 ma to pokryć. Do potwierdzenia testem wznowienia po zabiciu.

---

## Open Questions

- Czy okres łaski (R2) ma być stałą, czy parametrem workflow (`parameters`)?
- Czy `progress` ma trafiać do panelu TUI osobnym elementem (inna ikona), czy jako zwykły wpis agenta?
- Czy po zabiciu grupy procesów (R3) sesja `claude` nadal da się wznowić przez `--resume`? Dotyczy kroków z `awaiting_input` i `history: true`.

---

## Key Files

- `crates/narsor-engine/src/engine/actions/agent.rs`: `output_contract` (~100), `output_schema` (201), `AWAITING_INPUT` (230), `reprompt_config` (256), `continuation_config` (310), `spawn_and_wait` (1055), `read_response` (1430), `structured_response` (1445), `is_answer_object` (1512), `response_from_object` (1524).
- `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`: `DEFERRING_TOOLS` (91), argumenty (~150-200), `CONTRACT_NOTE` (225), parser strumienia.
- `crates/narsor-engine/src/engine/actions/harness/claude_code/tools.rs`: `structured_output_summary`.
- `tests/fixtures/claude-code/`: fixtury strumienia `claude`.
