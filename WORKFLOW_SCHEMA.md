# Workflow Schema Specification

## 1. Przegląd

Ten dokument definiuje deklaratywny format plików YAML używanych do opisywania workflow w AI Workflow Narsor. Format został zaprojektowany tak, aby był czytelny dla ludzi, łatwy do walidacji maszynowej i wspierał kluczowe funkcje systemu: deterministyczne przejścia stanów, izolację workspace'ów, integrację z agentami AI oraz human-in-the-loop.

### 1.1. Zasady projektowe

*   **Statyczny Graf:** Definicja workflow jest niezmienna podczas wykonania. Nie można dynamicznie dodawać stanów ani przejść.
*   **Jeden Stan = Jedna Akcja:** Każdy stan wykonuje dokładnie jedną akcję (`AgentAction`, `ScriptAction`, `HumanAction`, `WaitAction`).
*   **Deterministyczne Przejścia:** Przejścia między stanami są określane przez statyczne mapowanie etykiet wyjścia (`output labels`) do stanów docelowych. Nie ma dynamicznych wyrażeń warunkowych (np. `when: x > 5`).
*   **Strukturyzowane Wyjście:** Akcje zwracają ustrukturyzowany wynik JSON zawierający etykietę wyjścia (`output`), opcjonalne dane (`data`), zmienne do aktualizacji kontekstu (`variables`) oraz referencje do artefaktów (`artifacts`).
*   **Bezpieczeństwo przez Izolację:** Workspace jest definiowany na poziomie wykonania (execution) lub zadania (task), a nie zmieniany dynamicznie wewnątrz workflow. Dostęp do plików kontrolowany jest przez uprawnienia systemu plików i montowania.

---

## 2. Struktura Pliku Workflow

Plik workflow składa się z następujących sekcji:

```yaml
name: <string>              # Unikalna nazwa workflow (np. "code-review")
version: <integer>          # Wersja definicji (np. 1)
description: <string>       # Opis celu workflow (opcjonalne)

inputs:                     # Definica wejść wymaganych do uruchomienia (opcjonalne)
  - name: <string>
    type: <string>          # string, text, integer, boolean, repository
    required: <boolean>
    default: <any>          # Wartość domyślna (opcjonalne)

display:                    # Nagłówek zadania w TUI i CLI (opcjonalne, §3.6)
  title: <template>         # Np. "{{ inputs.ticket_id }}"
  links:
    - label: <string>       # Niepusta, unikalna w sekcji
      text: <template>      # Widoczny tekst (opcjonalne)
      url: <template>       # Tylko http/https po wyrenderowaniu

globals:                    # Ustawienia globalne dla całego workflow (opcjonalne)
  timeout: <duration>       # Np. "1h", "30m"
  retry_policy:             # Domyślna polityka retry dla akcji
    max_attempts: <int>
    backoff: <duration>
  workspace:                # Domyślna konfiguracja workspace
    type: <git|local>
    repository: <string>    # ID repozytorium z registry (dla typu git)
    ref: <string>           # Branch/tag/commit (dla typu git)
    path: <string>          # Ścieżka względna w folderze taska
    preserve_paths:         # Ścieżki ignorowane przez repo, które mimo to mają przetrwać do checkpointu (git i local); nie może to być plik z sekretami/poświadczeniami (checkpoint jest niezaszyfrowany)
      - <string>
  image: <image>            # Obraz bazowy zdalnego taska, nadpisuje obraz repozytorium (opcjonalne)

states:                     # Graf stanów (wymagane)
  <state_id>:
    description: <string>   # Opis stanu (opcjonalne)
    action:                 # Definicja akcji do wykonania
      type: <agent|script|human|wait>
      # ... konfiguracja specyficzna dla typu akcji
    outputs:                # Lista możliwych etykiet wyjścia (wymagane dla agent/script)
      - <label_1>
      - <label_2>
    transitions:            # Mapowanie etykiet na stany docelowe (wymagane)
      <label_1>: <next_state_id_1>
      <label_2>: <next_state_id_2>
    decision:               # Konfiguracja dostawcy decyzji (opcjonalne, domyślnie rules)
      type: <rules|agent|human>
      # ... konfiguracja specyficzna dla typu decyzji
    timeout: <duration>     # Timeout dla tego stanu (nadpisuje globalny)
    retry_policy:           # Polityka retry dla tej akcji (nadpisuje globalną)

terminal_states:            # Stany terminalne (kończące workflow) i ich status
  <state_id_1>: completed   # completed | failed | cancelled
  <state_id_2>: failed      # Forma listowa też działa — każdy wpis = completed
```

