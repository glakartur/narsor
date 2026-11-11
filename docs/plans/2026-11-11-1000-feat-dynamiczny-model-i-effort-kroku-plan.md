---
title: "feat: Dynamiczny model i effort kroku workflow"
type: feat
status: active
date: 2026-11-11
origin: docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md
---

# feat: Dynamiczny model i effort kroku workflow

## Summary

Krok agenta dostaje model i effort z trzech źródeł, w tej kolejności: wartość domyślna z definicji workflow, profil zapisany przez dyrygenta po kroku planu, eskalacja po powtórzonych błędach. Plan dodaje pola `effort` i górną granicę do definicji kroku, czystą funkcję, która wylicza profil efektywny, przekazanie effortu do harnessów, zapis profilu obok stanu dyrygenta, nowe wywołanie dyrygenta po planie, eskalację w silniku i podgląd w TUI. Plan nie dotyczy konkretnego workflow i nie zmienia ich wartości domyślnych. Tabela z dokumentu źródłowego pozostaje hipotezą.

---

## Problem Frame

Dziś krok agenta ma co najwyżej stały `model`. Pola `effort` nie ma. Każde zadanie używa tych samych ustawień (patrz origin: `docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md`). Plan opisuje, jak zrealizować R1–R11 i AE1–AE5 z tego dokumentu.

---

## Requirements

Wymagania R1–R11, aktorzy A1–A4, przepływy F1–F3 i przykłady AE1–AE5 pochodzą z origin i obowiązują bez zmian. Decyzje operatora z origin: brak edycji profilu (tylko podgląd), krok powyżej granicy startuje na granicy z otwartą prośbą, próg eskalacji to 3 i jest parametrem globalnym.

| Wymaganie | Jednostka |
|---|---|
| R1, R2, AE5 | U1, U2 |
| R3, R5 | U4, U5 |
| R4 | U3, U7 |
| R6, R7, AE1, AE2 | U1 (reguła), U5, U6 |
| R8, AE3 | U1 (reguła), U5 |
| R9, R10, AE4 | U1 (reguła), U8 |
| R11 | U3, U5 (klucz profilu) |

---

## Scope Boundaries

- Dobór profilu przed krokiem planu. Złożoność jest wtedy nieznana.
- Pytanie dyrygenta o profil przed każdym krokiem.
- Edycja profilu przez człowieka. TUI pokazuje tylko podgląd.
- Uczenie wartości domyślnych z historii.
- Zatwierdzenie wartości domyślnych z tabeli w dokumencie źródłowym.
- Model i effort samego dyrygenta ustawia konfiguracja demona (D10). Dyrygent nie dobiera ich sam.

### Deferred to Follow-Up Work

- Pomiar: które profile dają najlepszy stosunek kosztu do liczby rund przeglądu (2–3 realne zadania). To jest praca po wdrożeniu, nie część planu.
- Rozstrzygnięcie przez operatora prośby o wyjście poza granicę (zatwierdź lub odrzuć) wykracza poza podgląd. Plan tworzy otwartą prośbę. Mechanizm decyzji wchodzi w osobnym zadaniu, jeśli istniejący mechanizm próśb go nie obsłuży (patrz U6).

---

## Context & Research

### Relevant Code and Patterns

- `crates/narsor-engine/src/domain/workflow_definition.rs`: `AgentActionConfig` ma `model: Option<String>`. Brak `effort`.
- `crates/narsor-engine/src/workflow/schema.rs`, `loader.rs`, `validator.rs`: kształt YAML, kompilacja do domeny i walidacja. `loader.rs` przepisuje `model: agent.model`.
- `crates/narsor-engine/src/engine/actions/harness/`: adaptery `claude_code.rs`, `codex.rs`, `opencode.rs`, `github_copilot.rs`, `copilot_sdk.rs`. Tylko `claude_code.rs` i `codex.rs` i `opencode.rs` dodają `--model` z `config.model`. Żaden adapter nie czyta effortu.
- `crates/narsor-engine/src/engine/actions/agent.rs`: zdarzenie `AgentSessionStarted` zapisuje `effort` tylko wtedy, gdy `--effort` lub `--reasoning-effort` jest w argumentach wywołania. Po dodaniu `--effort` przez adapter oś czasu zapisze effort bez zmian w tym miejscu.
- `crates/narsor-engine/src/engine/retry.rs`: `RetrySchedule` liczy próby z `retry_policy`. Wzorzec: logika liczb w jednym miejscu, testowana bez uruchamiania workflow.
- `crates/narsor-engine/src/conductor/store.rs`: dyrygent trzyma własne dane w `conductor/` obok wykonania, bo silnik zapisuje `state.json` wykonania w całości. Plan używa tego samego katalogu dla profilu.
- `crates/narsor-engine/src/conductor/`: `invoke.rs`, `prompt.rs`, `response.rs`, `proposal.rs`, `failure.rs`. `ConductorPurpose` ma dziś m.in. `DiagnoseFailure`, `SummarizeGate`, `DecideFork`. Wzorzec dla nowego celu `ProfileSteps`.
- `crates/narsor-protocol/src/api.rs` (`ConductorPurposeDto`) i `crates/narsor-protocol/src/event.rs` (`ConductorPurpose`): wartość nieznana jest czytana przez `#[serde(other)]`. Nowy wariant jest zgodny wstecz.
- `src/tui/task_app/tabs/progress.rs`, `tabs/info.rs`, `panel/request.rs`: miejsca, w których TUI pokazuje krok i prośby.
- `docs/dev/workflow-schema.md` jest zewnętrznym kontraktem schematu. `workflows/template.yaml` jest wzorcem workflow.
- Ostatnie plany w `docs/plans/` (np. `2026-11-10-2142-feat-tui-ikony-stanu-zadan-plan.md`) pokazują styl: polskie, krótkie zdania, jednostki U1…Un.

