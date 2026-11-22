---
title: Unify the step text field to message - Plan
type: feat
date: 2026-11-22
origin: docs/brainstorms/2026-11-22-unify-step-message-field-requirements.md
artifact_contract: ce-unified-plan/v1
product_contract_source: legacy-requirements
execution: code
---

# Unify the step text field to message - Plan

---

## Goal Capsule

- **Objective:** A workflow author writes `message:` for the step text on every step type that has one, and every workflow that loads today still loads and runs the same way.
- **Means:** a serde alias `prompt` on the canonical `message` key of each in-scope schema struct, plus a raw-YAML check per state that rejects both keys on one step (KTD1, KTD2).
- **Authority:** the origin requirements (R1–R9) win on product behavior. KTDs win on mechanism. Units override neither.
- **Stop conditions:** stop and report if a sealed launch spec turns out to store workflow YAML (not the compiled JSON definition), or if a serde alias on a field of an internally tagged enum variant does not behave as KTD1 expects.
- **Execution profile:** one pull request. All changes sit in `crates/narsor-model`, docs and the bundled workflows.
- **Who finishes:** the implementer runs `cargo test --workspace` and opens the PR.

---

## Product Contract

Product Contract preservation: unchanged. R4 in the origin now names the canonical `message` (Q1, option a).

### Summary

The loader accepts `message` as the step-text key on `agent`, `profile`, `decision` and `human` actions and on the legacy `decision: { type: agent | human }` blocks. `prompt` stays accepted on all of them as an alias during a transition period with no end date. A step with both keys is a load error. Docs, the template and the bundled workflows switch to `message`.

### Problem Frame

Authors must remember that `agent` and `profile` take `prompt` while `decision` and `human` take `message`. Every action schema has `deny_unknown_fields`, so the wrong key is a load error. The two fields have the same role (see origin: Problem Frame).

### Actors

- A1. Workflow author: writes and edits workflow YAML.
- A2. Workflow loader and validator (`narsor-model`).
- A3. Operator of existing tasks: resumes tasks sealed with the old keys.

### Key Flows

- F1. A1 writes `message:` on a step of any in-scope type. A2 loads the file without error.
- F2. A2 loads a workflow (new task or resumed sealed task) that uses `prompt:` on an `agent` or `profile` step. The step loads and behaves as before.
- F3. A1 writes both keys on one step. A2 rejects the file with an error that names the state and says to keep only `message`.

### Requirements

**Key and alias**

- R1. `message` is the canonical step-text key on `agent`, `profile`, `decision` and `human` actions, and in the legacy `decision: { type: agent }` and `decision: { type: human }` blocks.
- R2. During the transition period, `prompt` is an alias of `message` on each of these, with the same type and meaning. A step written with `prompt` loads and runs exactly as one written with `message`.
- R3. A step that has both `message` and `prompt` is a load error. The error names the state, says the keys are aliases, and says only `message` must stay. No precedence rule applies.

**Errors and field rules**

- R4. Every validation and template error that names the step-text field names the canonical key `message`, with the state and location prefix that the loader and renderer already add. A part path reads `message[2].when`.
- R5. Required/optional status, blank-text checks, `PromptText` parts and `when`, the `steps.ID.*` reference rules and every other rule of the field stay the same. Only the key name changes.

**Compatibility**

- R6. Sealed workflows of existing tasks that use `prompt` keep loading and resuming. No migration of stored data.
- R7. The workflow `schema` version and the file `version` do not change.

**Docs and tests**

- R8. `docs/dev/workflow-schema.md`, `docs/user/workflows.md`, `docs/user/agents.md`, `workflows/template.yaml`, `workflows/compound-engineering-change.yaml` and the other workflow examples in `docs/` use `message`. The schema document says `prompt` is a deprecated alias accepted during the transition period, and that a step must not have both.
- R9. One table-driven test proves `prompt` and `message` give the same loaded step for an `agent`/`profile` step and for a `decision`/`human` step. One test proves R3. Most existing fixtures keep `prompt` and so also cover the alias.

### Key Decisions

- **All six step-text sites are in scope** (origin Z1). Governs R1.
- **The alias works in both directions** (origin Z2): `prompt` is also accepted on `decision` and `human`. Governs R2.
- **Both keys on one step is an error, not a precedence rule** (origin Z3). Governs R3.
- **No end date for the alias in this work** (origin Z4). Governs R2.
- **Deprecation is documented only, no load diagnostic** (origin Z5). Governs R8. See Q2: the premise of Z5 is not correct, because a `LoadDiagnostic` channel exists.
- **Internal Rust field names do not change** (origin Z6). Governs R5.
- **Docs, template and bundled workflows move to `message`; test fixtures stay** (origin Z7). Governs R8, R9.
- **Error messages name the canonical key** (origin Z8, decided in Q1). Governs R4.

