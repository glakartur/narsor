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
- **Execution profile:** jeden PR. Zmiany są małe, dotykają jednego widoku (nagłówek szczegółów) i dzielą stałe oraz testy. Wykonuje i publikuje ten sam wykonawca.
- **Means:** czysto prezentacyjne zmiany w `src/tui/task_app/*` i `src/client/task_usage_text.rs`, bez zmian protokołu (KTD1–KTD4).
- **Product Contract preservation:** Product Contract bez zmian (R1–R12, AE1–AE5 zachowane). Planowanie dodaje tylko sekcje implementacyjne. Nowy dokument. Rozszerza układ nagłówka z `docs/plans/2026-11-07-0518-refactor-naglowek-zadania-bez-display-plan.md` (R11, R12, R14, R16). Tam, gdzie ten dokument zmienia tamten zapis, ten dokument wygrywa.

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
- R11. Blok bez danych nie jest pokazany. Dotyczy `razem`, `✨`, `◈` i `⚡`. Część bloku bez danych (np. brak kosztu) też nie jest pokazana: nie ma znaku `—`. Dotyczy to tylko wiersza zużycia (`task_usage_text::line`). Komórki kroków w zakładce postępu (`step_cells`) zostają bez zmian (puste komórki, bez `—`, nie używają `UNKNOWN`). Wyjątek: `—` w wierszu workspace bez PR (R8) zostaje. Gdy nie ma żadnego bloku, wiersz zużycia nie istnieje.
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

---

## Planning Contract

### Wyniki badania kodu (zweryfikowane w repo)

- `SHORT_ID_CHARS = 8` w `src/tui/task_app/detail.rs` steruje tylko `edge_layout`. Lista ma osobne `short_task_id` w `src/tui/task_app/list.rs` (`id:` + `take(8)`). `short_id` w `src/tui/event_text.rs` dotyczy id akcji i sesji, nie zadania: poza zakresem.
- `title_is_cut` i `header` (`detail.rs`) już zwijają wiersz tytułu (A2). Zmiana R1 działa przez `room` w `edge_layout`.
- `status_parts` już pisze `krok {at} · {name}`. Nieaktualny jest komentarz modułu. Gałąź `(None, Some(name))` pisze `krok {name}` bez numeru: R4 jej nie dotyczy, bo nie ma numeru. Inne miejsca z numerem i nazwą kroku: nie znaleziono w `src` (Info ma osobny wiersz `krok`); wykonawca potwierdza to przeszukaniem.
- `workspace_rows.rs` ma kolumny `Name, Repo, Branch(es), Changes, Pr` rozdzielone `GAP`, a kolory daje `workspace_line` z `Part`. `Changes` to jeden `String` `+N −M`.
- `PrLabel::text()` (`crates/narsor-protocol/src/display.rs`) zwraca `PR #42` i jest używane też przez CLI (`task_text.rs`), `actions.rs` i zakładkę Info. `label.number` ma już prefiks `#`.
- `PositionUsage.total` zawiera już tokeny subagentów (komentarz w `timeline.rs`). `✨` = `total` zgadza się więc z D2 bez sumowania. Brak kosztu subagentów w protokole (D1).
- `Role` w `src/tui/style.rs` ma `Live` (zielony), `Problem` (czerwony, ale pogrubiony w `NO_COLOR`), `Done`, `Calm`. `Role::ALL` ma stałą długość 8 i jest używane w testach.
- Testy do zmiany: `tests/integration/tui_render.rs` (asercje na `PR #42 ↗`, `agenci`, `subagenci`, `id:task-`, nazwa testu "eight_character_id") oraz testy w `src/client/task_usage_text.rs`. `tests/integration/cli_task_text.rs` ma zostać bez zmian (A7).

### Key Technical Decisions