### Institutional Learnings

- Katalog `docs/solutions/` nie był przeszukiwany osobno. Wykonawca powinien przeszukać `docs/solutions/architecture-patterns/` pod kątem zapisu danych dyrygenta przed U3.

### External References

- Flagi effortu w harnessach (`--effort` w Claude Code, `-c model_reasoning_effort=...` w Codex, `--variant` w OpenCode) pochodzą z wiedzy ogólnej. Nie zostały sprawdzone w tej sesji. Wykonawca sprawdza je na zainstalowanych wersjach CLI przed U2 (patrz Deferred to Implementation).

---

## Key Technical Decisions

- **Jedna czysta funkcja profilu.** Nowy moduł `crates/narsor-engine/src/engine/step_profile.rs` (nazwa robocza) przyjmuje: wartość domyślną kroku, granicę, profil dyrygenta, liczbę kolejnych błędów i próg. Zwraca profil efektywny i powód (`default`, `conductor`, `escalated`, `clamped`). Kolejność źródeł, reguła granic i reguła eskalacji żyją tylko tu. Dzięki temu wykonawca niczego nie wymyśla (cel z Success Criteria).
- **Skala effortu i modeli jest uporządkowana danymi workflow.** Effort ma stałą skalę `low < medium < high < max` (rozszerzalną). Modele mają cztery poziomy niezależne od harnessu: `small < medium < large < max` (D11). Workflow, profil dyrygenta, eskalacja i sufit kroku podają poziom, nie nazwę modelu. Każdy harness mapuje poziom na nazwę modelu (`engine/actions/harness/mod.rs`, `HarnessAdapter::model_tiers`). Brak wpisu w mapie to błąd przy starcie demona. Wartość spoza czterech poziomów jest błędem walidacji, a w profilu dyrygenta jest odrzucona.
- **Granica jest opcjonalna.** Bez granicy dyrygent może podnieść ustawienie tylko do wartości domyślnej kroku (origin, założenia). Bez granicy eskalacja podnosi tylko do wartości domyślnej, czyli nie podnosi niczego.
- **Brak pól oznacza brak zmian (AE5).** Gdy krok nie ma `effort`, `model`, profilu ani eskalacji, adapter dostaje te same argumenty co dziś.
- **Profil w katalogu dyrygenta, nie w stanie wykonania.** Powód jest ten sam co w `conductor/store.rs`: silnik zapisuje `state.json` wykonania w całości i zgubiłby cudzy zapis. Silnik czyta profil przy starcie kroku. Odczyt jest bezpieczny z dowolnego miejsca.
- **Klucz profilu: krok i opcjonalna jednostka.** Klucz to `step_id` z opcjonalnym `unit` (R11). Gdy brak wpisu dla jednostki, obowiązuje wpis kroku, potem wartość domyślna.
- **Ocena złożoności jest danymi wyjścia kroku planu.** Krok planu zwraca strukturę (liczba plików i modułów, migracje lub kontrakty, brak testów, zależności jednostek). Dyrygent dostaje ją w prompcie razem z planem. Dyrygent jej nie wylicza.
- **Dyrygent nigdy nie blokuje zadania (R8).** Błąd wywołania, odpowiedź niepewna lub niepoprawna oznacza: brak profilu, wartości domyślne, jedno zdarzenie ostrzegawcze.
- **Harness bez obsługi effortu ignoruje pole i zapisuje ostrzeżenie w zdarzeniach.** Krok się nie zatrzymuje.
- **Próg eskalacji to jeden parametr globalny.** Dodany do konfiguracji w `crates/narsor-engine/src/config.rs`, wartość domyślna 3. Próg liczy kolejne nieudane próby tego samego kroku. Próby od 1 do progu używają tego samego profilu. Próba po progu podnosi effort. Gdy effort stoi na granicy, rośnie model. Gdy oba stoją na granicy, krok kończy się błędem.

