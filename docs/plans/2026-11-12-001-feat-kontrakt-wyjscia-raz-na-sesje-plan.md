---
title: "feat: Kontrakt wyjścia tylko raz na sesję"
type: feat
status: active
date: 2026-11-12
origin: docs/brainstorms/2026-11-12-kontrakt-wyjscia-przy-wznowieniu-sesji-requirements.md
---

# feat: Kontrakt wyjścia tylko raz na sesję

## Summary

Silnik nie dokleja bloku kontraktu do wiadomości operatora, gdy wznawia sesję agenta po odpowiedzi operatora w tym samym kroku. Silnik oznacza taką próbę znacznikiem w konfiguracji kroku. Wykonawca agenta czyta znacznik i pomija kontrakt. Pierwszy prompt, naprawa formatu i `resume_session` z workflow nie mają znacznika, więc dostają kontrakt jak dotąd. Rozmowa w TUI i API pokazuje prompt wysłany do agenta, więc R6 spełnia ta sama zmiana. Plan ma 2 jednostki. Rozmiar: Lightweight.

---

## Problem Frame

Każdy prompt agenta kończy się blokiem z `output_contract`. Wznowiona sesja ma ten blok już w historii. Operator widzi go przy każdej swojej wiadomości, a agent płaci za niego tokenami (see origin: `docs/brainstorms/2026-11-12-kontrakt-wyjscia-przy-wznowieniu-sesji-requirements.md`). Plan realizuje R1–R6 i AE1–AE4 z origin.

---

## Requirements

- R1. Wznowienie sesji z odpowiedzią operatora w tym samym kroku wysyła tekst operatora bez kontraktu. (U1, U2)
- R2. Pierwszy prompt i ponowienie kroku w nowej sesji mają pełny kontrakt. (U1, U2)
- R3. Naprawa formatu (engine reprompt i conductor reprompt) ma pełny kontrakt. (U1, U2)
- R4. Krok z `resume_session` z workflow ma kontrakt nowego kroku. (U1, U2)
- R5. Czytanie odpowiedzi bez zmian. (U1: zmiana tylko w treści promptu)
- R6. Rozmowa pokazuje wiadomość operatora bez kontraktu. (U1: rozmowa zapisuje wysłany prompt)

---

## Scope Boundaries

- Nie zmieniamy treści ani długości `output_contract`.
- Nie zmieniamy tekstu naprawy formatu (`OUTPUT_REPROMPT`) poza gwarancją, że kontrakt go dotyczy.
- Nie zmieniamy mechaniki wznawiania ani zapisu sesji.
- Nie zmieniamy profilu, dyrygenta ani kroku `profile`.

---

## Context & Research

Stan kodu (sprawdzony w tej sesji):

- `crates/narsor-engine/src/engine/actions/agent.rs`, `AgentExecutor::run`: składa `config.prompt + repositories + output_contract(...)` zawsze. Gałąź `resume.is_none()` pomija tylko blok repozytoriów.
- `crates/narsor-engine/src/engine/actions/agent/contract.rs`: `continue_session` ustawia `resume_session` i podmienia prompt. Używają go `reprompt_config` (naprawa formatu, także conductor) i `prepare_harness_attempt`.
- `crates/narsor-engine/src/engine/runner/step/harness_attempt.rs`, `prepare_harness_attempt`: gałąź z `input.harness` i `input.session` to powrót operatora w tym samym kroku (F1). To jedyne miejsce, które zna ten przypadek.
- `crates/narsor-engine/src/engine/actions/session.rs`: zapis rozmowy (`open_conversation`) bierze `prompt` wysłany do agenta jako element `ChatElement::Prompt`. Źródła osobnego dla widoku operatora nie ma.
- `HarnessStepConfig` (`crates/narsor-engine/src/domain/workflow_definition.rs`) ma już pole `permission_grants` z `#[serde(skip)]`. To wzór dla pola tylko w czasie wykonania.

Odpowiedzi na pytania odłożone z origin:

- **Rozróżnienie trzech przypadków:** sam parametr `resume_session` nie wystarcza, bo ustawiają go trzy ścieżki. Rozróżnia je dopiero znacznik w konfiguracji kroku, ustawiany tylko w gałęzi operatora.
- **Harnessy inne niż Claude Code:** nie ma w kodzie dowodu, że trzymają pierwszy prompt. Plan stosuje regułę do wszystkich harnessów (Key Decision z origin). Zapasem jest naprawa formatu z kontraktem (R3). Ryzyko opisane niżej.
- **R6:** wymaga tylko zmiany promptu. Osobnej zmiany w TUI i API nie ma.

Zewnętrzne badania: nie potrzebne. Wzorce lokalne wystarczają.

---

## Key Technical Decisions

- **Znacznik w `HarnessStepConfig`, `#[serde(skip)]`, domyślnie wyłączony.** Wzór: `permission_grants`. Nie trafia do definicji workflow ani do zapisu. Wartość domyślna daje dotychczasowe zachowanie, więc R2 i R4 działają bez zmian.
- **Znacznik ustawia tylko gałąź operatora w `prepare_harness_attempt`.** Nie ustawia go `continue_session` w `contract.rs`, bo używa go też naprawa formatu.
- **`reprompt_config` zawsze zeruje znacznik.** Naprawa formatu bierze akcję z wcześniejszej próby. Ta akcja mogła mieć znacznik. Zerowanie gwarantuje R3 niezależnie od tego, skąd akcja pochodzi.
- **Reguła dla wszystkich harnessów.** Zgodnie z origin. Nie ograniczamy do Claude Code.

---

