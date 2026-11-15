---
title: "feat: Liczba subagentów Claude Code w Przebiegu i w nagłówku taska (ikona 👥)"
type: feat
status: active
date: 2026-11-13
origin: docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md
---

# feat: Liczba subagentów Claude Code w Przebiegu i w nagłówku taska (ikona 👥)

## Summary

Decyzja użytkownika (wątki Q1, Q4): Claude Code nie podaje użytecznej liczby tokenów subagenta, więc plan **nie zbiera tokenów subagentów** i **nie zmienia liczb `✨`**. Dla Claude Code Przebieg i nagłówek pokazują tylko liczbę subagentów (`👥 N`), z `subagent_stats.spawned` z linii `result` (parser już ją czyta). Protokół i TUI nadal umieją pokazać tokeny (`↕`, `↑`/`↓`), gdy harness je poda (dziś GitHub Copilot, w przyszłości inne). TUI ma jedną funkcję z liczbą subagentów i jedną stałą z ikoną `👥`. Nagłówek pokazuje liczbę także bez tokenów. Format plików timeline się nie zmienia. Rozmiar: Lightweight/Standard (zero zmian w parserze, dwa widoki TUI, 3 jednostki).

---

## Problem Frame

Dziś parser Claude Code czyta tylko `subagent_stats` z linii `result`, a tam `tokens` jest zawsze `0`. Nagłówek taska nie bierze `subagent_stats`, więc dla Claude Code blok subagentów nie pojawia się wcale. Szczegóły: see origin: `docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md`.

---

## Wyniki badania danych harnessu (weryfikacja Z1–Z3)

Sprawdziłem lokalne zapisy sesji Claude Code (`~/.claude/projects/*/*.jsonl`): 11 wyników narzędzia `Agent` ze `status: completed` i (wg recenzji wykonalności) 238 wyników ze `status: async_launched`. Dane nie są z `stream-json`, ale obiekt `toolUseResult` w zapisie sesji to ten sam obiekt, który `stream-json` wysyła jako `tool_use_result` na linii `user` z blokiem `tool_result`. To trzeba potwierdzić nagraniem (U1).

Oznaczenia: F-… to fakty z tego badania; Q1–Q4 to wątki pytań na merge request (odpowiedzi użytkownika); P… to pytania w sekcji „Pytania otwarte dla użytkownika”.

- F-b. `totalTokens` w `toolUseResult` to rozmiar kontekstu na końcu pracy subagenta (plus ostatnie wyjście), nie suma zużycia.
- F-c. `toolUseResult` nie ma kosztu subagenta.

Wniosek (decyzja Q1): `totalTokens` to rozmiar kontekstu na końcu pracy subagenta, nie zużycie (F-b), a harness nie podaje kosztu (F-c). Nie pokazujemy takiej liczby jako tokenów. Parser Claude Code zostaje bez zmian. Znane fakty o `toolUseResult` (`totalTokens`, `usage` ostatniej wiadomości, subagent w tle `async_launched` bez tokenów) zostają dla przyszłej decyzji, gdy harness poda pełne zużycie.

- F-e. Linia `result` ma `modelUsage` (per model, z `costUSD`). Komentarz w `crates/narsor-engine/src/engine/timeline_file.rs` (`reading_from_element`) mówi, że `usage` Claude to zużycie całego uruchomienia, ale nikt tego nie sprawdził na nagraniu z subagentami. Z3 zostaje niepotwierdzone; sprawdza to U1 tylko po to, by opis grupy `✨` był zgodny z faktem (A7).

---

## Requirements

- R1–R6 (zbieranie tokenów subagentów w parserze) **wypadają** dla Claude Code (decyzja Q1). Liczba subagentów pochodzi z `subagent_stats.spawned` linii `result`, już czytanej przez parser.
- R7, R8. Wiersz kroku: `👥 N`, potem `↑`/`↓` albo `↕` (gdy harness poda tokeny). Dla Claude Code tylko `👥 N`. Koszt subagentów w wierszu kroku jest odroczony (A2): `PositionUsage` nie ma pola kosztu subagentów. (U2)
- R9, R10, R11, R12. Nagłówek: blok `👥` z sumą po krokach, także bez tokenów; skracanie zostaje. Koszt w bloku `👥` działa tylko przez istniejące pole `TaskUsage.subagent_cost_usd`, które `sum` dziś zawsze ustawia na `None` (A2). (U2, U3)
- R13. Jedna stała z ikoną dla nagłówka i Przebiegu. (U2)
- Odpowiedź Q1 zastępuje część origin: AE1, AE2 i kryterium sukcesu „operator widzi … ich tokeny” nie dotyczą Claude Code (brak tokenów i kosztu). Format AE1 (`👥 2 ↑80k ↓6k`) jest pokryty testem U2 na ręcznych danych. AE3 i AE4 są pokryte w U2/U3. AE5 (wznowienie) nie ma nowego testu: parser i `apply_usage` się nie zmieniają, a istniejące testy `apply_usage` pokrywają regułę prób.

