---
title: "refactor: Wspólna koperta launch spec w narsor-protocol"
type: refactor
status: active
date: 2026-10-24
---

# refactor: Wspólna koperta launch spec w narsor-protocol

## Summary

Lista pól najwyższego poziomu launch spec i stała wersji trafiają do `narsor-protocol`. Engine czyta launch spec przez tę kopertę, a potem nadaje typy częściom zagnieżdżonym. Kontroler waliduje tę samą kopertę zamiast własnej kopii. Dodanie pola po jednej stronie przestaje się kompilować.

---

## Problem Frame

Dziś ta sama lista pól istnieje w dwóch miejscach. `TaskLaunchSpecFields` w engine zna `ticket` i `title`. `LaunchInputEnvelope` w kontrolerze ich nie znała i ma `deny_unknown_fields`. Po #27 kontroler odrzucał każdy nowy task jako `Fenced`, zwalniał lease i pobierał go ponownie w pętli. Żaden nowy task nie wychodził ze stanu `provisioning`.

Hotfix `79a4ae9` dopisał oba pola w kontrolerze. Nie usuwa przyczyny: następne pole znów można dodać tylko po jednej stronie, a błąd wyjdzie dopiero na żywym daemonie. Stała wersji też jest zduplikowana: engine ma `TASK_LAUNCH_SPEC_VERSION`, a kontroler porównuje z wpisaną liczbą `3`.

Kontroler celowo nie importuje modelu domenowego engine (komentarz przy `LaunchInputEnvelope`). Plan to zachowuje: wspólna jest tylko koperta, a części zagnieżdżone są w niej nieprzezroczystym `Value`.

---

## Requirements

