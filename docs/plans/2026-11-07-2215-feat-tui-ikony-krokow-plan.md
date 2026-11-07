---
title: Ikony rodzaju kroku w widokach Rozmowa i Przebieg - Plan
type: feat
date: 2026-11-07
topic: tui-ikony-krokow
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Ikony rodzaju kroku w widokach Rozmowa i Przebieg - Plan

## Goal Capsule

- **Objective:** Operator w TUI rozpoznaje jednym spojrzeniem, kto wykonuje krok (agent, dyrygent, człowiek, skrypt) i czy krok jest decyzją albo czeka na zdarzenie. Nie musi czytać typu kroku ani otwierać definicji workflow.
- **Means:** Ikona (jedna lub dwie) przed nazwą kroku w widoku Rozmowa i w widoku Przebieg. Ikona pochodzi z definicji workflow: z typu akcji (`type`) i z `by:` kroku `decision`. Liczba wyjść nie wpływa na ikonę.
- **Product authority:** Zmiana prezentacji w TUI. Dane o rodzaju kroku muszą dojść z definicji workflow do TUI. Dokument powstał bez dialogu z użytkownikiem (tryb nieinteraktywny). Użytkownik potwierdza założenia i odpowiada na pytania w recenzji MR.
- **Open blockers:** Brak blokujących. Pytania Q1–Q5 mają odpowiedzi (patrz niżej).

---

## Product Contract

### Summary

Każdy krok w widoku Przebieg (oś kroków i nagłówek kroku) oraz w widoku Rozmowa (pasek kroków i nagłówki w transkrypcie) ma ikonę rodzaju przed nazwą, np. `✨ implement #1`, `⚡🔀 triage #1`, `📜 test #2`. Ikona to część wyświetlanej nazwy kroku. Pierwsza ikona mówi, kto wykonuje krok. Druga ikona (🔀) jest tylko przy kroku `decision`. Krok `wait` ma tylko ✋. Ikona pochodzi tylko z definicji workflow, nie ze stanu przebiegu.

### Problem Frame

Dziś TUI pokazuje przy kroku tylko status (`✓`, `✗`, `●`, `?`) i nazwę (`src/tui/task_app/tabs/progress.rs`, `src/tui/task_app/tabs/conversation.rs`). Typ akcji jest widoczny tylko w nagłówku kroku jako tekst (`· agent`). Pasek kroków w Rozmowie i oś w Przebiegu nie mówią, czy krok wykonuje agent, skrypt czy człowiek. Krok `decision` nie pokazuje, kto decyduje (`by: conductor` lub `by: human`), bo TUI dostaje tylko `action_type` jako tekst `"decision"`. Operator musi znać definicję workflow, aby wiedzieć, gdzie zadanie czeka na człowieka.

### Key Decisions

- **Ikona = "kto" + opcjonalnie "co".** Pierwsza ikona to wykonawca kroku (✨ agent, ⚡ dyrygent, 🧑 człowiek, 📜 skrypt). Druga ikona to 🔀 (krok `decision`). Najwięcej dwie ikony. Wyjątek: krok `wait` nie ma wykonawcy, więc ma tylko ikonę ✋ (R4). Governs R1–R5.
- **🔀 ma tylko krok `decision`.** Nie badamy liczby wyjść kroku. Agent i skrypt mają zawsze jedną ikonę. Governs R1–R3.
- **Źródło ikony to definicja workflow, której zadanie używa.** TUI nie zgaduje ikony ze statusu ani z nazwy kroku. Status (`✓`, `●`, `?`) zostaje osobnym znakiem przed ikoną. Governs R6–R8.

### Requirements

**Tabela ikon**

- R1. Krok `type: agent` ma ikonę `✨`, niezależnie od liczby wyjść.
- R2. Krok `type: script` ma ikonę `📜`, niezależnie od liczby wyjść.
- R3. Krok `type: decision` ma ikonę `⚡🔀` przy `by: conductor` i `🧑🔀` przy `by: human`.
- R4. Krok `type: wait` ma tylko ikonę `✋`. Nie wiemy, na co czeka krok, więc nie ma ikony wykonawcy.

**Przypadki brzegowe**

- R5. Starszy kształt definicji (`type: human`, blok `decision:`) nie jest wspierany. Nie dostaje ikon.
- R6. Krok bez znanego rodzaju (np. `action_type: "none"`) nie ma ikony. TUI pokazuje samą nazwę, bez wcięcia na ikonę. Nazwy nie muszą być wyrównane.

**Miejsca w TUI**

