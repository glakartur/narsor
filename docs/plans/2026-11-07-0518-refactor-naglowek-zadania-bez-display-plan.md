---
title: Nagłówek zadania bez sekcji display - stały widok repozytoriów, gałęzi i PR
type: refactor
date: 2026-11-07
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Nagłówek zadania bez sekcji display - stały widok repozytoriów, gałęzi i PR

## Goal Capsule

- **Objective:** Operator w szczegółach zadania widzi zawsze ten sam nagłówek: tytuł, status z krokiem i workflow oraz każde repozytorium zadania z gałęzią roboczą, gałęzią bazową i linkiem do PR. Autor workflow nie definiuje już widoku.
- **Means:** Usunąć sekcję `display` ze schematu workflow. Dodać stałe, systemowe wartości `context.workspace.<nazwa>.{repo,branch,base_branch,pr,pr_url}`. Nagłówek TUI i CLI czyta tylko te wartości, nie `context.variables.*`.
- **Product authority:** Zmiana schematu workflow, protokołu migawki (snapshot) runnera, nagłówka TUI i tekstu zadania w CLI. Dokument powstał bez dialogu z użytkownikiem (tryb nieinteraktywny). Każde przyjęte założenie jest w sekcji "Założenia do potwierdzenia".
- **Open blockers:** Brak twardych blokad. Pytania Q1 i Q2 w sekcji "Pytania otwarte dla użytkownika" zmieniają kształt R6 i R7 oraz założenie A4; plan może ruszyć z założeniami A3 i A4.

---

## Product Contract

### Summary

Sekcja `display` znika z definicji workflow. TUI dostaje stały nagłówek szczegółów: tytuł zadania, wiersz statusu (`status · krok N nazwa · workflow`) i po jednym wierszu na każdy workspace z definicji workflow (nazwa, gałąź → gałąź bazowa, numer PR z linkiem). Dane nagłówka pochodzą z nowej przestrzeni `context.workspace.<nazwa>.*`. Agent nadal ustawia `context.variables.pr_url`, a wyjście kroku (`exit` / choice) przenosi wartość do `context.workspace.<nazwa>.pr_url`.

### Problem Frame

Workflow opisuje proces backendowy, ale dziś zawiera też widok: `display.title` i `display.links` z szablonami (`docs/dev/workflow-schema.md` §3.6). Każdy autor workflow musi powtarzać ten sam blok (tytuł, link PR). Inny frontend (np. web) potrzebowałby innego widoku, a definicja workflow narzuca jeden. Link PR zależy od dowolnej nazwy zmiennej (`pr_url`, `pr_number`), więc nagłówek nie ma stałego kontraktu. Workflow z kilkoma repozytoriami (schema v2, `globals.workspaces`) nie ma dziś żadnego miejsca na PR każdego repozytorium osobno.

### Key Decisions

- **Widok należy do klienta, nie do workflow.** Każdy frontend sam decyduje, jak pokazać zadanie; workflow daje tylko dane. (session-settled: user-directed — chosen over keeping `display` in the workflow: workflow defines a backend task and different frontends may need different views.) Governs R1, R2, R11.
- **Nagłówek czyta tylko stałe ścieżki `context.workspace.<nazwa>.*`, nigdy `context.variables.*`.** Kontrakt nagłówka nie zależy od nazw zmiennych wybranych przez autora workflow. (session-settled: user-directed — chosen over rendering header from free context variables: the header must rely on fixed paths.) Governs R4, R5, R12.
- **Agent ustawia zwykłą zmienną, wyjście kroku przenosi ją do `context.workspace`.** Agent nie musi znać ścieżki workspace. (session-settled: user-directed — chosen over agents writing `context.workspace.*` directly: the agent should not need to know the path.) Governs R7, R8.
- **Przeniesienie jest jawne w definicji kroku (rozszerzenie `set` na wyjściu kroku), a nie automatyczne.** Workflow z kilkoma repozytoriami musi wskazać, do którego workspace należy PR; automatyczna reguła musiałaby zgadywać. Wariant automatyczny jest w Q1. Governs R7.

### Requirements

**Usunięcie sekcji `display`**