- R1. Lista pól najwyższego poziomu launch spec ma jedną definicję, w `narsor-protocol`.
- R2. `TASK_LAUNCH_SPEC_VERSION` ma jedną definicję, w `narsor-protocol`. Engine i kontroler porównują z tą samą stałą.
- R3. Engine czyta launch spec przez kopertę. Dodanie pola do koperty bez zmiany `TaskLaunchSpec` nie kompiluje się. W drugą stronę (nowe pole w `TaskLaunchSpec` bez zmiany koperty) pilnuje tego test z U4.
- R4. Kontroler waliduje kopertę z `narsor-protocol` i nie zależy od `narsor-engine`.
- R5. Format zapisu launch spec i digest się nie zmieniają. Istniejące rekordy `task.json` (także sprzed #27) nadal się czytają i mają poprawny digest.
- R6. Test pilnuje, że prawdziwy `TaskLaunchSpec` (z `ticket`, `title`, `definition` i bez nich) przechodzi walidację kontrolera.

---

## Scope Boundaries

- `TaskWorkspace`, `ResolvedEnvironment` i `ResourceProfile` zostają w engine. Nie przenosimy `TaskLaunchSpec` do protokołu.
- Nie zmieniamy formatu serializacji ani wersji launch spec (zostaje 3).
- Nie zmieniamy runnera ani adaptera Kubernetes. Oba czytają launch spec przez `Deserialize` engine'u.

### Deferred to Follow-Up Work

- Hotfix `79a4ae9` (dwa pola w kontrolerze): osobny PR, wcześniej niż ten plan.
- `TaskLaunchSpec::compute_digest` ma ręczną listę pól. To trzecia kopia listy i ten plan jej nie zamyka. Osobne zadanie: test, że zmiana każdego pola najwyższego poziomu zmienia digest.
- Wpis w `docs/solutions/` o awarii (pętla lease po dodaniu pola): przez `/ce-compound` po wdrożeniu.

---

## Context & Research

### Relevant Code and Patterns

- `crates/narsor-engine/src/domain/task.rs`: `TaskLaunchSpec`, prywatny `TaskLaunchSpecFields`, ręczny `Deserialize` ze sprawdzeniem wersji, `compute_digest`, stała `TASK_LAUNCH_SPEC_VERSION`.
- `crates/narsor-controller-runtime/src/backend.rs`: `LaunchInputEnvelope` i `validate_launch_input`. Wołane z `launch()` w `crates/narsor-controller-runtime/src/controller.rs`.
- `crates/narsor-engine/src/domain/task.rs` (linia z `pub use narsor_protocol::identifier::...`): wzorzec re-eksportu typu z protokołu przez engine. Stosujemy go dla stałej wersji.
- Graf zależności: `narsor-engine` i `narsor-controller-runtime` już zależą od `narsor-protocol`, a `narsor-protocol` nie zależy od żadnego z nich. Nowych zależności nie ma.
- Runner i `crates/narsor-controller/src/kubernetes/manifest.rs` (`plan`) czytają launch spec przez `TaskLaunchSpec::deserialize`. Zmiana jest dla nich przezroczysta.
- Testy: `tests/task_labels.rs` (budowa `TaskLaunchSpec` z etykietami), `tests/environment.rs` (importuje `TASK_LAUNCH_SPEC_VERSION` z engine), `tests/persistence.rs`.

### Institutional Learnings

- Brak wpisu w `docs/solutions/` o tej klasie błędu. Plan #27 (`docs/plans/2026-10-24-003-feat-task-ticket-i-tytul-plan.md`) wymieniał `TaskLaunchSpecFields` jako miejsce zmiany, ale nie kopię w kontrolerze.

---

## Key Technical Decisions

- **Koperta w `narsor-protocol`.** Obie strony już od niego zależą. Moduł `launch` nie zależy od niczego poza `serde` i `serde_json`.
- **Części zagnieżdżone to `Value` w kopercie.** Kontroler nie ma wiedzy o typach domenowych i nie ma jej zyskać. Engine nadaje typy po sparsowaniu koperty.
- **Blokada kompilacji działa w jedną stronę.** Engine rozpakowuje kopertę wyczerpującym wzorcem (bez `..`). Nowe pole w kopercie zatrzymuje kompilację. Nowe pole w `TaskLaunchSpec` zatrzymuje tylko wyczerpujący literał, a ten da się zaspokoić wartością `None`. Wtedy `Serialize` wyśle pole, którego koperta nie zna. W tę stronę pilnuje test z U4: każdy klucz zserializowanego `TaskLaunchSpec` musi istnieć w kopercie.
- **`deny_unknown_fields` zostaje** na kopercie. Z jedną definicją przestaje być pułapką i dalej łapie literówki.
- **Komunikat o wersji zostaje w engine.** Zawiera wskazówkę "archive the task and create it again", która dotyczy engine. Do protokołu przechodzi tylko stała.
- **Engine re-eksportuje stałą** (`pub use`), więc `tests/environment.rs` i inne importy nie zmieniają się.
- **Hotfix zostaje osobno i wcześniej.** Ten plan go zastępuje: usuwa lokalną kopertę kontrolera razem z polami z hotfixu.

---

## Open Questions

### Resolved During Planning

- Jeden typ generyczny w protokole z aliasem w engine? Odrzucone. Reguła osieroconych impl-i wymusza przepisanie metod `TaskLaunchSpec` (`new`, `with_definition`, `compute_digest` itd.) i dotyka ~70 miejsc użycia.
- Gdzie wspólny kod? W `narsor-protocol`, bo `narsor-shared` zależy od protokołu i nie jest lepszym miejscem dla typu zapisu.

### Deferred to Implementation

- Dokładne brzmienie błędu, gdy część zagnieżdżona jest niepoprawna. Ma zawierać nazwę pola. Test sprawdza nazwę pola, a nie całe zdanie.
- Czy koperta ma udostępniać pomocnik do kontroli pustych `workflow_id`, `workflow_version` i `digest`. Dziś robi to kontroler. Decyzja przy implementacji U3.

---

## Implementation Units

### U1. Koperta i stała wersji w narsor-protocol

**Goal:** Jedna definicja pól najwyższego poziomu launch spec i stałej wersji.

**Requirements:** R1, R2

**Dependencies:** Brak (hotfix `79a4ae9` jest wcześniej, osobnym PR).

**Files:**
- Create: `crates/narsor-protocol/src/launch.rs`
- Modify: `crates/narsor-protocol/src/lib.rs`
- Test: `crates/narsor-protocol/src/launch.rs` (moduł `tests`)

**Approach:**
- Koperta ma pola dokładnie takie jak dziś `TaskLaunchSpecFields`: `version`, `workflow_id`, `workflow_version`, `inputs`, `workspace`, `environment`, `resources`, `definition`, `ticket`, `title`, `digest`. Pola zagnieżdżone to `serde_json::Value`, a pola publiczne.
- `definition`, `ticket` i `title` są opcjonalne (`#[serde(default)]`). `deny_unknown_fields`.
- Stała `TASK_LAUNCH_SPEC_VERSION = 3` z komentarzem historii wersji, przeniesionym z engine.

**Patterns to follow:**
- Istniejące moduły `crates/narsor-protocol/src/controller.rs` (typy drutu z `serde`).

**Test scenarios:**
- Happy path: pełna koperta z `ticket` i `title` parsuje się, pola mają oczekiwane wartości.
- Happy path: koperta bez `ticket`, `title` i `definition` parsuje się (spec sprzed #27 i spec bez snapshotu).
- Error path: nieznane pole najwyższego poziomu daje błąd z nazwą pola.
- Error path: brak `digest` daje błąd.

**Verification:**
- `cargo test -p narsor-protocol` przechodzi. Stała `TASK_LAUNCH_SPEC_VERSION` jest dostępna jako `narsor_protocol::launch::TASK_LAUNCH_SPEC_VERSION`.

---

### U2. Engine czyta launch spec przez kopertę

**Goal:** Zastąpić `TaskLaunchSpecFields` wspólną kopertą, z blokadą kompilacji w obie strony.

**Requirements:** R2, R3, R5

**Dependencies:** U1

**Files:**
- Modify: `crates/narsor-engine/src/domain/task.rs`
- Test: `tests/task_labels.rs`, `tests/environment.rs`, `tests/persistence.rs` (istniejące, bez zmian oczekiwań)
- Test: `crates/narsor-engine/src/domain/task.rs` (moduł `tests`, jeśli istnieje; inaczej `tests/task_labels.rs`)

**Approach:**
- Usunąć `TaskLaunchSpecFields` i lokalną definicję stałej. Dodać `pub use narsor_protocol::launch::TASK_LAUNCH_SPEC_VERSION` obok istniejącego re-eksportu identyfikatorów.
- `Deserialize` zachowuje obecną kolejność: najpierw odczyt i sprawdzenie wersji (z obecnym komunikatem), potem parsowanie koperty.
- Po sparsowaniu koperty: rozpakowanie wyczerpującym wzorcem (bez `..`), nadanie typów `workspace`, `environment` i `resources` przez `serde_json::from_value`, złożenie `TaskLaunchSpec` wyczerpującym literałem. Błąd części zagnieżdżonej nazywa pole.
- `Serialize` i `compute_digest` bez zmian.

**Execution note:** Zacząć od uruchomienia istniejących testów jako testów charakteryzujących. Zachowanie `Deserialize` nie może się zmienić poza treścią błędów części zagnieżdżonych.

**Patterns to follow:**
- Obecny `Deserialize for TaskLaunchSpec` (kolejność: wersja, potem pola).

**Test scenarios:**
- Happy path: spec zapisany i odczytany (round trip) jest równy oryginałowi, także z `ticket`, `title` i `definition`.
- Edge case: spec sprzed #27 (bez `ticket` i `title`) czyta się i `digest_is_valid()` zwraca true.
- Error path: wersja inna niż 3 daje obecny komunikat z nazwą obu wersji.
- Error path: nieznane pole najwyższego poziomu jest odrzucone.
- Error path: niepoprawny `workspace` daje błąd zawierający nazwę pola `workspace`.

**Verification:**
- Istniejące testy `tests/task_labels.rs`, `tests/environment.rs`, `tests/persistence.rs` przechodzą bez zmian oczekiwań.
- Dopisanie pola do koperty w U1 (próbnie, lokalnie) zatrzymuje kompilację engine'u.

---

### U3. Kontroler waliduje wspólną kopertę

**Goal:** Usunąć lokalną kopię koperty z kontrolera. Zastąpić ją typem z protokołu.

**Requirements:** R2, R4

**Dependencies:** U1 (może iść równolegle z U2)

**Files:**
- Modify: `crates/narsor-controller-runtime/src/backend.rs`
- Modify: `crates/narsor-controller-runtime/src/controller.rs` (tylko literał wersji w teście `launch_spec`)
- Test: `crates/narsor-controller-runtime/src/backend.rs` (moduł `tests` z hotfixu)

**Approach:**
- Usunąć `LaunchInputEnvelope` i obejście `let _ = (...)` na polach (pola są teraz publiczne, więc nie ma ostrzeżenia `dead_code`).
- `validate_launch_input` parsuje kopertę z `narsor_protocol::launch`, porównuje `version` ze stałą i zachowuje kontrolę pustych `workflow_id`, `workflow_version` i `digest`.
- Literały `3` w testach zastąpić stałą.

**Test scenarios:**
- Happy path: test z hotfixu (`ticket` i `title`) nadal przechodzi.
- Happy path: spec bez `ticket`, `title` i `definition` przechodzi.
- Error path: wersja inna niż stała daje błąd.
- Error path: pusty `digest` daje błąd.
- Error path: nieznane pole daje błąd z nazwą pola.

**Verification:**
- `cargo test -p narsor-controller-runtime` przechodzi. `narsor-controller-runtime` nadal nie zależy od `narsor-engine` (sprawdzić w `Cargo.toml`).

---

### U4. Test kontraktowy engine–kontroler

**Goal:** Test, który pada, gdy prawdziwy `TaskLaunchSpec` przestanie przechodzić walidację kontrolera.

**Requirements:** R6

**Dependencies:** U2, U3

**Files:**
- Create: `tests/launch_spec_contract.rs`

**Approach:**
- Zbudować prawdziwy `TaskLaunchSpec` (jak w `tests/task_labels.rs`), serializować do `Value` i przepuścić przez `narsor_controller_runtime::backend::validate_launch_input`. Katalog główny ma obie zależności w `dev-dependencies`.
- Warianty: bez etykiet, z `ticket` i `title`, z `definition`.

**Test scenarios:**
- Integration: spec bez etykiet przechodzi walidację kontrolera.
- Integration: spec z `ticket` i `title` przechodzi. To odtwarza awarię z #27.
- Integration: spec z snapshotem `definition` przechodzi.
- Integration: każdy klucz najwyższego poziomu zserializowanego `TaskLaunchSpec` (z ustawionymi `ticket`, `title` i `definition`) istnieje w kopercie. Test pada, gdy ktoś doda pole tylko do `TaskLaunchSpec`. Nowe pole w teście trzeba ustawić na wartość niedomyślną.

**Verification:**
- Test pada na gałęzi bez poprawki z `79a4ae9` (sprawdzić na `main`) i przechodzi po U1–U3.

---

## System-Wide Impact

- **Interaction graph:** Zmienia się odczyt launch spec w engine i walidacja w kontrolerze. Runner i adapter Kubernetes czytają launch spec przez `Deserialize` engine'u i nie wymagają zmian kodu. Ścieżka zapisu się nie zmienia.
- **Error propagation:** Błąd części zagnieżdżonej zmienia treść komunikatu (dodana nazwa pola). Komunikaty pojawiają się w szczegółach niepowodzenia taska. Test sprawdza nazwę pola.
- **State lifecycle risks:** Rekordy `task.json` na dysku muszą nadal się czytać. Chroni to scenariusz specu sprzed #27 w U2.
- **API surface parity:** Adapter Kubernetes i lokalny kontroler dzielą kod `narsor-controller-runtime`, więc poprawka obejmuje oba.
- **Unchanged invariants:** Format JSON launch spec, `digest`, wersja 3 i zasada, że kontroler nie importuje modelu domenowego engine.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Zmiana treści błędów deserializacji w engine | Test nazwy pola w U2. Komunikat o wersji bez zmian. |
| Podwójne parsowanie części zagnieżdżonych (`Value`, potem typ) | Spec czytany jest rzadko (start i wznowienie taska). Koszt pomijalny. |
| Konflikt z hotfixem `79a4ae9` | Hotfix idzie pierwszy. U3 usuwa jego pola razem z lokalną kopertą. |
| Daemon w użyciu działa na starych binariach | Po wdrożeniu przebudować i zrestartować (`./run.sh`). Przy testach `backend_contract` wykonać `cargo build --workspace --bins`, bo `cargo test -p narsor` nie przebudowuje runnera. |

---

## Documentation / Operational Notes

- Wdrożenie wymaga przebudowy wszystkich binariów, bo koperta jest wspólna dla serwera, kontrolera i runnera.
- Po wdrożeniu: wpis w `docs/solutions/` (patrz Deferred).
- Znany stan sprzed zmiany: `cargo test -p narsor --test backend_contract` ma 18 porażek (`fake::*`, "zadanie jest zakończone" przy `Pause`), niezależnych od tego planu. Nie traktować ich jako regresji tej zmiany.

---

## Sources & References

- Related code: `crates/narsor-engine/src/domain/task.rs`, `crates/narsor-controller-runtime/src/backend.rs`, `crates/narsor-controller-runtime/src/controller.rs`
- Related commits: `c440e16` (#27, dodał `ticket` i `title`), `79a4ae9` (hotfix koperty kontrolera)
- Related plan: `docs/plans/2026-10-24-003-feat-task-ticket-i-tytul-plan.md`
