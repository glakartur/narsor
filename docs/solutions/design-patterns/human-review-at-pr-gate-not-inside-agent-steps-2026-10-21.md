---
title: "Human review at the pull request gate, not inside agent steps of the compound workflow"
date: 2026-10-21
category: design-patterns
module: workflows/compound-engineering
problem_type: design_pattern
component: development_workflow
severity: medium
applies_when:
  - "An unattended agent step would stop to ask the person or wait for approval of its own result"
  - "A review step fixes findings itself and its summary is only information for the person"
  - "A test would start a real daemon or process only to check a workflow definition"
tags: [workflow, human-gate, merge-request, review-summary, brainstorm, headless, requires-human-approval]
---

# Human review at the pull request gate, not inside agent steps

## Context

In `workflows/compound-engineering.yaml` the person was asked in the middle of
the steps: `brainstorm` asked questions, and `doc_review` and `plan_review` had
`requires_human_approval: true` on the `reviewed` choice. This stopped the
run before the result reached the merge request, where the person already
reads and decides at the `approve_*` gate.

## Guidance

- Agent steps never ask. The prompt says: do not ask, do not use a question
  tool, do not return `awaiting_input`. On unclear direction the agent takes the
  safest assumption and continues.
- Put the open points in the artifact, in fixed sections:
  "Założenia do potwierdzenia" (assumptions) and "Pytania otwarte dla
  użytkownika" (questions). The person answers on the merge request or in the
  gate input (`requirements_feedback`).
- Review steps report `review_summary` and go straight on. The `publish_*`
  step pushes the result and adds the summary as a merge request comment, once
  (skip if an identical comment exists) and without changing the description.
  The person reads it at the gate.
- A step may skip an optional review with its own choice
  (`requirements_ready`). It must then give the reason in `review_summary`
  (`Przegląd pominięto: <reason>`). Default to the review when in doubt.
  Keep `plan_review` always on.

## Why This Matters

An unattended run no longer blocks on a person in a place the person cannot
see well. There is one human decision point per stage. The reason for a
skipped review stays visible on the merge request and in the task history.

## When to Apply

Any workflow stage that ends in a publish step followed by a human gate.

## Examples

```yaml
choices:
  - label: reviewed
    requires_variables: [review_summary]   # no requires_human_approval
transitions:
  reviewed: publish_plan
```

## Testing note

Built-in workflow tests check the resolver and the embedded definitions through
the library. Tests that start a real daemon were removed
(`tests/builtin_workflows.rs`): they were slow and checked nothing that the
definition-level assertions do not check.
