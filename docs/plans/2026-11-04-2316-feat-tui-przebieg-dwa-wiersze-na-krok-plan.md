---
title: TUI Przebieg - dwa wiersze na krok i metryki subagentów - Wymagania
type: feat
date: 2026-11-04
execution: code
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
---

# TUI: lista kroków w *Przebiegu* w dwóch wierszach, z metrykami subagentów

## Goal Capsule

- **Objective:** Każdy krok na liście kroków *Przebiegu* zajmuje dokładnie 2 wiersze i pokazuje w pierwszym także liczbę subagentów oraz zużyte przez nich tokeny.
- **Means:** przebudowa osi kroków (`src/tui/task_app/tabs/progress.rs`) i komórek zużycia (`src/client/task_usage_text.rs`).
- **Product authority:** użytkownik (dialog 2026-11-04).
- **Open blockers:** brak.

## Product Contract

### Summary

Oś kroków w *Przebiegu* ma stały układ: wiersz 1 to status, etykieta, koszt, tokeny kroku i subagenci; wiersz 2 to start, czas trwania, a po większym odstępie model, effort i harness. Panel może zająć do 70% szerokości zakładki. Metryki subagentów pojawiają się, bo biorą się z danych, które harnessy faktycznie podają.

### Problem Frame

Po `#68` krok ma na osi 3–4 wiersze (główny, czasy, agent z licznikami `sub n ✓c ↕t`). Kolumny subagentów w wierszu głównym czytają `Position.usage.subagents` i `subagent_count`, których żaden harness nie wypełnia (Claude i Copilot podają tylko `subagent_stats`: liczba uruchomionych i suma tokenów razem). Dlatego subagenci nie pojawiają się tam nigdy, a ich dane są tylko w osobnym wierszu.

### Requirements

**Układ kroku**

- R1. Każdy krok ma dokładnie 2 wiersze; wiersz 2 zawsze należy do tego samego elementu zaznaczenia co wiersz 1.
- R2. Wiersz 1: glif i status, etykieta, koszt, tokeny kroku `↓in ↑out` oraz subagenci (R4–R6), wyrównane w kolumnach jak dziś.
- R3. Wiersz 2: wcięcie 3 znaki, data startu kroku, czas trwania (lub dotychczasowy czas biegnący dla kroku w toku), większy odstęp, następnie model, effort i harness, każde w swojej kolumnie. Brakujące wartości są puste, bez `—`. Data końca kroku z osi znika.

**Subagenci w wierszu 1**

- R4. Liczba subagentów to `subagent_stats.spawned`, a gdy go brak, `subagent_count`; zero lub brak daje puste pole.
- R5. Gdy harness podał tokeny subagentów rozdzielnie (`subagents`), pokazujemy `↓in ↑out`. Gdy podał tylko sumę (`subagent_stats.tokens`), pokazujemy `↕suma`. Gdy podał obie, pierwszeństwo ma wersja rozdzielna. Skala `k`/`M` jak w pozostałych liczbach.
- R6. Gdy znana jest tylko liczba (suma 0), pokazujemy samą liczbę.
- R7. Liczników `✓/✗/†/⊘` na osi nie ma; zostają w danych, bez nowego miejsca ich prezentacji.

**Szerokość**

- R8. Panel kroków może mieć do 70% szerokości obszaru zakładki (treść wybranego kroku zachowuje co najmniej 30% łącznie z separatorem); szerokość wynika z rzeczywistej szerokości kolumn obu wierszy, nie ze stałej.
- R9. Gdy kolumny się nie mieszczą, wiersze nie przechodzą do kolejnych linii: najpierw obcinana jest etykieta (`…`, nie mniej niż 8 znaków), potem wartości wiersza 2 od prawej (harness, effort, model). Bez żadnego zużycia i szczegółów panel nie rośnie ponad dzisiejszy.

**Zachowanie istniejące**

