---
title: "feat: Tokeny subagentów Claude Code w Przebiegu i w nagłówku taska (ikona 👥)"
type: feat
status: active
date: 2026-11-13
origin: docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md
---

# feat: Tokeny subagentów Claude Code w Przebiegu i w nagłówku taska (ikona 👥)

## Summary

Parser Claude Code zbiera liczbę tokenów, którą harness podaje dla każdego subagenta: z wyniku narzędzia `Agent`/`Task` (`tool_use_result`) dla subagenta na pierwszym planie i z powiadomienia o końcu zadania dla subagenta w tle. Parser liczy raz na `agentId` i zapisuje sumę w istniejącym polu `SubagentStats.tokens`. Ta liczba to rozmiar kontekstu na końcu pracy subagenta, nie pełne zużycie (F-b). To ten sam wzorzec, którego używa już GitHub Copilot. TUI ma jedną funkcję, która daje liczbę subagentów, i jedną stałą z ikoną `👥`. Przebieg i nagłówek taska używają obu. Nagłówek pokazuje liczbę subagentów także bez tokenów. Format plików timeline się nie zmienia w wariancie bez kosztu (A2). Jeśli U1 znajdzie koszt, pole kosztu wchodzi jako opcjonalne. Rozmiar: Standard (parser, odczyt zużycia, dwa widoki TUI, 4 jednostki).

---

## Problem Frame

Dziś parser Claude Code czyta tylko `subagent_stats` z linii `result`, a tam `tokens` jest zawsze `0`. Nagłówek taska nie bierze `subagent_stats`, więc dla Claude Code blok subagentów nie pojawia się wcale. Szczegóły: see origin: `docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md`.

---

## Wyniki badania danych harnessu (weryfikacja Z1–Z3)

Sprawdziłem lokalne zapisy sesji Claude Code (`~/.claude/projects/*/*.jsonl`): 11 wyników narzędzia `Agent` ze `status: completed` i (wg recenzji wykonalności) 238 wyników ze `status: async_launched`. Dane nie są z `stream-json`, ale obiekt `toolUseResult` w zapisie sesji to ten sam obiekt, który `stream-json` wysyła jako `tool_use_result` na linii `user` z blokiem `tool_result`. To trzeba potwierdzić nagraniem (U1).

Pola obiektu `toolUseResult` subagenta (bez treści):