### Szkic reguły (kierunek, nie implementacja)

Ilustruje zamierzone podejście. To wskazówka do przeglądu, nie kod do skopiowania.

```
effective(step):
  base     = workflow default (model, effort)
  ceiling  = workflow ceiling or base
  wanted   = conductor profile for step/unit, else base
  if wanted > ceiling: start = ceiling; open request for wanted
  else: start = wanted            # lowering is always allowed
  if failures >= threshold:
      raise effort one step up to ceiling.effort
      if effort at ceiling: raise model one tier up to ceiling.model
      if both at ceiling and a ceiling exists: limit reached (D8)
```

---

## Implementation Units

### U1. Pola `effort` i granica w definicji workflow oraz funkcja profilu

**Goal:** Krok agenta może mieć `effort` i górną granicę. Czysta funkcja wylicza profil efektywny.

**Requirements:** R1, R2, R6, R7, R8, R9, R10, AE1, AE2, AE4, AE5

**Dependencies:** brak

**Files:**
- Modify: `crates/narsor-engine/src/domain/workflow_definition.rs`
- Modify: `crates/narsor-engine/src/workflow/schema.rs`
- Modify: `crates/narsor-engine/src/workflow/loader.rs`
- Modify: `crates/narsor-engine/src/workflow/validator.rs`
- Create: `crates/narsor-engine/src/engine/step_profile.rs`
- Modify: `crates/narsor-engine/src/engine/mod.rs`
- Modify: `docs/dev/workflow-schema.md`
- Modify: `workflows/template.yaml`
- Test: `crates/narsor-engine/src/engine/step_profile.rs` (moduł `tests`), testy walidatora i loadera obok istniejących testów w tych plikach

**Approach:**
- Dodaj `effort: Option<String>` i `ceiling: Option<{model, effort}>` do `AgentActionConfig` z `#[serde(default)]`, tak jak `model`. Wykonania zapisane przed zmianą muszą się wczytywać.
- Dodaj do sekcji globalnej `default_model`, `default_effort` i sufit ogólny `ceiling` (D9). Krok nadpisuje pole po polu. Ładowanie kopiuje wartości do kroku.
- Walidator: effort tylko z dozwolonej skali. Granica nie jest niższa niż wartość domyślna. Model i model z granicy to jeden z czterech poziomów (D11). Krok z sufitem ma model i effort po dziedziczeniu z `globals` (D8). Pola `effort` i `ceiling` przechodzą ten sam test szablonów co `model` (`action.model` w `validator.rs`).
- `step_profile.rs` implementuje szkic z Key Technical Decisions jako funkcje bez wejścia/wyjścia.

**Patterns to follow:** `engine/retry.rs` (logika w jednym miejscu, testy bez workflow), obsługa `model` w `loader.rs` i `validator.rs`.

**Test scenarios:**
- Happy path: krok bez `effort` i bez granicy kompiluje się, a `step_profile` zwraca wartości domyślne (AE5).
- Happy path: profil dyrygenta niższy niż domyślny jest przyjęty bez ograniczeń. Covers AE1.
- Happy path: profil dyrygenta równy granicy jest przyjęty.
- Edge case: profil `opus/max` przy granicy `opus/high` daje start `opus/high` i znacznik „żądano powyżej granicy" z wartością `opus/max`. Covers AE2.
- Edge case: brak granicy, profil wyższy niż domyślny: wynik to wartość domyślna.
- Edge case: model spoza poziomów w profilu dyrygenta: wartość domyślna modelu i znacznik odrzucenia.
- Edge case: próg 3, błędy 0, 1, 2: ten sam profil. Błąd 3: effort o stopień wyżej. Covers AE4.
- Edge case: effort na granicy, błąd po progu: model o poziom wyżej. Covers AE4.
- Error path: oba na granicy, błąd po progu: wynik „granica osiągnięta". Bez sufitu nic nie jest osiągnięte (D8).
- Error path: walidator odrzuca `effort: ultra`, granicę niższą niż domyślna, model spoza poziomów oraz krok z sufitem bez modelu lub effortu po dziedziczeniu z `globals`.
- Integration: stary zapis `AgentActionConfig` bez nowych pól wczytuje się bez błędu.