- R10. Zaznaczenie (klamra obejmuje oba wiersze), przewijanie, śledzenie bieżącego kroku i klawisze osi działają bez zmian.
- R11. Krok bez raportu zużycia ma w wierszu 1 puste pola zużycia; krok bez startu (jeszcze nie ruszył) ma tylko wiersz 1 bez pustego drugiego, chyba że ma dane agenta.

### Key Flows

- F1. Operator otwiera *Przebieg* zadania z krokami Claude i Copilota: w każdym kroku widzi koszt, tokeny i, jeśli były, ilu subagentów i ile tokenów zużyli; pod spodem kiedy krok ruszył, jak długo trwał i na jakim modelu/effort/harnessie.
- F2. Okno się zwęża: etykiety są skracane, a kroki nadal mają po dwa wiersze.

### Acceptance Examples

- AE1. Covers R4, R5. Krok Claude z `subagent_stats{spawned:3, tokens:45000}` i bez `subagents`: wiersz 1 zawiera `3× ↕45k`.
- AE2. Covers R5. Krok z `subagents{input:40k, output:3k}` i `subagent_count:2`: `2× ↓40k ↑3k`.
- AE3. Covers R3. Krok z `started_at`, `model`, `effort`, `harness`: wiersz 2 to wcięcie + `MM-DD HH:MM:SS` + czas + odstęp + model + effort + harness; bez daty końca.
- AE4. Covers R7. Krok z `subagent_stats{failed:1}`: żaden wiersz osi nie zawiera `✗`.
- AE5. Covers R8. Przy 200 kolumnach oś ma najwyżej 140 (z separatorem 139), przy 80 kolumnach każdy krok ma nadal 2 wiersze.

### Scope Boundaries

- Poza zakresem: zmiany protokołu, serwera i harnessów (żaden harness nie ma zacząć raportować rozdzielnych tokenów subagentów), CLI `narsor steps`, nagłówek zadania, cache `cache_read`/`cache_write`.
- Poza zakresem: nowe miejsce prezentacji liczników `✓/✗/†/⊘` (np. w szczegółach kroku); to osobna decyzja.
- Poza zakresem: osobne kolumny dla poszczególnych subagentów.

## Planning Contract

Product Contract unchanged. Ten plan zastępuje w części układu osi plan `2026-11-02-2300-feat-tui-zuzycie-per-krok-w-przebiegu-plan.md` (jego druga linia subagentów) oraz wiersze szczegółów z `#68` (`AxisLayout::add_details`).

### Key Technical Decisions

- KTD1. Subagenci w wierszu 1 to cztery kolumny wyrównane do prawej: liczba `n×`, `↓in`, `↑out`, `↕suma`. `↓`/`↑` wypełnia `usage.subagents` (gdy harness podał rozdzielnie), `↕` wypełnia `subagent_stats.tokens`, ale tylko gdy `↓↑` jest puste (R5). Osobna kolumna `↕` utrzymuje wyrównanie w zadaniu mieszanym (część kroków z `↓↑`, część z `↕`) bez specjalnych przypadków. Governs R4–R6.
- KTD2. Liczba subagentów: `subagent_stats.spawned` jeśli `> 0`, w przeciwnym razie `usage.subagent_count` jeśli `> 0` (R4). Krok ma subagentów do pokazania, gdy którakolwiek z czterech komórek jest niepusta; samo `n×` bez tokenów jest poprawne (R6). Governs R4, R6.
- KTD3. Subagenci są zawsze w wierszu 1 (brak `second_line`/`inline` w `AxisLayout::table`). Gdy wiersz 1 nie mieści się w limicie, działa istniejące skracanie: etykieta (`…`, min. `MIN_LABEL`), potem słowo statusu. Governs R2, R9.
- KTD4. Wiersz 2 składa `add_details` z jednej grupy kolumn: `[start, czas trwania]`, odstęp `DETAIL_GAP` (4 spacje, większy niż `COLUMN_GAP` = 2), `[model, effort, harness]`; wcięcie 3 znaki. Kolumny mają wspólną szerokość po krokach (wyrównanie jak dziś). Kolumna końca kroku (`ended_at`) i `subagent_stats_text` znikają. Wiersz 2 dostaje krok z startem lub z którymkolwiek z model/effort/harness; reszta ma sam wiersz 1 (R11). Governs R1, R3, R7, R11.
- KTD5. Szerokość i limit 70% zostają w `axis_cap` i w logice `max(width, wanted.min(cap))`; `wanted` liczy teraz wiersz 1 (bez `second_line`) i wiersz 2. Wiersz 2, który nie mieści się w limicie, jest obcinany przez `fit` z `…` (R9). Bez zużycia i bez szczegółów szerokość to dziś (`today`). Governs R8, R9.
- KTD6. Liczniki `✓/✗/†/⊘` nie mają nowego miejsca (R7); `subagent_stats_text` jest usuwane razem z testami jako martwy kod (jedyne użycie to oś). Dane zostają w protokole. Governs R7.

