---
title: Branch repozytorium wybierany przy tworzeniu zadania - Plan
type: feat
date: 2026-11-05
topic: branch-repozytorium-w-zadaniu
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Branch repozytorium wybierany przy tworzeniu zadania - Plan

## Goal Capsule

- **Objective:** Użytkownik tworzący zadanie wybiera repozytorium razem z branchem, z którego zadanie startuje, w TUI i w CLI, a serwer podpowiada dostępne branche.
- **Product authority:** Decyzje użytkownika z dialogu brainstormu; ten plik jest ich zapisem. Rozwija odroczony punkt z `docs/plans/2026-11-05-0033-feat-workflow-multirepo-plan.md` ("Zmiana brancha repozytorium przy tworzeniu zadania").
- **Means:** Mapa `input → branch` w `LaunchRequest` i `LaunchIntent`, nowe źródło ref `User` w resolverze, serwis `ls-remote` za trait seam (KTD1, KTD2, KTD3).
- **Authority:** Product Contract wygrywa w sprawach zachowania. KTD wygrywają w sprawach mechanizmu. Jednostki nie zmieniają ani jednego, ani drugiego.
- **Execution profile:** Jeden PR. Jednostki U1 do U5 w kolejności zależności. Kod kończy `ce-work`.
- **Stop conditions:** Zatrzymaj się, gdy `ls-remote` na serwerze wymaga poświadczeń, których serwer nie ma, a R9 nie da się spełnić komunikatem z przyczyną. Zatrzymaj się, gdy zmiana `LaunchRequest` łamie `request_digest` istniejących zadań.
- **Open blockers:** Brak.

## Product Contract

### Summary

Każde repozytorium wybrane przy tworzeniu zadania to para repozytorium + branch. Formularz TUI podpowiada branche z listy z filtrem i zawsze pozwala wpisać branch z palca. CLI przyjmuje branch jako parametr. Serwer ma serwis zwracający listę branchy repozytorium z rejestru. Wybrane repozytorium staje się repozytorium w workspace workflow, a wybrany branch jego `base_branch`.

### Problem Frame

Branch, z którego startuje zadanie, wyznacza dziś workflow (`base_branch` lub `ref`) albo `default_branch` repozytorium z rejestru. Użytkownik nie może go wybrać przy tworzeniu zadania, choć stan zadania zapamiętuje już dla każdego repozytorium nazwę i branch (R15 planu multirepo). Praca na branchu innym niż domyślny wymaga dziś edycji workflow.

### Requirements

**Wybór pary repozytorium + branch**

- R1. Każde repozytorium wybierane przy tworzeniu zadania jest parą repozytorium + branch.
- R2. Wybrane repozytorium staje się repozytorium w workspace workflow, a wybrany branch jego `base_branch` (R6 planu multirepo).
- R3. Wybór użytkownika wygrywa z `base_branch` i `ref` podanymi w workflow.
- R4. Wybór dotyczy workspace'ów powiązanych z inputem typu `repository`. Workspace z repozytorium na stałe w workflow zachowuje swój `base_branch`.
- R5. Workflow z kilkoma inputami `repository` daje osobną parę repozytorium + branch dla każdego z nich.
- R6. Gdy użytkownik nie poda brancha, zadanie startuje z `default_branch` repozytorium z rejestru. Gdy go nie ma, startuje z domyślnego brancha remote (`HEAD`).

**Serwis listy branchy**

- R7. Serwer udostępnia serwis zwracający listę branchy dla repozytorium z rejestru.
- R8. Lista zawiera tylko branche, bez tagów i commitów.
- R9. Gdy serwer nie może zwrócić listy (brak sieci, brak uprawnień do remote), klient dostaje błąd z przyczyną, a nie pustą listę.

**Kontrolka w TUI**