---

## 3. Szczegółowa Specyfikacja Sekcji

### 3.1. Inputs (Wejścia)

Definiuje parametry, które muszą zostać dostarczone przy uruchamianiu wykonania (`WorkflowExecution`). Są one dostępne w kontekście workflow jako `inputs.<name>`.

**Przykład:**
```yaml
inputs:
  - name: task_description
    type: string
    required: true
    description: "Opis zadania do wykonania przez agenta"
  
  - name: target_repository
    type: repository
    required: true
    description: "Repozytorium, w którym mają być wprowadzone zmiany"
  
  - name: is_experimental
    type: boolean
    required: false
    default: false
```

Typy wejść: `string` (krótki tekst jednoliniowy), `text` (tekst wieloliniowy — dla CLI/API to zwykły string, w TUI duże pole z fokusem; używaj go dla promptów i opisów), `integer`, `boolean`, `repository`. Formularz nowego zadania w TUI pokazuje wejścia `string`/`integer`/`boolean`/`repository` w wąskim pasku, a wejścia `text` w polach wieloliniowych; workflow bez wejścia `text` ma tylko pasek.

### 3.2. Globals (Ustawienia Globalne)

Opcjonalna sekcja określająca domyślne zachowanie dla całego workflow.

*   `timeout`: Maksymalny czas życia pojedynczego wykonania.
*   `retry_policy`: Domyślne zasady ponawiania nieudanych akcji.
    *   `max_attempts`: Liczba prób (domyślnie 1).
    *   `backoff`: Czas oczekiwania między próbami (np. "10s").
*   `workspace`: Domyślna konfiguracja środowiska pracy.
    *   `type`: `git` (wymaga `repository`; `ref` jest opcjonalne) lub `local` (wymaga `path`).
    *   `repository`: ID repozytorium zarejestrowanego w systemie.
    *   `ref`: Gałąź, tag lub commit. Ma pierwszeństwo przed gałęzią z konfiguracji repozytorium. Bez `ref` zadanie pobiera tę gałąź, a bez niej `HEAD` zdalnego repozytorium.
    *   `path`: Ścieżka do folderu lokalnego (dla typu `local`).
    *   `preserve_paths` (dla obu typów, `git` i `local`): Lista ścieżek, które mają trafić do
        checkpointu mimo że `.gitignore` repozytorium je ignoruje — jedyny wyjątek od zasady
        "pliki ignorowane nie podróżują". Typowy przypadek to plik wygenerowany przez jeden krok
        i potrzebny kolejnemu, np. plik blokady (`lockfile`) zapisany przez krok instalacyjny:
        `preserve_paths: ["build/deps.lock"]`. Dotyczy to zarówno repozytorium sklonowanego przez
        `git`, jak i zwykłego folderu lokalnego — mechanizm checkpointu
        (`CaptureRequest::preserve`) działa identycznie w obu przypadkach. Wszystko inne worker,
        który przejmuje wykonanie, odtwarza sam. Ścieżka wskazana tutaj podróżuje do bundla
        checkpointu bez szyfrowania — **nie wolno** wskazywać w `preserve_paths` pliku
        zawierającego sekrety lub poświadczenia (np. `.env`), bo to właśnie ten plik trafi w
        jawnej postaci do checkpointu (R33).