**Verification:** Wszystkie testy funkcji profilu przechodzą bez uruchamiania workflow. Istniejące testy loadera i walidatora przechodzą bez zmian.

---

### U2. Przekazanie effortu do harnessów

**Goal:** Adapter dodaje effort do wywołania harnessu. Harness bez obsługi effortu ignoruje pole z ostrzeżeniem.

**Requirements:** R1, AE5

**Dependencies:** U1

**Files:**
- Modify: `crates/narsor-engine/src/engine/actions/harness/mod.rs` (request niesie profil efektywny)
- Modify: `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`
- Modify: `crates/narsor-engine/src/engine/actions/harness/codex.rs`
- Modify: `crates/narsor-engine/src/engine/actions/harness/opencode.rs`
- Modify: `crates/narsor-engine/src/engine/actions/harness/github_copilot.rs`
- Modify: `crates/narsor-engine/src/engine/actions/harness/copilot_sdk.rs`
- Modify: `crates/narsor-engine/src/engine/actions/agent.rs`
- Test: testy `tests` w każdym adapterze, `tests/integration/agent_harness_selection.rs`, `tests/integration/actions_agent.rs`

**Approach:**
- `HarnessRequest` dostaje profil efektywny (model i effort). Adapter czyta model i effort z niego, a nie bezpośrednio z `config`. Gdy profil jest pusty, zachowanie jest takie samo jak dziś.
- Adapter mapuje poziom effortu na argument harnessu. Mapowanie poziomów to szczegół adaptera (patrz Deferred to Implementation).
- Harness bez obsługi (domyślnie Copilot, do sprawdzenia): pole jest ignorowane, a silnik emituje zdarzenie ostrzegawcze raz na próbę.
- `reject_option_like` obejmuje effort tak jak model.
- Argumenty `extra_args` nadal wygrywają (patrz `apply_extra_args` w `harness/mod.rs`). Przy konflikcie z effortem z profilu obowiązuje reguła istniejąca: argument rozszerzony zastępuje poprzedni.

**Patterns to follow:** gałąź `config.model` w `claude_code.rs` i `codex.rs`.

**Test scenarios:**
- Happy path: Claude Code z effortem `high` dostaje `--effort high` obok `--model`.
- Happy path: Codex z effortem dostaje argument konfiguracji zgodny z jego CLI.
- Edge case: brak effortu i brak profilu daje te same argumenty co przed zmianą (test porównawczy z zapisanym wzorcem). Covers AE5.
- Edge case: `extra_args` z `--effort low` zastępuje effort z profilu.
- Error path: wartość effortu zaczynająca się od `-` jest odrzucona jak model.
- Error path: harness bez obsługi effortu nie przerywa kroku i emituje zdarzenie ostrzegawcze.
- Integration: `AgentSessionStarted` zapisuje effort z wywołania (istniejące pole `effort`).

**Verification:** Test porównawczy AE5 przechodzi. Oś czasu pokazuje effort dla adaptera, który go obsługuje.

---

### U3. Zapis i odczyt profilu kroków

**Goal:** Profil kroków z uzasadnieniem jest trwały i dostępny dla silnika, API i TUI.

**Requirements:** R3, R4, R11

**Dependencies:** U1

**Files:**
- Modify: `crates/narsor-engine/src/conductor/store.rs` (nowy plik `profile.json` w `conductor/`, odczyt i atomowy zapis)
- Modify: `crates/narsor-engine/src/persistence/` (ścieżka, jeśli `StatePaths` jej wymaga)
- Test: testy `tests` w `store.rs`

**Approach:**
- Struktura: wersja, lista wpisów `{step_id, unit?, model?, effort?, rationale, requested_over_ceiling?, decided_at}`. Wpis powstaje raz po planie (decyzja raz, R3). Kroki już uruchomione nie zmieniają profilu: zapis pomija wpisy dla kroków ze stanem rozpoczętym.
- Brak pliku to pusty profil, nie błąd (jak dla starych wykonań w `store.rs`).

**Patterns to follow:** `write_json_atomic`, `read_json` i `title.json` w `conductor/store.rs`.

