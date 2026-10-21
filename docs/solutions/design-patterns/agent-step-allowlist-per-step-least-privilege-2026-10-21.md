---
title: "Give each agent step its own allowlist of host operations: the clean-up step must not inherit the main step's rights"
date: 2026-10-21
category: design-patterns
module: workflows/compound-engineering
problem_type: design_pattern
component: development_workflow
severity: high
applies_when:
  - "A script step that called one host tool is replaced by an agent step that has shell access and merge credentials"
  - "A clean-up or recovery step is written by copying the prompt of the main step"
  - "A prompt limits an agent only by text, because the engine has no command allowlist"
tags: [workflow, agent-step, least-privilege, allowlist, disarm-merge, finalize, prompt-injection, gh, glab]
---

# Give each agent step its own allowlist of host operations

## Context

`finalize` in `workflows/compound-engineering.yaml` moved from a `glab` script to an agent step, so it works on any repository host (`gh` or `glab`). The agent has shell access and the operator's credentials. The engine has no command allowlist and `workspace_access: read-only` does not limit `gh` or `glab` calls. The only barrier is the prompt.

`disarm_merge` runs after `finalize` hits its time limit. It was first written with the same allowed operations as `finalize`: read, take out of Draft, restore Draft, approve, set auto-merge, cancel auto-merge, read checks. The commit `d2f25a8` found the problem: a clean-up step that can approve and set auto-merge can also cause the merge it should stop.

## Guidance

- Write the allowlist for each agent step from what that step must do, not from a copy of another step.
- A clean-up step may only reverse the effects: `disarm_merge` allows read, cancel auto-merge, restore Draft. Its prompt also says "Never approve, never take it out of Draft, never set auto-merge".
- Drop the matching command equivalents from the prompt. A command shown in the prompt is an invitation to use it.
- Keep the other layers: validate `pr_url` first (untrusted data, host must equal the `origin` host), no `git merge`, no push, no file changes.
- Say in the plan that a prompt is not a technical barrier. Use a minimal-rights account for the operator.

## Why This Matters

The `pr_url` and the PR text come from earlier agent steps and from the repository. A prompt injection there can use any right the step has. The smaller the list, the smaller the damage. A clean-up step with merge rights also hides a failure mode: the step that must make the PR safe makes it merge.

## When to Apply

- Any new agent step that replaces a script and keeps host credentials.
- Any recovery or `timeout` branch of such a step.

## Examples

Before (`disarm_merge`): "Allowed operations on this pull request only: read it, take it out of Draft, restore the Draft, approve it, set auto-merge, cancel auto-merge, read the state of its checks."

After: "Allowed operations on this pull request only: read it, cancel auto-merge, restore the Draft. Never approve, never take it out of Draft, never set auto-merge."

Related: [human-review-at-pr-gate-not-inside-agent-steps-2026-10-21.md](human-review-at-pr-gate-not-inside-agent-steps-2026-10-21.md). A person must verify the account rights and branch protection rules (not decided in the plan).