*   `image`: Obraz bazowy, w którym zdalny backend uruchamia task; nadpisuje `base_image`
    repozytorium z registry. Krok może go nadpisać własnym polem `image` (pierwszeństwo:
    krok > workflow > repozytorium). Zapis jako referencja z digestem
    (`ghcr.io/acme/dev@sha256:…`) albo jako mapa
    `{ reference: ghcr.io/acme/dev:2026.10, digest_policy: resolve }` — wtedy tag jest
    zamieniany na digest raz, przy tworzeniu taska. Domyślna polityka `pinned` wymaga
    digestu; sam tag (lub niejawne `latest`) jest błędem walidacji. Wybrany obraz, jego
    digest i źródło trafiają do `ResolvedEnvironment` taska i nie zmieniają się po późniejszej
    zmianie definicji.

### 3.3. States (Stany)

Serce definicji workflow. Każdy klucz w sekcji `states` to unikalny identyfikator stanu.

#### Struktura Stanu:

```yaml
<state_id>:
  description: <string>               # Opisowy tekst
  action:                             # Konfiguracja akcji
    type: <agent|script|human|wait>
    # ... szczegóły akcji
  outputs:                            # Lista dopuszczalnych etykiet wyjścia
    - success                         # Goła etykieta
    - failure
    - label: needs_human               # Etykieta z opisem (opcjonalnym)
      description: Agent potrzebuje decyzji człowieka, zanim pójdzie dalej.
      requires_human_approval: true    # tylko wyjście akcji agent
  transitions:                        # Mapowanie: etykieta -> następny stan
    success: next_state
    failure: error_handler
    needs_human: human_review
  decision:                           # Sposób wyboru przejścia (opcjonalne)
    type: <rules|agent|human>
    # ... szczegóły decyzji
  timeout: <duration>                 # Nadpisanie globalnego timeoutu
  retry_policy:                       # Nadpisanie globalnej polityki retry
    max_attempts: 3
    backoff: 5s
  max_entries: 5                     # limit wejść; oczekiwanie na decyzję operatora
  image: <image>                      # Nadpisanie obrazu bazowego dla tego kroku (§3.2)
```

#### Etykiety wyjścia (`outputs`)

Każdy element listy `outputs` ma jedną z dwóch form, które można mieszać w
jednym stanie:

*   **goła etykieta** — sam napis, np. `- success`;
*   **obiekt** — `{label, description}`: `label` jest wymagany, `description`
    opcjonalny. Nieznany klucz (np. literówka `descripton`) albo brak `label`
    to błąd ładowania wskazujący stan.

Opis jest jedynym źródłem wyjaśnienia, co znaczy dana etykieta. Silnik pokazuje
go wszędzie tam, gdzie etykieta jest do wyboru: w kontrakcie wyjścia doklejanym
do promptu agenta (§3.3.A), w oczekującym inpucie (`pending_input.json`) i w
menu ręcznego przejścia TUI (`t`). Pusty opis znaczy tyle co jego brak.
Etykieta może wystąpić w `outputs` tylko raz, niezależnie od formy (§5).

Obiekt wyjścia może też zawierać `requires_variables`: listę nazw zmiennych
wymaganych dla tej etykiety. Element może być nazwą (`- plan_doc`) albo
obiektem `{name, description}`. Brak klucza, `null` i pusty tekst są
odrzucane. Wymagania pojawiają się w kontrakcie agenta i formularzu operatora.

Obiekt wyjścia może też wyrażać dwa opcjonalne zachowania sterujące:

* `requires_human_approval: true` zatrzymuje wynik `agent` przed jego zwykłym
  przejściem. Operator jawnie go akceptuje albo odsyła do tej samej sesji agenta
  z wymaganym komentarzem.
* `resume_comment_field: <nazwa>` jest dostępne wyłącznie na wyjściu akcji
  `human`. Nazwany input musi być wymaganym polem `text`, a przejście tego
  wyjścia musi prowadzić do stanu `agent`; jego tekst jest wiadomością przy
  wznowieniu ostatniej sesji docelowego agenta.

