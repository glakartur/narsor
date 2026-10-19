---
date: 2026-10-19
topic: domyslny-branch-repozytorium
---

# Domyślny branch repozytorium zamiast sztywnego `main`

## Summary

Zadanie ma pobrać branch, który użytkownik ustawił przy konfiguracji repozytorium (np. `develop`). Gdy repozytorium nie ma `main`, pobranie nie może się nie udać z powodu `main`, którego nikt nie wybrał.

---

## Problem Frame

Użytkownik rejestruje repozytorium i podaje gałąź (`--default-branch develop` albo pole „gałąź" w TUI). Repozytorium nie ma gałęzi `main`. Zadanie mimo to próbuje pobrać `main`. Pobranie kończy się błędem `RefNotFound` i cały proces się zatrzymuje.

Przyczyna (zweryfikowana w kodzie):

- Dołączone workflow `workflows/compound-engineering.yaml` (linia 36) i `workflows/open-mercato-sdlc.yaml` (linia 46) mają na stałe `ref: main` w `globals.workspace`.
- `resolve_repository` w `crates/narsor-engine/src/tasks/mod.rs` używa `default_branch` z rejestru tylko wtedy, gdy workflow nie podaje `ref`. Jawny `ref` z workflow zawsze wygrywa.
- Dlatego `develop` z konfiguracji repozytorium nigdy nie jest użyty. Kod nie ma twardego `main`. Źródłem jest dane workflow.
- Dokumentacja `README.md` i opis pola w TUI (`src/tui/task_app/repositories.rs`) mówią: „gałąź, gdy workflow nie podaje `ref`". Zachowanie jest więc zgodne z opisem. Użytkownik jednak tego nie wie, bo dołączone workflow podają `ref` bez jego wiedzy.

Bez podanej gałęzi i bez `ref` klon używa `HEAD` zdalnego repozytorium. To działa dla dowolnej nazwy domyślnej gałęzi.

---

## Requirements

**Wybór gałęzi**

- R1. Gdy użytkownik ustawił gałąź przy konfiguracji repozytorium, zadanie pobiera tę gałąź, także wtedy, gdy dołączone workflow jest uruchomione bez zmian.
- R2. Gdy repozytorium nie ma ustawionej gałęzi, zadanie pobiera domyślną gałąź zdalnego repozytorium (`HEAD`), a nie `main`.
- R3. Dołączone workflow nie zawierają na stałe nazwy gałęzi `main`.
- R4. Workflow nadal może podać własny `ref` (gałąź, tag lub commit) dla przypadków, gdy autor workflow chce konkretnej wersji. Taki `ref` wygrywa nad gałęzią z rejestru. To zachowanie się nie zmienia.

**Komunikaty**

- R5. Gdy pobranie gałęzi się nie udaje, błąd podaje nazwę gałęzi i źródło tej nazwy: „z workflow" albo „z konfiguracji repozytorium".
- R6. Opis pola gałęzi (README, TUI) wyjaśnia, że jawny `ref` w workflow ma pierwszeństwo.

## Acceptance Examples

- AE1. **Covers R1, R3.** Dane: repozytorium z gałęzią `develop`, bez gałęzi `main`. Gdy użytkownik uruchamia `compound-engineering`, zadanie pobiera `develop` i nie kończy się błędem.
- AE2. **Covers R2.** Dane: repozytorium bez ustawionej gałęzi, domyślna gałąź zdalna to `master`. Gdy użytkownik uruchamia dołączone workflow, zadanie pobiera `master`.
- AE3. **Covers R4, R5.** Dane: workflow z `ref: release/1.0`, które nie istnieje. Gdy zadanie startuje, błąd mówi, że `release/1.0` pochodzi z workflow.

---

## Success Criteria

- Użytkownik z repozytorium bez `main` konfiguruje je raz i uruchamia dołączone workflow bez edycji plików YAML.
- Planista nie musi wybierać zasady pierwszeństwa. Zasada jest opisana w R1–R4.

---

## Scope Boundaries

- Brak zmiany zasady „jawny `ref` w workflow wygrywa". Zmieniamy dane workflow, nie tę zasadę.
- Brak nowego pola ani przełącznika „gałąź z rejestru zawsze wygrywa".
- Brak automatycznego wykrywania gałęzi przy rejestracji repozytorium.
- Brak zmian w wznawianiu zadań: wznowione zadanie nadal używa zapisanego commita.

---

## Key Decisions

- Usunąć `ref: main` z dołączonych workflow zamiast zmieniać pierwszeństwo w kodzie: to najmniejsza zmiana. Zachowuje kontrakt z `WORKFLOW_SCHEMA.md` i nie psuje workflow użytkowników, które celowo podają `ref`.

## Dependencies / Assumptions

- Założenie (do potwierdzenia przez Artura): użytkownik uruchamia dołączone workflow. Jeśli ma własne workflow z `ref: main`, to ten sam problem wystąpi i dotyczy go R6.
- Założenie: nie ma drugiego miejsca z twardym `main`. Przeszukanie `crates/` i `src/` nie znalazło go poza przykładem w `crates/narsor-protocol/src/chat.rs` (lista opcji, nie pobranie).

## Outstanding Questions

### Deferred to Planning

- [Affects R3][Technical] Czy inne workflow w `tests/` lub `deploy/` zależą od `ref: main`?
- [Affects R5][Technical] Skąd w `RefNotFound` wziąć informację o źródle nazwy gałęzi.