- `status`, `agentId`, `agentType`, `resolvedModel`, `totalDurationMs`, `totalToolUseCount`, `toolStats`, `canContinueAgent`.
- `totalTokens` — w próbce `133031`.
- `usage` — obiekt w pisowni Anthropic (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`, `iterations[]`).

Fakty:

- F-a. `usage` to zużycie **ostatniej wiadomości** subagenta, nie całego subagenta. `iterations` ma jeden element. W próbce `input_tokens = 2`, `output_tokens = 2797`, `cache_read_input_tokens = 127459`.
- F-b. `totalTokens` = suma pól `usage` ostatniej wiadomości (`2 + 2773 + 127459 + 2797 = 133031`). To jest rozmiar kontekstu na końcu pracy subagenta plus ostatnie wyjście, **nie** suma tokenów wszystkich wywołań modelu.
- F-c. Obiekt nie ma pola kosztu. Z2 się nie potwierdza: harness nie podaje kosztu per subagent w tym miejscu.
- F-d. W lokalnych zapisach nie ma zdarzeń `system` z `subtype` `task_*`. Takie zdarzenia (np. `task_notification`) są tylko w `stream-json`. Fixture `noschema_background.ndjson` (w innym workspace) ma `task_notification` dla zadania `Bash` w tle, ale nie dla subagenta.
- F-f. Subagent w tle: `tool_use_result` ma `status: async_launched`, `isAsync: true`, `agentId` i **nie ma** `totalTokens`. Tokeny przychodzą później w powiadomieniu o końcu zadania (w zapisie sesji: tekst `task-notification` z `<usage><subagent_tokens>N</subagent_tokens>`; w `stream-json` prawdopodobnie linia `system/task_notification`). Subagent w tle to częsty przypadek. Nie wiem, czy `subagent_tokens` to ta sama wielkość co `totalTokens`.
- F-e. Linia `result` ma `modelUsage` (per model, z `costUSD`). Nie wiem, czy `usage` i `modelUsage` z `result` zawierają subagentów. Z3 zostaje niepotwierdzone.

Wniosek: dane z harnessu wystarczą do liczby subagentów i do jednej liczby tokenów per subagent (`totalTokens`). Nie wystarczą do rzetelnego podziału `↑`/`↓` ani do kosztu per subagent. Plan przyjmuje drogę R8 (`↕` z łączną liczbą) i zostawia decyzję o znaczeniu tej liczby użytkownikowi (patrz „Pytania otwarte dla użytkownika”).

---

## Requirements

- R1, R2, R3, R6. Parser czyta `tool_use_result` subagenta, liczy raz na `agentId`, subagent zagnieżdżony liczy się osobno, subagent bez danych liczy się tylko do liczby. (U1, U2)
- R4, R5. Suma trafia do `SubagentStats.tokens` w odczycie zużycia. Reguły wznowienia i prób są te same co dziś dla `subagent_stats`. (U2)
- R7, R8. Wiersz kroku: `👥 N`, potem koszt (gdy znany), `↑`/`↓` (gdy znane), albo `↕` z łączną liczbą. (U3, U4)
- R9, R10, R11, R12. Nagłówek: blok `👥` z sumą po krokach, także bez tokenów; liczba z `subagent_stats.spawned`; obecne skracanie zostaje. (U3, U4)
- R13. Jedna stała z ikoną dla nagłówka i Przebiegu. (U3)
- AE1, AE3, AE4, AE5. Pokryte w scenariuszach testów (oznaczenia „Covers AE…”). AE1 jest pokryte w wariancie A1 (`↕` zamiast `↑`/`↓`).
- AE2. Pokryte tylko wtedy, gdy U1 pokaże koszt i U2 go doda (A2). W obecnym wariancie AE2 nie jest realizowane (pytanie P3).

---

## Assumptions

*Ten plan powstał bez potwierdzenia przez użytkownika. Poniższe punkty to założenia agenta. Należy je sprawdzić przed implementacją.*

- A1. Dla Claude Code liczba tokenów subagenta to `totalTokens` z `tool_use_result`. Przebieg i nagłówek pokazują ją jako `↕` (droga R8). Plan nie bierze `↑`/`↓` z `usage`, bo `usage` to tylko ostatnia wiadomość (F-a) i dałby mylące małe liczby (np. `↑2`). Pytanie P1 pozwala to zmienić.
- A2. Koszt subagentów dla Claude Code jest pusty (F-c). Plan **nie** dodaje nowego pola kosztu do `UsageReading` ani `PositionUsage`, dopóki żaden harness go nie podaje (zasada „bez wartości zastępczych”). `TaskUsage.subagent_cost_usd` zostaje i dalej jest `None`. Jeśli nagranie U1 pokaże koszt, U2 dodaje pole `subagent_cost_usd` z `serde(default, skip_serializing_if)`, tak jak `conductor_cost_usd`.
- A3. Dwa źródła tokenów (R1, R2): (a) `tool_use_result` na linii `user` ze `status: completed` i `totalTokens` — subagent na pierwszym planie; (b) powiadomienie o końcu zadania (`system/task_notification` albo tekst `task-notification` z `subagent_tokens`) — subagent w tle, którego `tool_use_result` ma `status: async_launched` bez tokenów (F-f). Klucz deduplikacji to `agentId` (w powiadomieniu `task-id`), a gdy go nie ma — `tool_use_id` wywołania `Agent`/`Task`. Gdy oba źródła podają tokeny tego samego subagenta, wygrywa (a).
- A4. Subagent zagnieżdżony ma własny `tool_use_result` (z `parent_tool_use_id` subagenta-rodzica na linii). Na podstawie F-b `totalTokens` rodzica nie zawiera tokenów dziecka (dziecko wraca do rodzica tylko jako tekst wyniku). Dlatego każdy subagent jest liczony osobno, a suma nie dubluje (R3).
- A5. `stream-json` po `--resume` nie powtarza starych linii `tool_use_result`. Licznik w parserze jest per proces, tak jak `subagent_stats`, więc AE5 jest spełnione bez nowych reguł w `apply_usage`.
- A6. Liczba subagentów w nagłówku i w Przebiegu (R11, dosłownie): `subagent_count`, gdy jest podane i większe od 0, w innym wypadku `subagent_stats.spawned`, gdy większe od 0. Ta sama funkcja daje liczbę w obu widokach, więc suma w nagłówku jest równa sumie wierszy (kryterium sukcesu).
- A7. Decyzja Q2 (opcja 2): gdy U1 pokaże, że `usage` z linii `result` nie zawiera tokenów subagentów, tokeny subagentów (suma `totalTokens`/`subagent_tokens`) wchodzą do liczb `✨` (`↑`/`↓` nie da się z nich zrobić, więc dodajemy je do łącznej liczby tokenów `✨` w sposób ustalony po U1). Opis grupy `✨` zostaje „liczby zawierają subagentów” tylko wtedy, gdy U1 potwierdzi, że liczby je już zawierają; wtedy liczb nie zmieniamy. To zmiana zakresu względem origin (Scope Boundaries): wymaga U1 i ponownej recenzji.

---

## Key Technical Decisions

- KTD1. Wzorzec Copilot zamiast nowego pola. `github_copilot.rs` liczy `SubagentStats` w parserze i wkłada je w `ChatElement::RunResult.subagent_stats` z `tokens` z `totalTokens`. Claude Code robi to samo: parser trzyma licznik per proces i wpisuje `tokens` do `subagent_stats` z linii `result`. Dzięki temu `reading_from_element`, `UsageReading::absorb` i `apply_usage` działają bez zmian, a pliki timeline się nie zmieniają (odpowiedź na pytanie odroczone R4).
- KTD2. `subagent_stats` z `result` jest per proces („whole process”, komentarz w `classify.rs`). Tokeny subagentów z parsera też są per proces. Obie wartości mają ten sam zakres, więc reguła „późniejszy odczyt w próbie zastępuje, próby się sumują” daje poprawny wynik także przy kilku liniach `result` w jednym procesie (zadania w tle) (odpowiedź na pytanie odroczone R5).
- KTD3. Gdy linia `result` nie ma `subagent_stats`, ale parser widział subagentów, parser tworzy `SubagentStats` z `spawned` = liczba widzianych subagentów i `tokens` = suma. Gdy `result` ma `subagent_stats`, parser zmienia tylko `tokens`.
- KTD4. Ikona i liczba subagentów mają jedno miejsce w kodzie: `src/client/task_usage_text.rs` (stała ikony i funkcja liczby). `step_cells`, `subagents_block` i Przebieg (`src/tui/task_app/tabs/progress.rs`) ich używają (R13).
- KTD5. Nagłówek dostaje sumę `SubagentStats` po krokach (`spawned`, `tokens`). `TaskUsage` dostaje pole na łączne tokeny subagentów bez podziału (np. `subagent_total_tokens: Option<u64>`). Nazwa jest decyzją implementacji.
- KTD6. Nagłówek przy krokach z różnymi formami tokenów: `↕` w nagłówku = suma `subagent_stats.tokens` wszystkich kroków plus `input + output` z `subagents` kroków z podziałem. `↑`/`↓` w nagłówku tylko wtedy, gdy żaden krok nie ma samego `↕`. Dzięki temu nagłówek nie gubi tokenów żadnego kroku. Dziś żaden harness nie wypełnia `subagents`, więc w praktyce nagłówek pokazuje `↕`.

---

## Scope Boundaries

- Bez kosztu z cennika modeli (origin).
- Bez zmiany liczenia `✨` i `⚡` (origin).
- Bez listy subagentów i czasu pracy subagenta (origin), mimo że `totalDurationMs` jest dostępne.
- Codex i OpenCode bez zmian. GitHub Copilot dostaje tylko nową ikonę i nowy format liczby (`👥 N` zamiast `N×`) (origin).

### Deferred to Follow-Up Work

- Legenda symboli zużycia w TUI (`✨`, `⚡`, `👥`, `↕`) z opisem, że `↕` subagentów Claude Code to tokeny w kontekście na końcu pracy. TUI nie ma dziś legendy. Zależy od odpowiedzi na P1.
- Widok zużycia subagentów podczas pracy kroku (przed linią `result`). Dziś `subagent_stats` przychodzi tylko z `result`, więc `👥` pojawia się po końcu uruchomienia — tak samo jak koszt i tokeny `✨`.

- Dodanie pola kosztu subagentów do protokołu, gdy jakiś harness zacznie go podawać (A2).
- Korekta `✨` o tokeny subagentów jest w zakresie, ale warunkowo: tylko gdy U1 pokaże, że `usage` z `result` ich nie zawiera (decyzja Q2, A7). Sposób dodania ustala się po U1, bez podwójnego liczenia z `👥`.

---

## Implementation Units

### U1. Nagranie prawdziwego zapisu Claude Code z subagentami

**Goal:** Potwierdzić nazwy i znaczenie pól przed zmianą parsera (Z1–Z3, A3–A5).

**Requirements:** R1, R2, R3, R5; kryterium sukcesu „fixture z prawdziwego zapisu”.

**Dependencies:** brak.

**Files:**
- Nowy: `tests/fixtures/claude-code/subagents.ndjson`
- Nowy: `tests/fixtures/claude-code/subagents_resume.ndjson` (wymagany dla AE5)
- Zmiana: `tests/fixtures/claude-code/README.md` (wiersz w tabeli scenariuszy i odpowiedzi na pytania)

**Approach:**
- Nagrać `claude --print --output-format stream-json --verbose --strict-mcp-config` z promptem, który uruchamia co najmniej trzech subagentów: jednego na pierwszym planie, jednego w tle i jednego zagnieżdżonego (jeśli harness na to pozwala), w pustym projekcie testowym, tak jak w README fixture.
- Nagrać drugi przebieg z `--resume` tej sesji (AE5).
- Zapisać w README: czy `tool_use_result` jest na linii `user`; nazwy pól (`agentId`, `totalTokens`, `usage`); czy jest koszt; czy są linie `system` `task_*` dla subagentów i co mają w `usage`; gdzie w `stream-json` są tokeny subagenta w tle i czy `subagent_tokens` to ta sama wielkość co `totalTokens`; czy powiadomienie o końcu subagenta w tle przychodzi przed ostatnią linią `result` procesu; czy `totalTokens` rodzica zawiera dziecko; czy `--resume` powtarza stare linie; czy `usage`/`modelUsage`/`total_cost_usd` z `result` zawierają subagentów (porównanie z sumą `modelUsage`).
- Zamaskować treść (prompt i odpowiedzi nie mogą mieć danych klientów ani ścieżek użytkownika).

**Test expectation:** none — nagranie danych; testy są w U2.

**Verification:** README ma odpowiedzi na wszystkie pytania z „Approach”. Jeśli odpowiedzi przeczą A1–A5, plan trzeba poprawić przed U2.

### U2. Parser Claude Code: zużycie subagentów z `tool_use_result`

**Goal:** Parser liczy tokeny subagentów raz na subagenta i wpisuje je do `subagent_stats` linii `result`.

**Requirements:** R1, R2, R3, R4, R5, R6.

**Dependencies:** U1.

**Files:**
- Zmiana: `crates/narsor-engine/src/engine/actions/harness/claude_code/subagents.rs` (licznik obok `SubagentTracker` albo w nim)
- Zmiana: `crates/narsor-engine/src/engine/actions/harness/claude_code.rs` (`on_stdout_line` przekazuje linię do licznika i uzupełnia `RunResult`)
- Zmiana: `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs` (`subagent_stats`, `run_result`: zostawić `tokens` do uzupełnienia; zmienić komentarz)
- Zmiana: `crates/narsor-engine/src/engine/timeline_file.rs` (tylko komentarz w `reading_from_element` o `subagents`)
- Zmiana tylko gdy U1 pokaże koszt: `crates/narsor-protocol/src/api/timeline.rs` (`PositionUsage`), `crates/narsor-protocol/src/chat.rs` (`ChatElement::RunResult`) — pola opcjonalne z `serde(default)`
- Test: testy w `claude_code.rs` / `claude_code/subagents.rs` oraz test integracyjny na fixture z U1 (obok istniejących testów fixture Claude Code)

**Approach:**
- `SubagentTracker::observe` już widzi linie `assistant` z wywołaniem `Agent`/`Task`. Licznik czyta linie `user` z `tool_use_result`, gdy `tool_use_id` bloku `tool_result` to wywołanie subagenta. Klucz: `agentId`, zapasowo `tool_use_id` (A3).
- `tool_use_result` ze `status: async_launched`: licznik zapisuje subagenta z tokenami „brak”. Gdy przyjdzie powiadomienie o końcu tego zadania (format wg U1), licznik wpisuje jego tokeny pod tym samym kluczem (A3 b).
- `classify::run_result` tworzy `RunResult` w `Classifier::classify`. Uzupełnienie `subagent_stats.tokens` robić w `on_stdout_line` po `classify` (licznik jest w parserze, nie w klasyfikatorze).
- Licznik trzyma mapę klucz → `totalTokens` (albo „brak”). Drugi odczyt tego samego klucza nie dodaje drugi raz (R2). Wartość z późniejszego odczytu zastępuje wcześniejszą (subagent kontynuowany przez `SendMessage` może podać nowy wynik — do sprawdzenia w U1).
- Subagent bez `totalTokens` (zatrzymany, odrzucony, błąd) liczy się do liczby, ale nie do tokenów (R6). Gdy żaden subagent nie ma tokenów, `tokens` zostaje `0` — `SubagentStats.tokens` ma znaczenie „0 = harness nie podał” (komentarz w `crates/narsor-protocol/src/api/timeline.rs`), więc TUI nie pokazuje `↕0`.
- Na linii `result` parser ustawia `subagent_stats.tokens` = suma; bez `subagent_stats` z harnessu tworzy je wg KTD3.
- Licznik żyje tyle co parser (jeden proces). Nie resetować go między liniami `result` jednego procesu (KTD2).
- Jeśli U1 pokaże koszt per subagent: dodać `subagent_cost_usd` do `ChatElement::RunResult`, `UsageReading` i `PositionUsage` z `serde(default, skip_serializing_if = "Option::is_none")` i sumować jak `conductor_cost_usd` (A2). W innym wypadku nie dodawać pól.

**Patterns to follow:** `crates/narsor-engine/src/engine/actions/harness/github_copilot.rs` (licznik `subagents: SubagentStats`, `totalTokens` z `subagent.completed`, test `the_subagents_of_a_run_are_counted_on_its_result`).

**Test scenarios:**
- Happy path: fixture U1 z dwoma subagentami → `RunResult.subagent_stats.tokens` = suma `totalTokens` obu; `spawned` z `result`.
- Covers AE4 (część silnika): dwa odczyty w dwóch próbach (`apply_usage`) z `tokens` 10k i 5k → `PositionUsage.subagent_stats.tokens` = 15k (istniejąca reguła; test potwierdza, że nowe dane jej nie łamią).
- Happy path (subagent w tle): `tool_use_result` `async_launched`, potem powiadomienie z tokenami 20k, potem `result` → `tokens` zawiera 20k jeden raz.
- Edge: subagent w tle bez powiadomienia przed `result` → liczony w `spawned`, bez tokenów.
- Edge (R2): ta sama linia `tool_use_result` dwa razy (albo `tool_use_result` i `task_notification` tego samego subagenta) → tokeny liczone raz.
- Edge (R3): subagent zagnieżdżony z własnym `tool_use_result` → suma = rodzic + dziecko, bez dodatkowego dodania dziecka do rodzica.
- Edge (R6): subagent bez `totalTokens` (status `killed`) → `spawned` go liczy, `tokens` bez zmiany; wszyscy bez danych → `tokens == 0`.
- Edge (KTD2): dwie linie `result` w jednym procesie, subagent między nimi → druga linia ma sumę obu, pierwsza tylko pierwszego; `absorb` daje wartość drugiej.
- Covers AE5: fixture `--resume` (U1) → nowy proces nie liczy subagentów z pierwszego uruchomienia; suma po dwóch próbach = suma subagentów obu procesów bez dubli.
- Error: `tool_use_result` nie jest obiektem albo `totalTokens` nie jest liczbą → linia ignorowana dla tokenów, brak paniki.
- Integration: wynik narzędzia innego niż `Agent`/`Task` z polem `totalTokens` → nie liczony.

**Verification:** Testy parsera i `timeline_file` przechodzą; fixture U1 daje `tokens > 0`; zapisane wcześniej pliki timeline czytają się bez zmian.

### U3. Wspólna ikona i liczba subagentów; Przebieg

**Goal:** Jedno miejsce dla ikony `👥` i liczby subagentów; wiersz kroku w Przebiegu pokazuje `👥 N`, koszt, `↑`/`↓` albo `↕`.

**Requirements:** R7, R8, R11, R13.

**Dependencies:** brak (może iść równolegle z U2; dane testowe są ręczne).

**Files:**
- Zmiana: `src/client/task_usage_text.rs` (stała ikony, funkcja liczby subagentów kroku, `step_cells`)
- Zmiana: `src/tui/task_app/tabs/progress.rs` (kolumny subagentów, szerokość komórki z emoji)
- Test: `src/client/task_usage_text.rs` (moduł `tests`), `src/tui/task_app/tabs/progress.rs` (moduł `tests`), `tests/integration/tui_render.rs` (gdy test sprawdza tekst `N×`)

**Approach:**
- Stała ikony (np. `SUBAGENTS_ICON = "👥"`) w `task_usage_text.rs`, publiczna dla TUI.
- Funkcja „liczba subagentów kroku” z `PositionUsage`: `subagent_stats.spawned` > 0, w innym wypadku `subagent_count` > 0, w innym wypadku brak (A6).
- `step_cells.subagent_count` = `👥 N` zamiast `N×` (Z5). Liczba jest widoczna także bez tokenów (AE3). Dziś liczba z `subagent_count` jest widoczna tylko z tokenami — po zmianie warunek to tylko „liczba > 0”.
- `↕` z `subagent_stats.tokens`, gdy brak podziału `↑`/`↓` (bez zmiany logiki R8).
- Pole kosztu subagentów w `StepCells` dodać tylko wtedy, gdy U2 doda koszt (A2).
- Przebieg: sprawdzić, że wyrównanie kolumn liczy szerokość w komórkach terminala (emoji = 2 komórki). Wzorzec: ikony kroków `⚖️`/`💻` z planu `docs/plans/2026-11-12-002-feat-tui-ikony-profile-i-script-plan.md`.

**Patterns to follow:** istniejące `step_cells` i testy `the_subagent_stats_stand_in_the_first_row`, `the_subagents_stay_in_the_first_row_even_on_a_narrow_tab` w `progress.rs`.

**Test scenarios:**
- Covers AE3: `subagent_stats.spawned = 3`, `tokens = 0` → komórka `👥 3`, brak `↕`, brak `↑`/`↓`.
- Covers AE1 (wariant A1): `spawned = 2`, `tokens = 86k` → `👥 2` i `↕86k`.
- Podział tokenów (dziś bez producenta): `subagents` = (40k, 3k), `subagent_count = 2` → `👥 2 ↑40k ↓3k` (aktualizacja testu z `2×   ↑40k   ↓3k`).
- GitHub Copilot (R8): `subagent_stats` `spawned = 2`, `tokens = 7978`, bez podziału → `👥 2 ↕8k`.
- Edge: `subagent_count = Some(0)` i brak stats → brak komórek subagentów.
- Edge: `subagent_count = 5` i `spawned = 2` → liczba 5 (R11, A6); `subagent_count` brak i `spawned = 2` → liczba 2.
- Integration: wiersz z `👥` w wąskiej zakładce zostaje w pierwszym wierszu i kolumny są wyrównane (szerokość 2 komórek).

**Verification:** W kodzie TUI nie ma już `×` dla subagentów ani innego literału `👥` poza stałą (R13).

### U4. Nagłówek taska: blok `👥` z sumą po krokach

**Goal:** Nagłówek pokazuje liczbę subagentów, koszt (gdy znany) i tokeny w bloku `👥`, także bez tokenów.

**Requirements:** R9, R10, R11, R12.

**Dependencies:** U3.

**Files:**
- Zmiana: `src/client/task_usage_text.rs` (`TaskUsage`, `sum`, `subagents_block`, `line`, komentarze)
- Zmiana (gdy trzeba): `src/tui/task_app/detail.rs` (`usage_text` — bez zmiany logiki skracania)
- Test: `src/client/task_usage_text.rs` (moduł `tests`), `tests/integration/tui_render.rs`

**Approach:**
- `sum` dodaje liczbę z funkcji U3 dla każdego kroku (R11) i sumę `subagent_stats.tokens` do nowego pola łącznych tokenów (KTD5).
- `sum` nie odrzuca wyniku, który ma tylko liczbę subagentów (R10); dziś filtr na końcu `sum` wymaga tokenów lub kosztu.
- `subagents_block`: `👥 N`, potem koszt (gdy znany), potem tokeny wg KTD6. Blok jest widoczny, gdy liczba > 0 albo są tokeny/koszt.
- `line`: bez zmian w kolejności grup; blok `👥` w nawiasie po `✨` albo sam `(👥 3)` (istniejąca gałąź `(true, Some)`).
- R12: `usage_text` w `detail.rs` już najpierw usuwa blok subagentów, potem ucina `…`. Test potwierdza to z nowym blokiem.

**Test scenarios:**
- Covers AE3: jeden krok `spawned = 3`, bez tokenów, `✨ $1.20 ↑120k ↓8k` → `… ✨ $1.20 ↑120k ↓8k (👥 3)`.
- Covers AE4: kroki z `spawned` 2 i 1 oraz `tokens` 50k i 36k → `(👥 3 ↕86k)`.
- Covers AE1 (wariant A1): `✨ $1.20 ↑120k ↓8k (👥 2 ↕86k)`.
- Covers AE2 (tylko gdy U2 doda koszt): `(👥 2 $0.40 ↕86k)`.
- Podział tokenów (dziś bez producenta): `subagents` = (100, 20k), `subagent_count = 2` → `(👥 2 ↑100 ↓20k)` (aktualizacja testu z `◈`).
- GitHub Copilot: `subagent_stats` `spawned = 2`, `tokens = 7978` → `(👥 2 ↕8k)`.
- KTD6: krok A z podziałem (1k, 2k) i krok B z `tokens = 5k` → `↕8k`, bez `↑`/`↓`.
- Edge (R10): tylko subagenci, bez `✨` → `(👥 3)`; `sum` zwraca `Some`.
- Edge (R12): wąski terminal → najpierw znika `(👥 …)`, potem `…`.
- Kryterium sukcesu: suma liczby i `↕` w nagłówku = suma wartości z `step_cells` wszystkich kroków (test na tych samych danych).

**Verification:** Brak `◈` w kodzie; komentarz `line` opisuje `👥`; nagłówek i Przebieg dają te same liczby.

---

## System-Wide Impact

- Protokół (`crates/narsor-protocol`): bez zmian w wariancie bez kosztu. Z kosztem: nowe pola opcjonalne z `serde(default)`, stare pliki timeline i stare klienty działają.
- Panel webowy lub inne klienty, które czytają `PositionUsage.subagent_stats.tokens`: dostaną wartość > 0 dla Claude Code. Brak znanych odbiorców poza TUI (do sprawdzenia `rg subagent_stats` w repo przy U2).
- `narsor task read` (`src/cli/commands/task_read.rs`) nie pokazuje zużycia subagentów — bez zmian.

---

## Risks

- Ryzyko: `totalTokens` to rozmiar kontekstu końcowego, nie zużycie (F-b). Operator może czytać `↕` jako „ile tokenów zużył subagent”. Mitygacja: pytanie P1; ewentualny opis w pomocy TUI.
- Ryzyko: format `tool_use_result` zmieni się w nowej wersji CLI. Mitygacja: parser ignoruje nieznany kształt (scenariusz „Error” w U2); README fixture zapisuje wersję CLI.
- Ryzyko: powiadomienie o końcu subagenta w tle przychodzi po ostatniej linii `result` procesu. Wtedy jego tokeny nie trafią do odczytu. Mitygacja: U1 to sprawdza; gdy tak jest, parser musi oddać odczyt także po `result` (decyzja w U2).
- Ryzyko: `stream-json` nie ma `tool_use_result` na linii `user`. Wtedy źródłem jest `system/task_notification`; U1 to rozstrzyga przed U2.

---

## Pytania otwarte dla użytkownika

- P1 (sekcja „Assumptions” A1, R7/AE1). Claude Code nie podaje sumy `↑`/`↓` per subagent. `totalTokens` to rozmiar kontekstu na końcu pracy subagenta plus ostatnie wyjście. Co pokazać?
  - (zalecane) `↕` z sumą `totalTokens` / `subagent_tokens` (jak GitHub Copilot, R8). Legenda z opisem tej liczby — osobne zadanie (Deferred to Follow-Up Work).
  - `↑`/`↓` z `usage` ostatniej wiadomości (dokładne pola, ale bardzo małe i mylące liczby).
  - Tylko liczba subagentów, bez tokenów, dopóki harness nie poda pełnego zużycia.
- P2 — ROZSTRZYGNIĘTE opcją 2 (wątek Q2, A7). (sekcja „Assumptions” A7, Z3). Jeśli U1 pokaże, że `usage` z linii `result` nie zawiera tokenów subagentów, co zrobić z opisem grupy `✨` („liczby zawierają subagentów”)?
  - (zalecane) Zmienić tylko opis: `✨` to główny agent, subagenci są w `👥`. Liczby `✨` bez zmian (zgodnie z Scope Boundaries).
  - Dodać tokeny subagentów do `✨` (zmiana liczenia `✨`, poza obecnym zakresem).
- P3 (sekcja „Assumptions” A2, Z2, AE2). Claude Code nie podaje kosztu per subagent w `tool_use_result`. Czy to akceptowalne, że AE2 nie jest realizowane dla Claude Code?
  - (zalecane) Tak: blok `👥` bez kosztu; pole kosztu w protokole dopiero, gdy harness je poda.
  - Nie: wyliczyć koszt subagentów z `modelUsage.costUSD` linii `result` (tylko gdy subagenci używają innego modelu niż główny agent; w innym wypadku niemożliwe bez cennika, co origin wyklucza).

---

## Sources & References

- Origin: `docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md`
- Parser: `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code/subagents.rs`
- Wzorzec: `crates/narsor-engine/src/engine/actions/harness/github_copilot.rs`
- Odczyt zużycia: `crates/narsor-engine/src/engine/timeline_file.rs`, `crates/narsor-protocol/src/api/timeline.rs`
- TUI: `src/client/task_usage_text.rs`, `src/tui/task_app/tabs/progress.rs`, `src/tui/task_app/detail.rs`
- Fixture: `tests/fixtures/claude-code/README.md`
