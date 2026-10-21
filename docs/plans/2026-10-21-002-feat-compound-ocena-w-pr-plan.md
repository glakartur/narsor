---
title: "feat: Workflow compound — ocena użytkownika w PR, nie w trakcie kroków"
type: feat
status: active
date: 2026-10-21
origin: docs/brainstorms/2026-10-21-compound-ocena-w-pr-requirements.md
---

# feat: Workflow compound — ocena użytkownika w PR, nie w trakcie kroków

## Summary

Zmiana dotyczy tylko definicji workflow i jej dokumentacji. Silnik nie zmienia się. Trzy zmiany:

1. Krok `brainstorm` dostaje drugi wybór. Agent może pominąć `doc_review` i iść od razu do `publish_requirements`.
2. Wybory `reviewed` w `doc_review` i `plan_review` tracą `requires_human_approval`. Wynik idzie do publikacji bez zatrzymania.
3. Prompt `brainstorm` zabrania pytań do użytkownika. Agent zapisuje założenia i pytania otwarte w dokumencie. Użytkownik odpowiada w PR.

Powód decyzji o przeglądzie i podsumowanie przeglądu trafiają do PR przez krok publikacji.

---

## Problem Frame

Użytkownik ocenia ten sam dokument dwa razy: przed publikacją (`requires_human_approval`) i po publikacji (bramki `approve_*`). Pierwsza ocena jest mniej wygodna niż PR. Krok `brainstorm` może też wstrzymać workflow pytaniem. (see origin: `docs/brainstorms/2026-10-21-compound-ocena-w-pr-requirements.md`)

---

## Requirements

Wymagania R1–R9 z dokumentu źródłowego obowiązują bez zmian. Mapa na jednostki:

- R1, R2, R3, R4 → U1, U2
- R5 → U2
- R6 → U2
- R7 → U1–U4 (nie ruszamy bramek `approve_*`; testy to sprawdzają w U4)
- R8, R9 → U1

---

## Scope Boundaries

- Bramki `approve_requirements`, `approve_plan`, `approve_work`, `approve_learning` oraz `finalize_blocked` zostają. Ich wybory i przejścia nie zmieniają się.
- Skill `ce-doc-review` nie zmienia się.
- Silnik, walidator i schemat (`crates/narsor-engine/src/workflow/`, `WORKFLOW_SCHEMA.md`) nie zmieniają się. Pole `requires_human_approval` zostaje w schemacie. Inne workflow i testy używają go dalej.
- Krok `work` i jego pytania zostają bez zmian (założenie z źródła).
- Pominięcie przeglądu dotyczy tylko wymagań. `plan_review` uruchamia się zawsze (założenie z źródła).

### Deferred to Follow-Up Work

- Gdy użytkownik zechce zostawić pytania blokujące w `brainstorm`, trzeba zawęzić R8. Ten plan realizuje R8 tak, jak jest napisane.

---

## Context & Research

Zweryfikowano w repozytorium:

