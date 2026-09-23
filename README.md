# Orchestrator

Silnik workflow dla zadań wykonywanych przez agentów AI i ludzi. Definicję
procesu opisuje się w YAML-u (graf stanów, akcje, przejścia), a orkiestrator
wykonuje go krok po kroku, utrwalając na dysku każdy stan i każde zdarzenie —
tak, żeby przerwane wykonanie dało się odtworzyć, a przebieg audytować.

Dokumentacja projektowa:

- [`ARCHITECTURE_DECISIONS.md`](ARCHITECTURE_DECISIONS.md) — decyzje D1–D46 wraz
  z uzasadnieniami.
- [`DOMAIN_MODEL.md`](DOMAIN_MODEL.md) — model domenowy, układ katalogów,
  kolejność emisji zdarzeń.
- [`WORKFLOW_SCHEMA.md`](WORKFLOW_SCHEMA.md) — kontrakt pliku YAML: sekcje,
  typy akcji, decyzje, reguły walidacji.
- [`docs/plans/`](docs/plans/) — plany wdrożeniowe.

---

## Wymagania środowiskowe

- **Rust** (edycja 2021) wraz z `cargo`.
- **`git` w `PATH`.** Orkiestrator klonuje repozytoria, wywołując polecenie
  `git` jako proces potomny. Biblioteka `libgit2` została świadomie odrzucona,
  ponieważ nie respektuje `~/.ssh/config`, credential helperów ani reguł
  `url.*.insteadOf` — czyli dokładnie tej konfiguracji, na której opiera się
  dostęp do prywatnych repozytoriów. Brak `git` w `PATH` ujawni się dopiero przy
  tworzeniu workspace'u typu `git`.
- System POSIX. Prawa tylko-do-odczytu dla repozytoriów pobocznych realizują
  bity uprawnień plików.

### Budowanie i testy

```bash
cargo build              # zbudowanie binarki target/debug/orchestrator
cargo test               # pełny zestaw testów (jednostkowe + integracyjne)
cargo build --release    # wydanie
```

Testy integracyjne działają na prawdziwym systemie plików i uruchamiają
prawdziwe procesy potomne — wszystko w katalogach tymczasowych, nigdy
w repozytorium.

---

## Szybki start

```bash
# 1. Sprawdź definicję
orchestrator validate tests/fixtures/cli/greet.yaml

# 2. Uruchom workflow
orchestrator run tests/fixtures/cli/greet.yaml \
    --input "greeting=Dzień dobry" \
    --input "script_path=$PWD/tests/fixtures/cli/announce.sh"

# 3. Zobacz, co się dzieje
orchestrator list
orchestrator show <ID_WYKONANIA>

# 4. Odpowiedz na pytanie zadane przez workflow
orchestrator input <ID_WYKONANIA> --field approval_status=approved
```

---

## Opcje globalne

| Opcja | Domyślnie | Znaczenie |
|---|---|---|
| `--root <KATALOG>` | `./.orchestrator` | Katalog danych: wykonania i workspace'y. Dotyczy każdej podkomendy. |
| `--help`, `--version` | — | Pomoc i wersja. |

Logi (`tracing`) trafiają na **stderr**, a wynik polecenia na **stdout** —
dzięki temu `orchestrator list | grep paused` działa także przy
`RUST_LOG=debug`.

---

## Podkomendy

### `validate <PLIK>`

Statyczna walidacja definicji według `WORKFLOW_SCHEMA.md` §5: spójność grafu,
poprawność akcji i decyzji, odwołania do zmiennych. Wypisuje **wszystkie**
naruszenia naraz, a nie pierwsze napotkane.

```console
$ orchestrator validate tests/fixtures/implement_feature.yaml
Definicja jest poprawna: implement-feature (wersja 1), stanów: 9, stan początkowy: analyze.

$ orchestrator validate tests/fixtures/invalid/five_violations.yaml
Definicja 'tests/fixtures/invalid/five_violations.yaml' jest niepoprawna — znaleziono 5 naruszeń:
  - ...
```

### `run <PLIK> [--input NAZWA=WARTOŚĆ]…`