- R10. Pole brancha w formularzu nowego zadania jest preselekcjonowane na branch z R6.
- R11. Pole podpowiada listę z serwisu (R7) i zawęża ją według wpisanego tekstu.
- R12. Użytkownik może wpisać dowolny branch z palca, także gdy lista jest dostępna. Wpisany branch nie jest sprawdzany względem listy.
- R13. Gdy lista jest niedostępna (R9), pole działa jako zwykły tekst, a formularz pokazuje komunikat z przyczyną.

**CLI**

- R14. CLI tworzące zadanie przyjmuje branch dla repozytorium, także osobno dla każdego inputu `repository` (R5). Bez podanego brancha obowiązuje R6.

**Błędy**

- R15. Branch, który nie istnieje w remote, wychodzi jako błąd przy starcie zadania i podaje nazwę brancha.

### Key Flows

- F1. Zadanie na innym branchu w TUI
  - **Trigger:** Użytkownik otwiera formularz nowego zadania dla workflow z inputem `repository`.
  - **Steps:** Wybiera repozytorium. Pole brancha pokazuje `default_branch` i podpowiada listę. Użytkownik filtruje i wybiera branch albo wpisuje własny. Zadanie startuje z tym branchem.
  - **Covered by:** R1, R2, R6, R10, R11, R12
- F2. Lista niedostępna
  - **Trigger:** Serwer nie może odczytać branchy remote.
  - **Steps:** Formularz pokazuje komunikat z przyczyną, a pole brancha działa jako tekst. Użytkownik wpisuje branch albo zostawia domyślny.
  - **Covered by:** R9, R12, R13
- F3. Zadanie z CLI
  - **Trigger:** Użytkownik tworzy zadanie z CLI i podaje branch.
  - **Steps:** CLI przekazuje parę repozytorium + branch. Bez brancha zadanie startuje jak w R6.
  - **Covered by:** R1, R6, R14

### Acceptance Examples

- AE1. **Covers R2, R3.** Workflow ma w workspace `code` powiązanym z inputem `repository` `base_branch: main`. Użytkownik wybiera `develop`. Zadanie startuje z `develop`, a `develop` jest `base_branch` workspace'u `code`.
- AE2. **Covers R4.** Workflow ma workspace `docs` z repozytorium na stałe i `base_branch: main`. Użytkownik nie może zmienić brancha `docs`, a zadanie używa `main`.
- AE3. **Covers R6.** Repozytorium nie ma `default_branch`, a domyślny branch remote to `master`. Użytkownik nie wybiera brancha. Zadanie startuje z `master`.
- AE4. **Covers R12, R13.** Serwis listy zwraca błąd. Użytkownik wpisuje `feature/x` i zadanie startuje z tym branchem.
- AE5. **Covers R12, R15.** Lista jest dostępna, a użytkownik wpisuje `feature/nowy`, którego lista nie zawiera. Formularz go przyjmuje. Gdy branch nie istnieje w remote, start zadania kończy się błędem z nazwą `feature/nowy`.

### Key Decisions

- **Wybór użytkownika wygrywa z workflow.** Workflow podaje tylko wartość domyślną dla workspace'ów z inputem `repository`. (session-settled: user-directed — chosen over "jawny `ref` z workflow wygrywa": użytkownik wybiera repo i branch, bo to element wskazany w `globals.workspaces`.) Governs R2, R3, R4.
- **Lista branchy jest podpowiedzią, nie ograniczeniem.** Wpisanie z palca jest zawsze dozwolone, także gdy lista działa. (session-settled: user-directed — chosen over "tekst tylko awaryjnie": CLI i tak przyjmuje branch z palca, a świeżo utworzonego brancha lista może nie mieć.) Governs R11, R12, R13.
- **CLI przyjmuje branch w pierwszej wersji.** (session-settled: user-directed.) Governs R14.

### Scope Boundaries

**Deferred for later**

- Brak brancha źródłowego, na przykład branch, który ma powstać dopiero w zadaniu. Na start nieistniejący branch to błąd (R15).
- Zmiana brancha po utworzeniu zadania.
- Tagi i commity jako `ref` w wyborze użytkownika.
- Zmiany w wznawianiu zadań: wznowione zadanie nadal używa zapisanego commita.

