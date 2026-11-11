---
title: "feat: Eskalacja kroku - effort +2, potem model i effort do bazy"
type: feat
status: superseded
date: 2026-11-11
origin: docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md
---

# feat: Eskalacja kroku - effort +2, potem model i effort do bazy

> **Odrzucony (2026-11-11).** Ten plan nie zostanie wdrożony. Powody: wyzwalacz (porażka techniczna w pętli `retry_policy`) nie obejmuje przypadków, które mają znaczenie, czyli braku postępu i pętli między krokami. Zmiana modelu w trakcie pracy zwiększa koszt, bo cała rozmowa idzie do nowego modelu bez cache. Kierunek zastępczy: `docs/follow-ups/2026-11-11-effort-w-sesji-przy-braku-postepu.md`. Kod i dokument wymagań nie zostały zmienione.

## Summary

Reguła eskalacji w `step_profile` zmienia się z "effort +1 aż do sufitu, potem model" na cykl: effort +2 (nie wyżej niż sufit), potem model +1 z effortem z powrotem na bazie. Eskalacja następuje co 2 nieudane próby. Reguła zostaje w jednej czystej funkcji. Runner i dyrygent nie zmieniają się.

---

## Problem Frame

Obecna reguła (R9, R10 w dokumencie źródłowym) podnosi effort o jeden stopień na eskalację i dotyka modelu dopiero na suficie effortu. Przy progu 3 model rośnie po wielu nieudanych próbach. Effort `max` przy słabym modelu jest drogi i często mniej skuteczny niż mocniejszy model z bazowym effortem. Nowa reguła szybciej sięga po mocniejszy model i nie zostawia effortu na wysokim poziomie po jego zmianie. Szczegóły sporu i rozmowy: patrz origin.

---

## Requirements

- R1. (zmienia R9) Próg eskalacji wynosi domyślnie 2 nieudane próby kroku. Co `threshold` porażek następuje jedna eskalacja. Parametr zostaje globalny, `0` wyłącza eskalację.
- R2. (zmienia R10) Nieparzysta eskalacja podnosi effort o 2 stopnie w jednym kroku, ale nie wyżej niż sufit effortu. Parzysta eskalacja podnosi model o 1 poziom, nie wyżej niż sufit modelu, i przywraca effort do bazy. Cykl powtarza się na nowym modelu.
- R3. Baza to ustawienie sprzed eskalacji: wartość z workflow albo profil dyrygenta, jeśli go zapisał (po ewentualnym przycięciu do sufitu).
- R4. Eskalacja nigdy nie przekracza sufitu (R2 origin). Rung, który nic nie zmienia (effort już na suficie, brak miejsca na model), jest pomijany. Gdy eskalacja jest należna, a drabinka się skończyła, krok pozostaje na ostatnim ustawieniu i zgłasza limit.
- R5. Reguły bez zmian: krok bez sufitu nie eskaluje i nie zgłasza limitu (D8), kolejność źródeł profilu (workflow, dyrygent, eskalacja), zdarzenia dla operatora.

**Origin actors:** A2 (dyrygent, profil bazowy), A4 (agent kroku)
**Origin flows:** F3 (eskalacja po błędach)
**Origin acceptance examples:** AE4 (zmienia się: patrz U3)

---

## Scope Boundaries

- Nie zmieniam wyzwalacza eskalacji. Nadal jest nim kolejna nieudana próba w pętli ponowień (`retry_policy`).
- Nie dodaję walidacji ani ostrzeżenia o relacji `threshold` i `max_attempts`.
- Nie zmieniam dyrygenta, wyboru profilu po kroku planu, ani zasady "decyzja raz".

### Deferred to Follow-Up Work

- Ochrona przed brakiem postępu: domyślny limit wejść w krok agenta, wykrywanie powtórzeń, budżet czasu lub tokenów jednego przebiegu.
- Eskalacja licznikiem wejść w krok zamiast porażek ponowienia.
- Dyrygent jako doradca eskalacji (przeskok poziomu, zatrzymanie i pytanie operatora).
- Ostrzeżenie przy ładowaniu workflow, gdy `max_attempts` jest za małe, żeby eskalacja zadziałała (przy progu 2 potrzeba `max_attempts` >= 3).

---

## Context & Research

### Relevant Code and Patterns