#### Typy Akcji (Action Types):

##### A. AgentAction

Uruchamia agenta AI (np. Claude Code, Codex, OpenCode).

```yaml
action:
  type: agent
  harness: <claude-code|codex|opencode> # Opcjonalne — bez niego harness instalacji:
                                      # wybór operatora (`narsor harness default set`, `h` w TUI),
                                      # inaczej konfiguracja (`agent_harness`, patrz README →
                                      # Configuration), a w jej braku `claude-code`.
                                      # Tylko jawne `harness:` przypina akcję; bez niego akcja
                                      # bierze domyślny przy pierwszej próbie i trzyma go do końca.
  model: <string>                     # Model AI (opcjonalne, np. "sonnet-3.5")
  workspace_access: <read-write|read-only> # Opcjonalne; domyślnie read-write.
  prompt: |                           # Instrukcja dla agenta
    Analizuj kod w bieżącym katalogu.
    Znajdź potencjalne błędy bezpieczeństwa.
    Zapisz raport do pliku security_report.md.
  
  # Opcjonalne ustawienia specyficzne dla harnessa, niezwiązane z dostępem
  # do workspace
  parameters:
    max_tokens: 4096
    temperature: 0.7
  
  # Foldery dodatkowe (tylko do odczytu)
  read_only_paths:
    - ../other-repo/src               # Ścieżki względem workspace
```

**Wymagane wyjścia (outputs):**
Agent musi zwrócić JSON z kluczem `output` odpowiadającym jednej z zadeklarowanych etykiet.

**Kontrakt wyjścia budowany przez silnik:** do każdego promptu agenta silnik
dokleja kontrakt wyjścia: format końcowego obiektu JSON oraz listę dozwolonych
etykiet stanu, w kolejności deklaracji w `outputs`, każdą z opisem po myślniku,
jeśli workflow go podał:

```text
`output` is required and must be exactly one of these labels:
- `success` — Zadanie wykonane, raport zapisany.
- `failure`
```

Prompt stanu nie musi więc (i nie powinien) powtarzać listy etykiet — wystarczy
opisać je w `outputs`. Instrukcje dotyczące `data` (np. „podaj pytanie w
`data.question`”) nadal należą do promptu, bo kontrakt mówi tylko o etykietach.

Przykładowy wynik agenta:
```json
{
  "output": "success",
  "data": {
    "findings_count": 3
  },
  "variables": {
    "security_level": "high"
  },
  "artifacts": ["security_report.md"]
}
```

**Pytanie agenta bez przejścia:** jeżeli agent potrzebuje odpowiedzi człowieka,
ale workflow nie ma jeszcze przejść do wykonania, może zamiast wyniku zwrócić:

```json
{"status":"awaiting_input","question":"Którą wersję API mam obsłużyć?","options":["v1","v2"]}
```

Execution przechodzi wtedy do statusu `input_needed`, ale pozostaje w tym samym
stanie `agent`. W TUI klawisz `i` jest dostępny wyłącznie dla takiego kroku
agenta/harnessu: można podać dowolną odpowiedź albo wybrać opcję przedstawioną
przez harnessa, opcjonalnie z komentarzem. Nie wybiera on wyjścia workflow;
wybór zadeklarowanego przejścia bramy `human` odbywa się przez `t`.
Odpowiedź przez `input` uruchamia ponownie tę samą akcję z dodaną odpowiedzią.
Nie używaj tego wyniku dla błędów harnessa, sandboxa,
uprawnień ani narzędzi — są to błędy akcji i kończą execution ze statusem
`failed`, bez transition.

##### B. ScriptAction

Uruchamia skrypt powłoki lub binarkę.

```yaml
action:
  type: script
  command: ./run-tests.sh             # Polecenie do wykonania
  args:                               # Argumenty (opcjonalne)
    - "--verbose"
    - "--coverage"
  env:                                # Zmienne środowiskowe (opcjonalne)
    CI: "true"
    DB_HOST: "localhost"
  
  # Mapowanie kodów wyjścia na etykiety (opcjonalne, domyślnie 0=success, !=0=failure)
  exit_codes:
    0: success
    1: failure
    10: needs_human                   # Niestandardowy kod wyjścia
    124: timeout                      # Specjalny kod dla timeoutu procesu
```

