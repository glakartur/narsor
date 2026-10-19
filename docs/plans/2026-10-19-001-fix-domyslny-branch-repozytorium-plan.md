---
title: "fix: Domyślny branch repozytorium zamiast sztywnego `main`"
type: fix
status: active
date: 2026-10-19
origin: docs/brainstorms/2026-10-19-domyslny-branch-repozytorium-requirements.md
---

# fix: Domyślny branch repozytorium zamiast sztywnego `main`

## Summary

Plan ma trzy zmiany w kodzie i jedną w dokumentacji:

- Ścieżka tworzenia taska (`environment/resolver.rs`) dziś w ogóle nie czyta `default_branch` z rejestru. Plan dodaje ten odczyt.
- Ścieżka CLI `run` (`tasks::resolve_repository`) już czyta `default_branch`. Plan dodaje do niej informację o źródle nazwy gałęzi.
- Błąd `RefNotFound` dostaje źródło nazwy: „z workflow" albo „z konfiguracji repozytorium".
- Dołączone workflow tracą `ref: main`. Ta zmiana wchodzi dopiero po zmianach w kodzie.

---

## Problem Frame

Użytkownik ustawia gałąź repozytorium (`develop`), a zadanie pobiera `main` z dołączonego workflow. Przyczyna z dokumentu źródłowego jest prawdziwa: `ref: main` jest w danych workflow, nie w kodzie (see origin: `docs/brainstorms/2026-10-19-domyslny-branch-repozytorium-requirements.md`).

**Nowe ustalenie planu.** Dokument źródłowy sprawdził tylko `resolve_repository`. Ta funkcja służy ścieżce CLI `run`. Zadania tworzy inna ścieżka: `Resolver` w `crates/narsor-engine/src/environment/resolver.rs`. Ona łączy `intent.commit` i `ref` z workflow, ale nie używa `entry.default_branch`. Gdyby plan tylko usunął `ref: main`, zadanie sklonowałoby `HEAD` zdalnego repozytorium, a nie `develop`. R1 nie byłby spełniony. Dlatego plan zmienia też kod. To nie zmienia zakresu: R1 wymaga tej zmiany.

---

## Requirements

- R1. Gałąź z konfiguracji repozytorium jest pobrana, gdy workflow nie podaje `ref`. → U1, U2, U3
- R2. Brak gałęzi i brak `ref` daje `HEAD` zdalnego repozytorium. → U1, U2
- R3. Dołączone workflow nie mają na stałe `main`. → U3
- R4. Jawny `ref` w workflow wygrywa. Zachowanie bez zmian. → U1, U2 (testy regresji)
- R5. Błąd pobrania podaje nazwę gałęzi i źródło nazwy. → U2
- R6. README i TUI opisują pierwszeństwo jawnego `ref`. → U4

**Przykłady akceptacji:** AE1 (U1, U3), AE2 (U1, U2), AE3 (U2).

---

## Scope Boundaries

- Zasada „jawny `ref` w workflow wygrywa" zostaje.
- Brak nowego pola lub przełącznika „gałąź z rejestru zawsze wygrywa".
- Brak wykrywania gałęzi przy rejestracji repozytorium.
- Wznowione zadanie dalej używa zapisanego commita. `takeover_request` w `crates/narsor-engine/src/tasks/mod.rs` się nie zmienia.
- Fixture `tests/fixtures/implement_feature.yaml` i przykład w `WORKFLOW_SCHEMA.md` (linia 490) zostają z `ref: main`. To przykłady jawnego `ref`.

### Deferred to Follow-Up Work

- Osobny przełącznik dla użytkowników z własnym workflow z `ref: main`. Dotyczy ich tylko R6 (opis).

---

## Context & Research