- `crates/narsor-engine/src/engine/step_profile.rs`: `resolve`, `raise`, `Setting`, `EffectiveProfile`, testy jednostkowe. Reguła kolejności żyje tylko tutaj.
- `crates/narsor-engine/src/engine/runner.rs` (`apply_step_profile`): woła `resolve` z `failures: attempt - 1` i progiem silnika. Czyta `source` i `limit_reached`. Bez zmian.
- `crates/narsor-engine/src/conductor/profile.rs` (`entries_of`): woła `resolve` z `failures: 0` i `threshold: 0`. Eskalacja go nie dotyczy.
- `crates/narsor-shared/src/config/settings.rs`: `DEFAULT_ESCALATION_THRESHOLD`, `MAX_ESCALATION_THRESHOLD`.
- `tests/integration/step_profile_flow.rs`: przepływ eskalacji z progiem 2 (`escalation_flow`).
- `tests/integration/config_resolve.rs`: asercje domyślnego progu.

### Institutional Learnings

- `docs/solutions/architecture-patterns/step-profile-tiers-decide-once-validate-at-load-2026-11-11.md`: reguła kolejności w czystych funkcjach (punkt 4), kompletność sprawdzana przy ładowaniu (punkt 3) - eskalacja nie spotyka niewypełnionego pola. Plan to zachowuje.

---

## Key Technical Decisions

- **Drabinka zamiast mutacji w pętli.** `resolve` buduje z bazy i sufitu listę kolejnych ustawień (rungów) i wybiera rung o numerze `failures / threshold`. Zalety: reguła jest deterministyczna, bez stanu poprzedniej próby, a rung pomijany (bez zmiany) znika z listy. To zastępuje `raise`.
- **Kolejność rungów:** dla każdego poziomu modelu od bazowego do sufitu: (a) na poziomie wyższym niż bazowy - model +1 z effortem bazowym, (b) effort podniesiony o 2 stopnie, przycięty do sufitu effortu, jeśli różni się od bieżącego. Pierwszy poziom modelu nie ma rungu (a), bo to baza.
- **Baza = ustawienie po kroku dyrygenta.** Eskalacja liczy od profilu, który dyrygent dobrał do złożoności, a nie od wartości z workflow. Jeśli dyrygent poprosił o więcej niż sufit, baza to sufit (źródło `Clamped`), drabinka jest pusta.
- **Próg domyślny 2.** Zmiana `DEFAULT_ESCALATION_THRESHOLD` z 3 na 2 zmienia zachowanie każdej instalacji, która nie ustawiła `escalation_threshold` jawnie. Dotyczy to też dokumentacji użytkownika.
- **`limit_reached`.** Ustawiany, gdy numer eskalacji jest większy od liczby rungów, a krok ma sufit. Bez sufitu nigdy (zasada D8 bez zmian).
- **Zdarzenia dla operatora bez zmian.** Komunikaty "raised" i "already runs on its ceiling" zostają, bo opisują zjawisko, nie szczegół reguły.

---

## Open Questions

### Resolved During Planning

- Skok +2 naraz czy dwa kroki po +1: skok +2 naraz (decyzja Artura).
- Do jakiej wartości wraca effort: do bazy, czyli ustawienia sprzed eskalacji (potwierdzone).
- Próg domyślny: 2 (potwierdzone).

### Deferred to Implementation

- Dokładny kształt struktury drabinki (lista rungów czy iterator). Decyzja przy pisaniu `resolve`.
- Czy komunikat zdarzenia ma wymieniać numer eskalacji. Do oceny przy zmianie testu integracyjnego.

---

## High-Level Technical Design

> *To jest ilustracja kierunku do przeglądu, nie specyfikacja implementacji.*

Dla bazy `medium/medium` i sufitu `large/max` (skala effortu: low, medium, high, max), próg 2:

| Porażki | Eskalacja | Model | Effort |
|---|---|---|---|
| 0-1 | 0 | medium | medium (baza) |
| 2-3 | 1 | medium | max (effort +2) |
| 4-5 | 2 | large | medium (model +1, effort = baza) |
| 6-7 | 3 | large | max (effort +2) |
| 8+ | 4 | large | max, `limit_reached` |

Przypadki brzegowe (rungi pomijane):

| Baza | Sufit | Rungi po kolei |
|---|---|---|
| `medium/high` | `medium/max` | `medium/max`, potem limit |
| `medium/max` | `large/max` | `large/max`, potem limit (effort bez zmian na bazie) |
| `large/medium` | `large/high` | `large/high`, potem limit |
| `medium/(brak)` | `large/(brak)` | `large/(brak)`, potem limit |
| `(brak)/low` | `(brak)/max` | `(brak)/high`, potem limit |

---

## Implementation Units

### U1. Drabinka eskalacji w `step_profile`

**Goal:** Zastąpić `raise` regułą z R2-R4 i zachować czystość `resolve`.

**Requirements:** R2, R3, R4, R5

**Dependencies:** None

**Files:**
- Modify: `crates/narsor-engine/src/engine/step_profile.rs`
- Test: `crates/narsor-engine/src/engine/step_profile.rs` (moduł `tests`)