### Success Criteria

- An author can write `message:` on every step type that has a step text, and the file loads.
- Every workflow that loads today loads with the same result after the change.
- `cargo test --workspace` passes.

### Scope Boundaries

- Internal Rust identifiers (`AgentActionConfig.prompt`, `HumanActionConfig.instruction`, `DecisionActionConfig.message`, `AgentDecisionConfig.prompt`) do not change.
- The word "prompt" elsewhere does not change: harness protocol payloads, the conductor `Reprompt { prompt }` decision, `{{ step.history }}`, and the prose term "prompt text" in §3.7.6.
- Removing the `prompt` alias is not part of this work.
- `script` steps have no step text and are out of scope.
- Mass rewrite of test fixtures from `prompt` to `message` is not required.
- Considered and not built: storing the written key in the compiled `State` so that every error can echo it (Q1, option b). It touches the sealed snapshot format and the signature of `TemplateScope::render_action`. The person chose option a.
- Considered and not built: a `LoadDiagnostic` for `prompt` (Q2). Evidence that would change the call: the person asks for it.

### Pytania otwarte dla użytkownika

Obie decyzje są podjęte: Q1 i Q2, opcja 1. Nic nie blokuje scalenia.

- Q1. **R4 / Z8 (sekcja Requirements, Errors and field rules): czy błędy mają powtarzać klucz, który napisał autor?** Alias serde jest rozwiązywany podczas parsowania, więc skompilowany model (`WorkflowDefinition`) zna tylko jedno pole. Walidator działa na tym modelu w kilku miejscach (ładowanie katalogu, tworzenie zadania, wznowienie zadania z zapieczętowanej migawki), a renderer szablonów działa w czasie wykonania.
  1. (Wybrane przez osobę, zaplanowane w U3) Wszystkie błędy walidacji i renderowania podają kanoniczny klucz `message`, np. `message[2].when`. Błąd R3 (oba klucze) podaje oba klucze. Najmniej zmian, bez zmiany formatu migawki.
- Q2. **Z5 (sekcja Key Decisions): czy loader ma zgłaszać ostrzeżenie o przestarzałym kluczu `prompt`?** Założenie Z5 mówi, że loader nie ma kanału ostrzeżeń. To nie jest prawda: `LoadDiagnostic` w `crates/narsor-model/src/workflow/loader.rs` już istnieje (np. `DeprecatedStateShape`), a `load_from_file` i `compile` zapisują diagnostyki do logu.
  1. (Wybrane przez osobę, zaplanowane) Bez ostrzeżenia w tej zmianie; deprecjacja tylko w dokumentacji, zgodnie z Z5. Ostrzeżenie można dodać razem z decyzją o usunięciu aliasu (Z4).
  2. Dodać jeden wariant `LoadDiagnostic` (jedno ostrzeżenie na workflow, który używa `prompt`). Mała zmiana w `narsor-model`, ale wiele testowych fixture'ów z `prompt` też będzie go generować.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **A serde `alias = "prompt"` on a field renamed to `message` in each schema struct.** `AgentActionSchema`, `ProfileActionSchema` and `AgentDecisionSchema` rename their field to `message` with alias `prompt`. `DecisionActionSchema`, `HumanActionSchema` and `HumanDecisionSchema` add alias `prompt` to `message`. With `deny_unknown_fields`, serde accepts either key, and a missing required key reports ``missing field `message` `` under the existing `states.<id>:` prefix that `parse_state` adds. The compile step maps the schema field to the unchanged domain field. Rejected: a custom `Deserialize` per struct (more code, same result).
