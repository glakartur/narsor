---
title: TUI - dłuższy id w ramce, wiersze workspace i zużycie z ikonami - Plan
type: feat
date: 2026-11-08
topic: tui-ramka-wiersze-i-zuzycie
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# TUI - dłuższy id w ramce, wiersze workspace i zużycie z ikonami - Plan

## Goal Capsule

- **Objective:** Operator czyta w szczegółach zadania krótszy, czytelniejszy nagłówek: pełniejszy identyfikator zadania w ramce, wiersze workspace z kolorowymi zmianami i linkiem PR oraz wiersz zużycia z ikonami, który nie pokazuje pustych bloków.
- **Product authority:** Zmiany dotyczą tylko wyglądu nagłówka szczegółów zadania w TUI. Dokument powstał bez dialogu z użytkownikiem (tryb nieinteraktywny). Użytkownik potwierdza założenia i odpowiada na pytania w recenzji MR.
- **Open blockers:** Brak. Użytkownik odpowiedział na Q1–Q3 w recenzji MR (patrz Decyzje).
- **Product Contract preservation:** nowy dokument. Rozszerza układ nagłówka z `docs/plans/2026-11-07-0518-refactor-naglowek-zadania-bez-display-plan.md` (R11, R12, R14, R16). Tam, gdzie ten dokument zmienia tamten zapis, ten dokument wygrywa.

---

## Product Contract

### Summary

Ramka szczegółów pokazuje 11 znaków identyfikatora zadania (`task-123456`) zamiast 8. Wiersz tytułu w nagłówku znika, gdy ramka mieści tytuł w całości. Wiersz workspace ma postać `workspace: repo ⎇ branch → base +N −M #PR ↗`, a liczby zmian mają kolory. Wiersz zużycia ma ikony: `razem` bez ikony, `✨` agenci, `◈ N` subagenci, `⚡` dyrygent. Blok bez danych nie jest pokazany.

### Problem Frame

Id `task-728` ma 8 znaków, więc zgadza się z początkiem identyfikatora tylko dla bardzo krótkich numerów. Dłuższy fragment lepiej odróżnia zadania. Wiersze workspace i wiersz zużycia mają dziś etykiety słowne (`agenci`, `dyrygent`, `PR`) i symbol `—` dla braku danych. Zajmują one miejsce i utrudniają szybkie czytanie.

### Requirements

**Ramka i tytuł**

- R1. Identyfikator zadania po prawej stronie górnej krawędzi ramki ma 11 znaków (dziś 8), np. `task-123456`. Id dłuższy niż 11 znaków jest skrócony do pierwszych 11 znaków. Id krótszy niż 11 znaków jest pokazany w całości. Krawędź ramki nigdy nie skraca id dalej, tylko tytuł.
- R2. Wiersz tytułu w nagłówku jest pokazany tylko wtedy, gdy krawędź ramki skraca tytuł. Gdy ramka mieści tytuł w całości, nagłówek nie ma tego wiersza i zaczyna się od wiersza statusu.
- R1a. Lista zadań w TUI pokazuje id zadania w tej samej długości co ramka (11 znaków, ta sama zasada skracania co w R1). CLI (`task_text`) zostaje bez zmian. Planowanie wskazuje miejsce w liście, które dziś skraca id, i używa jednej wspólnej stałej.
- R3. Dłuższy id (R1) zabiera ramce 3 komórki. Decyzję z R2 podejmuje układ krawędzi z nową szerokością id.

**Krok**

- R4. Między numerem kroku i jego nazwą stoi ` · `: `krok 3 · implement`. Dotyczy każdego miejsca TUI, które pokazuje numer razem z nazwą kroku.

**Wiersz workspace**

- R5. Wiersz ma postać `nazwa_workspace: nazwa_repo ⎇ nazwa_brancha → base_branch +N −M #PR ↗`. Po nazwie workspace stoi dwukropek. Przed nazwą brancha stoi `⎇`.
- R6. `+N` ma kolor dodanych linii (zielony), a `−M` kolor usuniętych (czerwony). Kolor nie jest jedyną informacją: znaki `+` i `−` zostają.
- R7. Numer pull requesta ma postać `#N`, bez słowa `PR`. Po nim stoi symbol linka `↗`, a cały fragment jest linkiem OSC 8 do `pr_url`, jak dziś.
- R8. Zasady z poprzedniego planu zostają bez zmian: kolumny są wyrównane między wierszami, `branch` równy `base_branch` daje jedną nazwę, workspace bez PR ma wyszarzony `—`, a przy wąskim terminalu skracanie zaczyna się od `base_branch`, potem zmian, potem `branch`, potem repo. Nazwa workspace i numer PR nie są skracane.

**Wiersz zużycia**

- R9. Koszt `✨` obejmuje agentów razem z subagentami (D2). Wiersz ma postać `razem $68.30 · ✨ $34.15 ↑304 ↓72k (◈ 2 $12 ↑100 ↓20k) · ⚡ $34.15 ↑304 ↓72k`. Bloki: `razem` (koszt agentów i dyrygenta), `✨` agenci, `◈` subagenci, `⚡` dyrygent.
- R10. `◈` stoi w nawiasie po bloku agentów. Pierwsza liczba po `◈` to liczba subagentów. Potem są koszt (gdy znany), `↑` tokeny wejścia i `↓` tokeny wyjścia.
- R11. Blok bez danych nie jest pokazany. Dotyczy `razem`, `✨`, `◈` i `⚡`. Część bloku bez danych (np. brak kosztu) też nie jest pokazana: nie ma znaku `—`. Dotyczy to tylko wiersza zużycia (`task_usage_text::line`). Komórki kroków w zakładce postępu (`step_cells`) używają tej samej stałej `UNKNOWN` i zostają bez zmian. Wyjątek: `—` w wierszu workspace bez PR (R8) zostaje. Gdy nie ma żadnego bloku, wiersz zużycia nie istnieje.
- R12. Gdy wiersz zużycia jest za szeroki, najpierw znika blok `◈`, potem tekst jest obcięty z `…`, jak dziś.