**Uwaga:** Skrypt powinien wypisać valid JSON na stdout (lub do wskazanego pliku), jeśli wymaga się przekazu zmiennych/artefaktów. Jeśli skrypt nie zwraca JSON, narsor polega wyłącznie na kodzie wyjścia do podjęcia decyzji.

##### C. HumanAction

Wstrzymuje workflow i czeka na interakcję użytkownika.

```yaml
action:
  type: human
  message: "Proszę o review zmian."   # Komunikat wyświetlany w UI
  inputs:                             # Pola do wypełnienia przez użytkownika (opcjonalne)
    - name: approval_status
      type: select
      options: [approved, changes_requested, rejected]
      required: true
    - name: comments
      type: text
      required: false
```

**Wyjścia:** Etykiety wyjścia odpowiadają wartościom wybranym przez użytkownika (np. `approved`, `changes_requested`).

##### D. WaitAction

Wstrzymuje workflow do czasu wystąpienia zdarzenia zewnętrznego.

```yaml
action:
  type: wait
  event_type: github.pull_request.reviewed  # Typ zdarzenia
  filter:                                   # Filtr zdarzeń (opcjonalne)
    pr_number: "{{ context.pr_number }}"    # Template zmiennej kontekstowej
  timeout: 24h                              # Maksymalny czas oczekiwania
```

**Wyjścia:** Zazwyczaj `received` (gdy zdarzenie nastąpiło) lub `timeout` (gdy upłynął czas).

#### Zakres szablonów `{{ }}`

Szablony są podstawiane tuż przed wykonaniem akcji. Obsługiwane są następujące formy odwołań:

- `{{ inputs.NAZWA }}` — wejście workflow zadeklarowane w sekcji `inputs`,
- `{{ context.variables.NAZWA }}` — zmienna kontekstu, wraz ze skrótem `{{ context.NAZWA }}`, który znaczy dokładnie to samo.
- `{{ steps.ID.reply }}` — końcowa odpowiedź tekstowa udanej próby kroku agenta `ID`.

Odwołanie `steps.ID.reply` dozwolone jest tylko w promptach agentów i treści
akcji `human`; krok źródłowy musi istnieć i być akcją `agent`. Odpowiedź jest
zapisywana tylko dla kroków faktycznie użytych jako źródło. Brak wcześniejszej
udanej odpowiedzi jest błędem szablonu. `max_entries` jest opcjonalnym limitem
wejść w krok agentowy (wartość co najmniej 1); po osiągnięciu limitu workflow
czeka na decyzję operatora.

Białe znaki wewnątrz `{{ }}` są ignorowane. Nie ma filtrów (`| default(...)`), wyrażeń, warunków ani pętli — to świadomie prosty mechanizm podstawiania, a nie silnik szablonów. Odwołanie do nieistniejącego wejścia lub nieistniejącej zmiennej jest **błędem**, który przerywa akcję; narsor nigdy nie podstawia po cichu pustego stringa. Podstawianie jest jednoprzebiegowe: jeśli wartość zmiennej sama zawiera `{{ ... }}`, nie jest rozwijana ponownie.

Wartości nie-stringowe renderują się jako JSON: liczba i wartość logiczna bez cudzysłowów (`123`, `true`), lista i obiekt w postaci kompaktowej (`["a","b"]`).

---

### 3.4. Decision Providers (Dostawcy Decyzji)

Określa mechanizm wybierania przejścia na podstawie wyniku akcji. Domyślnie `rules`.

#### A. Rules (Reguły Statyczne)

Najprostszy i domyślny mechanizm. Przejście jest wybierane bezpośrednio na podstawie etykiety `output` zwróconej przez akcję.