- KTD2. **R3 is a check on the raw YAML of each state, before serde runs.** `parse_state` in `crates/narsor-model/src/workflow/schema.rs` already holds each state as a `serde_yaml::Value` with its id. A small helper looks at the `action` mapping (both shapes) and the legacy `decision` mapping. When the mapping's `type` is an in-scope type and it contains both `message` and `prompt`, it returns `states.<id>.action: 'message' and 'prompt' are aliases of one field; keep only 'message'` (or `states.<id>.decision: …`). Without this check serde would report only ``duplicate field `message` ``, which does not say the keys are aliases. Rejected: relying on the serde duplicate-field error.
- KTD3. **Sealed tasks need no change.** `TaskLaunchSpec.definition` stores the compiled `WorkflowDefinition` as JSON with the internal Rust names (`crates/narsor-model/src/domain/task.rs`). The YAML key never reaches the snapshot, so R6 holds without migration, and the digest of existing specs does not change.
- KTD4. **Validator and renderer labels move to `message` for the agent and profile step text** (R4). In `crates/narsor-model/src/workflow/validator.rs`: the `missing_field(state, "agent"|"profile", "prompt")` calls, the `AgentDecisionMissingField { field: "prompt" }` entry, the `"action.prompt"` and `"decision.prompt"` template locations. In `crates/narsor-model/src/workflow/template.rs`: the `"{location}, prompt"` locations in `render_action`. The step-reply allowlist in the validator already accepts `action.message`, so the `action.prompt` branch can go.
- KTD5. **Docs, template and bundled workflows switch in the same PR.** `crates/narsor-exec/src/workflows/mod.rs` embeds `workflows/template.yaml` with `include_str!`, so the template change ships in the binary. `tests/integration/builtin_workflows.rs` loads the bundled workflows and proves they still load.

### Assumptions

- The serde alias works on fields of structs that are variants of the internally tagged enums `ActionSchema` and `DecisionSchema` (serde buffers the variant content, and alias plus `deny_unknown_fields` is supported there). U1 tests this first; if it fails, the stop condition applies.
- No consumer outside `narsor-model` reads the schema structs' `prompt` field. Repository search found uses only in `crates/narsor-model/src/workflow/loader.rs` and `schema.rs`.
- Tests that assert on error text containing `'prompt'` or `action.prompt` (for example in `validator.rs` unit tests and `tests/integration/validation.rs`) are updated to the new label as part of U3. They test the label, not the alias.

### Sequencing

U1 → U2 → U3 → U4.

---

## Implementation Units

### U1. Accept `message` and `prompt` on every in-scope schema struct

- **Goal:** `message` and `prompt` load to the same compiled step on all six sites.
- **Requirements:** R1, R2, R5, R6, R7; F1, F2.
- **Dependencies:** none.
- **Files:** `crates/narsor-model/src/workflow/schema.rs`, `crates/narsor-model/src/workflow/loader.rs`, `tests/integration/schema_loading.rs`.
- **Approach:**
  1. Apply KTD1 to the six structs; keep `#[serde(default)]` where the field is optional today (legacy `decision` blocks).
  2. Update `compile_action` and `compile_decision` to read the renamed schema field. Domain fields keep their names (Scope Boundaries).
  3. Update the doc comments on the structs to say `message` is canonical and `prompt` is a deprecated alias.
- **Execution note:** write the table-driven alias test first; it also checks the serde-alias assumption.
- **Patterns to follow:** `parse_state` error prefixing; existing `deny_unknown_fields` structs in `schema.rs`.
- **Test scenarios** (one table-driven test; the seam is the public loader, so it catches schema and compile mistakes but not runtime rendering):
  - Covers F1 / F2. Each row loads two otherwise equal workflows, one with `message:` and one with `prompt:`, and asserts the compiled state's action (or decision) config is equal. Rows: `agent` (unified shape), `profile` (unified shape), `decision` with `by: human` (unified shape), `human` (legacy shape), legacy `decision: { type: agent }`, legacy `decision: { type: human }`.
  - One row uses a list of parts with `when` under `prompt:` on a `decision` step, to prove `PromptText` parts work through the alias.
  - An `agent` step with neither key fails with an error that contains `states.<id>` and ``missing field `message` ``.
- **Verification:** the alias test passes; existing fixtures that use `prompt` still pass unchanged.

### U2. Reject a step that has both keys

- **Goal:** a step with `message` and `prompt` fails to load with the R3 error.
- **Requirements:** R3; F3.
- **Dependencies:** U1.
- **Files:** `crates/narsor-model/src/workflow/schema.rs`, `tests/integration/schema_loading.rs`.
- **Approach:** add the KTD2 helper and call it at the start of `parse_state` for both shapes. Limit it to `type` values `agent`, `profile`, `decision`, `human`, so a `script` step with stray keys keeps its normal unknown-field error.
- **Patterns to follow:** `legacy_marker` in `schema.rs` (reads raw state mappings); the error-string style of `check_unified_file`.
- **Test scenarios:**
  - Covers F3. Table-driven: an `agent` action (unified shape) and a legacy `decision: { type: human }` block, each with both keys. The load fails, and the error contains the state id, `'message' and 'prompt'`, and `keep only 'message'`.
- **Verification:** both rows fail with the R3 text, not with ``duplicate field``.

