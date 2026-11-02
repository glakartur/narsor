---
title: TUI Zużycie per krok w Przebiegu - Wymagania
type: feat
date: 2026-11-02
execution: code
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
---

# TUI: koszt i tokeny każdego kroku w widoku *Przebieg*

## Goal Capsule

- **Objective:** Operator widzi w *Przebiegu* przy każdym kroku workflow, ile kosztował i ile tokenów zużył (także subagenci), bez wchodzenia w szczegóły kroku.
- **Means:** poszerzony panel kroków z wyrównanymi kolumnami.
- **Product authority:** użytkownik (decyzje z dialogu 2026-11-02).
- **Open blockers:** brak.

## Product Contract

### Summary

Panel kroków (oś) w *Przebiegu* rośnie i pokazuje dla każdego kroku koszt, tokeny `↓in ↑out` oraz zużycie subagentów. Krok zajmuje jeden wiersz wyrównany jak tabela; gdy brakuje miejsca, subagenci przechodzą do drugiej linii kroku.

### Problem Frame

Zużycie jest dziś widoczne tylko jako suma w nagłówku zadania (`docs/plans/2026-11-02-001-feat-tui-zuzycie-w-naglowku-zadania-plan.md`). Oś (`src/tui/task_app/tabs/progress.rs`, `AXIS_MAX_WIDTH = 34`) pokazuje tylko status i etykietę, więc nie widać, który krok jest drogi. Dane są już w `Position.usage` (`total`, `subagents`, `subagent_count`, `cost_usd`); nie potrzeba zmian protokołu.

### Requirements

**Zawartość wiersza kroku**

- R1. Każdy krok pokazuje koszt w USD, tokeny całkowite jako `↓in ↑out` i, gdy są, zużycie subagentów: liczbę oraz ich `↓in ↑out`.
- R2. Tokeny `in`/`out` to pola `input`/`output` (jak w nagłówku zadania), skalowane `k`/`M` tym samym formatem; strzałki `↓` / `↑` zastępują słowa `in` / `out`.
- R3. Krok bez raportu zużycia (skrypt, krok człowieka, harness bez danych) ma puste pola; część, której harness nie podał, jest pusta, nie `—`.
- R4. Kroki z wieloma próbami lub wejściami pokazują zużycie swojego wejścia (`Position.usage`), bez zmiany definicji kroku.

**Układ**

- R5. Wartości są wyrównane w kolumnach: ta sama wielkość w tej samej kolumnie we wszystkich krokach (status, etykieta, koszt, in/out, subagenci).
- R6. Panel kroków jest znacząco szerszy niż dziś, ale nie przekracza 70% szerokości obszaru zakładki; reszta zostaje dla treści wybranego kroku.
- R7. Dopasowanie szerokości zależy od rzeczywistej szerokości kolumn z danymi, nie od stałej; bez raportów zużycia panel nie rośnie ponad dzisiejszy.
- R8. Gdy kolumn z subagentami nie da się zmieścić w jednym wierszu, krok dostaje drugą linię z subagentami; podstawowa linia (status, etykieta, koszt, in/out) zostaje w jednym wierszu. Dopiero gdy i to się nie mieści, etykieta jest obcinana z `…`.
- R9. Drugą linię mają tylko kroki, które mają co w niej pokazać.

**Zachowanie istniejące**

- R10. Zaznaczenie (klamra), przewijanie, śledzenie bieżącego kroku i klawisze osi działają bez zmian; druga linia kroku należy do tego samego wiersza zaznaczenia.

### Key Flows

- F1. Operator otwiera *Przebieg* zadania z kilkoma krokami: widzi tabelę kroków, w niej najdroższy krok i krok z subagentami bez otwierania szczegółów.
- F2. Terminal zwęża się: kolumny subagentów schodzą do drugiej linii kroku, a koszt i in/out zostają w pierwszej.

### Acceptance Examples

- AE1. Covers R1, R5. Kroki `review_api` ($0.42, ↓120k ↑8k, 2 subagenci ↓40k ↑3k) i `lint` ($0.03, ↓2.1k ↑0.4k): koszt i tokeny obu stoją w tych samych kolumnach.
- AE2. Covers R3. Krok-skrypt bez `usage` ma etykietę i status, a pola zużycia puste.
- AE3. Covers R6. Przy 200 kolumnach panel ma najwyżej 140; przy węższym oknie kolumny subagentów przechodzą do drugiej linii zgodnie z R8.
- AE4. Covers R7. Zadanie bez żadnego raportu zużycia ma oś szerokości jak dziś.