- R1. Schemat workflow nie ma sekcji `display`; dokumentacja schematu (§2, §3.6, zakres szablonów) i `workflows/template.yaml` jej nie zawierają.
- R2. Plik workflow, który nadal ma `display`, ładuje się poprawnie; loader pomija sekcję i zapisuje ostrzeżenie z nazwą pliku i informacją, że sekcja jest ignorowana (patrz A1).
- R3. Tytuł zadania w liście i w szczegółach to `task.title` (tytuł operatora, w przeciwnym razie tytuł od dyrygenta), a gdy go brak — identyfikator zadania. Nie ma już źródła tytułu z workflow.

**Dane `context.workspace`**

- R4. Dla każdego workspace z `globals.workspaces` (kolejność z pliku) zadanie ma wartości `context.workspace.<nazwa>.repo`, `.branch`, `.base_branch`, `.pr`, `.pr_url`. Workflow v1 po konwersji ma jeden workspace (nazwa wg §3.2.1).
- R5. `repo` i `base_branch` ustawia system przy tworzeniu zadania (repozytorium z rejestru lub wybrane w formularzu; gałąź bazowa z definicji, w przeciwnym razie `default_branch`). Workflow i agent nie mogą ich zmienić.
- R6. `branch` to aktualna gałąź robocza repozytorium. Na starcie ma wartość gałęzi startowej zadania. System odświeża ją ze stanu git workspace po każdym kroku (patrz A3, Q2). Runner klonuje repozytorium z odłączonym HEAD (detached), więc gdy po kroku HEAD jest odłączony albo odczyt git się nie udał, `branch` zachowuje ostatnią wartość (na starcie: gałąź startowa).
- R7. Wyjście kroku (`exit` lub choice) może zapisać `pr` i `pr_url` wybranego workspace przez `set`, np. `set: { workspace: { code: { pr_url: "{{ context.pr_url }}" } } }`. `set` jest dostępne tylko w kształcie unified (§3.7.1, §3.7.5): plik v1 i plik v2 w starym kształcie nie mogą zapisać `pr`/`pr_url`. Nazwa workspace musi istnieć w `globals.workspaces`; walidator odrzuca nieznaną nazwę i klucz inny niż `pr`/`pr_url`.
- R8. `context.workspace.<nazwa>.*` jest dostępne do odczytu w szablonach (`{{ context.workspace.code.pr_url }}`) i w `when`. Brak wartości daje pusty tekst, jak dla zadeklarowanej zmiennej bez wartości (§ "Scope of templates").
- R9. Nazwa `workspace` jest zarezerwowana: zmienna kroku o tej nazwie jest błędem ładowania, bo skrót `context.NAME` kolidowałby z `context.workspace.*`.
- R10. `pr_url` w nagłówku jest linkiem tylko, gdy to bezwzględny adres `http`/`https` z hostem; inne wartości są pokazane bez linku. Znaki sterujące są usuwane ze wszystkich wartości przed wyświetleniem (te same reguły co dziś w `TaskDisplay::sanitized`).

**Nagłówek szczegółów w TUI**

- R11. Nagłówek szczegółów ma stały układ (widok poniżej):
  - wiersz 1: tytuł zadania (R3) i krótki identyfikator, jak dziś;
  - wiersz 2: `● status · krok N nazwa_kroku · workflow vX`, potem obecne dodatki (czas, `bez slotu`, `kontroler offline`);
  - wiersze workspace: po jednym na workspace, w kolejności z definicji workflow;
  - wiersz zużycia: bez zmian, gdy harness podał zużycie.
- R12. Wiersz workspace pokazuje: nazwę workspace, `branch → base_branch` i `PR <numer>` z linkiem OSC 8 do `pr_url`. Kolumny są wyrównane między wierszami.
- R13. Gdy `branch` równa się `base_branch`, wiersz pokazuje tylko jedną nazwę gałęzi (zmiana nie ma jeszcze gałęzi roboczej).
- R14. Gdy nie ma PR, w miejscu PR stoi wyszarzony znak `—`. Gdy jest `pr_url`, a nie ma `pr`, numer jest brany z końcowego segmentu liczbowego adresu; gdy go brak, pokazany jest tekst `PR ↗`.
- R15. Workspace typu lokalny folder (`path`) pokazuje nazwę i ścieżkę zamiast gałęzi; nie ma kolumny PR.
- R16. Przy wąskim terminalu kolejność skracania: najpierw `base_branch`, potem `branch` (obcięta z `…`), nigdy nazwa workspace i numer PR.
- R17. Gdy workspace jest więcej niż 3, nagłówek pokazuje 3 pierwsze i wiersz `+N więcej — Info`; zakładka *Info* pokazuje pełną listę z `repo`.
- R18. Zadanie w kolejce (bez runnera) pokazuje wiersze workspace z danymi z tworzenia zadania (`repo`, gałąź startowa, `base_branch`), bez PR.

