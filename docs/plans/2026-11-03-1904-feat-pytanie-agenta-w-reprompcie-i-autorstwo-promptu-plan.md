---
title: Pytanie agenta w repromptcie i autorstwo promptu - Plan
type: feat
date: 2026-11-03
topic: pytanie-agenta-w-reprompcie-i-autorstwo-promptu
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Pytanie agenta w repromptcie i autorstwo promptu - Plan

## Goal Capsule

- **Objective:** Gdy agent zamiast etykiety zadaje pytanie, pytanie trafia do właściwego adresata i może zakończyć się dialogiem, a w rozmowie widać, kto wysłał każdy prompt.
- **Product authority:** Decyzje użytkownika z dialogu brainstormu; ten plik jest ich zapisem.
- **Open blockers:** Brak.

## Product Contract

### Summary

Pytanie agenta zadane w repromptcie przestaje być odrzuceniem odpowiedzi i staje się zwykłym pytaniem agenta. Zmienna kontekstu `autopilot` (domyślnie `off`) wybiera adresata: `off` to człowiek od razu, `on` to najpierw dyrygent, a w ostateczności człowiek. Każdy prompt w rozmowie ma autora: silnik, dyrygent, użytkownik albo silnik+użytkownik.

### Problem Frame

Reprompt silnika mówi agentowi, żeby nie kontynuował pracy i odpowiedział samym JSON-em. Jeśli agent w tym miejscu zapyta o coś, czego potrzebuje, silnik traktuje to jako błąd formatu i oddaje krok dyrygentowi. Człowiek nie dostaje pytania jako pytania, a dyrygent nie wie, że ostatnia odpowiedź była pytaniem.

Rozmowa kroku pokazuje każdy prompt Narsora pod jednym podpisem "Narsor". Prośbę silnika, prośbę dyrygenta i prompt z treścią użytkownika widać tak samo; różnicę widać dopiero w zakładce *Zdarzenia*.

### Requirements

**Pytanie agenta w repromptcie**

- R1. Odpowiedź `awaiting_input` w repromptcie, zarówno silnika, jak i dyrygenta, jest traktowana jako pytanie agenta, a nie jako błąd formatu.
- R2. Reprompt zawiera automatycznie dołączony opis `awaiting_input`, tak by agent wiedział, jak go użyć, i nie zakazuje zadania pytania.
- R3. Pytanie z reprompta nie uruchamia odzyskiwania wyjścia przez dyrygenta, o ile adresatem nie jest dyrygent według R5.
- R4. Odpowiedź na pytanie z reprompta wznawia agenta jak odpowiedź na zwykłe `awaiting_input`: z normalnymi uprawnieniami stanu i bez zakazu pracy.

**Autopilot**

- R5. Zmienna kontekstu `autopilot` decyduje o adresacie pytania z R1. Przy `off` pytanie trafia od razu do człowieka. Przy `on` dyrygent próbuje odpowiedzieć, a gdy nie potrafi, przekazuje pytanie człowiekowi.
- R6. Gdy `autopilot` nie jest ustawiony, działa jak `off`.
- R7. Workflow ustawia `autopilot` krokami, a operator może go zmienić w trakcie zadania. Zmiana działa od następnego pytania.
- R8. Dyrygent wywołany przy `on` wie, że ostatnia odpowiedź agenta była pytaniem, i może je przekazać człowiekowi.
- R9. `autopilot` dotyczy wyłącznie pytań agenta. Odzyskiwanie braku poprawnego JSON-a przez dyrygenta działa niezależnie od niego.

**Autorstwo promptu w rozmowie**

- R10. Każdy prompt w rozmowie kroku ma autora: silnik, dyrygent, użytkownik albo silnik+użytkownik. Autor jest widoczny w TUI i w odczycie rozmowy przez CLI.
- R11. Prompt kroku z szablonu workflow ma autora silnik. Ma autora silnik+użytkownik, gdy szablon wstawił treść od użytkownika: `inputs.*` albo odpowiedź człowieka z wcześniejszego kroku.
- R12. Reprompt silnika ma autora silnik. Reprompt i odpowiedź dyrygenta mają autora dyrygent.
- R13. Wiadomość i odpowiedź operatora mają autora użytkownik.
- R14. Zapisy rozmów sprzed tej zmiany, bez autora, są pokazywane jak dotąd jako "Narsor".

### Key Flows

- F1. Pytanie z reprompta przy `autopilot: off`
  - **Trigger:** Agent odpowiada na reprompt silnika albo dyrygenta pytaniem `awaiting_input`.
  - **Steps:** Krok przechodzi w `input_needed` z pytaniem agenta. Człowiek odpowiada. Agent wznawia pracę z normalnymi uprawnieniami (R4).
  - **Covered by:** R1, R3, R4, R5