- **KTD1. Jedna stała długości id.** Jedna publiczna stała `TASK_ID_CHARS = 11` (zastępuje `SHORT_ID_CHARS`) i jedna funkcja skracająca id w `detail.rs`; `list.rs` używa ich obu. Dotyczy R1, R1a. Id krótsze niż 11 znaków zostaje w całości (`take` to zapewnia).
- **KTD2. Nowe role kolorów zmian.** Dodać `Role::Added` (zielony) i `Role::Removed` (czerwony) we wszystkich gałęziach `Styles::of` i do `Role::ALL` (długość 9). W `NO_COLOR` obie bez koloru i bez pogrubienia: znaki `+` i `−` niosą znaczenie (R6). Rola `Problem` odpada, bo jest pogrubiona w `NO_COLOR`. Rola `Live` odpada, bo jej sens to "dzieje się teraz". Dotyczy A5. W stanie `stale` oba kawałki są `Done`, jak dziś.
- **KTD3. Wiersz workspace dalej kolumnowy.** Zostaje model kolumn z `Columns`. Zmienia się treść: kolumna nazwy dostaje `:` (nazwa i dwukropek nie są skracane), kolumna brancha dostaje `⎇ ` przed nazwą brancha, a zmiany dzielą się na dwa kawałki (`+N`, `−M`) o jednej szerokości kolumny. Kolejność skracania z R8 bez zmian. Separator między kolumnami i szerokość liczy `cells`, bez własnych tablic szerokości (A8). Wyrównanie kolumn przez dopełnianie spacjami, jak dziś.
- **KTD4. Format PR tylko w widoku TUI.** `pr_label()` i `PrLabel::text()` zostają bez zmian (CLI, Info, akcje). `workspace_rows::Entry::of` składa `#N ↗` z `label.number`. Gdy jest link, ale nie ma numeru, tekst to `PR ↗`, bo `#` bez numeru nie ma sensu. Jest to założenie wykonawcze, nie zmiana wymagań.
- **KTD5. Wiersz zużycia składany z bloków.** `task_usage_text::line` buduje listę bloków (`razem`, `✨`, `◈`, `⚡`), a każdy blok z listy części (koszt, tokeny) bez `—`. Pusty blok i pusty wiersz są pomijane. `UNKNOWN` jest dziś używane tylko przez `money` i `tokens` (wiersz `line`), więc po przepisaniu `line` stała jest nieużywana i jest usuwana. `with_subagents = false` pomija `◈` (R12). `◈` stoi w nawiasie po `✨`, a gdy `✨` nie istnieje, a są subagenci, nawias stoi sam po `razem` (decyzja wykonawcza, ścieżka rzadka).
- **KTD6. Wąski wiersz zużycia.** Zostaje obecna logika w `header`: pełny tekst, potem bez `◈`, potem `fit_cells` z `…` (R12). Bez zmian w `detail.rs` poza komentarzem i nowym wywołaniem.

### Założenia przyjęte w planowaniu

- Zakres jest zgodny z A1–A8 i D1–D3. Brak nowych pytań do użytkownika.
- Szerokość `⎇`, `✨`, `◈`, `⚡` w terminalu może różnić się od `cells`. Nie poprawiamy tego (A8).

### Scope Boundaries (planowanie)

Rozważone i niebudowane:

- Wyliczanie kosztu subagentów z cennika: zgodnie z D1. Zmieniłoby to rozmowę, gdyby harness zaczął podawać koszt (wtedy wystarczy wypełnić pole).
- Zmiana `PrLabel::text()` na `#N`: dotknęłaby CLI i Info bez wymagania (A7).
- Wspólna tablica szerokości znaków: A8.

---

## Implementation Units

### U1. Dłuższy id w ramce i na liście