**Test scenarios:**
- Happy path: zapis i odczyt zwracają ten sam profil z uzasadnieniem.
- Edge case: brak pliku zwraca pusty profil.
- Edge case: wpis z `unit` i wpis bez `unit` dla tego samego kroku współistnieją. Odczyt dla jednostki zwraca wpis jednostki, a dla nieznanej jednostki zwraca wpis kroku. Covers R11.
- Edge case: drugi zapis dla kroku już uruchomionego jest odrzucony.
- Error path: uszkodzony plik daje błąd, który wywołujący traktuje jak brak profilu (R8).

**Verification:** Profil przeżywa restart serwera. Odczyt nie wymaga blokady.

---

### U4. Ocena złożoności w wyniku kroku planu

**Goal:** Krok planu zwraca ocenę złożoności razem z planem.

**Requirements:** R5, F1

**Dependencies:** brak (równolegle z U1)

**Files:**
- Modify: `crates/narsor-engine/src/domain/` (typ oceny, np. w nowym pliku `complexity.rs`, eksport w `domain/mod.rs`)
- Modify: plik workflow z krokiem planu (patrz Deferred to Implementation: lokalizacja)
- Modify: `workflows/template.yaml` (przykład schematu wyjścia)
- Modify: `docs/dev/workflow-schema.md`
- Test: test typu oceny w `complexity.rs`

**Approach:**
- Typ oceny: liczba plików, liczba modułów, znacznik migracji lub zmiany kontraktu, znacznik braku testów, lista zależności jednostek. Wszystkie pola są opcjonalne, aby brak jednego pola nie unieważnił oceny.
- Ocena jest częścią schematu wyjścia kroku planu. Silnik ją tylko przechowuje i podaje dyrygentowi. Silnik jej nie interpretuje.

**Test scenarios:**
- Happy path: poprawna ocena parsuje się ze wszystkimi polami.
- Edge case: ocena z brakującymi polami parsuje się, a brakujące pola są puste.
- Error path: ocena o złym typie pola jest odrzucona, a plan nadal jest przyjęty (brak oceny, wartości domyślne).

**Verification:** Wyjście kroku planu w przykładzie z `template.yaml` przechodzi walidację schematu.

---

### U5. Wywołanie dyrygenta po planie

**Goal:** Po udanym kroku planu dyrygent zapisuje profil dla kroków, które jeszcze nie ruszyły.

**Requirements:** R3, R5, R6, R7, R8, F2, AE1, AE2, AE3

**Dependencies:** U1, U3, U4

**Files:**
- Modify: `crates/narsor-protocol/src/event.rs` (`ConductorPurpose::ProfileSteps`)
- Modify: `crates/narsor-protocol/src/api.rs` (`ConductorPurposeDto::ProfileSteps`)
- Modify: `crates/narsor-engine/src/conductor/prompt.rs`
- Modify: `crates/narsor-engine/src/conductor/response.rs`
- Modify: `crates/narsor-engine/src/conductor/invoke.rs`
- Create: `crates/narsor-engine/src/conductor/profile.rs`
- Modify: `crates/narsor-engine/src/conductor/mod.rs`
- Modify: `crates/narsor-engine/src/engine/runner.rs` (wyzwolenie po zakończeniu kroku planu)
- Test: testy `tests` w `profile.rs`, `tests/integration/` (nowy plik `step_profile_flow.rs`)

**Approach:**
- Wyzwalacz: zakończenie sukcesem kroku oznaczonego w workflow jako krok planu (znacznik w definicji kroku; nazwa pola ustala wykonawca razem z U1/U4). Pozostałe kroki nie wyzwalają.
- Prompt dostaje plan, ocenę złożoności, listę kroków jeszcze nieuruchomionych (z wartościami domyślnymi i granicami) oraz skale poziomów i effortu z kodu (`MODEL_TIERS`, `EFFORT_SCALE`, D9). Odpowiedź ma ściśle zdefiniowany schemat: lista `{step_id, unit?, model, effort, rationale, confident}`.
- Walidacja odpowiedzi przechodzi przez `step_profile`: wartości nad granicę są obcinane do granicy, a `requested_over_ceiling` zapisuje żądanie. Wpisy z `confident: false` albo niepoprawne są pomijane (wartość domyślna, R8).
- Wywołanie nie blokuje zadania. Silnik nie czeka na dyrygenta w nieskończoność: gdy wywołanie nie skończy się przed startem następnego kroku, ten krok startuje na wartościach domyślnych (R8). Limit czasu to parametr, wartość ustala wykonawca w U5 i dokumentuje w konfiguracji.
- Dyrygent nie pisze kodu i nie zmienia innych danych (A2).

**Patterns to follow:** `conductor/failure.rs` (diagnoza po błędzie, zapis wyniku w `conductor/`), `conductor/title.rs` (wywołanie pomocnicze bez wpływu na przepływ).