- R7. Ikona jest częścią wyświetlanej nazwy kroku i stoi bezpośrednio przed nią, po znaku statusu: `✓ ✨ implement #1`. Nie ma osobnej kolumny ikon. Dotyczy to: osi kroków w Przebiegu, nagłówka kroku w Przebiegu, paska kroków w Rozmowie i nagłówka kroku w transkrypcie Rozmowy (`── ✨ implement #1 ──`).
- R8. Pasek kroków w Rozmowie liczy szerokość chipa z ikoną. Wybrany krok zostaje widoczny na wąskim ekranie (zachowanie testu `a_narrow_strip_keeps_the_selected_step_in_view`).
- R9. Nagłówek kroku w Przebiegu nadal pokazuje typ akcji tekstem (`· agent`); dla `decision` pokazuje też, kto decyduje (`· decision · conductor`).
- R10. Lista zadań i nagłówek zadania pokazują ikonę w nazwie kroku: `krok N · ✨ nazwa`, gdzie `·` to znak rozdzielający.

**Dane**

- R11. TUI dostaje dla każdego kroku dane potrzebne do R1–R6 (typ akcji, `by`) z definicji workflow, której zadanie używa. Ikona kroku nie zmienia się w trakcie przebiegu.

### Key Flows

- F1. **Operator śledzi zadanie z bramką człowieka.**
  **Trigger:** zadanie z `workflows/template.yaml` dochodzi do kroku `review`.
  **Steps:** oś Przebiegu pokazuje `✓ 📜 demonstrate #1`, potem `? 🧑🔀 review #1`. Pasek Rozmowy pokazuje te same ikony.
  **Outcome:** operator widzi bez otwierania definicji, że zadanie czeka na wybór człowieka.

### Acceptance Examples

- AE1. **Covers R1, R2, R3.** Workflow `tests/fixtures/v2/decision_conductor.yaml`: `test` → `📜 test`, `triage` → `⚡🔀 triage`, `work` → `✨ work`, `approval` → `🧑🔀 approval`.
- AE2. **Covers R1, R2.** Krok `agent` lub `script` z dwoma wyjściami w `choices` ma tylko `✨` albo `📜`, bez 🔀.
- AE3. **Covers R6.** Zadanie z błędną definicją (`action_type: "none"`): krok pokazuje samą nazwę, bez ikony i bez wcięcia.

### Scope Boundaries

- Brak ikon w CLI (`narsor task ...`). Tylko TUI.
- Brak konfiguracji ikon w workflow ani w ustawieniach użytkownika.
- Brak trybu bez emoji i zastępczych znaków ASCII.
- Brak wsparcia dla starszego kształtu definicji workflow.
- Brak ikony dla stanu końcowego (`completed`, `cancelled`). Stan końcowy nie ma akcji, więc stosuje się R6.

### Dependencies / Assumptions

- Definicja schematu: `crates/narsor-engine/src/workflow/schema.rs` (`ActionSchema`, `DecisionActionSchema`, `DecisionSchema`), `DecisionMaker { Conductor, Human }` w `crates/narsor-engine/src/domain/workflow_definition.rs`.
- Dziś TUI dostaje z serwera tylko `Position.action_type` jako tekst (`crates/narsor-protocol/src/api/timeline.rs`), bez `by`. R11 wymaga rozszerzenia danych; sposób wybiera planowanie.

### Założenia

- Z1. **Układ "kto + co".** Pierwsza ikona to wykonawca, druga to 🔀 (tylko `decision`). ✋ ma tylko krok `wait`.
- Z2. **Liczba wyjść nie wpływa na ikonę.** Agent to zawsze `✨`, skrypt to zawsze `📜`.
- Z3. **Starszy kształt definicji nie jest wspierany.** Nie ma dla niego ikon.
- Z4. **Ikona jest częścią nazwy.** Nie ma kolumny ikon ani wyrównania nazw.
- Z5. **Status zostaje pierwszym znakiem.** Kolejność: status, ikona, nazwa (`✓ ✨ implement #1`).
- Z6. **Nie ma trybu bez emoji.**

### Odpowiedzi użytkownika z recenzji MR

- Q1. `decision` z `by: human` ma `🧑🔀`.
- Q2. `decision` z `by: conductor` ma `⚡🔀`.
- Q3. Nie robimy trybu bez emoji.
- Q4. `wait` ma `✋`: nie wiemy, na co czeka krok.
- Q5. Tak: ikony są też w liście zadań i w nagłówku zadania, w formie `krok N · ikona nazwa`.

## Deferred / Open Questions

Te pozycje to wyniki recenzji pozostawione autorowi. Nie blokują implementacji.

### From 2026-11-07 review

- **Dyrygent przekazuje decyzję operatorowi, a ikona dalej pokazuje dyrygenta** — R3 (`decision` z `by: conductor`, `⚡🔀`) i R11 (ikona nie zmienia się w trakcie przebiegu) (P3, coherence, confidence 75)

  Dyrygent, który nie jest pewny decyzji, przekazuje krok operatorowi. Wtedy operator musi działać, ale ikona `⚡🔀` dalej mówi, że decyduje dyrygent. Dokument nie mówi, czy to jest akceptowane.
  Change: Zalecenie: zostawić ikonę stałą (R11) i zapisać w założeniach, że oczekiwanie na operatora pokazuje znak statusu `?`, a nie ikona.
