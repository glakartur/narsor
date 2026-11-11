---
title: "Per-step model and effort profile: harness-independent tiers, a profile file read at every attempt, completeness checked at load"
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
  - "A workflow step picks a model or an effort, and a profile step or a retry loop may change it"
  - "One workflow must run on more than one harness (claude-code, codex, opencode, Copilot)"
  - "A value that configures a step can change while the execution runs"
  - "A rule needs a value (model, effort, ceiling) that may be unset at the moment the rule fires"
symptoms:
  - "A slow conductor answer changed the profile of steps that had already run"
  - "Escalation after failed attempts met an unset model or effort and showed a misleading ceiling message"
  - "Concrete model names in a workflow tied it to a single harness"
root_cause: missing_validation
resolution_type: code_fix
tags: [step-profile, profile-step, model-tiers, effort, ceiling, escalation, harness, validate-at-load]
---

# Per-step model and effort profile: harness-independent tiers, a profile file read at every attempt, completeness checked at load

> **Update 2026-11-11 (plan `docs/plans/2026-11-11-002-feat-krok-profile-workflow-plan.md`).**
> The conductor no longer profiles steps. A `profile` step of the workflow does it
> (`type: profile`, `docs/dev/workflow-schema.md` §3.7.10). The "decide once,
> before the first entry" rule (old guidance 2) is replaced: a `profile` step can
> run many times, each run replaces `executions/{id}/step-profile.json`, and the
> runner reads that file at every attempt of a step. `plan: true`, `complexity`,
> `conclude_profile`, `entries_of` and `conductor/profile.json` are gone. The
> guidance below is updated; the tiers and the load-time checks did not change.

## Context
Narsor agent steps got a model and an effort that can change at run time. Three
sources feed one step, in this order: the workflow default (step, then
`globals.default_model` / `default_effort`), the entry that a `profile` step
writes to `step-profile.json`, and an escalation after repeated failed attempts. A
`ceiling` (step, then `globals.ceiling`) caps the last two. PR #92 went through
review rounds that exposed four design faults, all fixed in the branch.

## Guidance

1. **Name tiers, not models.** Workflows, the `profile` step and escalation use
   `small | medium | large | max` and the efforts `low | medium | high | max`
   (`EFFORT_SCALE`, `MODEL_TIERS`). Each harness maps tier to model name
   (`HarnessAdapter::model_tiers`); the daemon config sets the conductor's own
   `conductor_model` / `conductor_effort` as tiers too, empty meaning the
   harness default. A harness with no name for a tier stops the daemon at start
   instead of failing in the middle of a task. Harnesses that ignore effort
   (opencode, Copilot) say so on the timeline.
2. **Read the profile at every attempt; the last run of the profile step
   counts.** (Replaces "decide the profile once, before the first entry".) The
   `profile` step is a step of the workflow, so it runs in order with the other
   steps: there is no late asynchronous answer to guard against. The author can
   profile more than once, for example again after a review loop. Each run
   replaces the whole file (also with an empty list), atomically, before the
   step takes its transition. `apply_step_profile` (`engine/runner/harness_attempt.rs`)
   reads the file at each attempt of an `agent` or `profile` step, so an entry
   written while a step is between attempts applies from its next attempt. A
   file that cannot be read gives the defaults and one warning per execution.
3. **Check completeness when the workflow loads, not when a rule fires.**
   The loader copies the general defaults and ceiling into every agent step, so
   a step holds its effective values. The validator then requires: a step with
   a ceiling has a model and an effort; its ceiling names both; a ceiling is
   never below the step's own value. Escalation and the clamp never meet an
   unset field, so their messages stay true.
4. **Keep the ordering rule in pure functions.** `step_profile::resolve`
   takes default, ceiling, profiled request, failure count and threshold, and
   returns the setting plus `requested_over_ceiling`. The `profile` step's
   entry check (`engine/actions/profile/entries.rs`) and the runner both call
   it, so the rule lives once.
5. **Judge model and effort separately.** An unusable model must not drop a
   usable effort. Entries that change nothing are not stored.
6. **A request above the ceiling is clamped and recorded.** The step starts on
   the ceiling (or, without a ceiling, on its default). The entry keeps the
   requested values, and the `profile` step's conversation shows the request,
   the clamp and the rationale in one Narsor entry.

## Why This Matters
- Concrete model names make a workflow harness-specific and rot when models
  change; tiers move the volatile part into one adapter table.
- A profile that comes from a step of the workflow is ordered with the
  other steps, so the result does not depend on the latency of an
  asynchronous actor. Reading the file at every attempt lets the author
  profile again later in the workflow.
- A rule that assumes a value which validation never guaranteed fails late,
  inside a retry loop, where the message is hardest to understand.

## When to Apply
- Any new per-step setting that a step of the workflow or the operator may change.
- Any setting that must work across harnesses.
- Any "cap" or "escalate" rule that reads fields which may be unset.

## Examples
Profile read at each attempt (`engine/runner/harness_attempt.rs`):

```rust
// This step's entry in step-profile.json, or None (the step's defaults).
let entry = self.stored_step_profile(execution, state);
let profiled = entry.as_ref().map(StepProfileEntry::setting);
```

Load-time rule (`workflow/validator.rs`): "a step with a ceiling needs a model:
set it, or `globals.default_model`" — reported when the workflow loads.

## Related
- PR https://github.com/glakartur/narsor/pull/92
- `docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md`
- `docs/plans/2026-11-11-1000-feat-dynamiczny-model-i-effort-kroku-plan.md`
- `docs/plans/2026-11-11-002-feat-krok-profile-workflow-plan.md` (the `profile` step)
- `docs/dev/workflow-schema.md` (globals, step `model`/`effort`/`ceiling`, §3.7.10 `profile` steps)
- Operator decisions of PR #92: one global escalation threshold, default 3
  (D3); profile per step, not per plan unit (D5). The profile preview in the
  conductor's TUI tab (D1) and the open request above the ceiling (D2) went
  with the conductor's profiling.