- `workflows/compound-engineering.yaml:36` i `workflows/open-mercato-sdlc.yaml:46` — jedyne `ref: main` w dołączonych workflow. Pole `ref` jest opcjonalne w kodzie (`GitWorkspaceSchema.git_ref` ma `#[serde(default)]`).
- `crates/narsor-engine/src/environment/resolver.rs` (okolice linii 645–755): `workflow_ref` jest jedynym źródłem `git_ref` w `GitIdentity`. `entry.default_branch` nie jest czytane. `intent.repository` ustawia `workflow_ref = None`.
- `crates/narsor-engine/src/domain/environment.rs`: `GitIdentity.git_ref` ma `skip_serializing_if = "Option::is_none"`. Nowe pole musi mieć to samo, aby zapisane rekordy dalej się czytały.
- `crates/narsor-runner/src/runner/init.rs:131`: runner wywołuje `clone_resolving_ref` z `identity.git_ref`. Tu powstaje `RefNotFound` dla zadań.
- `crates/narsor-engine/src/workspace/provisioner.rs`: `WorkspaceError::RefNotFound { git_ref, repository, detail }` — trzy miejsca budują ten błąd (około linii 740–815 i 1086).
- `crates/narsor-engine/src/tasks/mod.rs:389` `resolve_repository` — ścieżka CLI. Testy: `tests/tasks.rs:569`.
- Brak pasujących wpisów w `docs/solutions/`.
- Odpowiedź na pytanie z dokumentu źródłowego (R3): `tests/schema_loading.rs`, `tests/takeover.rs` i `tests/persistence.rs` używają `ref: main` we własnych fixture'ach. Nie zależą od dołączonych workflow. `tests/builtin_workflows.rs:623` tworzy repozytorium z gałęzią `main`, ale to tylko dane testu. `deploy/` nie zawiera workflow.

---

## Key Technical Decisions

- **Źródło nazwy to dane, nie tekst.** Nowy typ `RefOrigin` (`Workflow`, `Repository`) idzie razem z nazwą gałęzi: `GitIdentity` (zadania) oraz `RepositorySource::Git` (CLI). `RefNotFound` dostaje pole `origin`. Powód: błąd powstaje później i gdzie indziej (runner, po sieci) niż wybór gałęzi. Tekstu nie da się zbudować w miejscu wyboru.
- **Zgodność wstecz.** Pole w `GitIdentity` jest opcjonalne, z `#[serde(default, skip_serializing_if = "Option::is_none")]`. Stare rekordy nie mają źródła. Błąd wtedy nie podaje źródła (jak dziś).
- **Kolejność wdrożenia.** U3 zależy od U1 i U2. Odwrotna kolejność da ciche pobranie `HEAD` zamiast `develop`.
- **Z `intent.repository`** workflow `ref` jest dziś pomijany. To zostaje. Wtedy źródłem jest konfiguracja repozytorium.

---

## Implementation Units

### U1. Zadania: gałąź z rejestru, gdy workflow nie podaje `ref`

**Goal:** `Resolver` używa `entry.default_branch`, gdy nie ma `ref` z workflow i nie ma commita. Zapisuje źródło nazwy.

**Requirements:** R1, R2, R4, R5. **Dependencies:** brak.

**Files:**
- `crates/narsor-engine/src/domain/environment.rs` (typ `RefOrigin`, pole w `GitIdentity`)
- `crates/narsor-engine/src/environment/resolver.rs`
- Test: `tests/environment.rs`

**Approach:**
- Po wyznaczeniu `entry` wybierz: `workflow_ref` (źródło `Workflow`), inaczej `entry.default_branch` (źródło `Repository`), inaczej `None`.
- Zachowaj dotychczasowe reguły: jawny `intent.commit` wygrywa i zeruje `git_ref`; `ref` będący pełnym commit SHA przypina commit. `default_branch` nie jest sprawdzany jako commit.
- Backend uruchamiający obrazy wymaga commita z góry. Reguła `MissingCommit` się nie zmienia.
- Ścieżka lokalnego katalogu (`git_ref: None`) bez zmian.