**CLI**

- R19. Tekst zadania w CLI (`task_text`) pokazuje te same dane co nagłówek TUI (tytuł, status z krokiem i workflow, wiersz na workspace z linkiem PR) zamiast linii z `display`.

### Widok nagłówka

```text
NAR-12  Zmiana API zamówień                                      a1b2c3d4
● running · krok 3 implement · code-review v3 · 14m
  code    feat/nar-12-orders → main          PR !42 ↗
  docs    main                               —
$1.20 · in 120k out 8k · w tym subagenci in 40k out 3k (2)
────────────────────────────────────────────────────────────────────────
 Przebieg   Rozmowa   Zdarzenia   Dyrygent   Info                 podąża
```

Wąski terminal (R16):

```text
● running · krok 3 implement · code-review v3
  code    feat/nar-12-o… → …    PR !42 ↗
  docs    main                  —
```

### Przepływ wartości PR

```mermaid
flowchart LR
  A[Agent kroku open_pr] -->|variables.pr_url| V[context.variables.pr_url]
  V -->|exit.set.workspace.code.pr_url| W[context.workspace.code.pr_url]
  S[System: tworzenie zadania] -->|repo, base_branch| W
  G[System: stan git po kroku] -->|branch| W
  W --> T[Nagłówek TUI]
  W --> C[Tekst zadania CLI]
```

### Acceptance Examples

- AE1. **Covers R7, R12.** **Given** workflow z workspace `code` i `docs` oraz krokiem `open_pr` z `exit.set.workspace.code.pr_url: "{{ context.pr_url }}"`. **When** agent zwraca `pr_url = https://gitlab.example/g/api/-/merge_requests/42`. **Then** wiersz `code` pokazuje `PR !42 ↗` z linkiem, wiersz `docs` pokazuje `—`.
- AE2. **Covers R7.** **Given** `set.workspace.payments.pr_url` w workflow bez workspace `payments`. **When** workflow jest ładowany. **Then** ładowanie kończy się błędem z nazwą kroku i nazwą `payments`.
- AE3. **Covers R2.** **Given** stary plik z sekcją `display`. **When** jest ładowany. **Then** workflow działa, sekcja jest pominięta, w logu jest ostrzeżenie.
- AE4. **Covers R13, R6.** **Given** nowe zadanie na `main`, krok jeszcze nie utworzył gałęzi. **Then** wiersz pokazuje `main`. **When** krok tworzy `feat/x` i się kończy. **Then** wiersz pokazuje `feat/x → main`.
- AE5. **Covers R14.** **Given** `pr_url = https://github.com/o/r/pull/7` i brak `pr`. **Then** wiersz pokazuje `PR #7 ↗`. **Given** `pr_url = https://host/review/abc`. **Then** wiersz pokazuje `PR ↗`.
- AE6. **Covers R10.** **Given** agent zwraca `pr_url = javascript:alert(1)` i wyjście kopiuje go do workspace. **Then** nagłówek pokazuje tekst bez linku.
- AE7. **Covers R9.** **Given** krok deklaruje zmienną `workspace`. **Then** ładowanie kończy się błędem.

### Scope Boundaries

- Poza zakresem: konfiguracja widoku po stronie klienta (motywy, układ wybierany przez użytkownika).
- Poza zakresem: automatyczne wykrywanie PR przez API GitLab/GitHub; PR podaje tylko workflow (R7).
- Poza zakresem: inne pola `context.workspace` niż pięć z R4 (np. status pipeline, commit).
- Poza zakresem: zmiana listy zadań poza źródłem tytułu (R3).
- Odłożone: link Jira z `task.ticket` w nagłówku. Dziś możliwy przez `display.links`; po zmianie ticket nadal stoi w wierszu 1 bez linku (patrz Q4).

