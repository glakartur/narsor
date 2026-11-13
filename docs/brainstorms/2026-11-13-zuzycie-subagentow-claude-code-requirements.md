---
date: 2026-11-13
topic: zuzycie-subagentow-claude-code
---

# Tokeny i koszt subagentów Claude Code w Przebiegu i w nagłówku taska

## Summary

Dla harnessu Claude Code narsor zapisuje tokeny (i koszt, gdy harness go podaje) każdego subagenta ze zdarzeń harnessu. Przebieg pokazuje te wartości w wierszu kroku. Nagłówek taska pokazuje sumę: liczbę subagentów, ich tokeny i koszt. Subagenci mają jedną ikonę `👥` w nagłówku i w Przebiegu.

---

## Problem Frame

Dziś dla Claude Code narsor czyta tylko `subagent_stats` z linii `result` (`crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs`). Te dane mają liczbę subagentów, ale `tokens` jest zawsze `0`. Pole `subagents` (tokeny subagentów) w `UsageReading` zostaje puste (`crates/narsor-engine/src/engine/timeline_file.rs`, `reading_from_element`).

Skutki:

- Przebieg pokazuje tylko `3×` w kolumnie subagentów. Nie pokazuje tokenów ani kosztu subagentów.
- Nagłówek taska nie pokazuje subagentów wcale. `task_usage_text::sum` (`src/client/task_usage_text.rs`) nie bierze `subagent_stats`. Blok `◈` pojawia się tylko, gdy subagenci mają tokeny lub koszt, a dla Claude Code ich nie ma. `subagent_cost_usd` jest zawsze `None`.

Harness GitHub Copilot już zlicza tokeny subagentów z `subagent.completed` (`totalTokens`). Claude Code też podaje dane per subagent w strumieniu zdarzeń (wynik wywołania narzędzia `Agent`/`Task` oraz zdarzenia systemowe zadań), ale narsor ich nie czyta.

---

## Requirements

**Zbieranie danych (Claude Code)**

- R1. Parser Claude Code czyta dane o zużyciu każdego subagenta ze zdarzeń harnessu: tokeny wejścia i wyjścia (oraz cache, gdy są) i koszt, gdy harness go podaje.
- R2. Narsor liczy jednego subagenta jeden raz, także gdy harness podaje jego dane w więcej niż jednym zdarzeniu.
- R3. Subagent zagnieżdżony (subagent uruchomiony przez subagenta) wchodzi do sumy jeden raz. Jego tokeny nie mogą być policzone drugi raz przez subagenta-rodzica, jeśli harness już wlicza je w dane rodzica.
- R4. Suma tokenów i kosztu subagentów jednego uruchomienia trafia do odczytu zużycia kroku (`subagents` i nowy koszt subagentów), obok liczby subagentów z `subagent_stats`.
- R5. Wznowienie sesji i kolejne próby kroku nie dublują zużycia subagentów. Reguły są takie same jak dla zużycia agenta (odczyt przyrostowy kontra suma sesji).
- R6. Subagent bez danych o zużyciu (np. zatrzymany, odrzucony) liczy się do liczby subagentów, ale nie dodaje tokenów ani kosztu. Brak danych to „nie podano”, nie „zero”.

**Przebieg (wiersz kroku)**

- R7. Wiersz kroku w Przebiegu pokazuje dla subagentów: ikonę `👥`, liczbę, koszt (gdy znany), tokeny `↑` i `↓` (gdy znane).
- R8. Gdy harness podaje tylko łączne tokeny subagentów (jak GitHub Copilot), wiersz dalej pokazuje `↕` z tą liczbą.

**Nagłówek taska**

- R9. Nagłówek taska pokazuje sumę po wszystkich krokach: liczbę subagentów, ich koszt i tokeny, w bloku z ikoną `👥` (zamiast `◈`).
- R10. Nagłówek pokazuje liczbę subagentów także wtedy, gdy harness nie podał ich tokenów ani kosztu (dziś wtedy blok znika).
- R11. Suma liczby subagentów w nagłówku bierze dane z `subagent_stats` (`spawned`), gdy `subagent_count` nie jest podane. Ta sama liczba widoczna w Przebiegu wchodzi do sumy w nagłówku.
- R12. Gdy linia nagłówka nie mieści się w szerokości terminala, obecne zachowanie zostaje: najpierw znika blok subagentów, potem linia jest ucięta `…`.

**Ikona**

- R13. Ikona `👥` oznacza subagentów we wszystkich miejscach, w których TUI pokazuje ich zużycie: nagłówek taska i Przebieg. Ikona jest w jednym miejscu kodu, nie w każdym widoku osobno.