**Test scenarios:**
- Happy path: plan z jedną małą zmianą i odpowiedź obniżająca model dla `work` zapisuje profil z modelem niższym niż domyślny. Covers AE1.
- Happy path: profil dla jednostek `unit 3 → opus/high` zapisuje wpis z `unit`. Covers R11.
- Edge case: żądanie `opus/max` przy granicy `opus/high` zapisuje start `opus/high` i `requested_over_ceiling`. Zadanie idzie dalej bez czekania. Covers AE2.
- Edge case: wpis z `confident: false` nie powstaje.
- Edge case: wpis dla kroku już uruchomionego jest pominięty.
- Error path: wywołanie dyrygenta kończy się błędem. Brak profilu, krok `work` startuje na wartościach domyślnych, w zdarzeniach jest jedno ostrzeżenie. Covers AE3.
- Error path: odpowiedź niezgodna ze schematem jest traktowana jak błąd wywołania.
- Integration: pełny przebieg z fałszywym backendem (`crates/narsor-engine/src/backends/fake.rs`): plan kończy się, powstaje profil, następny krok dostaje argumenty z profilu.

**Verification:** Zadanie z awarią dyrygenta kończy się tak samo jak zadanie bez tej funkcji. Argumenty następnego kroku zgadzają się z profilem.

---

### U6. Otwarta prośba o wyjście poza granicę

**Goal:** Żądanie powyżej granicy tworzy otwartą prośbę dla operatora. Zadanie nie czeka.

**Requirements:** R7, AE2

**Dependencies:** U5

**Files:**
- Modify: `crates/narsor-engine/src/conductor/profile.rs`
- Modify: `crates/narsor-engine/src/engine/runner/` (miejsce, w którym tworzone są istniejące prośby, do ustalenia)
- Modify: `crates/narsor-protocol/src/api.rs` (jeśli prośba wymaga nowego typu DTO)
- Test: `tests/integration/step_profile_flow.rs`

**Approach:**
- Prośba używa istniejącego mechanizmu próśb zadania (patrz `src/tui/task_app/panel/request.rs`), jeśli ten pozwala na prośbę, która nie blokuje wykonania. Jeśli istniejący mechanizm zawsze blokuje, plan tworzy zapis prośby w profilu (`requested_over_ceiling`) i pokazuje go w TUI jako wpis informacyjny. Rozstrzygnięcie prośby jest poza zakresem (Deferred to Follow-Up Work).
- Prośba zawiera krok, żądaną wartość, granicę i uzasadnienie dyrygenta.

**Test scenarios:**
- Happy path: żądanie powyżej granicy tworzy dokładnie jedną otwartą prośbę. Covers AE2.
- Edge case: dwa kroki żądają ponad granicę: powstają dwie osobne prośby.
- Edge case: żądanie w granicy nie tworzy prośby.
- Integration: krok ze znacznikiem żądania startuje na granicy, a zadanie nie przechodzi w stan oczekiwania.

**Verification:** Zadanie z otwartą prośbą dochodzi do końca bez interwencji operatora.

---

### U7. Podgląd profilu w TUI

**Goal:** Operator widzi profil każdego kroku i jego uzasadnienie. Bez edycji.

**Requirements:** R4, A3

**Dependencies:** U3, U6

**Files:**
- Modify: `crates/narsor-engine/src/api/dto.rs` oraz `crates/narsor-protocol/src/api.rs` (pole profilu w odpowiedzi o zadaniu lub nowy punkt odczytu)
- Modify: `crates/narsor-api-client/src/api/client.rs`
- Modify: `src/tui/task_app/tabs/progress.rs`
- Modify: `src/tui/task_app/tabs/info.rs`
- Test: testy `tests` w tych plikach TUI, testy DTO obok istniejących

**Approach:**
- Krok pokazuje `model/effort` i skrót uzasadnienia. Jeśli źródłem jest wartość domyślna, TUI pokazuje to jako „domyślny". Jeśli krok został obcięty do granicy, TUI pokazuje żądaną wartość i znacznik otwartej prośby.
- Pole w DTO jest opcjonalne. Starszy klient je ignoruje, a nowy klient przy starszym serwerze nie pokazuje nic.
- Brak jakiegokolwiek klawisza edycji.

**Patterns to follow:** plany `2026-11-10-2142-feat-tui-ikony-stanu-zadan-plan.md` i `2026-11-07-2215-feat-tui-ikony-krokow-plan.md` dla układu kolumn i testów renderowania.