### Dependencies / Assumptions

- Rejestr repozytoriów ma pole `default_branch` (`crates/narsor-server/src/api/routes/control.rs:434`), a formularz TUI pokazuje je dla repozytorium (`src/tui/task_app/repositories.rs:144`).
- Stan zadania ma już miejsce na branch każdego repozytorium: `TASK_LAUNCH_SPEC_VERSION` 4 (`crates/narsor-protocol/src/launch.rs:19`) i pole `branch` w `crates/narsor-engine/src/domain/workspace.rs:87`.
- Formularz nowego zadania ma pola `repository` z listą identyfikatorów (`src/tui/task_app/new_task/mod.rs:259`), a CLI tworzy zadania przez `CreateTaskRequest` (`src/cli/remote.rs:908`). Obecne `LaunchRequest` niesie jedno `workspace` (`crates/narsor-protocol/src/controller.rs:1503`), więc kształt pary w API rozstrzyga planowanie.
- Zakładam, że schemat v2 z planu multirepo jest już wdrożony (commit `5d5305a`). Jeśli nie, R2 i R4 wymagają go jako warunku wstępnego.

---

## Planning Contract

Product Contract unchanged.

### Key Technical Decisions

- KTD1. **Mapa `input → branch` w żądaniu.** `LaunchRequest` dostaje pole `branches` (nazwa inputu `repository` na nazwę brancha), a `LaunchIntent` to samo pole. Pole ma `serde(default, skip_serializing_if = empty)`, więc `request_digest` istniejących żądań się nie zmienia. Kolejność wdrożenia: serwer, kontrolery i runnery przed klientami. `LaunchRequest` ma `deny_unknown_fields`, a starszy runner nie odczyta `ref_origin: user` (KTD2). Wybrano to zamiast rozszerzenia `workspace`: `workspace.repository` dotyczy tylko głównego repozytorium i nie opisuje kilku inputów (R5). Realizuje R1, R2, R5.
- KTD2. **Branch użytkownika to ref o pochodzeniu `User`.** `RefOrigin` dostaje wariant `User`. Resolver stosuje go tylko dla workspace'u, którego `source.repository` to dokładnie `{{ inputs.X }}`, i tylko gdy `branches` zawiera `X`. Taki ref wygrywa z `base_branch` workflow i z `default_branch` rejestru. Pusty branch oznacza brak wyboru (R6). Branch w formie SHA zostaje refem, nie commitem (commity są poza zakresem). `RefOrigin` jest zamkniętym enumem w specyfikacji zapisanej z zadaniem, więc starszy czytnik odrzuci `user`. Wersja specyfikacji się nie zmienia, a zgodność daje kolejność wdrożenia z KTD1. Wzorzec: jedna reguła `named_ref` dla obu ścieżek (`docs/solutions/logic-errors/workspace-ref-hardcoded-main-ignored-default-branch-2026-10-19.md`). Realizuje R2, R3, R4, R6, R15.
- KTD3. **Lista branchy z serwera przez `git ls-remote --heads` za trait seam.** Wąski trait `BranchLister` ma implementację z `Command::new("git")` i podwójne w testach (`docs/solutions/conventions/test-network-features-through-trait-seams-not-tcp-servers-2026-10-31.md`). Wywołanie zawsze czyści środowisko jak `docs/solutions/security-issues/untrusted-control-plane-reaches-user-git-and-harness-logins-2026-10-25.md` i zawsze (bez rozróżnienia zaufania) stosuje regułę https-only i bez adresów wewnętrznych z prywatnego `check_remote` w `crates/narsor-controller/src/local/untrusted.rs` (z `is_internal`). Silnikowy `check_remote` w `provisioner.rs` odrzuca tylko puste remote, `-` i `ext::`, więc nie wystarcza. Reguła przenosi się tam, skąd silnik może ją wywołać. Git działa z `GIT_TERMINAL_PROMPT=0`, z `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, `http.followRedirects=false`, `GIT_ALLOW_PROTOCOL=https`, bez `SSH_AUTH_SOCK`, `*_ASKPASS` i `GIT_SSH*`, z `--` przed adresem i z ograniczonym czasem (jak `restrict_git_access` w `provisioner.rs`). Przekroczenie czasu daje błąd R9 z nazwą limitu. Tekst błędu zawiera tylko zredagowaną przyczynę, bez URL z poświadczeniami. Libgit2 odpada (`docs/dev/building-and-testing.md`). Serwer nie ma poświadczeń backendu, więc dla prywatnego remote zwraca błąd z przyczyną (R9), a nie pustą listę. Realizuje R7, R8, R9.
- KTD4. **Trasa `GET /control/v1/repositories/{id}/branches`.** Odpowiedź ma jedno pole z listą nazw. Nazwa brancha nie trafia do ścieżki (`feature/x`). Trasa leży za bramką właściciela jak reszta `control::routes()`. Błędy idą przez `ProtocolError` z tekstem oczyszczonym przez `without_unsafe_chars`. Realizuje R7, R9.
- KTD5. **Pole brancha w TUI to pole tekstowe z podpowiedziami.** Istniejący `Select` odrzuca wartości spoza listy, więc nie spełnia R12. Nowy rodzaj pola trzyma wpisany tekst i listę podpowiedzi zawężaną do wpisanego tekstu. Lista przychodzi z klucza zadania w tle, uruchamianego przy zmianie repozytorium (`docs/solutions/design-patterns/tui-reads-as-keyed-background-jobs-with-loading-marker-2026-10-22.md`). Pole ma wypełnienie aktywnego pola jak inne (`docs/solutions/ui-bugs/tui-active-field-fill-only-under-text-2026-10-24.md`). Realizuje R10, R11, R12, R13.
- KTD6. **Backend uruchamiający obrazy nie zmienia reguł pinowania.** Dla głównego repozytorium z branchem bez commita resolver dziś zwraca `LaunchError::MissingCommit`, także dla `default_branch`. Branch użytkownika podlega tej samej regule. Rozwiązanie brancha na commit przy tworzeniu wymagałoby sieci, której tworzenie unika (komentarz w `crates/narsor-engine/src/environment/resolver.rs` przy `named_ref`). Konsekwencja trafia do dokumentacji (U5).

### Assumptions

- Schemat v2 z planu multirepo jest wdrożony (commit `5d5305a`). `TASK_LAUNCH_SPEC_VERSION` to 4, a `TaskRepository.branch` jest już zapisywany, więc wersja specyfikacji się nie zmienia.
- Workflow z plikiem lokalnym przez `POST /tasks` (`StartTaskRequest`) nie ma dziś ani repozytorium, ani brancha. Zostaje poza zakresem (patrz Scope Boundaries).
- Wpisany branch, którego nie ma w remote, wychodzi jako `WorkspaceError::RefNotFound` przy przyjęciu zadania przez runner. Komunikat już zawiera nazwę brancha (R15). Zmienia się tylko opis pochodzenia dla `User`.

### Scope Boundaries (planning)

**Considered and not built**

- Polecenie CLI `repo branches <id>`. R14 wymaga tylko parametru przy tworzeniu zadania, a serwis ma klienta w `narsor-api-client`. Dodaj, gdy użytkownicy poproszą o listę z CLI.
- Rozwiązywanie brancha na commit przy tworzeniu na backendach obrazów. Wymaga sieci przy tworzeniu (KTD6). Dodaj, gdy brak tej funkcji zablokuje realną pracę na takim backendzie.
- Pamięć podręczna listy branchy po stronie serwera. Lista jest podpowiedzią (R12), a odczyt jest rzadki. Dodaj, gdy `ls-remote` okaże się wolny w praktyce.

---

## Implementation Units

### U1. Mapa branchy w protokole i resolver

- **Goal:** Żądanie niesie branch dla każdego inputu `repository`, a resolver stosuje go jako ref `User`.
- **Requirements:** R1, R2, R3, R4, R5, R6, R15. AE1, AE2, AE3.
- **Dependencies:** brak.
- **Files:**
  - `crates/narsor-protocol/src/controller.rs` (`LaunchRequest`)
  - `crates/narsor-engine/src/controller_protocol/service.rs` (mapowanie do `LaunchIntent`)
  - `crates/narsor-engine/src/environment/resolver.rs` (`LaunchIntent`, `workspace()`, `git_repository`)
  - `crates/narsor-engine/src/registry/store.rs` (`named_ref`)
  - `crates/narsor-engine/src/domain/environment.rs` (`RefOrigin`, opis pochodzenia)
  - `crates/narsor-engine/src/workspace/provisioner.rs` (`origin_note`)
  - `tests/integration/environment.rs`, `tests/integration/launch_spec_contract.rs`
- **Approach:**
  - Dodaj pole `branches` zgodnie z KTD1. Przenieś je przez `ProtocolService::create_task` do `LaunchIntent`.
  - W pętli `workspace()` wykryj powiązanie z inputem przez istniejące `extract_input_references` z `validator.rs`. Workspace z repozytorium na stałe lub ze źródłem `path` zostaje bez zmian (R4).
  - Waliduj każdą wartość `branches` przy budowie `LaunchIntent`: pusta po przycięciu to brak wyboru, początek `-`, znaki sterujące lub nadmierna długość to błąd z nazwą inputu. Użyj reguł `check_ref` wystawionych z silnika. Tekst błędów przechodzi przez `without_unsafe_chars`.
  - Gdy klucz w `branches` nie odpowiada żadnemu workspace'owi powiązanemu z inputem, zwróć błąd uruchomienia z nazwą klucza.
  - Rozszerz `named_ref` o ref użytkownika według KTD2. Dodaj opis `User` w `origin_note`.
- **Patterns to follow:** `RefOrigin::Workflow` i `Repository` w `crates/narsor-engine/src/domain/environment.rs`. Test `named_ref` z `docs/solutions/logic-errors/workspace-ref-hardcoded-main-ignored-default-branch-2026-10-19.md`.
- **Test scenarios:**
  - Covers AE1. Workflow z `base_branch: main` na workspace `code` powiązanym z inputem. `branches` ma `develop`. Wynik: `git_ref` to `develop`, `branch` to `develop`, pochodzenie `User`.
  - Covers AE2. Workspace `docs` z repozytorium na stałe i `base_branch: main`. Żądanie ma `branches` dla innego inputu. Wynik: `docs` używa `main`.
  - Covers AE3. Rejestr bez `default_branch`, `branches` puste. Wynik: `git_ref` brak, klon bierze `HEAD`.
  - Dwa inputy `repository`, `branches` ma wpisy dla obu. Wynik: każdy workspace dostaje swój branch (R5).
  - `branches` ma pusty tekst dla inputu. Wynik: jak brak wyboru, `default_branch` rejestru.
  - `branches` ma klucz, którego nie ma w workflow. Wynik: błąd z nazwą klucza.
  - Wartość zaczyna się od `-`, ma znak sterujący lub jest za długa. Wynik: błąd przy tworzeniu, nie przy starcie runnera.
  - Żądanie bez `branches` serializuje się bez tego pola i ma ten sam `request_digest` co przed zmianą.
  - Starszy serwer: żądanie z `branches` jest odrzucane przez `deny_unknown_fields` (test kontraktu).
  - Specyfikacja z `ref_origin: user` serializuje się jako `user` i odczytuje z powrotem. Test dokumentuje, że starszy czytnik jej nie odczyta, a kolejność wdrożenia z KTD1 to wymaga.
  - Backend obrazów, branch użytkownika, brak commita. Wynik: `MissingCommit` (KTD6).
- **Verification:** Resolver zwraca poprawną parę `git_ref` + `branch` dla AE1 do AE3, a testy kontraktu przechodzą bez zmiany istniejących digestów.

### U2. Serwis listy branchy

- **Goal:** Serwer zwraca listę branchy repozytorium z rejestru albo błąd z przyczyną.
- **Requirements:** R7, R8, R9.
- **Dependencies:** brak (równoległa do U1).
- **Files:**
  - `crates/narsor-protocol/src/controller.rs` (DTO odpowiedzi, reeksporty ok. `:2122-2131`)
  - `crates/narsor-engine/src/registry/` (trait `BranchLister` i jego implementacja `git ls-remote`)
  - `crates/narsor-server/src/api/routes/control.rs` (trasa, handler)
  - `crates/narsor-api-client/src/api/client.rs` (metoda obok `control_repositories`)
  - `tests/integration/registry.rs` (nowy plik testów w razie potrzeby deklaruj w `tests/integration/main.rs`)
- **Approach:**
  - Zdefiniuj trait i implementację według KTD3. Filtruj tylko `refs/heads/`, odrzuć tagi i commity (R8).
  - Dodaj trasę według KTD4. Nieznane `id` daje 404 przez `registry_refusal`.
  - Metoda klienta przekazuje `id` w ścieżce i zwraca listę nazw.
- **Patterns to follow:** `repository_view` i `registry_refusal` w `crates/narsor-server/src/api/routes/control.rs`. Seam z commita `b42970e` (`KubeTransport`).
- **Execution note:** Zacznij od testów traitu na lokalnym bare repozytorium z kilkoma branchami i tagiem.
- **Test scenarios:**
  - Bare repozytorium z branchami `main`, `develop` i tagiem `v1`. Wynik: lista ma `main` i `develop`, bez `v1`.
  - Branch z ukośnikiem `feature/x`. Wynik: nazwa zwrócona w całości.
  - Remote nieosiągalny (ścieżka nie istnieje). Wynik: błąd z przyczyną, nie pusta lista (R9).
  - Remote bez uprawnień, wynik `ls-remote` z błędem. Wynik: błąd z przyczyną oczyszczoną z kodów sterujących.
  - Remote nie-https lub wewnętrzny, na każdym serwerze. Wynik: odmowa przez regułę przeniesioną z `untrusted.rs`, a `ls-remote` się nie uruchamia.
  - Uruchomiona komenda niesie ustawienia z KTD3 i nie dziedziczy konfiguracji użytkownika (wzorzec: test `a_pinned_only_git_uses_none_of_the_users_own_setup`).
  - Double traitu przekracza limit czasu. Wynik: błąd z przyczyną i nazwą limitu (R9).
  - Remote z poświadczeniami w URL i błędem `ls-remote`. Wynik: tekst błędu bez poświadczeń.
  - Nieznane `id`. Wynik: 404.
  - Klient: odpowiedź serwera mapuje się na listę, a błąd HTTP na błąd klienta.
- **Verification:** Trasa zwraca listę lub błąd z przyczyną, a wywołania `git` nie dziedziczą środowiska użytkownika.

### U3. Parametr `--branch` w CLI

- **Goal:** `task create` przyjmuje branch dla każdego inputu `repository`.
- **Requirements:** R1, R5, R6, R14. F3.
- **Dependencies:** U1.
- **Files:**
  - `src/cli/remote.rs` (`TaskCommand::Create`, budowa `LaunchRequest` ok. `:896-933`, `conflicts_with_all` ok. `:229`)
  - `tests/integration/cli_task_text.rs`
- **Approach:**
  - Dodaj powtarzalny argument `--branch INPUT=BRANCH`. Parsuj go jak `--input` przez `split_pair`, ale wartość zostaje tekstem.
  - Dodaj go do `conflicts_with_all` dla `--file`, bo workflow z pliku nie niesie brancha.
  - Przekaż mapę do `LaunchRequest.branches`. Opis w pomocy clap po polsku, w stylu sąsiednich argumentów.
- **Patterns to follow:** `--input NAME=VALUE` i `parse_pairs`.
- **Test scenarios:**
  - Covers F3. `--input code=acme --branch code=develop`. Wynik: żądanie ma `branches` z `code: develop`.
  - Dwa `--branch` dla dwóch inputów. Wynik: obie pary w mapie (R5).
  - Brak `--branch`. Wynik: pole `branches` nieobecne w żądaniu (R6).
  - `--branch develop` bez `INPUT=`. Wynik: błąd składni z podpowiedzią formatu.
  - `--branch` razem z `--file`. Wynik: błąd konfliktu argumentów.
- **Verification:** Żądanie budowane przez CLI niesie mapę zgodnie z argumentami, a pomoc `task create --help` opisuje nowy argument.

### U4. Pole brancha w formularzu TUI

- **Goal:** Formularz nowego zadania pokazuje dla każdego inputu `repository` pole brancha z podpowiedziami i wpisem z palca.
- **Requirements:** R1, R5, R6, R10, R11, R12, R13. F1, F2. AE4, AE5.
- **Dependencies:** U1, U2.
- **Files:**
  - `src/tui/chooser.rs` (nowy rodzaj pola, obsługa klawiszy, walidacja)
  - `src/tui/form/field.rs` (rysowanie z wypełnieniem aktywnego pola)
  - `src/tui/input_form.rs` (`workflow_input_fields`)
  - `src/tui/task_app/new_task/mod.rs` (`NewTaskForm`, `submit_new_task`, `send_task`)
  - `src/tui/task_app/background.rs` (nowy klucz zadania w tle)
  - `src/tui/task_app/repositories.rs` (przekazanie `default_branch`)
  - `tests/integration/tui_state.rs`, `tests/integration/tui_render.rs`, `tests/integration/task_app_support/mod.rs`
- **Approach:**
  - Formularz dostaje `default_branch` każdego repozytorium razem z identyfikatorem. Pole brancha stoi zaraz za polem `repository` i startuje od tej wartości (R10).
  - Zmiana repozytorium uruchamia odczyt listy w tle (KTD5). Wynik wypełnia podpowiedzi i zachowuje wpisany tekst (`refreshed` i `carry_over`).
  - Błąd odczytu zostawia pole jako zwykły tekst i pokazuje ostrzeżenie z przyczyną przez `notify`, jak istniejący fallback pola `repository` (R13).
  - Wpisany branch nie jest sprawdzany względem listy (R12). Do `branches[input]` trafia tylko wartość niepusta i różna od `default_branch` preselekcjonowanego w polu. Nietknięty formularz nie wysyła nic, więc `base_branch` workflow działa jak dziś (R3, KTD2). Formularz nie zna wartości `base_branch` workflow, bo to szablon.
  - Ścieżka `Path` (workflow z pliku) nie ma pola brancha.
- **Patterns to follow:** `reload_repositories` (`JobKey::Reload`), `ChooserField::with_value`, `set_field_error`, wklejanie z `docs/solutions/design-patterns/tui-bracketed-paste-into-text-fields-2026-10-18.md`.
- **Test scenarios:**
  - Covers F1. Repozytorium z `default_branch: main`. Wynik: pole brancha ma `main` zaraz po wyborze repozytorium (R10).
  - Lista `main`, `develop`, `feature/x`. Użytkownik wpisuje `dev`. Wynik: podpowiedzi zawężone do `develop` (R11).
  - Użytkownik wybiera `develop` z podpowiedzi. Wynik: wysłane żądanie ma `branches` z `develop`.
  - Covers AE4. Odczyt listy kończy się błędem, użytkownik wpisuje `feature/x`. Wynik: pokazane ostrzeżenie z przyczyną, a żądanie ma `feature/x`.
  - Covers AE5. Lista bez `feature/nowy`, użytkownik go wpisuje. Wynik: formularz go przyjmuje i wysyła.
  - Zmiana repozytorium po wpisaniu brancha. Wynik: pole dostaje `default_branch` nowego repozytorium, a podpowiedzi są przeładowane.
  - Workflow z dwoma inputami `repository`. Wynik: dwa pola brancha, każde wysyła swój wpis (R5).
  - Rejestr bez `default_branch`, brak wpisu. Wynik: `branches` bez klucza (R6).
  - Nietknięty formularz z preselekcją `main`. Wynik: `branches` bez klucza, więc `base_branch` workflow zostaje.
  - Użytkownik wpisuje `main` po zmianie na `develop`, a preselekcja to `main`. Wynik: `branches` bez klucza (ten sam efekt co brak wyboru).
  - Render: aktywne pole brancha ma wypełnienie na całej szerokości (`mod active_field_fill`).
  - Istniejące testy z indeksami pól (`tui_state.rs` ok. `:9618-9645`) mają zaktualizowane indeksy.
- **Verification:** Formularz pozwala wybrać lub wpisać branch w obu stanach listy, a żądanie niesie wybór zgodnie z AE4 i AE5.

### U5. Dokumentacja

- **Goal:** Użytkownik i deweloper znajdują nowe zachowanie w dokumentach.
- **Requirements:** R2, R3, R4, R14. KTD6.
- **Dependencies:** U1, U3, U4.
- **Files:**
  - `docs/dev/workflow-schema.md` (workspace powiązany z inputem bierze branch użytkownika)
  - `docs/user/cli.md` i `docs/user/remote.md` (`--branch`)
  - `docs/user/tui.md` (nowe pole formularza)
  - `docs/dev/architecture.md` (nowa decyzja w tabeli D-numerowanej)
  - `tests/integration/docs_links.rs` (istniejący test linków, bez zmian)
- **Approach:** Opisz KTD6 jako ograniczenie backendów obrazów. Każdy nowy nagłówek i link musi przejść test linków.
- **Test expectation:** none -- dokumentacja. Pokrywa ją istniejący `docs_links`.
- **Verification:** `docs_links` przechodzi, a opisy zgadzają się z zachowaniem z U1 do U4.

---

## Verification Contract

| Gate | Komenda | Jednostki |
|---|---|---|
| Testy workspace | `cargo test --workspace` | U1 do U5 |
| Testy blokujące w CI | `cargo test --locked --workspace` | U1 do U5 |
| Kompilacja binariów | `cargo build --workspace --bins` | U1 do U4 |
| Format i lint | `cargo fmt --check`, `cargo clippy --workspace` | U1 do U4 |

Testy integracyjne to jeden crate (`tests/integration/main.rs`). Nowy plik deklaruj tam przez `mod`. Testy nie używają `set_var`, stałych portów ani procesów zewnętrznych poza lokalnym bare repozytorium w katalogu tymczasowym (`docs/solutions/conventions/single-integration-test-crate-and-lean-target-2026-11-01.md`).

## Definition of Done

- Każde R1 do R15 ma test albo zapis w dokumentacji, a AE1 do AE5 mają testy z `Covers`.
- Żądanie bez `branches` ma niezmieniony `request_digest`.
- Wywołania `git ls-remote` czyszczą środowisko i nie używają libgit2.
- Pola TUI mają wypełnienie aktywnego pola i test renderu.
- Dokumentacja z U5 jest zgodna z zachowaniem, a `docs_links` przechodzi.
- Kod z nieudanych prób jest usunięty z diffu.

## Risks & Dependencies

- Backend obrazów wymaga commita dla głównego repozytorium, więc branch użytkownika działa tam tylko z commitem (KTD6). Mitygacja: opis w dokumentacji i test `MissingCommit`.
- Serwer bez poświadczeń backendu nie wylistuje prywatnego remote (KTD3). Mitygacja: R9, pole działa jako tekst (R13).
- Starsze komponenty: pole `branches` łamie `deny_unknown_fields` na starym serwerze, a `ref_origin: user` łamie odczyt specyfikacji na starym runnerze lub kontrolerze. Mitygacja: wdrożenie serwera, kontrolerów i runnerów przed klientami (KTD1, KTD2).
- Na backendzie obrazów formularz oferuje branch, a start kończy się `MissingCommit` (KTD6). Użytkownik widzi to po wysłaniu. Akceptowane w pierwszej wersji, opisane w dokumentacji.
- Wstawienie pola do formularza przesuwa indeksy w istniejących testach TUI (U4).