Tworzy wykonanie i prowadzi je aż do stanu terminalnego, wstrzymania albo
błędu. Wypisuje identyfikator wykonania jako pierwszą linię.

Wartości `--input` są parsowane **zgodnie z typem zadeklarowanym w definicji**:
`string` i `repository` jako tekst, `integer` jako liczba całkowita, `boolean`
jako `true`/`false`. Dlatego `--input is_experimental=true` trafia do kontekstu
jako wartość logiczna, a nie jako napis. Brakujące wejścia wymagane są
zgłaszane **zanim** cokolwiek powstanie na dysku.

```console
$ orchestrator run tests/fixtures/cli/greet.yaml \
      --input "greeting=Dzień dobry" \
      --input "script_path=$PWD/tests/fixtures/cli/announce.sh" \
      --input is_experimental=true
Utworzono wykonanie: 4f1c8f0e-...-9b21
Status: paused
Bieżący stan: approve
Oczekiwane wejście człowieka: Czy ogłoszenie jest w porządku?
  --field approval_status=…  (Select, wymagane, dozwolone wartości: approved, rejected)
```

**Skąd bierze się workspace.** Kolejność jest następująca:

1. `globals.workspace: { type: git, repository: …, ref: … }` — pole
   `repository` może zawierać szablon `{{ inputs.* }}`, który jest rozwijany
   przed użyciem; wynik trafia do `git clone` (URL albo ścieżka lokalna).
2. `globals.workspace: { type: local, path: … }` — zawartość wskazanego
   katalogu jest kopiowana do workspace'u.
3. Brak sekcji `workspace` — powstaje pusty katalog
   `{root}/tasks/{execution_id}/code/workspace`. To wystarcza workflow, których
   skrypty leżą poza repozytorium.

Wszystko, co da się sprawdzić bez dotykania dysku (poprawność remote'a,
istnienie ścieżki lokalnej), jest sprawdzane **przed** utworzeniem wykonania.
Sam klon następuje po utworzeniu, bo katalog zadania nazywa się identyfikatorem
wykonania. Jeśli klon się nie uda, wykonanie jest oznaczane jako `failed`, a nie
zostawiane w stanie `running`.

Ścieżka do pliku definicji jest zapamiętywana w `state.json`
(`metadata.cli.definition_path`), dzięki czemu `input` i `resume` nie wymagają
jej ponownego podawania.

### `list`

Wypisuje wykonania z katalogu danych, od najnowszego. Brak wykonań to nie błąd.

```console
$ orchestrator list
ID                                     STATUS     STAN                   WORKFLOW
4f1c8f0e-...-9b21                      paused     approve                cli-greet
```

### `show <ID_WYKONANIA>`

Status, bieżący stan, workspace i zmienne kontekstu; gdy wykonanie czeka na
człowieka — także treść pytania i pola formularza.

```console
$ orchestrator show 4f1c8f0e-...-9b21
Wykonanie:     4f1c8f0e-...-9b21
Workflow:      cli-greet (wersja 1)
Status:        paused
Bieżący stan:  approve
Rozpoczęto:    2026-09-23T19:43:49.018665031+00:00
Workspace:     /tmp/dane/tasks/4f1c8f0e-...-9b21/code/workspace
Zmienne kontekstu:
  announced = "Dzień dobry"
  greeting = "Dzień dobry"
  is_experimental = true
```

### `events <ID_WYKONANIA> [--follow]`

Historia zdarzeń w kolejności sekwencji: numer, znacznik czasu, typ, opis.
`--follow` dopisuje nowe zdarzenia, dopóki wykonanie nie osiągnie stanu
końcowego.

```console
$ orchestrator events 4f1c8f0e-...-9b21
    1  2026-09-23T19:43:49+00:00  workflow_started       Workflow 'cli-greet' started
    2  2026-09-23T19:43:49+00:00  state_entered          Entered state 'announce'
    3  2026-09-23T19:43:49+00:00  action_started         Action 'script' started
```

### `input <ID_WYKONANIA> [--field NAZWA=WARTOŚĆ]… [--label ETYKIETA]`