**Test scenarios:**
- Happy path: krok z profilem pokazuje `opus/high` i uzasadnienie.
- Edge case: krok bez profilu pokazuje „domyślny".
- Edge case: krok obcięty do granicy pokazuje żądaną wartość i znacznik prośby.
- Edge case: zadanie sprzed zmiany (brak profilu) nie psuje widoku.
- Error path: klient dostaje profil z nieznanym modelem i pokazuje go jako tekst.

**Verification:** Operator widzi profil przed startem kroku (F2, wynik).

---

### U8. Eskalacja po błędach w silniku

**Goal:** Po progu nieudanych prób krok dostaje mocniejszy profil w granicy.

**Requirements:** R9, R10, F3, AE4

**Dependencies:** U1, U2

**Files:**
- Modify: `crates/narsor-engine/src/config.rs` (parametr progu, domyślnie 3)
- Modify: `crates/narsor-engine/src/engine/runner.rs` oraz `crates/narsor-engine/src/engine/retry.rs` (licznik kolejnych błędów kroku, wywołanie `step_profile`)
- Modify: `docs/user/` lub `docs/operations/` (opis parametru; wykonawca wybiera właściwy plik)
- Test: `crates/narsor-engine/src/engine/retry.rs` (moduł `tests`), `tests/integration/step_profile_flow.rs`

**Approach:**
- Licznik liczy kolejne nieudane próby tego samego kroku (i jednostki). Sukces zeruje licznik. Ponowne wejście do kroku po cofnięciu przez człowieka zeruje licznik (do potwierdzenia w testach).
- Przed każdą próbą silnik wywołuje `step_profile` z licznikiem i progiem. Wynik trafia do `HarnessRequest` (U2).
- Zdarzenie osi czasu zapisuje eskalację (z którego profilu na który).
- Przy osiągniętej granicy krok kończy się błędem jak dziś. Niczego nie podnosimy.
- Eskalacja działa także na kroku bez profilu dyrygenta, o ile krok ma granicę.

**Test scenarios:**
- Happy path: próg 3, próby 1–3 z tym samym profilem, próba 4 z effortem o stopień wyższym. Covers AE4.
- Happy path: effort na granicy, próba 4 podnosi model. Covers AE4.
- Edge case: próba udana po dwóch błędach zeruje licznik.
- Edge case: krok bez granicy nie eskaluje.
- Edge case: próg zmieniony w konfiguracji (np. 2) zmienia moment eskalacji.
- Error path: oba na granicy, kolejny błąd kończy krok błędem bez eskalacji.
- Integration: oś czasu zawiera zdarzenie eskalacji i `AgentSessionStarted` z nowym effortem.

**Verification:** Test integracyjny z fałszywym backendem pokazuje zmianę argumentów w próbie 4.

---

## System-Wide Impact

- **Interakcje:** Silnik czyta profil z katalogu dyrygenta. Dyrygent pisze profil. Obie strony działają równolegle, a atomowy zapis chroni odczyt (U3).
- **Propagacja błędów:** Każdy błąd w U5 i U6 kończy się wartościami domyślnymi. Zadanie nie zmienia stanu z powodu profilu.
- **Zmiana stanu:** Nowy plik `profile.json` w `conductor/`. Zadania sprzed zmiany go nie mają i działają bez zmian.
- **Zgodność wstecz:** Nowe pola definicji mają `serde(default)`. Nowy wariant `ConductorPurpose` jest czytany przez `#[serde(other)]` u starszych klientów.
- **Zgodność API:** Pole profilu w DTO jest opcjonalne.
- **Niezmienniki:** Workflow bez nowych pól daje te same argumenty harnessu co dziś (AE5). Test porównawczy w U2 pilnuje tego.

---

## Risks & Dependencies

| Ryzyko | Skutek | Środek |
|---|---|---|
| Flagi effortu różnią się między harnessami i wersjami CLI | Zły argument psuje start kroku | Sprawdzenie na zainstalowanych CLI przed U2. Harness bez obsługi ignoruje pole z ostrzeżeniem. |
| Model nie ma naturalnego porządku | Granica i eskalacja modelu są niejednoznaczne | Cztery poziomy modeli niezależne od harnessu (D11). Mapa poziom na nazwę modelu w adapterze harnessu. |
| Wywołanie dyrygenta po planie opóźnia następny krok | Zadanie wolniejsze | Limit czasu i wartości domyślne po jego upływie (U5). |
| Dyrygent obniża profil za agresywnie | Więcej rund przeglądu | Eskalacja po błędach (U8) i pomiar po wdrożeniu (Deferred to Follow-Up Work). |