---

## Założenia do potwierdzenia

- A1. **Stary plik z `display` nie przestaje działać.** Loader pomija sekcję z ostrzeżeniem (R2), zamiast zgłosić błąd. Powód: bezpieczniej dla istniejących workflow użytkowników. Alternatywa: błąd ładowania w `schema: v2` (ostrzejszy kontrakt).
- A2. **Krok w wierszu 2 używa obecnej numeracji** (`krok N` z pozycji osi czasu w *Przebiegu*) i identyfikatora stanu jako nazwy kroku. Pokazuję `krok N nazwa` zamiast obecnego `krok N/M`, bo przy pętlach w grafie `M` jest mylące. Nazwa workflow to `workflow_id vX`, jak dziś w wierszu 3.
- A3. **`branch` odświeża system ze stanu git po każdym kroku**, a nie agent. Powód: nagłówek ma nie zależeć od zmiennych ustawianych przez agenta, a Narsor nie tworzy gałęzi roboczej sam (§3.2.1: robi to krok). Dla zdalnego runnera dane idą w migawce (snapshot) tak jak dziś `display`.
- A4. **Do zapisu przez `set` dostępne są tylko `pr` i `pr_url`.** `repo`, `base_branch` i `branch` są tylko do odczytu (R5, R6).
- A5. **Numer PR bez prefiksu zależnego od dostawcy w danych; prefiks `!` dla adresu GitLab (`/merge_requests/`), `#` w pozostałych przypadkach** — tylko w prezentacji. Na szkicu: `PR !42`.
- A6. **Nagłówek pokazuje nazwę workspace, nie identyfikator repozytorium** (zgodnie z prośbą). `repo` jest w zakładce *Info* (R17). Dla workflow z jednym workspace nazwa po konwersji v1 to i tak identyfikator repozytorium.
- A7. **Limit 3 wierszy workspace w nagłówku** (R17), żeby nagłówek nie zabierał miejsca zakładkom w workflow z wieloma repozytoriami.
- A8. **CLI dostaje ten sam układ danych co TUI** (R19), bo dziś też czyta `display`; inaczej po usunięciu sekcji CLI straciłby link PR.
- A9. **Starszy runner/serwer, który jeszcze raportuje `display`, jest ignorowany przez nowego klienta** — nowy klient pokazuje wtedy wiersze workspace z danych tworzenia zadania (R18). Szczegóły kompatybilności protokołu ustala plan.

## Pytania otwarte dla użytkownika

- Q1. **Jawne `set` czy automatyczne kopiowanie?** Plan zakłada jawne `exit.set.workspace.<nazwa>.pr_url` (Key Decisions). Alternatywa: silnik po każdym kroku sam kopiuje `context.variables.pr_url` (i `pr`) do workspace, w którym krok startuje (`workspaces.root`, domyślnie primary). Jest prostsza dla autora workflow, ale niejednoznaczna dla `root: .` i kroku, który otwiera PR w dwóch repozytoriach. Którą wersję wybierasz? *(Resolve Before Planning — zmienia R7.)*
- Q2. **Skąd `branch`?** Plan zakłada odczyt stanu git przez system (A3). Czy wolisz, żeby `branch` też przychodził od agenta przez `set` (jak `pr_url`)? *(Resolve Before Planning — zmienia R6 i A4.)*
- Q3. **Plik z `display`: ostrzeżenie czy błąd?** (A1). *(Deferred to Planning, jeśli brak odpowiedzi — zostaje ostrzeżenie.)*
- Q4. **Link Jira w nagłówku.** Dziś workflow może dać link do ticketu przez `display.links`. Czy nagłówek ma budować link z `task.ticket` według ustawienia klienta (np. bazowy adres Jira w konfiguracji TUI), czy ticket zostaje bez linku? *(Deferred to Planning — domyślnie bez linku.)*

### Deferred to Planning

- Jak dane `context.workspace` trafiają do `TaskSummary` (pole w migawce runnera vs widok serwera) i jak to wpływa na wersję protokołu i launch spec.
- Czy `TaskDisplay` / `DisplaySpec` w `narsor-protocol` znika całkowicie, czy zostaje tylko jako zgodność wsteczna odczytu.
- Jak walidator rozszerza `set` o klucz zarezerwowany `workspace` (wzór: istniejący klucz `local` w §3.7.5).