### Układ (kierunkowo)

```
✓ review_api  ok  $0.42  ↓120k ↑8k  3×  ↕45k
   11-04 21:30:05  4m12s    sonnet  high  claude-code
✓ lint        ok  $0.03  ↓2.1k ↑400  2×  ↓40k ↑3k
   11-04 21:35:17  12s      gpt-5   xhigh codex
```

## Implementation Units

### U1. Komórki subagentów z `subagent_stats`

- **Goal:** `step_cells` zwraca liczbę subagentów i tokeny także wtedy, gdy znane są tylko `subagent_stats`; usunąć `subagent_stats_text`.
- **Requirements:** R4, R5, R6, R7; KTD1, KTD2, KTD6.
- **Files:** `src/client/task_usage_text.rs` (struktura `StepCells`: nowe pole `subagent_total`; `has_subagents` sprawdza wszystkie cztery komórki; usunięcie `subagent_stats_text` i jego testów).
- **Approach:** `subagent_count` z `stats.spawned` albo `usage.subagent_count`; `subagent_input/output` z `usage.subagents` (jak dziś, ale już bez wymogu liczby); `subagent_total = ↕scaled(stats.tokens)` tylko gdy `input/output` puste i `tokens > 0`. Pozostała logika (`line`, `sum`) bez zmian.
- **Test scenarios:** (a) tylko `subagent_stats{spawned:3,tokens:45000}` -> `3×`, `↕45k`, `↓↑` puste; (b) `subagents` + `subagent_count:2` -> `2×`, `↓40k`, `↑3k`, `↕` puste; (c) oba źródła -> `↓↑` wygrywają, liczba ze `spawned`; (d) `spawned:2, tokens:0` -> samo `2×`; (e) `spawned:0` i brak `subagent_count` -> wszystkie puste i `has_subagents` fałsz; (f) krok bez `usage` -> wszystko puste (AE2 z poprzedniego planu); (g) liczniki `failed/killed` nie wpływają na komórki (AE4).
- **Verification:** `cargo test -p narsor --lib task_usage_text`.

### U2. Dwa wiersze na krok w osi