---

## Documentation Plan

- `docs/dev/workflow-schema.md`: pola `effort`, `ceiling`, `default_model`, `default_effort`, znacznik kroku planu, schemat oceny złożoności.
- `workflows/template.yaml`: przykład użycia pól.
- Dokument operacyjny: parametr progu eskalacji i znaczenie profilu w TUI.
- `docs/follow-ups/2026-10-28-dobor-modelu-i-effortu-per-krok-workflow.md`: dopisać odnośnik do tego planu po wdrożeniu.

---

## Deferred to Implementation

- Dokładne flagi effortu i mapowanie poziomów dla Claude Code, Codex i OpenCode. Wykonawca sprawdza je na zainstalowanych CLI. Obsługa effortu w Copilot (SDK i CLI) jest niesprawdzona.
- Dokładne nazwy pól i typów (`ceiling`, `default_model`, `default_effort`, znacznik kroku planu, `step_profile`).
- Limit czasu wywołania dyrygenta po planie (U5).
- Czy ponowne wejście do kroku zeruje licznik błędów (U8).
- Czy istniejący mechanizm próśb obsługuje prośbę nieblokującą (U6).

---

## Decyzje operatora i pytania otwarte

1. **Zakres: każdy workflow.** Plan nie dotyczy konkretnego workflow (np. `compound-engineering.yaml`). Plan tworzy możliwość sterowania modelem i effortem w każdym workflow. Pola i dokumentacja trafiają do silnika, `template.yaml` i `docs/dev/workflow-schema.md`. Wartości domyślne konkretnych workflow są poza zakresem.
2. **Jak krok `work` rozpoznaje jednostkę planu (R11): profil per krok (opcja A).** Operator wybrał opcję A. Origin wymaga profilu per jednostka (`ce-work: unit 3`). Nie wiadomo, czy workflow uruchamia `work` osobnym krokiem na jednostkę, czy jednym krokiem na cały plan.
   - Opcja A (wybrana): Pierwsza wersja zapisuje profil per krok, a klucz `unit` jest w danych (U3), ale silnik go jeszcze nie wypełnia. Profil per jednostka wchodzi, gdy workflow zacznie przekazywać numer jednostki.
   - Opcja B (odrzucona): Pierwsza wersja wymaga, aby workflow przekazywał numer jednostki do kroku `work` (zmiana definicji kroku i pętli). Zakres rośnie o zmianę modelu pętli.
3. **Rozstrzyganie prośby o wyjście poza granicę: poza zakresem.** Operator na razie tym nie steruje. Plan tworzy i pokazuje prośbę (U6, U7). Krok zostaje na granicy. Plan nie dodaje akcji zatwierdź/odrzuć.
4. **Krok jest „rozpoczęty" według licznika wejść (D7, Q7 A).** `conclude_profile` i `profile_after_plan` liczą kroki rozpoczęte z `state_entry_count > 0`. Wpis profilu dla takiego kroku nie jest zapisywany. `apply_step_profile` ignoruje wpis ustalony po pierwszym wejściu w krok (silnik zapisuje czas pierwszego wejścia).
5. **Sufit i eskalacja (D8, Q8 A).** Walidator przy ładowaniu workflow sprawdza każdy krok z sufitem (własnym lub ogólnym): ma model i effort po dziedziczeniu z `globals`, a sufit ma wartość dla każdego pola ustawionego w kroku. `limit_reached` ma wartość prawda tylko wtedy, gdy sufit istnieje i został osiągnięty.
6. **Wartości domyślne ogólne (D9).** `globals.default_model`, `globals.default_effort` i `globals.ceiling`. Krok nadpisuje pole po polu. Prompt dyrygenta bierze skale z `EFFORT_SCALE` i `MODEL_TIERS`, nie z kopii w tekście.
7. **Model i effort dyrygenta (D10).** Ustawienia demona `conductor_model` (poziom) i `conductor_effort`. Puste pole znaczy domyślny harnessu. `conductor_config` w `invoke.rs` je stosuje.
8. **Poziomy modeli niezależne od harnessu (D11, Q8 A).** `globals.model_ladder` usunięte. Cztery poziomy `small`, `medium`, `large`, `max` obowiązują w workflow, w profilu dyrygenta, w eskalacji, w suficie kroku oraz w `conductor_model`. Mapa poziom na nazwę modelu jest w adapterze harnessu; brak wpisu to błąd przy starcie demona. Nazwy modeli w mapie to wartości domyślne do potwierdzenia przez operatora (zwłaszcza codex, opencode i copilot).