**Approach:**
- Po wyliczeniu ustawienia z profilu dyrygenta (baza) zbuduj drabinkę rungów według kolejności z Key Technical Decisions. Wybierz rung o numerze `failures / threshold`.
- Pomiń rung równy poprzedniemu ustawieniu. Użyj `tier_rank` i `effort_rank` oraz sufitu liczonego jak dotąd (pola sufitu, w razie braku wartość domyślna).
- Zachowaj ustawianie `source` (`Escalated`, nie nadpisuj `Clamped`) i `limit_reached` zgodnie z Key Technical Decisions.
- Zaktualizuj komentarze modułu i `ProfileInput` opisujące regułę.

**Execution note:** Najpierw zmień testy jednostkowe (test-first), bo reguła jest czystą funkcją i ma wiele przypadków brzegowych.

**Patterns to follow:**
- Istniejąca konwencja testów w `step_profile.rs` (pomocnik `input`, `Setting::new`).

**Test scenarios:**
- Happy path: baza `medium/medium`, sufit `large/max`, próg 2. Porażki 0-1 dają bazę, 2-3 dają `medium/max`, 4-5 dają `large/medium`, 6-7 dają `large/max`, 8 daje `large/max` i `limit_reached`.
- Happy path: źródło to `Escalated` od pierwszej eskalacji, `Default` przed nią.
- Edge case: baza `medium/high`, sufit `medium/max` - effort rośnie o 1 (do sufitu), potem limit.
- Edge case: effort bazowy już na suficie - pierwsza eskalacja podnosi od razu model, effort bez zmian.
- Edge case: model bazowy na suficie - tylko rungi effortu, potem limit.
- Edge case: brak effortu lub brak modelu w ustawieniu - tylko rungi drugiej osi, bez paniki.
- Edge case: baza z profilu dyrygenta (`conductor`), np. `small/low` przy sufitie `large/high` - eskalacja liczy od `small/low`, nie od wartości z workflow.
- Edge case: prośba dyrygenta nad sufitem (`Clamped`) - baza to sufit, drabinka pusta, `limit_reached`, źródło zostaje `Clamped`.
- Edge case: próg `0` wyłącza eskalację przy dowolnej liczbie porażek.
- Edge case: bez sufitu nic się nie eskaluje, `limit_reached` fałszywe (D8).
- Edge case: próg większy niż liczba porażek nie eskaluje (granica `failures = threshold - 1`).
- Zaktualizuj istniejące testy: `the_effort_rises_only_after_the_threshold` (po progu effort rośnie o 2, nie o 1), `a_changed_threshold_moves_the_escalation`, `the_model_rises_when_the_effort_stands_on_the_ceiling`, `both_on_the_ceiling_reports_the_limit`.

**Verification:**
- Wszystkie testy jednostkowe `step_profile` przechodzą. Dla tych samych danych wejściowych `resolve` zwraca ten sam wynik (brak stanu).

---

### U2. Domyślny próg 2 i dokumentacja konfiguracji

**Goal:** Zmienić `DEFAULT_ESCALATION_THRESHOLD` na 2 i dostosować wszystko, co go opisuje.

**Requirements:** R1

**Dependencies:** None

**Files:**
- Modify: `crates/narsor-shared/src/config/settings.rs`
- Modify: `docs/user/configuration.md`
- Test: `tests/integration/config_resolve.rs`

**Approach:**
- Zmień stałą na 2 i jej komentarz.
- W `docs/user/configuration.md` zmień wartość w przykładzie i w tabeli oraz opis klucza: "effort rośnie o 2, potem model" zamiast "effort, potem model".
- Przeszukaj `src/cli/init.rs` i inne miejsca wypisujące domyślny próg (komentarze szablonu konfiguracji) i dostosuj, jeśli mają wartość 3.

**Patterns to follow:**
- Sposób, w jaki testy w `config_resolve.rs` sprawdzają wartości domyślne.

**Test scenarios:**
- Happy path: konfiguracja bez `escalation_threshold` daje próg 2 (dwa miejsca w `config_resolve.rs`: `escalation_threshold()` i `agent_defaults().escalation_threshold`).
- Edge case: jawne `escalation_threshold: 0` nadal wyłącza eskalację, a `100` nadal przechodzi (istniejące testy zostają).

**Verification:**
- Testy konfiguracji przechodzą. W dokumentacji nie ma już wartości 3 dla tego klucza.

---

### U3. Test przepływu, dokumentacja reguły i poprawka wymagań

**Goal:** Dopasować test integracyjny do nowej reguły i zapisać regułę w dokumentacji i w dokumencie wymagań.

**Requirements:** R2, R3, R4, R5

**Dependencies:** U1, U2

**Files:**
- Modify: `tests/integration/step_profile_flow.rs`
- Modify: `docs/dev/workflow-schema.md`
- Modify: `docs/solutions/architecture-patterns/step-profile-tiers-decide-once-validate-at-load-2026-11-11.md`
- Modify: `docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md`