**Patterns to follow:** istniejący `match (&intent.commit, workflow_ref)` w `resolver.rs`.

**Test scenarios:**
- Covers AE1. Wpis rejestru z `default_branch = develop`, workflow bez `ref` → `GitIdentity.git_ref == Some("develop")`, źródło `Repository`.
- Covers AE2. Wpis bez `default_branch`, workflow bez `ref` → `git_ref == None` (klon użyje `HEAD`).
- Regresja R4. Wpis z `default_branch = develop`, workflow z `ref: release/1.0` → `git_ref == Some("release/1.0")`, źródło `Workflow`.
- `intent.repository` ustawione, workflow z `ref` → używa `default_branch`, źródło `Repository` (zachowanie dzisiejsze dla `ref`).
- `intent.commit` ustawione, wpis z `default_branch` → `git_ref == None`, commit przypięty.
- `ref` workflow jest pełnym SHA → commit przypięty, `git_ref == None`.
- Odczyt starego rekordu `GitIdentity` bez pola źródła → poprawny, źródło `None`.

**Verification:** zadanie z repozytorium z `default_branch` i bez `ref` w workflow ma w `GitIdentity` tę gałąź.

---

### U2. Błąd `RefNotFound` ze źródłem nazwy; ścieżka CLI

**Goal:** `RefNotFound` podaje źródło nazwy. Ścieżka CLI i runner przekazują źródło.

**Requirements:** R2, R4, R5. **Dependencies:** U1 (typ `RefOrigin`).

**Files:**
- `crates/narsor-engine/src/workspace/provisioner.rs` (`RepositorySource::Git`, `RepositorySpec::git`, `WorkspaceError::RefNotFound`, `clone_resolving_ref`, trzy miejsca budujące błąd)
- `crates/narsor-engine/src/tasks/mod.rs` (`resolve_repository`)
- `crates/narsor-runner/src/runner/init.rs`
- Testy: `tests/tasks.rs`, `tests/workspace.rs`, `tests/runner.rs`

**Approach:**
- `RefNotFound` dostaje pole `origin: Option<RefOrigin>`. Komunikat: `git ref 'X' (z workflow) not found in repository 'R' (...)` albo `(z konfiguracji repozytorium)`. Brak źródła — komunikat jak dziś.
- `resolve_repository` ustawia `Workflow` dla jawnego `ref`, `Repository` dla `default_branch`. Raw remote bez rejestru: źródło `Workflow`, gdy jest `ref`.
- `clone_resolving_ref` i ścieżka `materialise` przyjmują źródło i wkładają je w błąd. Runner bierze źródło z `GitIdentity`.
- Dokładne sygnatury wybiera implementer. Nie zmieniaj reguł wyboru `ref` — tylko dopisz źródło.

**Patterns to follow:** istniejące budowanie `RefNotFound` w `provisioner.rs`.

**Test scenarios:**
- Covers AE3. Workflow z `ref: release/1.0`, którego nie ma → błąd zawiera `release/1.0` i „z workflow".
- Gałąź z rejestru nie istnieje → błąd zawiera nazwę i „z konfiguracji repozytorium".
- Covers AE2. Zdalne repozytorium z domyślną gałęzią `master`, brak gałęzi i `ref` → pobiera `master` (bez `main`).
- `resolve_repository`: wpis z `default_branch` → źródło `Repository`; z jawnym `ref` → `Workflow`; istniejący test `tests/tasks.rs:569` dalej przechodzi.
- Runner (`tests/runner.rs`): `GitIdentity` z brakującą gałęzią i źródłem → błąd z tym źródłem; bez źródła → komunikat jak dziś.
- Istniejące testy `RefNotFound` (`tests/workspace.rs:542`, `:845`) dostosowane do nowego pola.

**Verification:** każdy z trzech punktów budujących `RefNotFound` przenosi źródło; test dla każdego z dwóch źródeł przechodzi.

---

### U3. Dołączone workflow bez `ref: main`