**Goal:** Ramka i lista pokazują 11 znaków id; wiersz tytułu znika, gdy ramka mieści tytuł.
**Requirements:** R1, R1a, R2, R3; AE1, AE2
**Dependencies:** brak
**Files:** `src/tui/task_app/detail.rs`, `src/tui/task_app/list.rs`, `tests/integration/tui_render.rs`
**Approach:**
1. Zastąpić `SHORT_ID_CHARS` stałą `TASK_ID_CHARS = 11` i wyciągnąć skracanie id do jednej funkcji (KTD1).
2. Użyć jej w `edge_layout` i w `short_task_id` w `list.rs`.
3. Poprawić komentarz stałej ("nigdy nie skracane") i komentarz modułu o `krok`.
4. Nie zmieniać `title_is_cut` ani `header`: A2.
**Patterns to follow:** istniejące `edge_layout`, `title_is_cut`.
**Test scenarios:**
- Covers AE1. Zadanie `task-1234567890abcdef`, ramka 80 szer.: prawa krawędź ma `task-123456`, a tytuł dostaje o 3 komórki mniej niż przy starej długości (porównanie z szerokością, przy której tytuł jest dokładnie obcinany).
- Id `task-12` (7 znaków): pokazane w całości, bez dopełnienia.
- Id dokładnie 11 znaków: bez zmian.
- Covers AE2. Tytuł mieści się: pierwszy wiersz nagłówka to status. Tytuł obcięty: pierwszy wiersz to pełny tytuł. Brzeg: tytuł mieści się przy starych 8 znakach, a przy 11 już nie, więc wiersz tytułu się pojawia.
- Lista: wiersz bez ticketu zaczyna się od `id:task-abcdef1`-podobnego prefiksu 11 znaków (zmienić test `a_missing_ticket_uses_an_eight_character_id_prefix_without_colour` i jego nazwę; trzy asercje `id:task-` na linii ~6211–6237 sprawdzić pod kątem szerokości).
**Verification:** ramka i lista pokazują ten sam skrót; żadne miejsce nie używa 8 znaków dla id zadania.

### U2. Wiersz workspace z kolorami i linkiem PR

**Goal:** Wiersz ma postać `nazwa: repo ⎇ branch → base +N −M #PR ↗` z kolorami zmian.
**Requirements:** R4, R5, R6, R7, R8; AE3
**Dependencies:** brak (niezależne od U1)
**Files:** `src/tui/style.rs`, `src/tui/task_app/workspace_rows.rs`, `src/tui/task_app/detail.rs`, `tests/integration/tui_render.rs`
**Approach:**
1. Dodać `Role::Added` i `Role::Removed` (KTD2), zaktualizować `Role::ALL` i testy, które chodzą po rolach.
2. W `workspace_rows.rs`: `Part::Changes` rozdzielić na `Added` i `Removed` (lub dwa kawałki jednej kolumny); `:` po nazwie; `⎇ ` przed nazwą brancha; PR jako `#N ↗` (KTD3, KTD4).
3. `workspace_line` w `detail.rs`: mapowanie nowych `Part` na role; `stale` daje `Done`.
4. Sprawdzić `link_spots`: kolumna i szerokość linku liczą się z nowego tekstu `#N ↗`.
5. Zaktualizować komentarze modułów (`workspace_rows.rs`, `detail.rs`), w tym przykład wiersza.
6. R4: potwierdzić, że `krok N · nazwa` jest w każdym miejscu z numerem i nazwą; poprawić brakujące.
**Execution note:** zacząć od testu AE3 w `tui_render.rs` (kolory przez bufor, jak istniejące testy stylów).
**Patterns to follow:** `Part`/`Piece` w `workspace_rows.rs`, testy kolorów w `tui_render.rs` z `Styles::new(false/true)`.
**Test scenarios:**
- Covers AE3. Workspace `code`/`orders-api`/`feat/x`/`main`, 123 i 45, PR 42 z adresem: tekst `code: orders-api ⎇ feat/x → main +123 −45 #42 ↗`; `+123` ma styl `Added`, `−45` styl `Removed`; `#42 ↗` jest linkiem OSC 8 do `pr_url`.
- `NO_COLOR`: `+123` i `−45` bez koloru, znaki obecne.
- `stale`: wszystkie kawałki `Done`.
- `branch == base_branch`: jedna nazwa po `⎇`.
- Workspace bez PR z znanym branchem: wyszarzone `—` (R8).
- Dwa workspace o różnych szerokościach nazw: kolumny wyrównane.
- Wąski terminal: kolejność skracania base → zmiany → branch → repo; nazwa z `:` i `#N ↗` nigdy skracane (rozszerzyć istniejący test `a_narrower_terminal_cuts_the_workspace_row_in_a_fixed_order`).
- PR bez linku: `#42` bez `↗` (test `a_workspace_row_never_shows_an_address_that_is_not_http` i test przy linii ~2325).
- Link bez numeru: `PR ↗` (KTD4).
- Tylko jedna z liczb zmian znana: drugi człon `0`, jak dziś.
- Asercje OSC 8 (~2877–2897) i `link_spots` zgodne z nowym tekstem.
**Verification:** żaden wiersz TUI w nagłówku nie zawiera słowa `PR` przed numerem; CLI i Info bez zmian.