### Scope Boundaries

- Poza zakresem: cache (`cache_read` / `cache_write`), zmiany protokołu i serwera, zmiany w nagłówku zadania, CLI `narsor steps`, osobne kolumny dla poszczególnych subagentów.
- Poza zakresem: sumy w stopce tabeli (suma jest już w nagłówku).

## Planning Contract

Product Contract unchanged.

### Key Technical Decisions

- KTD1. Układ liczy czysta funkcja `AxisLayout` w `src/tui/task_app/tabs/progress.rs` z `positions` i szerokości obszaru zakładki; zwraca szerokość osi i wiersze (`Vec<Vec<Line>>`) dla `render_bracketed_list`, która już obsługuje elementy wielolinijkowe. Dzięki temu cała logika szerokości jest testowalna bez terminala (R5–R9).
- KTD2. Bez żadnego kroku z `usage` (R7) `AxisLayout` zwraca dzisiejszy wynik bez zmian: szerokość `(area/3).clamp(18, AXIS_MAX_WIDTH)` i `axis_line` w obecnej postaci. Istniejące testy osi zostają nietknięte.
- KTD3. Tryb tabelaryczny (co najmniej jeden krok ma wyświetlalną wartość: `cost_usd`, `total` lub niezerowych subagentów; `Some(PositionUsage::default())` to brak raportu, jak w `task_usage_text::sum`): kolumny to glif, etykieta, słowo statusu, koszt, `↓in ↑out`, subagenci (`n× ↓in ↑out`). Szerokość każdej kolumny to maksimum po krokach; liczby w `↓` i `↑` są wyrównane do prawej w własnych podkolumnach. Etykieta i status są wyrównane do lewej.
- KTD4. Szerokość docelowa osi to suma kolumn pierwszej linii (bez subagentów) lub pełnej, z górnym limitem `min(floor(0.70 * area.width), area.width - 1 - ceil(0.30 * area.width))` (separator liczy się do treści, więc przy 200 kolumnach oś ma najwyżej 139) i dolnym limitem dzisiejszej szerokości (R6, R7). Szerokość osi to `PREFIX_WIDTH + szerokość zawartości`; dopasowanie i limity liczy się na tej całkowitej szerokości.
- KTD5. Decyzja o drugiej linii jest globalna dla tabeli, nie per krok: jeśli pełny wiersz mieści się w limicie, wszystkie kroki mają subagentów w pierwszej linii; w przeciwnym razie wszystkie subagenci schodzą do drugiej linii (wcięcie pod etykietą), tylko dla kroków, które je mają (R8, R9). Kolumny pozostają wyrównane. Gdy nawet pierwsza linia nie mieści się, kolumna etykiety jest obcinana (`fit`, `…`), nie mniej niż 8 znaków; potem odpada słowo statusu (zostaje glif).
- KTD6. Formatowanie liczb reużywa `scaled` z `src/client/task_usage_text.rs` (jedno źródło skali `k`/`M`); w tym module dochodzą nowe funkcje dla komórek kroku, tekst nagłówka zadania zostaje bez zmian. Brakująca część to pusty napis (R3), nie `—`, więc nie reużywamy `line`.
- KTD7. Subagenci niezgłoszeni lub zerowi (`None`, `0 in / 0 out`) nie dają segmentu i nie dają drugiej linii, tak jak w nagłówku zadania.

### Układ (kierunkowo)

Szeroko: `● review_api  w toku  $0.42  ↓120k ↑8k  2× ↓40k ↑3k`. Wąsko: ta sama linia bez ostatniej kolumny, a pod nią, pod etykietą, `2× ↓40k ↑3k`.

## Implementation Units

### U1. Komórki zużycia kroku

- **Goal:** formatowanie kosztu, `↓in ↑out` i segmentu subagentów jednego kroku.
- **Requirements:** R1, R2, R3, R4; KTD6, KTD7.
- **Files:** `src/client/task_usage_text.rs` (funkcje + testy w module).
- **Approach:** funkcja przyjmująca `Option<&PositionUsage>` i zwracająca strukturę komórek (`cost`, `tokens` jako para liczb `in`/`out`, `subagents`: liczba i para liczb), z `None` dla części niepodanej; osobna funkcja składa napis z szerokościami kolumn. Pomijaj subagentów z zerowym `in + out`.
- **Test scenarios:** pełne dane (`$0.42`, `↓120k ↑8k`, `2× ↓40k ↑3k`); tylko koszt; tylko tokeny; `usage: None` daje puste komórki; subagenci zero/`None` bez segmentu; skala (`950`, `1.5k`, `2.4M`); `cache_*` nie wpływa na liczby.
- **Verification:** `cargo test -p narsor --lib task_usage_text`.