**Goal:** Usunąć `ref: main` z dwóch dołączonych workflow.

**Requirements:** R1, R3. **Dependencies:** U1, U2.

**Files:**
- `workflows/compound-engineering.yaml`
- `workflows/open-mercato-sdlc.yaml`
- Test: `tests/builtin_workflows.rs`

**Approach:**
- Usuń linię `ref: main` z `globals.workspace`. Sprawdź, czy diagramy (`*.html`, `*.mmd`) nie wspominają `main` jako gałęzi pobrania (wstępne wyszukiwanie: nie wspominają).

**Test scenarios:**
- Każde dołączone workflow ma `git_ref == None` w `globals.workspace` (R3).
- Covers AE1. Integracyjnie: repozytorium z samą gałęzią `develop` i wpisem rejestru z `default_branch = develop`; start `compound-engineering` → workspace na `develop`, brak błędu. Bez wpisu `default_branch` → `HEAD`.
- Istniejące testy w `tests/builtin_workflows.rs` (walidacja, rozwiązywanie nazw) dalej przechodzą.

**Verification:** dołączone workflow ładują się bez naruszeń; test AE1 przechodzi.

---

### U4. Dokumentacja i opis pola

**Goal:** Opisy mówią o pierwszeństwie jawnego `ref`.

**Requirements:** R6. **Dependencies:** U1–U3.

**Files:**
- `README.md` (sekcja „Resolution precedence": jawny `ref` wygrywa; dołączone workflow go nie podają)
- `crates/narsor-engine/src/registry/mod.rs` (komentarz modułu)
- `src/tui/task_app/repositories.rs` (opis pola `gałąź`, linia ~183)
- `src/cli/mod.rs` (pomoc `--default-branch`, linia ~466)
- `WORKFLOW_SCHEMA.md` (linia 114: „wymaga `repository` i `ref`" jest nieprawdziwe — `ref` jest opcjonalne)
- `workflows/README.md` (jedno zdanie o gałęzi)
- Test: `tests/` dla tekstów CLI/TUI, jeśli istnieją asercje na te opisy (sprawdź `tests/cli_smoke.rs`)

**Approach:** Jedno zdanie w każdym miejscu, np. „gałąź, gdy workflow nie podaje `ref`; jawny `ref` w workflow ma pierwszeństwo". Styl krótkich zdań, jak w repozytorium.

**Test expectation:** none — zmiana tekstu. Popraw tylko asercje, które porównują zmieniony tekst.

**Verification:** żaden opis nie sugeruje, że gałąź z rejestru wygrywa nad `ref` workflow.

---

## Risks

- **Ukryta zależność od `main`.** Użytkownik bez `default_branch` w rejestrze dostaje po zmianie `HEAD` zamiast `main`. To zamierzone (R2), ale zmienia zachowanie dla repozytoriów, w których `HEAD` ≠ `main`. Wspomnij w opisie PR.
- **Zgodność zapisanych rekordów.** Nowe pole w `GitIdentity` musi się czytać ze starych rekordów (test w U1).
- **Własne workflow z `ref: main`** nadal pobierają `main` (R4). Zasięg tylko R6.

## Deferred to Implementation

- Dokładne sygnatury i nazwa pola w `RepositorySource::Git`, `RefNotFound`, `clone_resolving_ref`.
- Pusta lub złożona z samych spacji wartość `default_branch` (TUI mapuje puste pole przez `value(BRANCH)`): sprawdź, czy trafia do rejestru jako `None`. Jeśli nie, traktuj ją jako brak gałęzi w U1.
- Zgodność manifestu kontrolera (`crates/narsor-controller/src/kubernetes/manifest.rs`) z nowym polem: sprawdź, czy serializuje `GitIdentity` w sposób, który wymaga zmiany.

---

## Sources

- Origin: `docs/brainstorms/2026-10-19-domyslny-branch-repozytorium-requirements.md`