```yaml
decision:
  type: rules
  # Brak dodatkowej konfiguracji. 
  # Przejście jest deterministycznie mapowane w sekcji 'transitions'.
```

#### B. Agent (Decyzja przez AI)

Uruchamia drugą sesję agenta AI, który analizuje wynik poprzedniej akcji i wybiera przejście. Przydatne w sytuacjach niejednoznacznych.

```yaml
decision:
  type: agent
  harness: claude-code
  prompt: |
    Przeanalizuj wynik poprzedniego kroku.
    Czy zmiany są bezpieczne do wdrożenia?
    Wybierz przejście: 'safe_to_merge', 'needs_more_tests', 'rollback'.
  
  # Agent decyzyjny MUSI wybrać jedną z etykiet zdefiniowanych w 'outputs' stanu.
```

#### C. Human (Decyzja przez Człowieka)

Wymusza ręczny wybór przejścia przez użytkownika w UI, nawet jeśli akcja zwróciła wynik.

```yaml
decision:
  type: human
  message: "Wybierz kolejny krok na podstawie wyników testów."
```

---

### 3.5. Terminal States (Stany Końcowe)

Deklaruje stany, których osiągnięcie kończy wykonanie workflow. Sekcja przyjmuje dwie formy.

**Forma mapująca (zalecana).** Każdy stan terminalny wprost deklaruje status, jaki nadaje wykonaniu:

```yaml
terminal_states:
  completed: completed
  failed: failed
  cancelled: cancelled
```

Dozwolone wartości to `completed`, `failed` i `cancelled` — odpowiadają one statusom `COMPLETED`, `FAILED` i `CANCELLED` encji `WorkflowExecution`. Inna wartość jest błędem wczytania; komunikat podaje nazwę stanu, błędną wartość i zbiór dozwolonych wartości.

**Forma listowa (zgodność wsteczna).** Zwykła lista identyfikatorów stanów:

```yaml
terminal_states:
  - completed
  - failed
```

W tej formie **każdy** wpis oznacza `completed` — lista nie niesie żadnej informacji o niepowodzeniu. Silnik nie zgaduje po nazwie stanu: stan nazwany `failed` na liście również zakończy wykonanie statusem `COMPLETED`. Zgadywanie po nazwie dawało cichą, błędną odpowiedź dla stanów takich jak `rejected_by_reviewer` czy `timed_out`, dlatego zostało usunięte. Jeżeli stan terminalny ma oznaczać niepowodzenie lub anulowanie, użyj formy mapującej.

Identyfikatory wymienione w `terminal_states` muszą odpowiadać istniejącym stanom (§5.1) — w obu formach.

---

### 3.6. Display (Nagłówek Zadania)

Opcjonalna sekcja mówi, jak zadanie przedstawia się operatorowi: tytuł i linki (zgłoszenie, pull request).

```yaml
display:
  title: "{{ inputs.ticket_id }}"
  links:
    - label: Jira
      text: "{{ inputs.ticket_id }}"
      url: "https://jira.example/browse/{{ inputs.ticket_id }}"
    - label: PR
      text: "!{{ context.variables.pr_number }}"
      url: "{{ context.variables.pr_url }}"
```

*   `title`, `links[].text` i `links[].url` są szablonami z tej samej składni co reszta workflow: `{{ inputs.NAZWA }}` i `{{ context.variables.NAZWA }}` (skrót `{{ context.NAZWA }}`). Nie ma filtrów.
*   **Łagodne renderowanie.** Inaczej niż w akcjach, nieznana albo jeszcze nieustawiona zmienna nie jest błędem: cały tekst, który ją zawiera, jest pusty. Tytuł pojawia się, gdy zadanie czeka w kolejce, a zmienne kontekstu (np. `pr_url`) powstają dopiero w trakcie pracy.
    *   Pusty `title` — klient pokazuje identyfikator zadania.
    *   Pusty `text` — link pokazuje `—`. Brak pola `text` — link pokazuje swój adres, a gdy go nie ma, `—`.
    *   Pusty `url` — link bez adresu (pokazany, ale nieaktywny).
