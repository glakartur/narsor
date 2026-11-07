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

- **Objective:** Operator w TUI rozpoznaje jednym spojrzeniem, kto wykonuje krok (agent, dyrygent, człowiek, skrypt) i czy krok wybiera ścieżkę lub czeka na działanie z zewnątrz. Nie musi czytać typu kroku ani otwierać definicji workflow.
- **Means:** Ikona (jedna lub dwie) przed nazwą kroku w widoku Rozmowa i w widoku Przebieg. Ikona pochodzi z definicji workflow: z typu akcji (`type`), z `by:` kroku `decision` i z liczby wyjść (`choices`).
- **Product authority:** Zmiana prezentacji w TUI. Dane o rodzaju kroku muszą dojść z definicji workflow do TUI. Dokument powstał bez dialogu z użytkownikiem (tryb nieinteraktywny). Użytkownik potwierdza założenia i odpowiada na pytania w recenzji MR.
- **Open blockers:** Brak blokujących. Q1, Q2 i Q4 mogą zmienić tabelę ikon (R1–R7), ale nie zmieniają zakresu pracy.

---

## Product Contract

### Summary

Każdy krok w widoku Przebieg (oś kroków i nagłówek kroku) oraz w widoku Rozmowa (pasek kroków i nagłówki w transkrypcie) ma ikonę rodzaju przed nazwą, np. `✨ implement #1`, `⚡🔀 triage #1`, `📜🔀 test #2`. Pierwsza ikona mówi, kto wykonuje krok. Druga ikona (opcjonalna) mówi, że krok wybiera ścieżkę (🔀) albo czeka na działanie z zewnątrz (✋). Ikona pochodzi tylko z definicji workflow, nie ze stanu przebiegu.

### Problem Frame

Dziś TUI pokazuje przy kroku tylko status (`✓`, `✗`, `●`, `?`) i nazwę (`src/tui/task_app/tabs/progress.rs`, `src/tui/task_app/tabs/conversation.rs`). Typ akcji jest widoczny tylko w nagłówku kroku jako tekst (`· agent`). Pasek kroków w Rozmowie i oś w Przebiegu nie mówią, czy krok wykonuje agent, skrypt czy człowiek. Krok `decision` nie pokazuje, kto decyduje (`by: conductor` lub `by: human`), bo TUI dostaje tylko `action_type` jako tekst `"decision"`. Operator musi znać definicję workflow, aby wiedzieć, gdzie zadanie czeka na człowieka.

### Key Decisions

- **Ikona = "kto" + opcjonalnie "co".** Pierwsza ikona to wykonawca kroku (✨ agent, ⚡ dyrygent, 🧑 człowiek, 📜 skrypt). Druga ikona to cecha kroku: 🔀 wybór ścieżki albo ✋ oczekiwanie na działanie z zewnątrz. Najwięcej dwie ikony. Wyjątek: krok `wait` nie ma wykonawcy, więc ma tylko ikonę ✋ (R6). Ten układ daje stałe znaczenie każdej ikony i obejmuje podwójne ikony z prośby (`✨🔀`). Governs R1–R8.
- **Krok wybiera ścieżkę, gdy jego wynik decyduje o następnym kroku.** Krok `decision` zawsze wybiera ścieżkę. Krok `agent` lub `script` wybiera ścieżkę, gdy definicja ma dwa lub więcej wyjść w `choices`. Krok z jednym wyjściem (`exit` albo jedno `choices`) nie dostaje 🔀. Governs R2, R3, R6.
- **Źródło ikony to definicja workflow, której zadanie używa.** TUI nie zgaduje ikony ze statusu ani z nazwy kroku. Status (`✓`, `●`, `?`) zostaje osobnym znakiem przed ikoną. Governs R9, R10.

### Requirements

**Tabela ikon (kształt `schema: v2`, unified)**

- R1. Krok `type: agent` z jednym wyjściem ma ikonę `✨`.
- R2. Krok `type: agent` z dwoma lub więcej wyjściami w `choices` ma ikonę `✨🔀`.
- R3. Krok `type: script` ma ikonę `📜`; z dwoma lub więcej wyjściami w `choices` ma ikonę `📜🔀`.
- R4. Krok `type: decision` z `by: conductor` ma ikonę `⚡🔀`.
- R5. Krok `type: decision` z `by: human` ma ikonę `🧑🔀`.
- R6. Krok `type: wait` ma tylko ikonę `✋`, bez ikony wykonawcy (krok czeka na zdarzenie z zewnątrz, np. działanie człowieka w panelu).

**Kształt starszy (legacy) i przypadki brzegowe**

