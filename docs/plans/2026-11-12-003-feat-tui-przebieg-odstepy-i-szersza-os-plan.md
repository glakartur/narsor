---
title: "feat: Większe odstępy i szersza oś kroków w Przebiegu"
type: feat
status: active
date: 2026-11-12
origin: docs/brainstorms/2026-11-12-tui-przebieg-odstepy-i-szersza-os-requirements.md
---

# feat: Większe odstępy i szersza oś kroków w Przebiegu

## Summary

Plan zmienia dwie liczby w `src/tui/task_app/tabs/progress.rs`: odstęp między kolumnami danych osi (2 → 3 spacje) i limit szerokości osi (70% → 75% zakładki). Wiersz szczegółów zachowuje dotychczasowy odstęp. Rozmiar: Lightweight (jeden plik kodu, jeden opis w dokumentacji, 2 jednostki).

---

## Problem Frame

Kolumny za nazwą kroku stoją co `COLUMN_GAP` (2 spacje) i zlewają się. Limit 70% zmusza do skracania nazw i czasu, a większy odstęp zwiększa ten nacisk (see origin: `docs/brainstorms/2026-11-12-tui-przebieg-odstepy-i-szersza-os-requirements.md`). Plan realizuje R1–R8 i AE1–AE4 z origin.

---

## Requirements

- R1, R2, R3, R4. Odstęp 3 spacje między nazwą i kolumnami danych oraz między kolumnami. Brak spacji za ostatnią wartością. Wiersz szczegółów bez zmian. (U1)
- R5, R6, R7, R8. Limit osi 75%, rozmowa min. 25%. Oś nie rośnie ponad potrzeby danych. Kolejność skracania bez zmian. Ta sama oś dla taska w toku i historii. (U1)
- Dokumentacja użytkownika zgodna z nowymi wartościami. (U2)

---

## Scope Boundaries

- Bez zmian: formaty czasu i trwania, kolejność kolumn, *Historia*, choosery, `AXIS_MAX_WIDTH`, klamra zaznaczenia, ustawienie szerokości przez operatora.

---

## Key Technical Decisions

- Dwie stałe zamiast jednej. `COLUMN_GAP` (kolumny osi) rośnie do 3. Wiersz szczegółów (`add_details`, `joined`, `agent_width`) używa osobnej stałej o wartości 2. Powód: R4 wymaga starego separatora szczegółów, a dziś obie ścieżki dzielą `COLUMN_GAP`.
- `axis_cap` zmienia oba człony: udział rozmowy z 30% na 25% (`(area * 3).div_ceil(10)` → `area.div_ceil(4)`) i górny limit z `area * 7 / 10` na `area * 3 / 4`. Powód: dziś wiąże człon „rozmowa ma 30%", więc sama zmiana limitu 70% nic by nie dała.
- Wąskie terminale (pytanie odroczone z origin): nowy udział 75/25 dotyczy zakładek szerokich co najmniej 100 kolumn. Poniżej zostaje 70/30. Powód: przy 80 kolumnach 25% to 20 kolumn dla rozmowy, co jest zbyt mało. Próg to założenie startowe; osoba może go zmienić, bez wpływu na resztę planu. Wartość progu trafia do nazwanej stałej.

---

## Implementation Units

### U1. Odstęp 3 spacje i limit 75% w osi

**Goal:** Oś rysuje kolumny co 3 spacje i może zająć do 75% szerokości zakładki (od progu szerokości).

**Requirements:** R1–R8; AE1–AE4.

**Dependencies:** brak.

**Files:**
- Modify: `src/tui/task_app/tabs/progress.rs` (stałe, `axis_cap`, `columns_width`, `AxisLayout::table`, `add_details`, testy w module `tests`)

**Approach:**
- `COLUMN_GAP` = 3. Nowa stała (np. `DETAIL_GAP` = 2) dla `joined` i `agent_width` w `add_details`.
- `axis_cap`: udział rozmowy i górny limit zależne od progu szerokości (nazwana stała). Zachowaj `.max(today)`.
- `table` liczy już czasy i kolumny zużycia przez `COLUMN_GAP`, więc kolejność skracania (R7) i brak spacji na końcu (R3) działają bez zmian logiki. Sprawdź to testami, nie zmieniaj logiki.
- Odstęp widoczny między kolumnami zużycia zawiera też dopełnienie kolumn do wspólnej szerokości; testy mierzą odstęp od najdłuższej wartości.

**Patterns to follow:** istniejące testy `layout(...)`, `text(...)`, `column_of(...)` w `progress.rs`.

**Test scenarios:**
- Covers AE1. Krok z czasem startu, trwania i zużyciem: między nazwą, czasem startu, czasem trwania i każdą kolumną zużycia są po 3 spacje (przy najdłuższej wartości w kolumnie). Zaktualizuj `metric_columns_are_two_spaces_apart` (zmień nazwę na `..._three_spaces_apart`, oczekiwany ciąg z odstępem 3).
- Covers AE2. Krok bez zużycia: wiersz kończy się czasem trwania, bez spacji na końcu (`a_step_without_a_start...` i test końca wiersza zostają zielone).
- Covers AE3. Zmień `the_axis_stays_within_seventy_percent...` na 75%: dla szerokości ≥ próg `axis.width <= max(width * 3 / 4, today)` i rozmowa ma ≥ `width.div_ceil(4)`; dla szerokości < próg zostaje 70/30. Przy 200: oś ≤ 150, rozmowa ≥ 50. Oś bez zużycia jest wąska jak dziś (`without_a_report_or_a_start...` bez zmian).
- Covers AE4. Wąska zakładka: nazwa skraca się do `MIN_LABEL`, potem znika czas trwania, potem czas startu (dodaj test dla szerokości na granicy progu).
- Wiersz szczegółów: `the_first_row_has_the_times_and_the_second_the_agent` nadal oczekuje `"   sonnet  high  claude-code"` (separator 2, wcięcie 3).
- Edge: wiersze są wyrównane między krokami o różnych nazwach i czasach (`the_times_stand_in_the_same_columns_in_every_row` zielony).

**Verification:** testy modułu `progress` przechodzą; `cargo clippy` i `cargo fmt --check` bez uwag; ręcznie w TUI oś ma wyraźne odstępy, a rozmowa kroku zawija się czytelnie przy 100 i 200 kolumnach.

### U2. Opis w dokumentacji użytkownika

**Goal:** `docs/user/tui.md` opisuje nowe wartości.

**Requirements:** zgodność dokumentacji.

**Dependencies:** U1.

**Files:**
- Modify: `docs/user/tui.md` (zdanie o „up to 70% of the tab", ok. linii 125; dopisz próg szerokości i odstęp, jeśli opis go wymienia)

**Approach:** zmień 70% na 75% z zastrzeżeniem o progu. Sprawdź `grep` po `docs/` i `README` za innymi wzmiankami o 70% osi lub o dwóch spacjach.

**Test expectation:** none -- zmiana dokumentacji.

**Verification:** brak starych wartości w dokumentacji użytkownika.

---

## Risks

- Rozmowa 25% na średnich terminalach (100–120 kolumn) może zawijać się ciaśniej. Łagodzi to próg 100 kolumn i fakt, że oś rośnie tylko do potrzeb danych. Zweryfikuj ręcznie.

## Deferred to Implementation

- Dokładna nazwa stałych i wartość progu, jeśli ręczny przegląd pokaże lepszą granicę.