## Implementation Units

### U1. Znacznik i pominięcie kontraktu w wykonawcy

**Goal:** Wykonawca agenta nie dokleja kontraktu, gdy konfiguracja kroku ma znacznik. Naprawa formatu zeruje znacznik.

**Requirements:** R1, R2, R3, R4, R5, R6

**Dependencies:** brak

**Files:**
- Modify: `crates/narsor-engine/src/domain/workflow_definition.rs` (pole w `HarnessStepConfig`)
- Modify: `crates/narsor-engine/src/engine/actions/agent.rs` (`AgentExecutor::run`)
- Modify: `crates/narsor-engine/src/engine/actions/agent/contract.rs` (`reprompt_config`)
- Test: testy modułowe obok `contract.rs` i `agent.rs` (wzór: istniejące testy `output_contract` i `reprompt_config`)

**Approach:** Nowe pole bool z `#[serde(skip)]`, domyślnie `false`, z komentarzem według wzoru `permission_grants`. W `run` kontrakt dochodzi do promptu tylko przy `false`. W `reprompt_config` pole dostaje `false` po `continue_session`. Pozostałe miejsca, które tworzą `HarnessStepConfig` literałem, dostają wartość domyślną (kompilator wskaże je).

**Patterns to follow:** `permission_grants` w `HarnessStepConfig`. Testy `reprompt_config` w `contract.rs`.

**Test scenarios:**
- Covers AE2. Krok bez znacznika i bez `resume_session`: prompt kończy się pełnym kontraktem.
- Covers AE4. Krok z `resume_session` z workflow i bez znacznika: prompt zawiera kontrakt nowego kroku.
- Covers AE1. Krok ze znacznikiem: prompt równa się tekstowi operatora, bez „When you are done”.
- Covers AE3. `reprompt_config` z akcji, która miała znacznik: wynikowa konfiguracja ma znacznik wyłączony, a prompt wykonawcy zawiera kontrakt.
- Conductor reprompt (`conductor_reprompt_config`): prompt zawiera kontrakt.
- Znacznik nie trafia do serializacji `HarnessStepConfig` i nie wpływa na odczyt zapisu starszego wykonania.
- Rozmowa: element `Prompt` zapisany przy znaczniku nie zawiera kontraktu (R6).

**Verification:** Wszystkie przypadki z listy przechodzą. Istniejące testy kontraktu przechodzą bez zmian.

---

### U2. Ustawienie znacznika przy powrocie operatora

**Goal:** `prepare_harness_attempt` włącza znacznik wtedy, gdy wznawia sesję odpowiedzią operatora.

**Requirements:** R1, R2, R3, R4

**Dependencies:** U1

**Files:**
- Modify: `crates/narsor-engine/src/engine/runner/step/harness_attempt.rs`
- Test: testy runnera tego modułu lub testy integracyjne kroku z odpowiedzią operatora (wzór: istniejące testy `take_agent_input` i `continue_session`)

**Approach:** W gałęzi `Some(harness)` i `Some(session)` po `continue_session` ustaw znacznik. Gałąź bez sesji (tekst „Human answer to your question”) zostaje bez zmian: nowa sesja potrzebuje kontraktu (R2). Nie dotykaj ścieżek reprompt.

**Execution note:** Zacznij od testu, który wysyła odpowiedź operatora i sprawdza prompt przekazany do harnessu.

**Patterns to follow:** Obecna gałąź `continue_session` w `prepare_harness_attempt`.

**Test scenarios:**
- Covers AE1. Krok czeka na odpowiedź, operator odpowiada „zrób wariant B”: prompt do harnessu to „zrób wariant B” bez kontraktu. Odpowiedź z labelem kończy krok.
- Odpowiedź operatora bez zapisanej sesji harnessu: prompt zawiera tekst operatora i kontrakt (nowa sesja).
- Po powrocie operatora odpowiedź agenta bez JSON: engine reprompt zawiera kontrakt (Covers AE3).
- Kolejna próba kroku po błędzie, już bez odpowiedzi operatora, nie dziedziczy znacznika.
- Notatka odzyskiwania (`take_operator_recovery_note`) dopisana do promptu nie przywraca kontraktu i nie psuje znacznika.

**Verification:** Operator widzi w rozmowie samą swoją wiadomość. Krok kończy się labelem tak jak dotąd.

---

## Risks & Dependencies

| Ryzyko | Skutek | Środek |
|---|---|---|
| Harness gubi kontrakt w długiej sesji (nie sprawdzono) | Odpowiedź bez JSON po wznowieniu | Naprawa formatu z kontraktem (R3). Sprawdzić po wdrożeniu liczbę napraw formatu po wznowieniu (kryterium sukcesu z origin) |
| Znacznik przeżywa do próby, która go nie powinna mieć | Brak kontraktu w pierwszym promptcie nowej sesji | Pole `serde(skip)`, zerowanie w `reprompt_config`, test „kolejna próba” w U2 |

---

## Deferred to Implementation

- Dokładna nazwa pola i jego komentarz.
- Czy `claude-code` z `--json-schema` pozwala w przyszłości na prostszą regułę. Poza zakresem tego planu.

---

## Sources & References

- Origin: `docs/brainstorms/2026-11-12-kontrakt-wyjscia-przy-wznowieniu-sesji-requirements.md`
- `crates/narsor-engine/src/engine/actions/agent.rs`
- `crates/narsor-engine/src/engine/actions/agent/contract.rs`
- `crates/narsor-engine/src/engine/runner/step/harness_attempt.rs`
- `crates/narsor-engine/src/engine/actions/session.rs`