### U3. Wiersz zużycia z ikonami, bez pustych bloków

**Goal:** `razem … · ✨ … (◈ N …) · ⚡ …` bez `—`.
**Requirements:** R9, R10, R11, R12; AE4, AE5
**Dependencies:** brak (niezależne)
**Files:** `src/client/task_usage_text.rs`, `src/tui/task_app/detail.rs`, `tests/integration/tui_render.rs`
**Approach:**
1. Przepisać `line` na bloki (KTD5): `razem` tylko gdy znany jest koszt łączny (A6); `✨` z kosztu i tokenów `total`, części bez danych pominięte; `◈ N` z kosztem tylko gdy `Some`, potem `↑`/`↓`; `⚡` tak samo.
2. Zwracać pusty `String` albo `Option<String>`, gdy brak bloków; `header` w `detail.rs` nie dodaje wtedy wiersza (R11).
3. `money`/`tokens` dostosować do bloków bez `—`; zostawić `step_cells` bez zmian (R11). Usunąć stałą `UNKNOWN`, bo po przepisaniu `line` nic jej nie używa (zweryfikowane: `step_cells` jej nie używa).
3a. Dodać do `TaskUsage` pole `subagent_cost_usd: Option<f64>`, ustawiane na `None` w `sum` (protokół nie niesie kosztu, D1) i czytane tylko przez `line`. Test `◈ 2 $12` buduje `TaskUsage` ręcznie. Pole jest miejscem na koszt, gdy harness go poda.
4. Zaktualizować komentarze `line` i modułu `detail.rs`.
5. R12 zostaje w `header` (KTD6).
**Patterns to follow:** istniejące `scaled`, `add`, testy `task_usage_text`.
**Test scenarios:**
- Pełne dane (przykład z R9): `razem $68.30 · ✨ $34.15 ↑304 ↓72k (◈ 2 ↑100 ↓20k) · ⚡ $34.15 ↑304 ↓72k` (dla kosztu subagentów `Some` także `◈ 2 $12 ↑100 ↓20k`; ten przypadek tylko przez dane testowe, bo protokół kosztu nie niesie, D1).
- Covers AE4. Są koszt i tokeny agentów, brak dyrygenta i subagentów: tylko `razem $… · ✨ $… ↑… ↓…`.
- Covers AE5. Dwóch subagentów, tokeny, brak kosztu: `(◈ 2 ↑304 ↓72k)` bez `—`.
- Tylko koszt: `razem $0.50 · ✨ $0.50`; tylko tokeny: `✨ ↑1.5k ↓20` bez `razem` (A6).
- Subagenci z zerem tokenów i bez licznika: brak bloku `◈`.
- Brak jakichkolwiek danych: brak wiersza zużycia w nagłówku (liczba linii nagłówka mniejsza o 1).
- Szeroki vs wąski terminal: najpierw znika `◈`, potem tekst z `…` (adaptować asercje ~2435–2479).
- `step_cells` bez zmian (istniejące testy przechodzą).
**Verification:** w wierszu zużycia nigdzie nie ma `—`; w komórkach kroków działają jak dotąd.

---

## Verification Contract

- `cargo test --workspace` przechodzi, w tym `tests/integration/tui_render.rs` i `tests/integration/cli_task_text.rs` bez zmian asercji CLI.
- `cargo clippy --workspace` i `cargo fmt --check` bez nowych ostrzeżeń (komendy z `CONTRIBUTING.md`).
- Ręczne obejrzenie nagłówka w TUI (np. przez skill `run`) dla zadania z workspace, PR i zużyciem, w trybie z kolorem i z `NO_COLOR`.

## Definition of Done

- R1–R12 i AE1–AE5 spełnione, każdy AE ma test z `Covers AE<N>`.
- Jedna stała długości id; brak `8` dla id zadania w ramce i liście.
- CLI (`task_text`) i zakładka Info bez zmiany zachowania.
- Komentarze modułów w `detail.rs` i `workspace_rows.rs` pokazują nowy układ.
- Brak martwego kodu: nieużywane `UNKNOWN`, stare `Part::Changes` i `SHORT_ID_CHARS` usunięte.
