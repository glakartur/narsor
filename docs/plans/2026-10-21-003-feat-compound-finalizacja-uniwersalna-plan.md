---
title: "feat: Workflow compound — finalizacja niezależna od hostingu repozytorium"
type: feat
status: active
date: 2026-10-21
origin: docs/brainstorms/2026-10-21-compound-finalizacja-uniwersalna-requirements.md
---

# feat: Workflow compound — finalizacja niezależna od hostingu repozytorium

## Summary

Zmiana dotyczy definicji workflow, jego dokumentacji i testów. Silnik nie zmienia się. Graf stanów i etykiety wyników zostają bez zmian (R6).

1. `finalize` i `disarm_merge` zmieniają się ze skryptów `glab` na kroki agentowe. Agent rozpoznaje hosting z `pr_url` i z remote, wybiera narzędzie (`gh`, `glab`) i wykonuje te same czynności co dziś.
2. Prompty publikacji i `repair_merge` oraz opisy w YAML i README przestają zakładać GitLab.
3. Testy przestają sprawdzać skrypt. Sprawdzają kontrakt: graf, etykiety, treść promptów (gwarancje R4–R9) i przejścia po odpowiedziach agenta.

---

## Problem Frame

Zob. origin: `workflows/compound-engineering.yaml` kończy się na GitHub błędem „not a GitLab merge request URL". Zależność od GitLab jest też w 4 promptach publikacji, w `repair_merge`, w opisie na początku YAML i w `workflows/README.md` (see origin: docs/brainstorms/2026-10-21-compound-finalizacja-uniwersalna-requirements.md).

---

## Requirements Trace

Wszystkie R1–R11, AE1–AE5, F1–F2 z origin. Mapowanie na jednostki:

- R1, R2, R3, R4, R7, R8 → U1
- R5, R6 → U1, U2 (kontrakt wyników, przejścia) i U4 (testy)
- R9 → U2
- R10 → U3
- R11 → U3
- Success Criteria (testy i fixture zaktualizowane) → U4

---

## Scope Boundaries

- Nie zmieniamy grafu: te same stany, etykiety i przejścia (`finalize`, `repair_merge`, `disarm_merge`, `finalize_blocked`, terminale). Zmieniają się typ akcji i teksty.
- Nie zmieniamy pozostałych etapów poza tekstami z R10.
- Nie zmieniamy uwierzytelniania `glab` i `gh`: zostaje po stronie operatora.
- Silnik, schemat workflow i `WORKFLOW_SCHEMA.md` nie zmieniają się.
- Bitbucket i Gitea nie są kryterium odbioru.

### Deferred to Follow-Up Work

- Deterministyczna weryfikacja wyniku `merged` po stronie silnika (np. krok skryptowy po agencie). Patrz Risks.

---

## Context & Research