*   **Adres.** Po wyrenderowaniu musi być bezwzględnym adresem `http` albo `https` z hostem. Inny schemat (`javascript:`, `file:` itd.) albo znaki białe i sterujące dają link bez adresu. Ze wszystkich tekstów są usuwane znaki sterujące.
*   **Dwa miejsca renderowania.** Control plane renderuje sekcję z zapieczętowanych `inputs` przy tworzeniu zadania (zmienne są wtedy puste). Runner renderuje ją ponownie z `context.variables` i raportuje w snapshocie; ostatni raport wygrywa.
*   `label` jest niepusty i unikalny w sekcji; nieznane pole w `display` albo w linku jest błędem wczytania.

---

## 4. Przykładowe Workflow

Poniżej kompletny przykład workflow realizującego prosty proces implementacji funkcji, testowania i tworzenia PR.

```yaml
name: implement-feature
version: 1
description: Implementuje funkcję, testuje ją i tworzy Pull Request.

inputs:
  - name: feature_description
    type: string
    required: true
  - name: target_repo
    type: repository
    required: true

globals:
  timeout: 2h
  retry_policy:
    max_attempts: 2
    backoff: 10s
  workspace:
    type: git
    repository: "{{ inputs.target_repo }}"
    ref: main

states:
  analyze:
    description: Analiza istniejącego kodu i wymagań.
    action:
      type: agent
      harness: claude-code
      prompt: |
        Przeanalizuj repozytorium pod kątem implementacji: {{ inputs.feature_description }}.
        Zidentyfikuj pliki, które trzeba zmodyfikować.
        Zapisz plan w pliku plan.md.
    outputs:
      - ready
      - ambiguous
    transitions:
      ready: implement
      ambiguous: human_clarification
    
  human_clarification:
    description: Człowiek wyjaśnia niejasności.
    action:
      type: human
      message: "Agent ma wątpliwości. Proszę doprecyzować wymagania."
      inputs:
        - name: clarification_text
          type: text
          required: true
    outputs:
      - clarified
      - abort
    transitions:
      clarified: implement
      abort: failed

  implement:
    description: Implementacja funkcji.
    action:
      type: agent
      harness: claude-code
      prompt: |
        Zaimplementuj funkcję zgodnie z planem w plan.md
        oraz wymaganiem: {{ inputs.feature_description }}.
        Upewnij się, że kod kompiluje się.
    outputs:
      - implemented
      - blocked
    transitions:
      implemented: test
      blocked: failed

  test:
    description: Uruchomienie testów jednostkowych.
    action:
      type: script
      command: ./run_tests.sh
      exit_codes:
        0: success
        1: failure
        2: partial_failure
    outputs:
      - success
      - failure
      - partial_failure
    transitions:
      success: create_pr
      failure: fix_code
      partial_failure: human_review_tests

  fix_code:
    description: Naprawa błędów wykrytych przez testy.
    action:
      type: agent
      harness: claude-code
      prompt: |
        Testy nie przeszły. Przeanalizuj logi i napraw kod.
    outputs:
      - fixed
    transitions:
      fixed: test  # Pętla powrotu do testów

  human_review_tests:
    description: Człowiek ocenia częściowe niepowodzenie testów.
    action:
      type: human
      message: "Część testów nie przeszła. Czy ignorować?"
    outputs:
      - ignore
      - fix_it
    transitions:
      ignore: create_pr
      fix_it: fix_code

  create_pr:
    description: Tworzenie Pull Requesta.
    action:
      type: script
      command: ./create_pr.sh
      args:
        - "--title"
        - "Feature: {{ inputs.feature_description }}"
    outputs:
      - created
    transitions:
      created: completed

  completed:
    description: Sukces.
    # Brak akcji, stan terminalny
  
  failed:
    description: Niepowodzenie.
    # Brak akcji, stan terminalny

terminal_states:
  completed: completed
  failed: failed
```

---

## 5. Walidacja Schematu