Dostarcza odpowiedź człowieka do wstrzymanego wykonania i wznawia bieg.
`--field` wypełnia pola formularza zadeklarowane w akcji `human`; `--label`
wskazuje etykietę wyjścia wprost — potrzebne, gdy akcja nie ma pól albo gdy ma
ich kilka i etykiety nie da się wywieść z pojedynczego pola `select`.
`--file` pozwala wskazać definicję, gdy plik został przeniesiony.

```console
$ orchestrator input 4f1c8f0e-...-9b21 --field approval_status=approved
Dostarczono wejście do wykonania 4f1c8f0e-...-9b21.
Status: completed
Bieżący stan: completed
```

### `resume <ID_WYKONANIA>`

Wznawia wykonanie wstrzymane **nie** pytaniem do człowieka (np. ręcznym
`pause`). Wykonanie czekające na odpowiedź wymaga `input`, nie `resume` — to
zabezpieczenie przed dwukrotnym dostarczeniem tej samej odpowiedzi.

### `tui`

Zarezerwowane dla pełnoekranowego interfejsu terminalowego. W tej wersji
zwraca komunikat o niedostępności.

---

## Kody wyjścia

Kody wyjścia są kontraktem dla CI:

| Kod | Znaczenie |
|---|---|
| `0` | Polecenie wykonało to, o co poproszono. |
| `1` | Błąd walidacji definicji, niepowodzenie wykonania, nieznane ID, nieczytelny magazyn. |
| `2` | Błędne użycie: zła składnia argumentów, nieznane wejście, brak wejścia wymaganego. |

Rozróżnienie jest celowe: **workflow zakończony niepowodzeniem to kod `1`** —
silnik zrobił dokładnie to, o co go poproszono, a proces skończył się w stanie
terminalnym oznaczającym porażkę. **Brakujące `--input` to kod `2`**, bo nic nie
zostało uruchomione. Kod `2` zwraca też sam parser argumentów, więc oba źródła
błędnego użycia są nieodróżnialne dla skryptu.

---

## Układ danych na dysku

Katalog `--root` (domyślnie `./.orchestrator`):

```text
{root}/
├── executions/{execution_id}/
│   ├── state.json                  # WorkflowExecution: status, bieżący stan, kontekst
│   ├── context.json                # projekcja kontekstu (zmienne, artefakty, workspace)
│   ├── pending_input.json          # pytanie, na które czeka wstrzymane wykonanie
│   ├── events/{sequence:08}.json   # append-only log zdarzeń
│   └── artifacts/{artifact_id}/    # pliki wytworzone przez akcje
└── tasks/{execution_id}/
    └── code/
        ├── {repozytorium_główne}/  # do zapisu
        └── {repozytorium_boczne}/  # tylko do odczytu
```

Każdy zapis `state.json` i `context.json` jest atomowy (plik tymczasowy w tym
samym katalogu, `fsync`, `rename`), a numeracja zdarzeń rośnie monotonicznie bez
luk. Przerwany zapis zostawia nienaruszoną poprzednią wersję pliku.

---

## Czego MVP jeszcze **nie** robi

Świadomie odłożone do Fazy 3 — każda pozycja wymaga własnego planu:

- **`AgentAction`** i adaptery harnessów (Claude Code, OpenCode). Workflow ze
  stanem typu `agent` uruchomi się, ale ta akcja zakończy się błędem
  „not available in this version" — dotyczy to także przykładu
  `tests/fixtures/implement_feature.yaml`.
- **`WaitAction`** i usługa pollingu zdarzeń zewnętrznych.
- **Decyzje agentowe** (`decision: { type: agent }`) — zwracają jawny błąd
  o niedostępności zamiast po cichu zachowywać się jak decyzja regułowa.
- **REST API i SSE** — TUI i CLI rozmawiają z silnikiem w procesie.
- **Konteneryzacja (Docker)** — izolacja opiera się dziś na uprawnieniach
  systemu plików, co chroni przed pomyłką, a nie przed złośliwym agentem. Nie
  uruchamiaj niezaufanych workflow.
- **Uwierzytelnianie, wielu użytkowników, Web UI, metryki i distributed
  tracing.**