### U2. Układ osi w trybie tabeli

- **Goal:** `AxisLayout`: szerokość osi i wiersze (jedna lub dwie linie) zgodnie z KTD1–KTD5.
- **Requirements:** R5–R10; KTD1–KTD5.
- **Dependencies:** U1.
- **Files:** `src/tui/task_app/tabs/progress.rs`; testy układu w module oraz w `tests/integration/tui_render.rs` (obok `the_axis_has_a_row_per_step_and_brackets_the_selected_one`, helpery `running_app_with_usage` / `reported`).
- **Approach:** `render` zastępuje stałe `axis_width` i `axis_line` wywołaniem `AxisLayout::new(positions, area.width, styles, stale)`. Tryb bez zużycia deleguje do dotychczasowego `axis_line`. Druga linia dostaje to samo wcięcie co etykieta i rolę `Role::Done`. Zaznaczenie obejmuje oba wiersze (`render_bracketed_list` i tak traktuje element jako całość, R10). Treść wybranego kroku (po separatorze) dostaje ≥30% szerokości; test asercją na jej szerokość.
- **Test scenarios:** (a) kroki z danymi: koszt i tokeny w tych samych kolumnach w każdym wierszu (AE1); (b) krok bez `usage` ma etykietę i status, pola puste (AE2); (c) 200 kolumn: oś ≤ 140, 70% nie przekroczone; wąskie okno: subagenci w drugiej linii dla wszystkich kroków, które je mają, pierwsza linia nadal z kosztem i in/out (AE3); (d) zadanie bez żadnego `usage`, także z `Some(PositionUsage::default())`: szerokość i tekst osi jak dziś (AE4, istniejące testy przechodzą bez zmian); (e) bardzo wąskie okno: etykieta obcięta `…`, bez paniki; (f) zaznaczony krok z drugą linią: klamra obejmuje obie linie, `j`/`k` przesuwają o krok; (g) odświeżenie timeline nie zmienia zaznaczenia ani `top`.
- **Verification:** `cargo test -p narsor --test integration tui_render` oraz `tui_state` (zaznaczenie i przewijanie).

### U3. Dokumentacja użytkownika

- **Goal:** opis wiersza kroku w *Przebiegu*.
- **Requirements:** R1, R6, R8.
- **Dependencies:** U2.
- **Files:** `docs/user/tui.md` (akapit o *Przebiegu* w „The details”; szkic ekranu, jeśli pokazuje oś).
- **Approach:** dopisz, co pokazuje wiersz kroku, strzałki `↓`/`↑`, to, że subagenci schodzą do drugiej linii w wąskim oknie i że kroki bez raportu mają puste pola.
- **Test expectation:** none -- dokumentacja.

## Verification Contract

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets` (bez nowych ostrzeżeń)
- `cargo test --workspace` (w tym nowe testy z U1 i U2 oraz istniejące testy osi bez zmian)

## Definition of Done

- R1–R10 spełnione; AE1–AE4 pokryte testami U2.
- Brak zmian w protokole, serwerze, CLI i nagłówku zadania.
- Krok bez raportu zużycia nie zmienia dotychczasowego wyglądu osi w zadaniu bez żadnych raportów.
- Brak martwego kodu z porzuconych podejść; `docs/user/tui.md` zaktualizowany.

## Sources / Research

- `src/tui/task_app/tabs/progress.rs`: `axis_line`, `AXIS_MAX_WIDTH`, `render` (oś `(area.width / 3).clamp(18, 34)`).
- `src/tui/selection.rs`: `render_bracketed_list` przyjmuje `&[Vec<Line>]`, więc element może mieć kilka linii.
- `crates/narsor-protocol/src/api/timeline.rs`: `PositionUsage`, `TokenCounts`.
- `src/client/task_usage_text.rs`: `scaled`, `line`; wzorzec nagłówka zadania z `docs/plans/2026-11-02-001-feat-tui-zuzycie-w-naglowku-zadania-plan.md`.