- `workflows/compound-engineering.yaml`: `requires_human_approval: true` jest tylko w `doc_review` i `plan_review`. Przejście `requirements_drafted: doc_review` jest w `brainstorm`.
- `crates/narsor-engine/src/workflow/validator.rs`: pole jest dozwolone tylko na wyborze kroku `agent`. Jego brak nie łamie walidacji.
- `crates/narsor-engine/src/engine/runner.rs` (około linii 3001): zatrzymanie następuje tylko dla wyboru z tym polem. Bez pola wynik idzie od razu do przejścia.
- Publikacja nie wspomina dziś o `review_summary`. Prompty `publish_requirements` i `publish_plan` mówią „bez przepisywania opisu MR". Komunikaty bramek mówią „dokument i jego przegląd w MR". Bez zmiany promptu publikacji podsumowanie nie trafi do PR (R6).
- `check_requirements_feedback` używa już `reason` do przekazania uzasadnienia. Wybór kroku zapisuje `reason` w historii zadania.
- `tests/builtin_workflows.rs`: test `compound_engineering_choices_require_their_variables_and_artifacts` sprawdza `requires_human_approval` dla obu przeglądów. Ten test trzeba odwrócić. Ignorowany test `an_execution_started_from_a_builtin_by_name_is_resumable_in_a_fresh_process` zakłada, że `brainstorm` pyta użytkownika (`awaiting_input`).
- `workflows/README.md` (sekcja „five stages") i `workflows/compound-engineering-flow.html` opisują akceptację przed publikacją.

Badania zewnętrzne: pominięte. Zmiana jest lokalna, wzorce są w tym samym pliku.

---

## Key Technical Decisions

- **Dwa wybory w `brainstorm`.** `requirements_drafted` zostaje z przejściem do `doc_review`. Nowy wybór `requirements_ready` idzie do `publish_requirements`. Nazwa istniejącego wyboru nie zmienia się, więc `check_requirements_feedback` i historia nie łamią się. Szczegół nazwy nowego wyboru: implementator może ją zmienić, jeśli koliduje z konwencją.
- **Powód w zmiennej `review_summary`.** `brainstorm` definiuje zmienną `review_summary`. Nowy wybór ją wymaga (`requires_variables`). Agent pisze tam jedno zdanie: „Przegląd pominięto: <powód>". Taka sama zmienna jest wyjściem `doc_review`, więc `publish_requirements` czyta jedno pole w obu ścieżkach. Zapis nadpisuje stare podsumowanie z wcześniejszej rundy, więc nie zostaje nieaktualny tekst. Powód trafia też do historii zadania przez `reason` wyboru.
- **Publikacja wstawia podsumowanie do PR.** Prompty `publish_requirements` i `publish_plan` dostają polecenie: dodaj `{{ context.review_summary }}` jako komentarz do MR (nie zmieniaj opisu). Komentarz jest widoczny przy bramce. To spełnia R2 i R6.
- **Przegląd jest domyślny (R4).** Prompt `brainstorm` mówi: pomiń przegląd tylko wtedy, gdy zmiana jest mała i oczywista. Przy wątpliwości wybierz `requirements_drafted`.
- **Brak pytań (R8).** Prompt `brainstorm` zastępuje zdanie „ask the person" zakazem. Treść: nie pytaj użytkownika. Nie używaj narzędzia do pytań. Nie zwracaj `awaiting_input`. Gdy kierunek jest niejasny, przyjmij najbezpieczniejsze założenie. Zapisz je w sekcji „Założenia do potwierdzenia". Zapisz pytania w sekcji „Pytania otwarte dla użytkownika". Sekcje są osobne i oznaczone. Skill `ce-brainstorm` pyta domyślnie, więc zakaz musi być w prompcie jawny i w tym samym akapicie co wywołanie skilla.
- **Komunikat bramki `approve_requirements`.** Dodać zdanie: odpowiedz na pytania otwarte w PR lub w polu `requirements_feedback`. Dzięki temu R9 jest widoczne dla użytkownika. Wybory i przejścia bramki nie zmieniają się.
- **Wynik przeglądu bez zatrzymania.** Usuwamy tylko `requires_human_approval: true`. Zostają `requires_variables`, `retry_policy` i `timeout`.

---

## Implementation Units

### U1. Krok `brainstorm`: dwie ścieżki i brak pytań

**Goal:** Agent wybiera przegląd albo publikację. Agent nie pyta użytkownika.

**Requirements:** R1, R2, R3, R4, R8, R9

**Dependencies:** brak

**Files:**
- `workflows/compound-engineering.yaml` (modyfikacja)
- `tests/builtin_workflows.rs` (test)

**Approach:**
- Dodać do `variables` kroku `brainstorm` zmienną `review_summary` z opisem: powód pominięcia przeglądu w jednym zdaniu.
- Dodać wybór `requirements_ready` z `requires_artifacts: [requirements_doc]` i `requires_variables: [review_summary]`. Opis: dokument jest gotowy, przegląd niepotrzebny.
- Dodać przejście `requirements_ready: publish_requirements`.
- Przepisać akapit promptu: zakaz pytań (R8), zasada przeglądu (R4), wymóg sekcji „Założenia do potwierdzenia" i „Pytania otwarte dla użytkownika". Zdanie o `requirements_blocker` zostaje.
- Zaktualizować `description` kroku: brak rozmowy z użytkownikiem.

**Patterns to follow:** Wybór z `requires_variables` w `plan` (`needs_requirements_revision`). Zmienne czyszczone w prompcie (`requirements_blocker`).

**Test scenarios:**
- Happy path: `brainstorm` ma wybory `requirements_drafted` i `requirements_ready`. Pierwszy przechodzi do `doc_review`, drugi do `publish_requirements`.
- Happy path: `requirements_ready` wymaga zmiennej `review_summary` i artefaktu `requirements_doc`. `review_summary` jest zdefiniowana na kroku.
- Happy path: prompt `brainstorm` nie zawiera zdania „ask the person". Prompt zawiera zakaz pytań i nazwy obu sekcji.
- Edge case: `requirements_drafted` nadal wymaga tylko `requirements_doc` (bez `review_summary`).
- Integration: walidacja wbudowanych workflow (`every_builtin_is_embedded_and_validates_without_a_single_violation`) przechodzi. Każdy stan jest osiągalny.

**Verification:** Definicja ładuje się bez naruszeń. Oba przejścia istnieją. Prompt nie zachęca do pytań.

---

### U2. Przeglądy bez akceptacji przed publikacją, podsumowanie w PR

**Goal:** Wynik `doc_review` i `plan_review` idzie do publikacji bez zatrzymania. Podsumowanie jest w PR.

**Requirements:** R2, R5, R6, R7

**Dependencies:** U1 (zmienna `review_summary` jest wspólna dla ścieżek)

**Files:**
- `workflows/compound-engineering.yaml` (modyfikacja)
- `tests/builtin_workflows.rs` (test)

**Approach:**
- Usunąć `requires_human_approval: true` z wyboru `reviewed` w `doc_review` i w `plan_review`.
- Zmienić `description` obu kroków: zdanie „A person accepts the result before it is published" znika. Zamiast niego: wynik trafia do MR, osoba czyta go przy bramce.
- W promptach `publish_requirements` i `publish_plan` dodać polecenie: dodaj `{{ context.review_summary }}` jako komentarz do MR. Opis MR zostaje bez zmian.
- Zmienić `description` workflow (nagłówek pliku), jeśli mówi o akceptacji przed publikacją. Dziś nie mówi, więc zmiana jest tylko gdy tekst się nie zgadza.
- Dodać w komunikacie `approve_requirements` zdanie o pytaniach otwartych (patrz decyzje). Nie ruszać wyborów, przejść ani pól `inputs`.

**Patterns to follow:** Istniejące polecenia w promptach publikacji („without rewriting its description").

**Test scenarios:**
- Happy path: wybór `reviewed` w `doc_review` i `plan_review` ma `requires_human_approval == false` i nadal wymaga `review_summary`.
- Happy path: prompty `publish_requirements` i `publish_plan` zawierają `{{ context.review_summary }}`.
- Edge case: `publish_requirements` dostaje przejście z `brainstorm` (`requirements_ready`), `doc_review` (`reviewed`) i `check_requirements_feedback` (`fixed_minor`). Podsumowanie jest ustawione w każdej z tych ścieżek. Sprawdzić, że w ścieżce `fixed_minor` stare `review_summary` nie wprowadza w błąd (ostatni przegląd lub ostatnie pominięcie, patrz ryzyko).
- Integration: żaden wybór w `compound-engineering` nie ma `requires_human_approval`.
- Integration (R7): bramki `approve_requirements`, `approve_plan`, `approve_work`, `approve_learning` mają te same wybory i przejścia co przed zmianą.

**Verification:** Test odwrócony zgodnie z nowym zachowaniem. Żaden krok przed publikacją nie czeka na człowieka.

---

### U3. Dokumentacja: README i diagram

**Goal:** Opis zgadza się z workflow.

**Requirements:** R1, R5, R6, R7

**Dependencies:** U1, U2

**Files:**
- `workflows/README.md` (modyfikacja)
- `workflows/compound-engineering-flow.html` (modyfikacja)

**Approach:**
- README, schemat pięciu etapów: `brainstorm → doc_review* → …` zmienić tak, aby pokazać drugą ścieżkę (`brainstorm → publish_requirements` bez przeglądu).
- README, akapit pod `*`: zamienić „a person accepts that result before it is published, or returns it to the same review session with a comment" na opis nowy. Przegląd poprawia dokument. Podsumowanie trafia do MR jako komentarz. Osoba czyta je przy bramce. Dodać zdanie o pominięciu przeglądu przez agenta tylko dla wymagań, z powodem w MR i w historii.
- README: dodać zdanie, że `brainstorm` nie pyta użytkownika. Założenia i pytania otwarte są w dokumencie. Użytkownik odpowiada w PR.
- HTML: dodać krawędź `brainstorm → publish_requirements` z etykietą (np. `requirements_ready`). Współrzędne SVG dobrać tak, aby krawędź nie przecinała węzła `doc_review` (węzły stoją pionowo w kolumnie x=425). Zaktualizować akapit „Wymagania i plan": zamiast „Człowiek akceptuje ten wynik, zanim trafi do MR" opisać nowy przepływ.

**Test expectation:** none — dokumentacja i diagram. Sprawdzenie ręczne: zdania w README zgadzają się z YAML.

**Verification:** Nie ma w README ani HTML zdania o akceptacji przed publikacją. Diagram pokazuje obie ścieżki.

---

### U4. Dostosowanie testów istniejących

**Goal:** Testy odzwierciedlają nowe zachowanie. Żaden test nie zakłada pytania w `brainstorm`.

**Requirements:** R5, R7, R8

**Dependencies:** U1, U2

**Files:**
- `tests/builtin_workflows.rs`
- `tests/workflows.rs` (skrypty silnika `compound-engineering`)

**Approach:**
- Odwrócić asercję w `compound_engineering_choices_require_their_variables_and_artifacts`: pętla po `doc_review` i `plan_review` sprawdza, że `requires_human_approval` jest `false`. Dodać do tego testu asercję dla `requirements_ready`.
- Ignorowany test `an_execution_started_from_a_builtin_by_name_is_resumable_in_a_fresh_process` zakłada `awaiting_input` z `brainstorm`. Przepisać scenariusz: sztuczny harness odpowiada kolejno wyborami do `approve_requirements` (pierwszy stały punkt oczekiwania na człowieka). Pozostała część testu (rozstrzygnięcie definicji po nazwie w nowym procesie) zostaje. Użyć istniejącego `tests/fixtures/glab/fake_glab.sh` lub innej atrapy, jeśli publikacja jej wymaga.
- W `tests/workflows.rs`: usunąć `Answer::approval("doc_review")` z `requirements_gates()` (około linii 672) i `Answer::approval("plan_review")` z `plan_gates()` (około 682) oraz z testu około linii 1043. Silnik nie prosi już o te zatwierdzenia.
- W `tests/workflows.rs`: dodać `("brainstorm", "requirements_ready", "publish_requirements")` do `COMPOUND_EDGES` i zmienić długość z 62 na 63 (około linii 1241). Dodać jeden skryptowy przebieg ścieżką `requirements_ready`. Sprawdzić, że odpowiedzi dla `doc_review` i `plan_review` zawierają `review_summary` i że liczba odpowiedzi zgadza się z indeksami `run.prompt(n)`.
- Dodać test: żaden stan `compound-engineering` nie ma wyboru z `requires_human_approval`.
- Dodać test: wybory bramek `approve_*` nie zmieniły się (lista etykiet i przejść).

**Execution note:** Najpierw odwrócić asercje i zobaczyć czerwony test, potem zmieniać YAML (U1, U2). Test ignorowany uruchomić ręcznie z `--ignored`.

**Test scenarios:**
- Happy path: asercje z U1 i U2 w jednym miejscu, zielone po zmianie YAML.
- Integration: ignorowany test docieka do `approve_requirements` i odpowiedź przez `narsor input` działa w nowym procesie.
- Error path: gdyby `brainstorm` zwrócił `awaiting_input`, test nie liczy na to. Kontrakt z silnikiem dla `awaiting_input` zostaje pokryty innymi testami (`tests/actions_agent.rs`).

**Verification:** Pełny zestaw `cargo test` przechodzi. Test ignorowany przechodzi z `--ignored`.

---

## System-Wide Impact

- Użytkownik widzi mniej zatrzymań. Pierwsze zatrzymanie to `approve_requirements`.
- Historia zadania zawiera powód pominięcia przeglądu (`reason` wyboru).
- Zadania w toku (zapisane przed zmianą): definicja jest zapieczętowana przy starcie zadania, więc stare wykonania działają według starej definicji. Implementator ma to potwierdzić w `crates/narsor-engine/src/orchestration/` przed scaleniem (patrz niżej).
- Inne workflow (`open-mercato-sdlc.yaml`) i schemat nie zmieniają się.

## Risks & Dependencies

- **Agent pomija przegląd zbyt często.** Środek zaradczy: domyślny jest przegląd (R4). Powód jest widoczny w PR. Użytkownik może wrócić `revise` z komentarzem.
- **Agent zadaje pytanie mimo zakazu.** Skill `ce-brainstorm` pyta domyślnie. Środek zaradczy: zakaz w tym samym akapicie co wywołanie skilla. Jeśli agent zwróci `awaiting_input`, silnik nadal obsłuży to jak dziś (krok wstrzymany). Zmiana jest więc bezpieczna w najgorszym przypadku.
- **Stare `review_summary` po `fixed_minor`.** Ścieżka `check_requirements_feedback → publish_requirements` nie przechodzi przez przegląd. Komentarz z podsumowaniem może zostać dodany drugi raz. Środek zaradczy: prompt publikacji mówi „dodaj komentarz tylko wtedy, gdy nie ma w MR takiego samego".
- **Nazwa wyboru `requirements_ready`.** Ma opis (test `every_builtin_label_has_a_description` wymaga opisu każdej etykiety).

## Deferred to Implementation

- Dokładne brzmienie promptów po polsku/angielsku: prompty w pliku są po angielsku, zostają po angielsku.
- Dokładne współrzędne nowej krawędzi w SVG.
- Dokładny sposób atrapy harnessu w ignorowanym teście (U4).
- Potwierdzenie, że definicja jest zapieczętowana przy starcie wykonania (`sealed_spec` w `tests/builtin_workflows.rs` sugeruje, że tak). Jeśli nie, dodać uwagę do README.