- F2. Pytanie z reprompta przy `autopilot: on`
  - **Trigger:** Jak w F1.
  - **Steps:** Dyrygent dostaje pytanie wraz z informacją, że to pytanie. Odpowiada sam albo przekazuje je człowiekowi jak w F1. Odpowiedź dyrygenta wznawia agenta (R4) i w rozmowie ma autora dyrygent.
  - **Covered by:** R5, R8, R12

### Acceptance Examples

- AE1. **Covers R1, R3, R5.** Gdy `autopilot` jest nieustawiony, a agent zapyta w repromptcie, krok czeka na człowieka i dyrygent nie jest wywoływany.
- AE2. **Covers R5, R8.** Gdy `autopilot: on` i dyrygent nie potrafi odpowiedzieć, człowiek dostaje pytanie agenta, a nie problem `ResolveOutput`.
- AE3. **Covers R7.** Gdy operator zmieni `autopilot` na `off`, gdy pytanie już czeka u dyrygenta, to pytanie nie zmienia adresata; zmiana dotyczy następnego pytania.
- AE4. **Covers R11.** Prompt kroku, którego szablon zawiera `{{ inputs.description }}`, jest pokazany jako silnik+użytkownik, a prompt bez takiej treści jako silnik.
- AE5. **Covers R14.** Rozmowa zapisana przed zmianą nadal się wyświetla, a jej prompty mają podpis "Narsor".

### Key Decisions

- **Adresat pytania zależy od `autopilot`, domyślnie człowiek.** Dyrygent odpowiadający na każde pytanie zwiększałby koszt zadania. Governs R5, R6.
- **`autopilot` jest zmienną kontekstu, nie inputem.** Workflow ustawia ją krokami, a operator zmienia w trakcie zadania. Governs R7.
- **Po odpowiedzi agent kontynuuje pracę z normalnymi uprawnieniami.** Pytanie oznacza, że praca nie jest skończona, więc tryb tylko do odczytu byłby sprzeczny. Governs R4.
- **Autor prompta wynika ze źródła treści, nie z miejsca wysłania.** Silnik+użytkownik oznacza prompt z szablonu workflow zawierający treść użytkownika. Governs R10, R11, R12, R13.

### Scope Boundaries

**Deferred for later**

- Osobne ustawienia `autopilot` per krok albo per workflow w definicji, poza zmienną kontekstu.
- Limit kosztu lub liczby pytań, na które dyrygent odpowiada sam.

**Poza zakresem**

- Zmiana odzyskiwania braku JSON-a przez dyrygenta (R9).
- Zmiana kontraktu wyjścia agenta poza dołączeniem opisu `awaiting_input` do reprompta (R2).

### Dependencies / Assumptions

- Kontrakt wyjścia z opisem `awaiting_input` jest już dołączany do promptu, także do reprompta (`crates/narsor-engine/src/engine/actions/agent/contract.rs:93`). Zmienia się jego traktowanie w repromptcie, nie sam opis.
- Dziś pytanie z reprompta jest odrzuceniem (`crates/narsor-engine/src/engine/runner.rs:432`, `OutputRejection::from_question`) i prowadzi do `rescue_output` (`crates/narsor-engine/src/engine/runner/rescue.rs`).
- Prompt w rozmowie to `ChatElement::Prompt` z `PromptPurpose` (`crates/narsor-protocol/src/chat.rs:156`), pokazywany jako "Narsor" (`src/tui/conversation.rs:226`).
- Zakładam, że wyrażenie "odpowiedź człowieka z wcześniejszego kroku" da się rozpoznać w renderowanym szablonie (R11); sposób rozpoznania wyznacza planowanie.

### Outstanding Questions

**Deferred to Planning**

- Jak rozpoznać, że szablon wstawił treść użytkownika (R11), i gdzie przechować autora prompta w zapisie rozmowy (R10, R14).
- Jak dyrygent odsyła pytanie człowiekowi (R8) w istniejącym modelu decyzji dyrygenta.
- Jak operator zmienia `autopilot` w trakcie zadania (TUI i CLI) (R7).

## Planning Contract

**Product Contract preservation:** Product Contract unchanged.

### Key Technical Decisions