- `workflows/compound-engineering.yaml`, sekcja „5. Finalize": skrypt `finalize` (linie ok. 757–864), `repair_merge`, `disarm_merge`, `finalize_blocked`. To jest wzorzec zachowania do odtworzenia w prompcie: kolejność (czytaj stan → Draft off → zatwierdź → auto-merge → pętla oczekiwania), reguła „każdy wynik inny niż `merged` najpierw anuluje auto-merge i przywraca Draft, potem czyta PR jeszcze raz", ponowne uruchomienie sprawdza, czy PR jest scalony.
- Inne kroki agentowe w tym pliku: wzorzec `variables`/`choices`/`transitions`/`timeout` oraz kontrakt odpowiedzi doklejany przez silnik (`WORKFLOW_SCHEMA.md`, „AgentAction"). Prompt nie powtarza formatu JSON.
- `AgentAction` ma `workspace_access: read-only` (`WORKFLOW_SCHEMA.md`). Używamy go dla `finalize` i `disarm_merge`, aby agent nie mógł zmienić kodu w workspace (wspiera R4).
- Wejścia workflow mają `default` (`WORKFLOW_SCHEMA.md`, sekcja wejść). Pozwala to zachować konfigurowalny limit oczekiwania (R8).
- Testy: `tests/workflows.rs` (`CompoundRun`, `FakeGlab`, 8 testów dotyka `glab`, linie ok. 683–1210), `tests/builtin_workflows.rs` (`compound_engineering_finalize_is_a_script_with_a_timeout_choice`, ok. linii 475), stałe grafu `COMPOUND_STATES` i `COMPOUND_EDGES` w `tests/workflows.rs`. `tests/fixtures/glab/fake_glab.sh`.
- Wcześniejszy plan tego samego pliku: `docs/plans/2026-10-21-002-feat-compound-ocena-w-pr-plan.md`. Należy sprawdzić, czy nie zmienia tych samych linii (kolejność scalania).
- Zewnętrzne badania pominięte: wzorzec lokalny jest silny, a poniższe odpowiedniki poleceń są znane. Weryfikacja w U1 (patrz Deferred to Implementation).

---

## Key Technical Decisions

- **KTD1. Oba kroki, `finalize` i `disarm_merge`, są agentowe.** `disarm_merge` musi sam rozpoznać hosting (R9). Mały skrypt z wykrywaniem hostingu powielałby logikę hostingów, której R1 każe nie utrzymywać. Cena: mniej deterministyczne zachowanie (Key Decisions origin).
- **KTD2. Rozpoznanie hostingu: z `pr_url` jako źródła głównego, z `git remote` jako potwierdzenia.** Host z `pr_url` (`github.com/.../pull/N`, `.../-/merge_requests/N`) mówi, które narzędzie wybrać. Remote potwierdza i obsługuje instancje własne (GitHub Enterprise, GitLab self-hosted), gdzie sam host nie zdradza produktu. Gdy źródła się różnią albo hosting jest nieznany → `blocked` (R4). Prompt nie zawiera listy hostingów poza przykładami odpowiedników.
- **KTD3. Ograniczenie agenta (R5) trzema warstwami:** (a) `workspace_access: read-only`, (b) prompt z zakazami: żadnego `git merge`, `git push` do gałęzi docelowej ani wywołania merge bez auto-merge/oczekiwania na sprawdzenia, (c) obowiązek: przed każdą odpowiedzią inną niż `merged` agent anuluje auto-merge, przywraca Draft i czyta PR jeszcze raz; w `reason` cytuje stan odczytany z hostingu. `read-only` nie jest dowodem, że agent nie wywoła API hostingu. To ryzyko jest opisane niżej i świadomie przyjęte (origin: koszt kroku agentowego).
- **KTD4. Odpowiedniki poleceń zapisane w prompcie jako wskazówki, nie jako skrypt.** Prompt podaje tabelę GitLab ↔ GitHub (czytaj stan, wyjmij z Draft, zatwierdź, auto-merge, anuluj auto-merge, przywróć Draft). Agent może zastosować inne polecenie tego samego narzędzia. Tabela jest częścią promptu, nie kodu.
- **KTD5. Limit oczekiwania przez wejście workflow.** Nowe wejście `finalize_wait_minutes` (`type: integer`, `required: false`, `default: 60`) trafia do promptu `finalize`. Limit kroku `70m` zostaje (R8).
- **KTD6. Zachowanie na GitHub bez możliwości zatwierdzenia:** agent próbuje zatwierdzić. Gdy zatwierdzenie jest niemożliwe (autor = zatwierdzający), idzie dalej, jeśli reguły repozytorium go nie wymagają. Gdy auto-merge nie jest włączony w repozytorium albo reguły wymagają zatwierdzenia, wynik to `blocked` z powodem; agent nie scala od razu (origin, Założenia).

---

## Implementation Units

### U1. `finalize` jako krok agentowy

**Goal:** Zastąpić skrypt agentem, który na GitLab i GitHub wykonuje finalizację z gwarancjami R4–R8.

**Requirements:** R1, R2, R3, R4, R5, R6, R7, R8; F1, F2; AE1–AE4

**Dependencies:** brak

**Files:**
- `workflows/compound-engineering.yaml` (stan `finalize`, sekcja wejść, nagłówek sekcji 5)
- test: `tests/workflows.rs`, `tests/builtin_workflows.rs` (patrz U4)

**Approach:**
- Akcja `type: agent`, `workspace_access: read-only`, `timeout: 70m`, te same `choices` i `transitions` co dziś (`merged`, `pipeline_failed`, `conflict`, `blocked`, `timeout`). Etykiety zachowują opisy. `timeout` pozostaje etykietą wyboru, której agent nie zwraca: ustawia ją silnik (zob. istniejący opis).
- Prompt (po polsku lub po angielsku zgodnie z resztą pliku: reszta promptów jest po angielsku) zawiera, w tej kolejności:
  1. Wejście: `{{ context.pr_url }}`; numer PR, repozytorium i gałąź docelowa pochodzą z `pr_url` i z samego PR, nie z nazwy gałęzi (R3).
  2. Rozpoznanie hostingu (KTD2) i sprawdzenie narzędzia (`command -v`); brak narzędzia lub uprawnień → `blocked` z konkretnym powodem i jak to naprawić (R4, R7). Zakaz obejść: lokalny `git merge`, `git push` do gałęzi docelowej, edycja kodu.
  3. Najpierw: czy PR jest scalony → `merged` (ponowne uruchomienie). Stan inny niż otwarty → `blocked`.
  4. Kroki: wyjmij z Draft, zatwierdź (brak możliwości zatwierdzenia nie blokuje, KTD6; już zatwierdzony nie blokuje), ustaw auto-merge po zielonych sprawdzeniach. Brak auto-merge → `blocked`, nie merge od razu.
  5. Oczekiwanie do `{{ inputs.finalize_wait_minutes }}` minut (odpytywanie co ok. 30 s): `merged` → `merged`; zamknięty → `blocked`; konflikt → `conflict`; pipeline/checks nieudane lub anulowane → `pipeline_failed`; po limicie → `blocked` z stanem pipeline.
  6. Zasada wyjścia (R5): przed każdą odpowiedzią inną niż `merged` anuluj auto-merge, przywróć Draft, odczytaj PR ponownie. Jeśli PR jest scalony → `merged`. W `reason` podaj stan odczytany z hostingu.
  7. Tabela odpowiedników (KTD4).
- Nowe wejście `finalize_wait_minutes` (KTD5).
- Nagłówek sekcji „5. Finalize — merge on GitLab" i opis kroku nie wspominają GitLab.

**Patterns to follow:** inne kroki agentowe w tym pliku (kontrakt odpowiedzi dokleja silnik); logika skryptu `finalize` jako specyfikacja zachowania.

**Test scenarios:** patrz U4 (testy dotyczą całego zestawu zmian).

**Verification:** `narsor validate workflows/compound-engineering.yaml` przechodzi. Prompt zawiera każdą gwarancję z listy 1–7. Graf bez zmian.

---

### U2. `disarm_merge` jako krok agentowy

**Goal:** Sprzątanie po `finalize` przerwanym limitem czasu na każdym hostingu (R9).

**Requirements:** R5, R6, R9; AE5

**Dependencies:** U1 (wspólna tabela odpowiedników i reguły rozpoznania)

**Files:**
- `workflows/compound-engineering.yaml` (stan `disarm_merge`)

**Approach:**
- `type: agent`, `workspace_access: read-only`, `timeout: 10m`. Zostają `choices` `merged` i `disarmed` oraz przejścia `merged → finished`, `disarmed → finalize_blocked`.
- Prompt: rozpoznaj hosting jak w `finalize`; odczytaj PR; jeśli scalony → `merged`; w przeciwnym razie anuluj auto-merge, przywróć Draft, odczytaj PR jeszcze raz (scalony w międzyczasie → `merged`), potem `disarmed` z stanem w `reason`. Błąd anulowania lub brak narzędzia: nie ukrywać, opisać w `reason` (wynik pozostaje `disarmed`, bo bramka i tak oddaje decyzję człowiekowi, jak dziś).
- Wspólne reguły (rozpoznanie hostingu, tabela) są powtórzone w prompcie; YAML nie ma mechanizmu współdzielenia promptów. Powtórzenie jest krótkie.

**Test scenarios:** patrz U4.

**Verification:** walidacja YAML; prompt zawiera anulowanie auto-merge, przywrócenie Draft, ponowny odczyt.

---

### U3. Teksty niezależne od hostingu

**Goal:** Usunąć założenie o GitLab z promptów, opisów i dokumentacji.

**Requirements:** R10, R11

**Dependencies:** brak

**Files:**
- `workflows/compound-engineering.yaml` (opis na początku; prompty `publish_requirements`, `publish_plan`, `publish_work`, `publish_learning`; `repair_merge`; `finalize_blocked`; opisy terminali)
- `workflows/README.md` (tabela przy `compound-engineering.yaml`, akapit „targets GitLab", opis finalize, opis `disarm_merge`)

**Approach:**
- 4 prompty publikacji: zamienić „This is a GitLab repository: the pull request is a merge request." na zdanie w rodzaju „The pull request is a pull request or a merge request, depending on the repository host." Pozostałe „merge request" w promptach i opisach zastąpić „pull request / merge request (PR)" tam, gdzie jest to przyczyna nieporozumienia. Nie zmieniać wszystkich wystąpień, jeśli sens jest jasny; nazwa wyświetlana `PR` już jest neutralna.
- `repair_merge`: „the failed pipeline jobs with `glab ci`" → „the failed pipeline or check jobs, with the CLI of the host (`glab ci`, `gh run`/`gh pr checks`)".
- `finalize_blocked`: „Fix what blocks it on GitLab" → „on the repository host".
- README: tabela (wymagania: `glab` lub `gh`, zależnie od hostingu), akapit o hostingu (agent rozpoznaje hosting; miejsce uruchomienia potrzebuje uwierzytelnionego `glab` na GitLab albo `gh` na GitHub, tożsamości, która może zatwierdzać i ustawiać auto-merge; GitHub wymaga włączonego auto-merge i reguł ochrony gałęzi), opis finalize bez „is a script with `glab`, not an agent", wzmianka o `finalize_wait_minutes` zamiast `FINALIZE_WAIT_SECS` i `FINALIZE_POLL_SECS`.
- Opis tekstowy w README o testach („a fake `glab`") dostosować do U4.

**Test scenarios:** `Test expectation: none -- zmiany tekstów; pokrywa je test treści promptów w U4.`

**Verification:** `grep -i "gitlab\|glab"` w YAML zwraca tylko tabelę odpowiedników i przykłady narzędzi; README nie twierdzi, że workflow „targets GitLab".

---

### U4. Testy i fixture

**Goal:** Dopasować testy do kroków agentowych. Testy nie mogą zbadać zachowania agenta, więc sprawdzają kontrakt kroku i przejścia.

**Requirements:** R5, R6, R7, R9; AE1–AE5 (w zakresie, w jakim da się je sprawdzić deterministycznie); Success Criteria

**Dependencies:** U1, U2, U3

**Files:**
- `tests/workflows.rs`
- `tests/builtin_workflows.rs`
- `tests/fixtures/glab/fake_glab.sh` (usunąć razem z `FakeGlab`, jeśli nic go nie używa; wtedy też z katalogu `tests/fixtures/glab/`)

**Approach:**
- Usunąć `FakeGlab`, `merge_request`, `open_merge_request`, `merged_merge_request`, parametr `views` w `CompoundRun::new` i podmianę env dla skryptów. `FakeHarness` odpowiada na wywołania `finalize` i `disarm_merge` odpowiedziami `reply(...)` jak w innych krokach. Zaktualizować komentarz modułu (linie 11–15) i stałą `PR_URL`: pozostaje URL GitLab; dodać drugi URL GitHub dla testu promptu.
- Przepisać testy, które dziś liczyły wywołania `glab`, na testy przejść według odpowiedzi agenta (patrz scenariusze).
- `tests/builtin_workflows.rs`: zastąpić `compound_engineering_finalize_is_a_script_with_a_timeout_choice` testem, że `finalize` i `disarm_merge` są krokami agentowymi z `workspace_access: read-only`, `finalize` ma `timeout_secs = 70*60` i przejście `timeout → disarm_merge`, a prompty nie mają literału gałęzi `main`.
- `COMPOUND_STATES` i `COMPOUND_EDGES` bez zmian (graf nie zmienia się). Test grafu pilnuje R6.

**Execution note:** Najpierw zaktualizować test grafu i `builtin_workflows` (powinny przejść), potem usuwać `FakeGlab`.

**Test scenarios:**
- Happy path: pełny pipeline, agent `finalize` zwraca `merged` → stan `finished`, status `Completed`. (zastępuje `the_compound_engineering_pipeline_runs_through_all_of_its_gates_and_merges`; zachować asercje o promptach 2, 5, 6, 9 i `pr_url`.)
- `nothing_to_document` → `finalize` → `merged` → `finished` (Covers AE8 z wcześniejszej specyfikacji; test zostaje).
- Covers AE4. Agent `finalize` zwraca `pipeline_failed` → `repair_merge`; prompt `repair_merge` zawiera powód z `steps.finalize.stdout`.
- `conflict` → `repair_merge`.
- Covers AE3. Agent zwraca `blocked` z powodem „brak `gh`" → bramka `finalize_blocked` pokazuje ten powód.
- `retry` w `finalize_blocked` → `finalize` → `merged` → `finished`.
- Covers AE5. `finalize` zwraca `timeout` (silnik zatrzymuje krok po limicie; zachować test z krótkim limitem i agentem, którego odpowiedź trwa dłużej) → `disarm_merge` → `disarmed` → `finalize_blocked`.
- `disarm_merge` zwraca `merged` → `finished`.
- Kontrakt promptu `finalize` (jeden test, kilka asercji): zawiera `{{ context.pr_url }}` po renderowaniu, wzmiankę o `gh` i `glab`, zakaz lokalnego merge/push do gałęzi docelowej, regułę anulowania auto-merge i przywrócenia Draft przed odpowiedzią inną niż `merged`, sprawdzenie „już scalony", `finalize_wait_minutes`. Dla `pr_url` GitHub (`https://github.com/acme/web/pull/12`) prompt renderuje się bez błędu i zawiera ten adres (Covers AE1 w zakresie, w jakim da się to sprawdzić bez agenta).
- Kontrakt promptu `disarm_merge`: zawiera anulowanie auto-merge, przywrócenie Draft i ponowny odczyt.
- Edge: prompty publikacji i `repair_merge` nie zawierają „This is a GitLab repository" ani samego `glab ci` jako jedynej wskazówki (Covers R10).
- Edge: wejście `finalize_wait_minutes` ma wartość domyślną; workflow ładuje się i waliduje bez podania go.
- Test grafu: `COMPOUND_EDGES` przechodzi bez zmian (Covers R6).

**Verification:** `cargo test --test workflows --test builtin_workflows` przechodzi. Brak odwołań do `FakeGlab` i `fake_glab.sh`.

---

## System-Wide Impact

- Użytkownicy GitLab: ta sama ścieżka, ale wykonuje ją agent, więc zachowanie jest mniej deterministyczne (KTD1). AE2 nie da się udowodnić testem automatycznym bez prawdziwego agenta. Wymaga ręcznej weryfikacji na repozytorium GitLab przed uznaniem za zakończone.
- Operatorzy: miejsce uruchomienia potrzebuje `gh` lub `glab` (README, U3). Zmienne `FINALIZE_WAIT_SECS` i `FINALIZE_POLL_SECS` znikają. Sprawdzić, czy nie są użyte w innych miejscach (`grep` w repozytorium, w tym `docs/` i konfiguracji).
- Koszt: `finalize` zużywa tokeny agenta przez do 60 minut odpytywania. Prompt każe odpytywać rzadko (co ok. 30 s, krótkie polecenia, bez rozbudowanych komentarzy).

---

## Risks & Dependencies

| Ryzyko | Skutek | Ograniczenie |
|---|---|---|
| Agent scala PR bez człowieka (R5) lub zgłosi `merged` bez scalenia | Naruszenie głównej gwarancji | KTD3: read-only, zakazy, reguła wyjścia z cytatem stanu. Wymaga ręcznej weryfikacji na obu hostingach. Deferred: deterministyczne sprawdzenie po agencie |
| Agent przerwany limitem 70 min | Auto-merge pozostaje włączony | `timeout → disarm_merge` (bez zmian), także agentowy (KTD1) |
| `workspace_access: read-only` blokuje potrzebne narzędzia (sieć, `gh`) | `finalize` zawsze `blocked` | Zweryfikować w U1; jeśli tak, użyć domyślnego `read-write` i polegać na zakazach w prompcie, zapisać to w KTD3 |
| Reguły ochrony gałęzi i uprawnienia konta w repozytorium klienta | `blocked` zamiast merge | Zamierzone (KTD6). Weryfikacja reguł należy do właściwego specjalisty repozytorium; plan tego nie rozstrzyga |
| Konflikt z planem `2026-10-21-002` (te same linie YAML) | Trudne scalanie | Sprawdzić kolejność przed startem; zmiany w prompcie publikacji są małe |

---

## Deferred to Implementation

- Dokładna treść promptów i tabeli odpowiedników `glab` ↔ `gh` (stan PR, gotowość, zatwierdzenie, auto-merge, anulowanie, Draft). Zweryfikować w dokumentacji `gh` i `glab`, np. `gh pr ready`/`gh pr ready --undo`, `gh pr review --approve`, `gh pr merge --auto`, `gh pr merge --disable-auto`, `gh pr view --json state,isDraft,mergeStateStatus,statusCheckRollup,autoMergeRequest`.
- Czy `read-only` działa z poleceniami sieciowymi w używanych harnessach (Risks).
- Czy zmienne `FINALIZE_*` są użyte poza tym workflow.
- Czy wynik agenta `blocked` po przekroczeniu limitu oczekiwania powinien mieć stały początek `reason` (np. „not merged within"), na którym polega bramka. Zależy od końcowej treści promptu.

---

## Sources & References

- Origin: `docs/brainstorms/2026-10-21-compound-finalizacja-uniwersalna-requirements.md`
- `workflows/compound-engineering.yaml`, `workflows/README.md`, `WORKFLOW_SCHEMA.md` (AgentAction, wejścia)
- `tests/workflows.rs`, `tests/builtin_workflows.rs`, `tests/fixtures/glab/fake_glab.sh`
- `docs/plans/2026-10-21-002-feat-compound-ocena-w-pr-plan.md`