---

## Acceptance Examples

- AE1. Krok Claude Code uruchomił 2 subagentów, każdy zużył 40k tokenów wejścia i 3k wyjścia. Przebieg: `👥 2 ↑80k ↓6k`. Nagłówek: `… ✨ $1.20 ↑120k ↓8k (👥 2 ↑80k ↓6k)`.
- AE2. Harness podał koszt subagentów `$0.40`. Przebieg: `👥 2 $0.40 ↑80k ↓6k`. Nagłówek ma `$0.40` w bloku `👥`.
- AE3. Krok uruchomił 3 subagentów, harness nie podał ich tokenów. Przebieg: `👥 3`. Nagłówek: `(👥 3)` po grupie `✨`.
- AE4. Dwa kroki: 2 i 1 subagent. Nagłówek: `👥 3` z sumą tokenów obu kroków.
- AE5. Krok wznowiony w tej samej sesji Claude Code. Subagenci z pierwszego uruchomienia nie są policzeni drugi raz.

---

## Success Criteria

- Dla taska z subagentami Claude Code operator widzi w nagłówku i w Przebiegu liczbę subagentów i ich tokeny.
- Suma w nagłówku jest równa sumie wartości z wierszy Przebiegu.
- Testy parsera używają fixture z prawdziwego zapisu Claude Code z co najmniej dwoma subagentami (w tym jednym zagnieżdżonym, jeśli harness na to pozwala).

---

## Scope Boundaries

- Nie liczymy kosztu z cennika modeli. Koszt subagentów pokazujemy tylko wtedy, gdy harness go podaje.
- Nie zmieniamy sposobu liczenia zużycia agenta (`✨`) i dyrygenta (`⚡`).
- Nie dodajemy osobnego widoku listy subagentów ani czasu pracy każdego subagenta.
- Nie zmieniamy harnessów Codex i OpenCode. GitHub Copilot dostaje tylko nową ikonę (R7, R9, R13).

---

## Założenia do potwierdzenia

- Z1. Zdarzenia Claude Code mają dane per subagent: wynik narzędzia `Agent`/`Task` (`tool_use_result` z `usage`, `totalTokens`) i/lub zdarzenie `system` zadania z `usage`. W repo nie ma fixture z takim zdarzeniem i lokalne logi tego nie potwierdzają. Plan musi najpierw nagrać prawdziwy zapis i sprawdzić nazwy pól.
- Z2. Koszt per subagent: autor zlecenia widział go w logach. Nie sprawdziłem tego. Jeśli harness nie podaje kosztu per subagent, blok `👥` pokazuje tylko liczbę i tokeny (zgodnie z zasadą „bez wartości zastępczych”). Kosztu nie liczymy z cennika.
- Z3. `total_cost_usd` i `usage` z linii `result` Claude Code już zawierają (koszt) lub nie zawierają (tokeny) zużycie subagentów — to trzeba sprawdzić. Opis grupy `✨` mówi, że jej liczby zawierają subagentów. Jeśli `usage` z `result` nie zawiera tokenów subagentów, plan musi zdecydować, czy dodać je do `✨`, aby ten opis był prawdziwy.
- Z4. Ikona `👥` (grupa osób = pomocnicy agenta). Ma szerokość 2 komórek, jak inne emoji kroków (`✨`, `⚡`, `⚖️`, `💻`). Alternatywy, jeśli `👥` nie pasuje: `🤖`, `🧩`, `🐝`.
- Z5. `👥` zastępuje `◈` w nagłówku i `N×` w Przebiegu. Liczba stoi po ikonie (`👥 3`), bez `×`.
- Z6. „Nagłówek taska” to linia zużycia w nagłówku szczegółów taska (`src/tui/task_app/detail.rs`, `usage_text`).

---

## Pytania otwarte dla użytkownika

Brak. Wszystkie niejasności są w sekcji „Założenia do potwierdzenia”.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R2][Needs research] Które zdarzenie Claude Code jest źródłem prawdy dla zużycia subagenta, gdy dane są w więcej niż jednym miejscu.
- [Affects R3][Needs research] Czy dane subagenta-rodzica zawierają już zużycie subagentów zagnieżdżonych.
- [Affects R5][Technical] Czy `subagent_stats` i zużycie subagentów są per proces (suma sesji) czy per uruchomienie, przy wznowieniu sesji.
- [Affects R4][Technical] Gdzie dodać koszt subagentów w `UsageReading` i `PositionUsage` bez zmiany zgodności zapisanych plików timeline.