- R7. Krok w starszym kształcie dostaje ikonę wg tych samych zasad: `type: human` (formularz `inputs`) ma `🧑✋`; blok `decision: { type: agent }` daje drugą ikonę `🔀`; blok `decision: { type: human }` daje `🧑🔀`; `decision: { type: rules }` z kilkoma wyjściami daje `🔀` po ikonie wykonawcy.
- R8. Krok bez znanego rodzaju (np. `action_type: "none"` przy błędnej definicji) nie ma ikony, ale zajmuje pustym miejscem całą szerokość kolumny ikon z R10 (miejsce na dwie ikony), aby nazwy kroków były wyrównane.

**Miejsca w TUI**

- R9. Ikona stoi bezpośrednio przed nazwą kroku, po znaku statusu: `✓ ✨ implement #1`. Dotyczy to: osi kroków w Przebiegu, nagłówka kroku w Przebiegu, paska kroków w Rozmowie i nagłówka kroku w transkrypcie Rozmowy (`── ✨ implement #1 ──`).
- R10. Kolumna ikon w osi Przebiegu ma stałą szerokość (miejsce na dwie ikony). Nazwy, statusy słowne i kolumny zużycia zostają wyrównane w pionie także wtedy, gdy kroki mają jedną i dwie ikony.
- R11. Pasek kroków w Rozmowie liczy szerokość chipa z ikoną. Wybrany krok zostaje widoczny na wąskim ekranie (zachowanie testu `a_narrow_strip_keeps_the_selected_step_in_view`).
- R12. Nagłówek kroku w Przebiegu nadal pokazuje typ akcji tekstem (`· agent`); dla `decision` pokazuje też, kto decyduje (`· decision · conductor`).

**Dane**

- R13. TUI dostaje dla każdego kroku dane potrzebne do R1–R8 (typ akcji, `by`, czy krok wybiera ścieżkę) z definicji workflow, której zadanie używa. Ikona kroku nie zmienia się w trakcie przebiegu.

### Key Flows

- F1. **Operator śledzi zadanie z bramką człowieka.**
  **Trigger:** zadanie z `workflows/template.yaml` dochodzi do kroku `review`.
  **Steps:** oś Przebiegu pokazuje `✓ 📜 demonstrate #1`, potem `? 🧑🔀 review #1`. Pasek Rozmowy pokazuje te same ikony.
  **Outcome:** operator widzi bez otwierania definicji, że zadanie czeka na wybór człowieka.

### Acceptance Examples

- AE1. **Covers R3, R4, R5, R1.** Workflow `tests/fixtures/v2/decision_conductor.yaml`: `test` → `📜🔀 test`, `triage` → `⚡🔀 triage`, `work` → `✨ work`, `approval` → `🧑🔀 approval`.
- AE2. **Covers R10.** Oś Przebiegu z krokami `✨ work` i `⚡🔀 triage`: pierwsza litera nazwy `work` i `triage` jest w tej samej kolumnie terminala.
- AE3. **Covers R8.** Zadanie z błędną definicją (`action_type: "none"`): krok nie ma ikony, nazwa jest w tej samej kolumnie co nazwy innych kroków.

### Scope Boundaries

- Brak ikon w liście zadań i w nagłówku zadania (status `krok N nazwa`). Można dodać później.
- Brak ikon w CLI (`narsor task ...`). Tylko TUI.
- Brak konfiguracji ikon w workflow ani w ustawieniach użytkownika.
- Brak zastępczych znaków ASCII dla terminali bez emoji (patrz Q3).
- Brak ikony dla stanu końcowego (`completed`, `cancelled`). Stan końcowy nie ma akcji, więc stosuje się R8.

### Dependencies / Assumptions

- Definicja schematu: `crates/narsor-engine/src/workflow/schema.rs` (`ActionSchema`, `DecisionActionSchema`, `DecisionSchema`), `DecisionMaker { Conductor, Human }` w `crates/narsor-engine/src/domain/workflow_definition.rs`.
- Dziś TUI dostaje z serwera tylko `Position.action_type` jako tekst (`crates/narsor-protocol/src/api/timeline.rs`), bez `by` i bez liczby wyjść. R13 wymaga rozszerzenia danych; sposób wybiera planowanie.

### Założenia do potwierdzenia