---

## Assumptions

*Ten plan powstał bez pełnego potwierdzenia użytkownika. Poniższe punkty sprawdź przed implementacją.*

- A1. Claude Code: tylko liczba subagentów, bez tokenów (Q1, odpowiedź użytkownika: „przewidujemy w komunikatach, ale nie uzupełniamy, jeśli claude code ich nie zwraca”). Pola tokenów w protokole i TUI zostają dla harnessów, które je podają.
- A2. Koszt subagentów jest pusty (F-c, Q3 opcja 1). Plan nie dodaje pola kosztu, dopóki harness go nie poda.
- A6. Liczba subagentów w nagłówku i w Przebiegu (R11): `subagent_count`, gdy jest podane i większe od 0, w innym wypadku `subagent_stats.spawned`, gdy większe od 0. Jedna funkcja daje liczbę w obu widokach.
- Luki w numeracji (A3–A5, KTD2, KTD3, KTD6, U4) są celowe: te pozycje wypadły po odpowiedzi Q1, a pozostałe ID zostają bez zmian.
- A7. Decyzja Q2 opcja 2 jest **wycofana** przez odpowiedź Q1 (nie uzupełniamy tokenów subagentów). `✨` bez zmian (zgodnie z origin). Opis grupy `✨` się nie zmienia (Q5, odpowiedź użytkownika: „zmianę pokazywania wycofujemy … ograniczamy się do kolekcjonowania tych danych”). Plan tylko zbiera dane i zapisuje w README fixture, czy `usage` z `result` zawiera subagentów; nic w TUI się z tego nie zmienia.

---

## Key Technical Decisions

- KTD1. Parser Claude Code bez zmian: `subagent_stats` z `result` (`spawned`) już trafia do `RunResult`, `UsageReading` i `PositionUsage`. Pliki timeline się nie zmieniają.
- KTD4. Ikona i liczba subagentów mają jedno miejsce w kodzie: `src/client/task_usage_text.rs` (stała ikony i funkcja liczby). `step_cells`, `subagents_block` i Przebieg (`src/tui/task_app/tabs/progress.rs`) ich używają (R13).
- KTD5. Nagłówek sumuje liczbę subagentów po krokach tą samą funkcją co Przebieg (U2), nie surowe `subagent_count`. Dzięki temu suma w nagłówku jest równa sumie wierszy (kryterium sukcesu). `TaskUsage` ma już `subagent_count`; dostaje nowe pole na łączne tokeny subagentów bez podziału (suma `subagent_stats.tokens`), bo dziś `sum` nie czyta `subagent_stats` i `↕` z Copilota nie trafia do nagłówka. Nazwa pola to decyzja implementacji.
- KTD7. Blok `👥` w nagłówku: `↑`/`↓`, gdy suma `subagents` jest niepusta; w innym wypadku `↕` z nowego pola. Mieszanych kroków (jeden z podziałem, drugi tylko z `↕`) dziś nie ma, bo żaden harness nie wypełnia `subagents` (patrz Risks).

---

## Scope Boundaries

- Bez kosztu z cennika modeli (origin).
- Bez zmiany liczenia `✨` i `⚡` (origin).
- Bez listy subagentów i czasu pracy subagenta (origin), mimo że `totalDurationMs` jest dostępne.
- Codex i OpenCode bez zmian. GitHub Copilot dostaje tylko nową ikonę i nowy format liczby (`👥 N` zamiast `N×`) (origin).

### Deferred to Follow-Up Work

- Zbieranie tokenów subagentów Claude Code (`totalTokens`, `subagent_tokens`, subagenci w tle), gdy harness poda pełne zużycie.
- Legenda symboli zużycia w TUI (`✨`, `⚡`, `👥`).
- Pole kosztu subagentów w protokole, gdy harness je poda (A2).
- Widok zużycia subagentów podczas pracy kroku (przed linią `result`).