---

## Sources / Research

- `docs/dev/workflow-schema.md` §3.6 (obecny `display`), §3.2.1 (`globals.workspaces`, `base_branch`, "Narsor nie tworzy gałęzi roboczej"), §3.7.5 (`set` z kluczem `local`), sekcja "Scope of `{{ }}` templates".
- `crates/narsor-protocol/src/display.rs` — `DisplaySpec`, `TaskDisplay`, `sanitized`, `safe_link_url`.
- `crates/narsor-engine/src/workflow/template.rs` (`render_display`), `crates/narsor-engine/src/workflow/validator.rs` (walidacja `display.links`), `crates/narsor-runner/src/runner/snapshot.rs` (raport `display` w migawce).
- `src/tui/task_app/detail.rs` — funkcja `header` (obecne wiersze 1–4, `LinkSpot`, OSC 8), `status_parts`; `src/tui/task_app/list.rs` — `title_of` czyta `display.title`.
- `src/cli/task_text.rs` — `display_title`, `display_lines`.
- `crates/narsor-protocol/src/controller.rs` — `TaskSummary` (dziś bez listy repozytoriów), `WorkspaceRequest`, `WorkflowWorkspaceView`.
- `workflows/template.yaml` — przykład z `display`.
- Poprzednie plany: `docs/plans/2026-11-05-0033-feat-workflow-multirepo-plan.md`, `docs/plans/2026-11-05-1818-feat-branch-repozytorium-w-zadaniu-plan.md`, `docs/plans/2026-11-06-0745-refactor-schemat-workflow-v2-plan.md`.

## Deferred / Open Questions

Te pozycje to wyniki recenzji pozostawione autorowi. Nie blokują implementacji.

### From 2026-11-07 review

- **Goal Capsule mówi "brak blokad", a Q1 i Q2 mają znacznik "Resolve Before Planning"** — Goal Capsule, Open blockers (P1, coherence, confidence 100)

  Wykonawca nie wie, czy planowanie może ruszyć. Odpowiedź na Q2 może unieważnić założenia A3 i A4, na których plan ma ruszyć.
  Change: wybrać jedną wersję — albo Q1 i Q2 są blokadami planowania (Goal Capsule to mówi), albo zmienić ich znacznik na "Deferred to Planning" z domyślnymi A3 i A4.

- **Workflow v1 i v2 w starym kształcie tracą link PR bez widocznego sygnału** — R2, A1 (P1, adversarial, feasibility, confidence 100)

  Dziś te pliki pokazują PR przez `display.links`. Po zmianie nagłówek pokazuje `—`, a ostrzeżenie w logu mówi tylko, że sekcja jest pominięta. Autor nie wie, co dodać.
  Change: rozszerzyć ostrzeżenie z R2: gdy pominięta sekcja `display` ma link, ostrzeżenie mówi, że link PR wymaga `exit.set.workspace.<nazwa>.pr_url`, a dla pliku v1 lub starego kształtu — przejścia na kształt unified. Alternatywa: automatyczne kopiowanie z Q1, które działa też dla tych plików.

- **Brak reguły wyświetlania `pr_url`, który nie jest linkiem** — R10, R14, AE6 (P2, design-lens, confidence 75)

  "Tekst bez linku" nie mówi, co stoi w kolumnie PR. Surowa, długa wartość od agenta psuje wyrównanie kolumn, a R16 zabrania skracania kolumny PR.
  Change: wartość, która nie jest bezwzględnym adresem `http`/`https`, nigdy nie jest pokazana dosłownie; kolumna PR pokazuje numer z `pr` albo `PR` bez znaku `↗` i bez linku. AE6 oczekuje `PR` bez linku.

- **Limit 3 wierszy workspace przeczy celowi "każde repozytorium"** — R17, A7, Summary (P2, coherence, scope-guardian, confidence 100)

  Objective i Summary obiecują wiersz dla każdego workspace, a R17 ukrywa czwarty i dalsze za wierszem `+N więcej — Info`. Wykonawca musi wybrać jedną z dwóch reguł.
  Change: decyzja użytkownika — zostawić limit (i dopisać go do Summary i Objective) albo usunąć limit z R17 i A7.