### U3. Name the canonical key in validation and template errors

- **Goal:** every error that names the step-text field says `message` (R4 planned form, KTD4).
- **Requirements:** R4, R5.
- **Dependencies:** U1.
- **Files:** `crates/narsor-model/src/workflow/validator.rs`, `crates/narsor-model/src/workflow/template.rs`, `tests/integration/validation.rs`, `tests/integration/template.rs`.
- **Approach:**
  1. Change the labels listed in KTD4.
  2. Remove the `action.prompt` branch from the step-reply allowlist; keep `action.message`.
  3. Update existing assertions that look for `'prompt'`, `action.prompt` or `prompt[N]` in error text.
- **Patterns to follow:** `prompt_templates` builds `field[N]` and `field[N].when` from the label it gets, so only the label passed in changes.
- **Test scenarios:**
  - An `agent` step written with `prompt:` and a list of parts with an invalid `when` in part 2 fails validation with an error that names `action.message[2].when`.
  - A `profile` step with a blank step text reports a missing-field error that names `'message'` (adapt `a_profile_step_needs_a_prompt_and_a_usable_harness` in `validator.rs`).
  - A `steps.ID.reply` reference in an agent step text is still allowed (the existing step-reply tests keep passing after the allowlist change).
- **Verification:** no validator or template error text contains `prompt` as the step-text field name.

### U4. Switch docs, template and bundled workflows to `message`

- **Goal:** new authors copy `message:` from every example (R8).
- **Requirements:** R8, R7.
- **Dependencies:** U1, U2.
- **Files:** `docs/dev/workflow-schema.md`, `docs/user/workflows.md`, `docs/user/agents.md`, `workflows/template.yaml`, `workflows/compound-engineering-change.yaml`, `workflows/README.md` (only if it shows a step text).
- **Approach:**
  1. Replace the step-text key `prompt:` with `message:` in YAML examples for `agent`, `profile` and legacy `decision: { type: agent }`.
  2. In `docs/dev/workflow-schema.md`: §3.3.A, §3.3 profile section, §3.4.B, §3.7.6 (rename the heading to "Message parts (`when`)" and change the example and `prompt[2].when` to `message[2].when`), §3.7.7, and the §5 rule list. Add one paragraph that says `prompt` is a deprecated alias accepted during the transition period, that a step must not have both keys, and that errors name `message`.
  3. Leave the prose term "prompt text" and other uses of "prompt" listed in Scope Boundaries.
- **Test expectation:** none for the docs themselves -- `tests/integration/builtin_workflows.rs` and `tests/integration/docs_links.rs` already cover that the bundled workflows load and the doc links resolve.
- **Verification:** `grep` finds no `prompt:` step-text key in the files listed above; the bundled-workflow tests pass.


---

## Verification Contract

| Gate | Command | Proves |
|---|---|---|
| Workspace tests | `cargo test --workspace` | R2, R3, R4, R5, R6 and that bundled workflows load (R8) |
| Locked build (CI parity) | `cargo test --workspace --locked` | no dependency drift |

`#[ignore]`d tests are not needed: the change does not touch process lifecycle, the real `claude` binary or Kubernetes.

---

## Definition of Done

- U1–U4 are done and their test scenarios pass.
- `cargo test --workspace` passes.
- No YAML example in the R8 files uses `prompt:` as the step-text key.
- No dead code from abandoned attempts (for example an unused custom `Deserialize`) stays in the diff.
- The PR description lists Q1 (option a) and Q2 (option a).

---

## Sources

- Origin: `docs/brainstorms/2026-11-22-unify-step-message-field-requirements.md`, including its 2026-11-22 review item on R4.
- Schema structs: `crates/narsor-model/src/workflow/schema.rs` (`AgentActionSchema`, `ProfileActionSchema`, `DecisionActionSchema`, `HumanActionSchema`, `AgentDecisionSchema`, `HumanDecisionSchema`, `parse_state`, `legacy_marker`).
- Compile step and diagnostics: `crates/narsor-model/src/workflow/loader.rs` (`compile_action`, `compile_decision`, `LoadDiagnostic`).
- Error labels: `crates/narsor-model/src/workflow/validator.rs` (`missing_field`, `AgentDecisionMissingField`, `prompt_templates`, step-reply allowlist); `crates/narsor-model/src/workflow/template.rs` (`render_action`).
- Sealed snapshot: `crates/narsor-model/src/domain/task.rs` (`TaskLaunchSpec.definition`).
- Embedded template: `crates/narsor-exec/src/workflows/mod.rs`.