---

## Implementation Units

### U1. Sprawdzenie nagrania Claude Code z subagentami (tylko liczba)

**Goal:** Potwierdzić, że `result` ma `subagent_stats.spawned` dla subagentów (w tym w tle), i zapisać pełny testowy zrzut logów Claude Code z subagentami jako wzorzec na przyszłość (Q5). Zapisać też, czy `usage` z `result` zawiera subagentów (A7), bez zmiany TUI.

**Requirements:** R11; A7.

**Dependencies:** brak.

**Files:**
- Nowy: `tests/fixtures/claude-code/subagents.ndjson`
- Zmiana: `tests/fixtures/claude-code/README.md` (wiersz scenariusza i odpowiedzi)
- Test: `tests/integration/claude_code_classification.rs`

**Context:** dziś żadna fixture Claude Code nie ma `spawned > 0` (`subagent_denial.ndjson` jest napisana ręcznie). Nagranie spełnia kryterium sukcesu origin „fixture z prawdziwego zapisu z co najmniej dwoma subagentami”.

**Approach:** Nagrać `claude --print --output-format stream-json --verbose --strict-mcp-config` z promptem uruchamiającym co najmniej dwóch subagentów (jeden w tle) w pustym projekcie testowym. Zapisać w README: czy `result.subagent_stats.spawned` liczy wszystkich; czy `usage`/`modelUsage` z `result` zawierają subagentów. Zapisać cały zrzut bez skracania linii (w tym `tool_use_result` subagentów, `async_launched`, powiadomienia o końcu zadania, `usage`, `modelUsage`, `subagent_stats`), żeby przyszłe prace mogły się na nim oprzeć. Zamaskować treść (bez danych klientów i ścieżek użytkownika). README opisuje, które pola dotyczą subagentów i które z nich są dostępne (np. `totalTokens`), ale ich nie używamy w kodzie.

**Test scenarios:**
- Happy path: fixture `subagents.ndjson` → `RunResult.subagent_stats.spawned` jest równe liczbie uruchomionych subagentów (w tym w tle), `tokens == 0`. Test obok istniejących testów fixture w `tests/integration/claude_code_classification.rs`.
- Edge: subagent w tle — gdy `spawned` go nie liczy, test zapisuje faktyczną wartość, a ryzyko z sekcji Risks wraca do użytkownika.

**Verification:** README odpowiada na oba pytania i opisuje pola subagentów w zrzucie. Zmiany opisu grupy `✨` nie ma.

### U2. Wspólna ikona i liczba subagentów; Przebieg

**Goal:** Jedno miejsce dla ikony `👥` i liczby subagentów; wiersz kroku w Przebiegu pokazuje `👥 N`, koszt, `↑`/`↓` albo `↕`.

**Requirements:** R7, R8, R11, R13.

**Dependencies:** brak (równolegle z U1; dane testowe są ręczne).

**Files:**
- Zmiana: `src/client/task_usage_text.rs` (stała ikony, funkcja liczby subagentów kroku, `step_cells`)
- Zmiana: `src/tui/task_app/tabs/progress.rs` (kolumny subagentów, szerokość komórki z emoji)
- Test: `src/client/task_usage_text.rs` (moduł `tests`), `src/tui/task_app/tabs/progress.rs` (moduł `tests`), `tests/integration/tui_render.rs` (asercje `1×   ↑40k   ↓3k` i brak `×` w wąskim widoku — test Przebiegu; trzeba je zmienić)

**Approach:**
- Stała ikony (np. `SUBAGENTS_ICON = "👥"`) w `task_usage_text.rs`, publiczna dla TUI.
- Funkcja „liczba subagentów kroku” z `PositionUsage`: `subagent_count` > 0, w innym wypadku `subagent_stats.spawned` > 0, w innym wypadku brak (A6, R11). Uwaga: dziś `step_cells` robi odwrotnie (`subagent_stats` nadpisuje liczbę z `subagent_count`); po zmianie kolejność jest zgodna z R11.
- `step_cells.subagent_count` = `👥 N` zamiast `N×` (Z5). Liczba jest widoczna także bez tokenów (AE3). Dziś liczba z `subagent_count` jest widoczna tylko z tokenami — po zmianie warunek to tylko „liczba > 0”.
- `↕` z `subagent_stats.tokens`, gdy brak podziału `↑`/`↓` (bez zmiany logiki R8).
- Przebieg: `Column::width` w `progress.rs` liczy szerokość przez `detail::cells` (komórki terminala, emoji = 2 komórki), więc kod wyrównania nie wymaga zmiany. Test integracyjny to potwierdza. Wzorzec: ikony kroków `⚖️`/`💻` z planu `docs/plans/2026-11-12-002-feat-tui-ikony-profile-i-script-plan.md`.