Przed uruchomieniem workflow, definicja musi przejść walidację:

1.  **Spójność Grafu:**
    *   Wszystkie stany wskazane w `transitions` muszą istnieć w sekcji `states`.
    *   Każda etykieta w `outputs` musi mieć odpowiadające jej przejście w `transitions` (chyba że stan jest terminalny).
    *   Etykieta w `outputs` nie może się powtarzać, również między formą gołą a obiektem (§3.3).
    *   Musi istnieć co najmniej jeden stan początkowy (pierwszy zdefiniowany lub oznaczony flagą `initial: true`).
    *   Muszą istnieć stany terminalne osiągalne z każdego innego stanu (brak deadlocków).

2.  **Poprawność Akcji:**
    *   Typ akcji musi być obsługiwany (`agent`, `script`, `human`, `wait`).
    *   Wymagane pola dla danego typu akcji muszą być obecne (np. `prompt` dla agenta, `command` dla skryptu).
        `harness` **nie** jest wymagany: każde ustawienie ma wartość wbudowaną,
        więc „brak harnessu gdziekolwiek" nie może zaistnieć, a walidacja
        pozostaje funkcją samego źródła.
    *   Dla `AgentAction` i `ScriptAction` lista `outputs` nie może być pusta.
    *   `max_entries` jest dozwolone wyłącznie dla akcji `agent` i musi być >= 1.
    *   Nazwy w `requires_variables` muszą być poprawnymi identyfikatorami zmiennych.

3.  **Poprawność Decyzji:**
    *   Typ dostawcy decyzji musi być poprawny.
    *   Jeśli wybrano `AgentDecision`, muszą być dostarczone odpowiednie parametry (harness, prompt).

4.  **Zmienne i Kontekst:**
    *   Odwołania do zmiennych (np. `{{ inputs.xxx }}`) muszą dotyczyć zdefiniowanych wejść lub pól kontekstu.
    *   `steps.ID.reply` musi wskazywać istniejący krok `agent`; jest dozwolone tylko w promptach agentów i treści akcji `human`.
    *   Po `steps.ID.` może być tylko pole `reply`. Inne pole (np. `steps.ID.data`) jest błędem.

5.  **Display (§3.6):**
    *   `{{ inputs.xxx }}` w `title`, `text` i `url` musi dotyczyć zdefiniowanego wejścia; zmienna kontekstu bez wartości nie jest błędem.
    *   Filtr albo niedomknięte `{{` w tych szablonach jest błędem.
    *   `label` jest niepusty i unikalny.

6.  **Obrazy:**
    *   `globals.image` i `image` kroku muszą być poprawną referencją OCI z jawnym tagiem lub digestem; przy polityce `pinned` digest jest obowiązkowy.
    *   Stan terminalny nie może nadpisywać obrazu — niczego nie uruchamia.

Walidacja odbywa się statycznie przed utworzeniem `WorkflowExecution`. Błędy walidacji uniemożliwiają start workflow.

---

## 6. Rozszerzalność

Schema została zaprojektowana z myślą o przyszłych rozszerzeniach:

*   **Nowe typy akcji:** Dodanie nowego typu (np. `http`, `docker`) wymaga jedynie rozszerzenia enum `type` w definicji akcji i dodania odpowiedniego parsera/handlera w silniku.
*   **Parallel States:** W przyszłości można dodać pole `parallel: true` do stanu, które pozwoliłoby na równoległe wykonanie pod-stanów, ale wymaga to znaczącej zmiany w silniku (obecnie sekwencyjnym).
*   **Dynamiczne Workspace'y:** Obecnie workspace jest stały dla wykonania. W przyszłości można dodać pole `workspace_override` w stanie, aby zmieniać środowisko pracy między krokami (wymaga obsługi migracji kontekstu).

Dokument ten stanowi kontrakt między użytkownikiem (twórcą workflow) a silnikiem narsora. Zmiany w schemacie powinny być wersjonowane (pole `version` w pliku YAML).
