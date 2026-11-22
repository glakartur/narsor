---
date: 2026-11-22
topic: unify-step-message-field
---

# Unify the step text field to `message`

## Summary

Every workflow step type that carries a text for its executor uses the key
`message`. Today `agent` and `profile` steps (and the legacy
`decision: { type: agent }` block) use `prompt`, while `decision` and `human`
steps (and the legacy `decision: { type: human }` block) use `message`. During
a transition period the loader accepts both keys on every one of these step
types and treats them as the same field. Documentation, the workflow template
and the bundled workflows switch to `message`.

---

## Problem Frame

A workflow author must remember which step type takes `prompt` and which takes
`message`. The two fields have the same role (the text the step gives to its
executor: an agent, the conductor or a person) and, for `agent`, `profile` and
`decision`, the same type (`PromptText`: a string or a list of parts with
`when`, §3.7.6 of `docs/dev/workflow-schema.md`). Because every action schema
has `deny_unknown_fields`, the wrong key is a load error. The split is a
frequent, avoidable mistake.

Current state (from `crates/narsor-model/src/workflow/schema.rs`):

| Step / block                         | Key today | Type          |
|--------------------------------------|-----------|---------------|
| `action: { type: agent }`            | `prompt`  | `PromptText`  |
| `action: { type: profile }`          | `prompt`  | `PromptText`  |
| `action: { type: decision }`         | `message` | `PromptText`  |
| `action: { type: human }`            | `message` | `String`      |
| legacy `decision: { type: agent }`   | `prompt`  | `Option<String>` |
| legacy `decision: { type: human }`   | `message` | `Option<String>` |

`script` steps have neither field and are out of scope.

---

## Actors

- A1. Workflow author: writes and edits workflow YAML.
- A2. Workflow loader and validator (`narsor-model`): read the YAML and report
  errors.
- A3. Operator of existing tasks: resumes tasks whose workflow was sealed with
  the old keys.

---

## Key Flows

- F1. Author writes a new workflow
  - **Trigger:** A1 writes a step of any in-scope type.
  - **Steps:** A1 writes `message:`. A2 loads the file.
  - **Outcome:** The step loads, whatever its type.
- F2. Existing workflow with `prompt`
  - **Trigger:** A2 loads a workflow (new task or resumed sealed task) that uses
    `prompt:` on an `agent` or `profile` step.
  - **Outcome:** The step loads and behaves exactly as before.
- F3. Author writes both keys on one step
  - **Outcome:** A2 rejects the file with an error that names the state and
    says to keep only `message`.

---

## Requirements

- R1. `message` is the canonical key of the step text on `agent`, `profile`,
  `decision` and `human` actions, and in the legacy `decision: { type: agent }`
  and `decision: { type: human }` blocks.
- R2. Transition period: on each of these, `prompt` is accepted as an alias of
  `message` with the same type and the same meaning. A step written with
  `prompt` loads and runs exactly as a step written with `message`.
- R3. A step that has both `message` and `prompt` is a load error. The error
  names the state and says that the keys are aliases and only `message` must
  stay. No precedence rule is applied.
- R4. Validation and template errors that name the field (missing or blank
  field, a part path such as `prompt[2].when`, rendering locations) name the
  key the author wrote. Where the key is missing, the error names `message`.
- R5. Required/optional status, blank-text checks, `PromptText` parts and
  `when`, the `steps.ID.*` reference rules and every other rule of a field do
  not change. Only the key name changes.
- R6. Sealed workflows of existing tasks that use `prompt` keep loading and
  resuming. No migration of stored data is needed.
- R7. The workflow `schema` version and the file `version` do not change: the
  change is additive.
- R8. `docs/dev/workflow-schema.md`, `docs/user/workflows.md`,
  `workflows/template.yaml`, `workflows/compound-engineering-change.yaml` and
  other examples in `docs/` use `message`. The schema document says that
  `prompt` is a deprecated alias accepted during the transition period, and
  that a step must not have both.
- R9. Tests: one test proves that `prompt` and `message` give the same loaded
  step for an `agent`/`profile` step and for a `decision`/`human` step
  (table-driven); one test proves R3. Most existing test fixtures keep
  `prompt`, so they also cover the alias.

---

## Scope Boundaries

- Internal Rust identifiers (`AgentAction.prompt`, `HumanAction.instruction`,
  `DecisionAction.message`, …) do not change. Only the YAML contract changes.
- The word "prompt" in other contexts does not change: harness protocol
  payloads, the conductor `Reprompt { prompt }` decision, `{{ step.history }}`
  and the prose term "prompt text" in §3.7.6.
- The removal of the `prompt` alias is not part of this work (see
  assumptions).
- No new warning channel for deprecated keys is added to the loader.
- Mass rewrite of test fixtures from `prompt` to `message` is not required.

---

## Success Criteria

- An author can write `message:` on every step type that has a step text, and
  the file loads.
- Every workflow that loads today loads with the same result after the change.
- `cargo test --workspace` passes.

---

## Assumptions to Confirm

- Z1. The scope includes all step types with a step text: `agent`, `profile`,
  `decision`, `human`, and the legacy `decision: { type: agent | human }`
  blocks. Reason: the request says "part of the step types"; a partial
  unification would keep the problem.
- Z2. The alias works in both directions: `prompt` is also accepted on
  `decision` and `human` steps. Reason: the request says the keys are
  interchangeable during the transition period.
- Z3. Both keys on one step is an error, not a precedence rule. Reason: this is
  the safest option; a silent choice could hide an author mistake.
- Z4. The transition period has no end date in this work. The `prompt` alias
  stays until a separate decision removes it (for example together with a
  future schema version). Reason: removal breaks existing workflows and sealed
  tasks, so it needs its own decision.
- Z5. The loader shows no deprecation warning for `prompt`, because it has no
  warning channel today. The deprecation is documented only. Reason: a new
  warning channel is a larger change than this request.
- Z6. Internal Rust field names do not change. Reason: the request is about
  workflow parameters; renaming internals adds churn and risk without value
  for the author.
- Z7. Repository documentation, the template and the bundled workflows move to
  `message` in the same change; test fixtures stay as they are. Reason: new
  authors copy from docs and templates; fixtures that keep `prompt` test the
  alias.
- Z8. Error messages name the key the author wrote (R4). Reason: an error that
  names a key that is not in the file confuses the author.

---

## Open Questions for the User

None. All unclear points have a safe assumption in the section above.

---

## Deferred / Open Questions

These entries are review items left for the author. They do not block implementation.

### From 2026-11-22 review

- **Echoing the author's key in errors needs the written key stored in the workflow model** — R4 and Z8 (P2, feasibility and scope-guardian, confidence 75)

  Requirement R4 asks every error that names the step text field to repeat the key the author wrote. A serde alias is resolved while the file is parsed, so the compiled workflow model keeps only one field. The validator and the template renderer both run on that model and hard-code the key name in their messages. To keep R4, the written key must be stored per field in the model and survive the round trip through the sealed task data. This is much more than the "only the key name changes" that R5 and the Scope Boundaries state. Whether to relax R4 and Z8 is the author's decision.

  Change: Make every validation and template error name the canonical key `message`, with the state and location prefix the loader already adds. Drop the requirement to echo the author's key, and update the example part path in R4 to `message[2].when`.