**Patterns to follow:** istniejące `step_cells` i testy `the_subagent_stats_stand_in_the_first_row`, `the_subagents_stay_in_the_first_row_even_on_a_narrow_tab` w `progress.rs`.

**Test scenarios:**
- Covers AE3: `subagent_stats.spawned = 3`, `tokens = 0` → komórka `👥 3`, brak `↕`, brak `↑`/`↓`.
- Tokeny od harnessu (Copilot, ręczne dane): `spawned = 2`, `tokens = 86k` → `👥 2` i `↕86k`.
- Format AE1 (dziś bez producenta): `subagents` = (80k, 6k), `subagent_count = 2` → `👥 2 ↑80k ↓6k` (aktualizacja testu z `N×   ↑…   ↓…`).
- GitHub Copilot (R8): `subagent_stats` `spawned = 2`, `tokens = 7978`, bez podziału → `👥 2 ↕8k`.
- Edge: `subagent_count = Some(0)` i brak stats → brak komórek subagentów.
- Edge: `subagent_count = 5` i `spawned = 2` → liczba 5 (R11, A6); `subagent_count` brak i `spawned = 2` → liczba 2; `subagent_count = Some(0)` i `spawned = 2` → liczba 2.
- Integration: wiersz z `👥` w wąskiej zakładce zostaje w pierwszym wierszu i kolumny są wyrównane (szerokość 2 komórek).

**Verification:** W kodzie TUI nie ma już `×` dla subagentów ani innego literału `👥` poza stałą (R13).

### U3. Nagłówek taska: blok `👥` z sumą po krokach

**Goal:** Nagłówek pokazuje liczbę subagentów, koszt (gdy znany) i tokeny w bloku `👥`, także bez tokenów.

**Requirements:** R9, R10, R11, R12.

**Dependencies:** U2.

**Files:**
- Zmiana: `src/client/task_usage_text.rs` (`TaskUsage`, `sum`, `subagents_block`, `line`, komentarze)
- Zmiana: `src/tui/task_app/detail.rs` (komentarz modułu z przykładem `◈`; `usage_text` bez zmiany logiki skracania)
- Test: `src/client/task_usage_text.rs` (moduł `tests`), `tests/integration/tui_render.rs` (asercje nagłówka z `◈`)

**Approach:**
- `sum` dodaje liczbę z funkcji U2 dla każdego kroku (R11, KTD5) i sumę `subagent_stats.tokens` do nowego pola łącznych tokenów (KTD5).
- `sum` nie odrzuca wyniku, który ma tylko liczbę subagentów (R10); dziś filtr na końcu `sum` wymaga tokenów lub kosztu.
- `subagents_block`: `👥 N`, potem koszt (gdy znany), potem tokeny wg KTD7. Blok jest widoczny, gdy liczba > 0 albo są tokeny/koszt. Dziś komentarz i kod mówią odwrotnie („Subagents that used nothing and cost nothing are not a block, whatever their count”). Ten komentarz i komentarz modułu w `src/tui/task_app/detail.rs` (przykład z `◈`) trzeba zmienić.
- Istniejące testy sprzeczne z R10 trzeba zmienić, nie usunąć: w `subagents_alone_stand_in_brackets_and_a_task_with_nothing_has_no_line` przypadek `subagents = (0, 0)`, `subagent_count = 2` daje teraz `(👥 2)`, nie `None`. W `subagents_that_used_nothing_are_not_shown` przypadek `Some((0, 0))` ma `subagent_count = 1` (helper `used`), więc daje teraz `… ✨ $0.10 ↑1k ↓100 (👥 1)`; przypadek `None` zostaje bez bloku. Nazwę testu trzeba dopasować do nowego zachowania.
- `line`: bez zmian w kolejności grup; blok `👥` w nawiasie po `✨` albo sam `(👥 3)` (istniejąca gałąź `(true, Some)`).
- R12: `usage_text` w `detail.rs` już najpierw usuwa blok subagentów, potem ucina `…`. Test potwierdza to z nowym blokiem.