- **KTD1. Pytanie z reprompta zawiesza krok jak zwykłe `awaiting_input`.** W `reprompt_attempt` (`runner.rs`) `SuspendedAction::AwaitingAgentInput` nie jest już `from_question`, tylko `RepromptEnd::Asked(request)`. Wywołujący zawiesza krok przez `suspend_for_agent_input` (`InputContinuation::ResumeAgent`), czyści `OutputRescue`/`OutputRejection` i wznawia agenta istniejącą ścieżką `store_agent_input`. Governs R1, R3, R4.
- **KTD2. Reprompt przestaje zakazywać pytań.** Treść `OUTPUT_REPROMPT` i `CONDUCTOR_REPROMPT_GUARD` (`contract.rs`) dopuszcza `awaiting_input`; opis `awaiting_input` jest już w `output_contract`. Governs R2.
- **KTD3. `autopilot` jest czytany z `context.variables` w chwili pytania.** Wartość `on`/`true` (bez rozróżniania wielkości liter) włącza dyrygenta; brak lub inna wartość to `off`. Pytanie już zawieszone zachowuje adresata (AE3). Governs R5–R7.
- **KTD4. Dyrygent dostaje nowy cel `AnswerAgentQuestion`** (`ConductorPurpose` w `narsor-protocol/src/event.rs`) z decyzjami `Reply` (odpowiedź, wznawia agenta jako `AnsweredBy::Conductor`) i `Escalate` (przekazanie pytania człowiekowi jako zwykłego `input_needed` agenta, nie `ResolveOutput`). Governs R5, R8, R12, AE2.
- **KTD5. Autor prompta to pole `author` w `ChatElement::Prompt`** (`serde(default)`, `Option`), więc stare zapisy się wczytują, a brak autora wyświetla się jako "Narsor". Governs R10, R14.
- **KTD6. Autor `engine+user` wynika z użycia `inputs.*` w szablonie promptu kroku** albo odpowiedzi człowieka (`agent_message`/odpowiedź wstawiona do kontekstu); rozpoznanie na surowym szablonie, przed renderowaniem. Governs R11.

### Implementation Units

- U1. **Autor prompta w protokole i rozmowie**
  - **Goal:** `PromptAuthor { Engine, EngineUser, Conductor, User }` w `ChatElement::Prompt`.
  - **Files:** `crates/narsor-protocol/src/chat.rs`, `crates/narsor-engine/src/engine/actions/agent.rs`, `crates/narsor-engine/src/engine/actions/mod.rs`
  - **Tests:** serializacja z i bez pola `author` (zgodność wsteczna).
  - **Covers:** R10–R14
- U2. **Wyznaczanie autora i etykiety w TUI/CLI**
  - **Files:** `src/tui/conversation.rs`, `src/tui/event_text.rs`, odczyt rozmowy w `src/cli/`
  - **Tests:** podpisy dla czterech autorów i dla zapisu bez autora.
  - **Covers:** R10, R14, AE4, AE5
- U3. **Pytanie w repromptcie zawiesza krok**
  - **Files:** `crates/narsor-engine/src/engine/runner.rs`, `crates/narsor-engine/src/engine/actions/agent/contract.rs`
  - **Tests:** reprompt silnika odpowiada `awaiting_input` → krok `input_needed`, dyrygent niewywołany (AE1); odpowiedź człowieka wznawia agenta.
  - **Covers:** R1–R4, R6
- U4. **`autopilot: on` — dyrygent odpowiada lub przekazuje**
  - **Files:** `crates/narsor-protocol/src/event.rs`, `crates/narsor-protocol/src/api.rs`, `crates/narsor-engine/src/conductor/{prompt,response,rescue}.rs`, `crates/narsor-engine/src/engine/runner/rescue.rs`
  - **Tests:** dyrygent `Reply` wznawia agenta; `Escalate` daje pytanie agenta, nie `ResolveOutput` (AE2).
  - **Covers:** R5, R8, R9, F2
- U5. **Zmiana `autopilot` przez operatora w trakcie zadania**
  - **Files:** istniejący kanał poleceń operatora (`narsor-engine/src/orchestration`, `narsor-runner/src/runner/commands.rs`), TUI/CLI
  - **Covers:** R7, AE3
  - **Uwaga:** jeśli wymaga nowego polecenia przez trzy crate'y, wykonać jako ostatnia jednostka i ocenić koszt w trakcie.
- U6. **Dokumentacja**
  - **Files:** `docs/dev/workflow-schema.md`, `docs/user/agents.md`, `docs/user/conductor.md`, `docs/user/tui.md`, `docs/user/cli.md`

### Verification Contract

`cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --check`.

### Definition of Done

AE1–AE5 pokryte testami (AE3 w zakresie, w jakim U5 jest wykonane), dokumentacja zaktualizowana, brak regresji w odzyskiwaniu braku JSON-a (R9).