**Approach:**
- `a_step_that_fails_repeatedly_is_raised_within_its_ceiling`: po dwóch porażkach trzecia próba używa effortu podniesionego o 2 (w fixture `v2/engine_step_profile.yaml` domyślny effort kroku `work` to `low`, sufit `high`, więc oczekiwane `medium/high`). Sprawdź fixture przed zmianą asercji.
- `the_limit_is_announced_only_for_a_step_with_a_ceiling`: bez zmian zachowania, tylko sprawdź, że nadal przechodzi.
- `docs/dev/workflow-schema.md`: dodaj krótki opis reguły eskalacji (cykl effort +2, model +1 z effortem do bazy, sufit, próg) w opisie `globals.ceiling` i kroku agenta.
- Dokument w `docs/solutions/`: popraw tam, gdzie opisuje "effort rośnie, potem model", i dodaj uwagę o bazie (profil dyrygenta).
- Dokument wymagań: zaktualizuj R9, R10, AE4 i "Założenia do potwierdzenia" z datowaną adnotacją (2026-11-11) o zmianie reguły i wskazaniem tego planu. Nie usuwaj historii decyzji.

**Patterns to follow:**
- Styl adnotacji decyzji operatora w dokumencie wymagań (sekcja "Decyzje operatora").

**Test scenarios:**
- Integration: krok `work` kończy się błędem 2 razy, trzecia próba dostaje `medium/high` (effort +2 do sufitu), zdarzenie "raised" pojawia się raz.
- Integration: krok z profilem dyrygenta `large/high` i sufitem `large/high` po porażkach zgłasza limit raz (istniejący test).
- Integration (nowy, jeśli fixture pozwala bez dużej zmiany): krok z sufitem `large/max` po 4 porażkach dostaje `large/medium` (model +1, effort = baza). Jeśli wymaga to nowego fixture, odłóż do implementacji i zapisz jako Deferred.

**Verification:**
- Testy integracyjne przechodzą. Dokumentacja opisuje tę samą regułę co kod. Dokument wymagań wskazuje na zmianę.

---

## System-Wide Impact

- **Interaction graph:** Zmienia się tylko wynik `resolve`. Runner (`apply_step_profile`) i dyrygent (`entries_of` z `threshold: 0`) nie wymagają zmian.
- **Error propagation:** Bez zmian. Eskalacja nie tworzy nowych błędów, a rung pomijany nie powoduje paniki.
- **State lifecycle risks:** Brak stanu. Drabinka wynika z danych wejściowych, więc restart demona nie zmienia reguły. Licznik porażek to zmienna lokalna pętli, więc po restarcie liczy od nowa. To zachowanie istnieje dziś i zostaje.
- **API surface parity:** Zmiana domyślnego progu widoczna w konfiguracji (`narsor-cli` i TUI wypisują wartość domyślną). Sprawdzić miejsca z `SettingKey::EscalationThreshold` pod kątem komunikatów.
- **Unchanged invariants:** Kolejność źródeł profilu, sufit jako górna granica, brak eskalacji bez sufitu, decyzja raz przed pierwszym wejściem.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Próg 2 wymaga `max_attempts` >= 3, a domyślnie jest 1 próba. Eskalacja nadal nie działa bez `retry_policy`. | Zapisane jako Deferred (ostrzeżenie przy ładowaniu). Opis w dokumentacji: kiedy eskalacja się uruchamia. |
| Zmiana domyślnego progu zmienia zachowanie istniejących instalacji. | Wpis w dokumentacji konfiguracji. Zmiana jest jawna w planie i w komunikacie zmiany. |
| Skok +2 i reset effortu mogą dać słabszą próbę niż poprzednia (`large/medium` kontra `medium/max`). | Założenie projektowe (mocniejszy model potrzebuje mniej effortu), niezweryfikowane pomiarem. Pomiar jest w Success Criteria origin. |
| Test integracyjny zależy od fixture `engine_step_profile.yaml`. | Sprawdzić wartości fixture przed zmianą asercji (U3). |

---

## Documentation / Operational Notes

- Wpis o zmianie domyślnego progu w opisie zmiany (PR).
- Brak zmian w działaniu demona poza regułą profilu.

---

## Sources & References

- **Origin document:** [docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md](../brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md)
- Poprzedni plan: `docs/plans/2026-11-11-1000-feat-dynamiczny-model-i-effort-kroku-plan.md`
- Related PR: https://github.com/glakartur/narsor/pull/92
- Related code: `crates/narsor-engine/src/engine/step_profile.rs`, `crates/narsor-engine/src/engine/runner.rs` (`apply_step_profile`, `run_action`)