**Test scenarios:**
- Covers AE3: jeden krok `spawned = 3`, bez tokenów, `✨ $1.20 ↑120k ↓8k` → `… ✨ $1.20 ↑120k ↓8k (👥 3)`.
- Covers AE4: kroki z `spawned` 2 i 1 oraz `tokens` 50k i 36k → `(👥 3 ↕86k)`.
- Tokeny od harnessu: `✨ $1.20 ↑120k ↓8k (👥 2 ↕86k)`.
- Covers AE2 (tylko test `subagents_block` z ręcznie ustawionym `subagent_cost_usd`; żaden harness dziś go nie podaje): `(👥 2 $0.40 ↕86k)`.
- Podział tokenów (dziś bez producenta): `subagents` = (100, 20k), `subagent_count = 2` → `(👥 2 ↑100 ↓20k)` (aktualizacja testu z `◈`).
- GitHub Copilot: `subagent_stats` `spawned = 2`, `tokens = 7978` → `(👥 2 ↕8k)`.
- Edge (R10): tylko subagenci, bez `✨` → `(👥 3)`; `sum` zwraca `Some`.
- Edge (R10, R6): subagenci z liczbą i tokenami `(0, 0)` → `(👥 N)` bez `↑`/`↓`; krok bez liczby i bez tokenów → brak bloku.
- Edge (R12): wąski terminal → najpierw znika `(👥 …)`, potem `…`.
- Kryterium sukcesu: suma liczby i `↕` w nagłówku = suma wartości z `step_cells` wszystkich kroków (test na tych samych danych).

**Verification:** Brak `◈` w kodzie; komentarz `line` opisuje `👥`; nagłówek i Przebieg dają te same liczby.

---

## System-Wide Impact

- Protokół (`crates/narsor-protocol`): bez zmian. Parser Claude Code bez zmian. Stare pliki timeline czytają się bez zmian.
- TUI: zmiana ikony i formatu liczby (`👥 N` zamiast `N×`) także dla GitHub Copilot.

---

## Risks

- Ryzyko: `spawned` z `result` nie liczy subagentów w tle. Mitygacja: U1 to sprawdza; wtedy liczba będzie niższa i trzeba zdecydować osobno.
- Ryzyko: opis `✨` („liczby zawierają subagentów”) może być nieprawdziwy dla Claude Code. Mitygacja: świadomie nie zmieniamy opisu (Q5); U1 zapisuje w README, co pokazał zrzut.
- Ryzyko: przyszły harness poda podział `↑`/`↓` w jednych krokach i tylko `↕` w innych; wtedy nagłówek wg KTD7 pokaże tylko `↑`/`↓` i zgubi `↕`. Mitygacja: dziś żaden harness nie wypełnia `subagents`; decyzja przy dodaniu takiego harnessu.
- Ryzyko: zmiana kolejności źródła liczby w `step_cells` (A6) zmienia wynik dla kroku z oboma polami. Mitygacja: dziś żaden harness nie podaje obu; test U2 to pokrywa.

---

## Pytania otwarte dla użytkownika

Brak otwartych pytań. P2 rozstrzygnięte w Q5 (patrz niżej).

### Rozstrzygnięte

- P1 (Q1): tokeny są przewidziane w komunikatach, ale Claude Code ich nie uzupełnia; dla Claude Code tylko liczba.
- P3 (Q3 opcja 1): bez kosztu subagentów; AE2 nie dotyczy Claude Code.
- Q2 opcja 2 (dodanie tokenów subagentów do `✨`) jest wycofana odpowiedzią Q1.
- P2 (Q5): opis `✨` bez zmian; plan tylko zbiera dane, a U1 zapisuje pełny testowy zrzut logów Claude Code z subagentami jako wzorzec.

---

## Sources & References

- Origin: `docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md`
- Parser: `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code/subagents.rs`
- Wzorzec: `crates/narsor-engine/src/engine/actions/harness/github_copilot.rs`
- Odczyt zużycia: `crates/narsor-engine/src/engine/timeline_file.rs`, `crates/narsor-protocol/src/api/timeline.rs`
- TUI: `src/client/task_usage_text.rs`, `src/tui/task_app/tabs/progress.rs`, `src/tui/task_app/detail.rs`
- Fixture: `tests/fixtures/claude-code/README.md`