- Z1. **Układ "kto + co".** Pierwsza ikona to wykonawca, druga to cecha (🔀 lub ✋). Prośba podaje 🧑 i ✋ osobno; założenie: 🧑 to wykonawca (człowiek), ✋ to cecha "krok czeka na działanie z zewnątrz".
- Z2. **`decision` z `by: human` ma `🧑🔀`, nie `✋`.** Krok wybiera ścieżkę, a decyduje człowiek. Przykład z prośby (`✋ review_code`) wskazuje, że może być inaczej — patrz Q1.
- Z3. **`wait` ma `✋`.** Krok `type: wait` czeka na zdarzenie (`event_type`), np. akcję w panelu. Dziś żaden workflow w `workflows/` go nie używa.
- Z4. **Agent i skrypt dostają 🔀 tylko przy dwóch lub więcej wyjściach.** Przykład `✨🔀 automat_switch` z prośby to agent z kilkoma `choices`. Nie ma osobnego typu kroku "switch" (plan schematu v2 go odkłada).
- Z5. **`decision` z `by: conductor` ma `⚡🔀`, a nie samo `⚡`.** Daje to spójność z `🧑🔀`: każdy krok `decision` ma 🔀.
- Z6. **Starszy kształt (`type: human`, blok `decision:`) dostaje ikony wg tych samych zasad (R7).** Schemat v2 w kształcie unified go odrzuca, ale starsze pliki nadal się ładują.
- Z7. **Status zostaje pierwszym znakiem.** Kolejność: status, ikona, nazwa (`✓ ✨ implement #1`). Prośba mówi "ikona przed nazwą kroku"; ta kolejność jej nie narusza.
- Z8. **Terminal operatora pokazuje emoji o szerokości dwóch kolumn.** Wyrównanie (R10) liczy szerokość wg szerokości wyświetlania znaków, nie wg liczby bajtów.

### Pytania otwarte dla użytkownika

- Q1. **[Deferred to Planning]** Krok `decision` z `by: human`: `🧑🔀` (propozycja), `✋` (jak w przykładzie `✋ review_code`), czy `🧑✋`?
- Q2. **[Deferred to Planning]** Krok `decision` z `by: conductor`: `⚡🔀` (propozycja) czy samo `⚡`?
- Q3. **[Deferred to Planning]** Czy potrzebny jest tryb bez emoji (np. znaki ASCII `A`, `C`, `H`, `S`, `>`, `!`) dla terminali, które źle pokazują emoji?
- Q4. **[Deferred to Planning]** Czy krok `wait` ma mieć `✋`, czy inną ikonę (np. `⏳`), gdy czeka na zdarzenie systemowe, a nie na człowieka?
- Q5. **[Deferred to Planning]** Czy ikony mają być też w liście zadań i w nagłówku zadania (`krok N nazwa`)?

## Deferred / Open Questions

Te pozycje to wyniki recenzji pozostawione autorowi. Nie blokują implementacji.

### From 2026-11-07 review

- **Ikona ✋ nie oznacza spójnie bramki człowieka** — Key Decisions / R5–R7, tabela ikon dla kroków człowieka i `wait` (P2, adversarial, feasibility, confidence 75)

  Operator, który szuka ✋, nie zobaczy jedynej bramki człowieka w `workflows/template.yaml` (krok `review`, `decision` z `by: human` ma `🧑🔀`). Ten sam rodzaj bramki ma inną ikonę zależnie od kształtu pliku: starszy `type: human` z kilkoma `choices` (np. `human_clarification` w `tests/fixtures/implement_feature.yaml`) ma `🧑✋`, a loader kształtu unified wskazuje jako jego odpowiednik `decision` z `by: human` (`🧑🔀`). ✋ ma też krok `wait`, który może czekać na zdarzenie systemowe (Q4). Trzeba zdecydować razem Q1 i Q4: czy ✋ znaczy „musi działać człowiek”, czy „krok czeka na zdarzenie z zewnątrz”.
  Change: Rozstrzygnąć Q1 i Q4 w Product Contract. Zalecenie: starszy `type: human` ma tę samą ikonę co krok R5 (`decision` z `by: human`), a znaczenie ✋ ma być jedno dla wszystkich bramek człowieka i dla `wait`.

- **Dyrygent przekazuje decyzję operatorowi, a ikona dalej pokazuje dyrygenta** — R4 (`decision` z `by: conductor`, `⚡🔀`) i R13 (ikona nie zmienia się w trakcie przebiegu) (P3, coherence, confidence 75)

  Dyrygent, który nie jest pewny decyzji, przekazuje krok operatorowi. Wtedy operator musi działać, ale ikona `⚡🔀` dalej mówi, że decyduje dyrygent. Dokument nie mówi, czy to jest akceptowane.
  Change: Zalecenie: zostawić ikonę stałą (R13) i zapisać w założeniach, że oczekiwanie na operatora pokazuje znak statusu `?`, a nie ikona.
