---
title: "Per-step model and effort profile: harness-independent tiers, decided once before first entry, completeness checked at load"
date: 2026-11-11
category: architecture-patterns
module: engine/step_profile
problem_type: architecture_pattern
component: tooling
related_components:
  - service_object
  - background_job
severity: medium
applies_when:
  - "A workflow step picks a model or an effort, and a conductor or a retry loop may change it"
  - "One workflow must run on more than one harness (claude-code, codex, opencode, Copilot)"
  - "A late asynchronous answer can arrive after the thing it configures has already started"
  - "A rule needs a value (model, effort, ceiling) that may be unset at the moment the rule fires"
symptoms:
  - "A slow conductor answer changed the profile of steps that had already run"
  - "Escalation after failed attempts met an unset model or effort and showed a misleading ceiling message"
  - "Concrete model names in a workflow tied it to a single harness"
root_cause: missing_validation
resolution_type: code_fix
tags: [step-profile, model-tiers, effort, ceiling, escalation, conductor, harness, validate-at-load, decide-once]
---

# Per-step model and effort profile: harness-independent tiers, decided once before first entry, completeness checked at load

## Context
Narsor agent steps got a model and an effort that can change at run time. Three
sources feed one step, in this order: the workflow default (step, then
`globals.default_model` / `default_effort`), the profile the conductor stores
after the plan step, and an escalation after repeated failed attempts. A
`ceiling` (step, then `globals.ceiling`) caps the last two. PR #92 went through
review rounds that exposed four design faults, all fixed in the branch.

## Guidance

1. **Name tiers, not models.** Workflows, the conductor and escalation use
   `small | medium | large | max` and the efforts `low | medium | high | max`
   (`EFFORT_SCALE`, `MODEL_TIERS`). Each harness maps tier to model name
   (`HarnessAdapter::model_tiers`); the daemon config sets the conductor's own
   `conductor_model` / `conductor_effort` as tiers too, empty meaning the
   harness default. A harness with no name for a tier stops the daemon at start
   instead of failing in the middle of a task. Harnesses that ignore effort
   (opencode, Copilot) say so on the timeline.
2. **Decide the profile once, before the first entry.** A conductor answer
   can arrive late. `conclude_profile` takes the set `started` from
   `started_states` (entry count > 0 in the engine record) and the store drops
   entries for those steps. A step that has run keeps what it ran with.
3. **Check completeness when the workflow loads, not when a rule fires.**
   The loader copies the general defaults and ceiling into every agent step, so
   a step holds its effective values. The validator then requires: a step with
   a ceiling has a model and an effort; its ceiling names both; a ceiling is
   never below the step's own value. Escalation and the clamp never meet an
   unset field, so their messages stay true.
4. **Keep the ordering rule in pure functions.** `step_profile::resolve`
   takes default, ceiling, conductor request, failure count and threshold, and
   returns the setting plus `requested_over_ceiling`. The conductor path
   (`entries_of`) and the runner both call it, so the rule lives once.
5. **Judge model and effort separately.** An unusable model must not drop a
   usable effort. Entries that change nothing are not stored.
6. **A request above the ceiling is clamped and kept open.** The step starts on
   the ceiling; the request is recorded and shown (TUI, read-only). Resolving
   it is out of scope for the feature.

## Why This Matters
- Concrete model names make a workflow harness-specific and rot when models
  change; tiers move the volatile part into one adapter table.
- Without decide-once, the same task could run two attempts of a step on
  different profiles depending on conductor latency; the result is not
  reproducible.
- A rule that assumes a value which validation never guaranteed fails late,
  inside a retry loop, where the message is hardest to understand.

## When to Apply
- Any new per-step setting that an async actor (conductor, operator) may change.
- Any setting that must work across harnesses.
- Any "cap" or "escalate" rule that reads fields which may be unset.

## Examples
Late answer guard (`conductor/actor.rs`):

```rust
let started = started_states(&execution)?;       // entry count > 0
conclude_profile(&store, &request, outcome, &started)?; // skips started steps
```

Load-time rule (`workflow/validator.rs`): "a step with a ceiling needs a model:
set it, or `globals.default_model`" — reported when the workflow loads.

## Related
- PR https://github.com/glakartur/narsor/pull/92
- `docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md`
- `docs/plans/2026-11-11-1000-feat-dynamiczny-model-i-effort-kroku-plan.md`
- `docs/dev/workflow-schema.md` (globals, step `model`/`effort`/`ceiling`/`plan`)
- Operator decisions: profile preview only in TUI (D1); a request above the
  ceiling stays open (D2); one global escalation threshold, default 3 (D3);
  profile per step, not per plan unit (D5).