### Acceptance Examples

- AE1. **Covers R1, R3.** Given zadanie `task-1234567890abcdef`, when TUI rysuje ramkę, then po prawej stoi `task-123456` i ramka ma o 3 komórki mniej miejsca na tytuł niż przed zmianą.
- AE2. **Covers R2.** Given tytuł mieści się w ramce, when TUI rysuje nagłówek, then pierwszy wiersz to wiersz statusu. Given tytuł jest obcięty, then pierwszy wiersz to pełny tytuł.
- AE3. **Covers R5, R6, R7.** Given workspace `code`, repo `orders-api`, branch `feat/x`, base `main`, zmiany 123 i 45 oraz PR 42 z adresem, when TUI rysuje wiersz, then widać `code: orders-api ⎇ feat/x → main +123 −45 #42 ↗`, `+123` jest zielone, `−45` czerwone, a `#42 ↗` jest linkiem.
- AE4. **Covers R9, R11.** Given harness podał koszt i tokeny agentów, a dyrygent i subagenci nic nie podali, when TUI rysuje wiersz zużycia, then wiersz ma tylko `razem` i `✨`.
- AE5. **Covers R10.** Given dwóch subagentów, tokeny `↑304 ↓72k` i brak ich kosztu, when TUI rysuje wiersz, then blok to `(◈ 2 ↑304 ↓72k)` bez znaku `—`.

### Scope Boundaries

- Bez zmian: protokół, dane `context.workspace.*`, źródła zużycia i CLI (`task_text`).
- Poza zakresem: nowy kontrakt danych kosztu subagentów i wyliczanie kosztu z cennika. Dziś harness podaje ich tokeny i liczbę, nie koszt (patrz D1).
- Poza zakresem: zmiana długości id w CLI i w identyfikatorach zapisanych w systemie (patrz D3).

### Założenia do potwierdzenia

- A1. "Numer w ramce" to krótki identyfikator zadania po prawej stronie górnej krawędzi (`SHORT_ID_CHARS` w `src/tui/task_app/detail.rs`, dziś 8). Zmieniamy jego długość: 8 → 11. To samo dotyczy id w liście zadań (D3). Id ma prefiks `task-`, więc 11 znaków to `task-` i 6 znaków numeru.
- A2. Zwijanie wiersza tytułu (R2) już działa przez `title_is_cut`. Dokument potwierdza to zachowanie i wymaga, żeby przeżyło zmianę R1. Nie zakładamy żadnej nowej logiki.
- A3. ` · ` między krokiem i nazwą (R4) istnieje już w wierszu statusu (`krok 3 · implement`). Nieaktualny jest komentarz modułu w `src/tui/task_app/detail.rs`, który pokazuje `krok 3 implement`. Planowanie ma sprawdzić, czy inne miejsca TUI (zakładki, lista, Info) pokazują numer z nazwą bez separatora, i ujednolicić je.
- A4. W wierszu zmian używamy znaku `−` (U+2212), jak dziś, a nie myślnika `-` z opisu zadania.
- A5. Kolory zmian biorą się z istniejących ról stylu TUI. Jeśli potrzebna jest nowa rola (zielony, czerwony), planowanie dodaje ją we wszystkich motywach. Gdy zadanie jest bez połączenia (`stale`), wiersz jest szary jak dziś.
- A6. `razem` zostaje słowem bez ikony, bo opis zadania podaje je tak. Blok `razem` jest pokazany tylko wtedy, gdy znamy koszt.
- A7. Zmiany nie obejmują CLI. Tekst zadania w CLI nie ma wiersza zużycia i zostaje taki sam.
- A8. Symbol `⎇` i ikony `✨`, `◈`, `⚡` mogą mieć różną szerokość w różnych terminalach. Układ liczy szerokość tak jak dziś (funkcja `cells`) i nie wprowadza własnych tablic szerokości.

### Decyzje użytkownika

- D1 (Q1). Koszt subagentów jest pokazany tylko wtedy, gdy harness go poda. Nie wyliczamy go z cennika. Bez kosztu blok `◈` ma liczbę i tokeny (AE5).
- D2 (Q2). `✨` pokazuje agentów razem z subagentami. `◈` pokazuje same subagenty. Blok `◈` stoi w nawiasie po `✨` (R10).
- D3 (Q3). 11 znaków id obowiązuje w ramce i w liście zadań (R1a). CLI zostaje bez zmian.

### Sources / Research

- `src/tui/task_app/detail.rs`: `SHORT_ID_CHARS`, `edge_layout`, `title_is_cut`, `header`, `status_parts`, `workspace_line`.
- `src/tui/task_app/workspace_rows.rs`: kolumny i skracanie wierszy workspace.
- `src/client/task_usage_text.rs`: `line`, format wiersza zużycia i znak `—` dla braku danych.
- `crates/narsor-protocol/src/api/timeline.rs`: `PositionUsage`, `SubagentStats`. Brak kosztu subagentów.
- `docs/plans/2026-11-07-0518-refactor-naglowek-zadania-bez-display-plan.md`: obecny kontrakt nagłówka (R11–R18).