- **Goal:** `AxisLayout` daje krokowi wiersz 1 (z subagentami zawsze inline) i wiersz 2 (start, czas, model, effort, harness) w jednej grupie szerokości do 70%.
- **Requirements:** R1, R2, R3, R7, R8, R9, R10, R11; KTD1, KTD3, KTD4, KTD5.
- **Dependencies:** U1.
- **Files:** `src/tui/task_app/tabs/progress.rs` (`AxisLayout::table` bez `second_line`/`inline`, kolumna `subagent_total`; `AxisLayout::add_details` przepisane na jeden wiersz; stała `DETAIL_GAP`; `subagent_stats_text` z importu usunięty), testy w module `tests` tego pliku, ewentualne poprawki w `tests/integration/tui_render.rs` (asercje na wiersze osi, np. `assert_axis_bracket`, `the_axis_has_a_row_per_step…`).
- **Approach:** w `table` subagenci jako zawsze-inline `push(&subs)` z kolumnami count/input/output/total; usunąć gałąź drugiej linii i warunek `second_line` w `needed`. `add_details`: `times = [clock(started_at), duration_of]`, `agent = [model, effort, harness]`, `row_width = 3 + width(times) + DETAIL_GAP + width(agent)` (pomijając puste grupy bez podwójnego odstępu), wiersz doklejany, gdy krok ma start lub agent. Zaznaczenie obejmuje oba wiersze jak dziś przez `render_bracketed_list` (R10).
- **Test scenarios:** (a) `sample()` przy 200: wiersz 1 z `2×  ↓40k  ↑3k`, kolumny koszt/in/out nadal wyrównane; (b) subagenci z samych `subagent_stats` (`3×  ↕45k`) i mieszany zestaw wyrównany (AE1); (c) wiersz 2: 3 spacje wcięcia, `MM-DD HH:MM:SS`, czas, `DETAIL_GAP`, model/effort/harness w tej kolejności, bez daty końca (AE3); (d) krok bez startu i bez agenta ma 1 wiersz (R11); (e) żaden wiersz osi nie zawiera `✓` po stronie subagentów ani `sub ` przy `failed:1` (AE4); (f) 60..=220 kolumn: oś ≤ 70% i ≥ 30% dla treści, każdy krok ≤ 2 wiersze (AE5, R1); (g) wąskie okno 60: subagenci nadal w wierszu 1, etykieta obcięta `…`, bez paniki dla 0..=60 kolumn; (h) zadanie bez zużycia i bez szczegółów: szerokość jak dziś, istniejący test `without_a_report_the_axis_is_as_it_was` przechodzi bez zmian; (i) zaznaczony krok z 2 wierszami: klamra obejmuje oba (istniejące testy osi w `tui_render.rs`).
- **Verification:** `cargo test -p narsor --lib progress` oraz `cargo test -p narsor --test integration tui_render tui_state`.

### U3. Dokumentacja użytkownika

- **Goal:** `docs/user/tui.md` opisuje nowy układ kroku.
- **Requirements:** R1, R2, R3, R8.
- **Dependencies:** U2.
- **Files:** `docs/user/tui.md` (akapit o *Przebiegu* w „The details”, ok. linii 82–88: zastąpić zdanie o przenoszeniu subagentów do drugiej linii opisem: dwa wiersze na krok, subagenci `n× ↓in ↑out` albo `n× ↕suma`, drugi wiersz ze startem, czasem, modelem, effortem i harnessem, panel do 70% szerokości).
- **Test expectation:** none -- dokumentacja.

## Verification Contract

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets` (bez nowych ostrzeżeń)
- `cargo test --workspace`

## Definition of Done

- R1–R11 spełnione; AE1–AE5 pokryte testami U1 i U2.
- Brak zmian w protokole, serwerze, harnessach i nagłówku zadania.
- Brak martwego kodu: `subagent_stats_text`, `second_line`/`inline`, kolumna końca kroku usunięte razem z testami, które je opisywały.
- `docs/user/tui.md` zaktualizowany.

## Sources / Research

- `src/tui/task_app/tabs/progress.rs`: `AxisLayout::new`, `usage`, `add_details`, `axis_cap`.
- `src/client/task_usage_text.rs`: `step_cells`, `StepCells`, `subagent_stats_text`.
- `crates/narsor-protocol/src/api/timeline.rs`: `Position`, `PositionUsage`, `SubagentStats`.
- `crates/narsor-engine/src/engine/timeline_file.rs`: agregacja zużycia (`subagents` pozostaje nieustawione dla Claude).
- `docs/plans/2026-11-02-2300-feat-tui-zuzycie-per-krok-w-przebiegu-plan.md`.
