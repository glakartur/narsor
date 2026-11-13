---
title: "Subagent usage: show only what the harness reports (count, not tokens) and record a raw fixture for the rest"
date: 2026-11-13
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: medium
applies_when:
  - "A harness reports a number that looks like usage but may measure something else (for example Claude Code subagent totalTokens)"
  - "A display field needs a figure that one harness cannot give (cost, tokens per subagent)"
  - "A plan has an unverified claim about harness output that decides what the TUI says"
tags: [claude-code, subagents, usage, tokens, tui, fixture, ndjson, scope, plan-review]
---

# Subagent usage: show only what the harness reports (count, not tokens) and record a raw fixture for the rest

## Context
The branch (PR 105) added subagent usage for Claude Code to the Progress tab and the task header. The first plan showed `↕` with `totalTokens` from `toolUseResult`. The author asked whether this number is "the tokens used, or what is left of the context window". Checking session logs showed that `totalTokens` is the context size at the end of the subagent run plus the last output. It is not the sum of usage (plan F-b). Claude Code also gives no cost per subagent (F-c). Background subagents (`status: async_launched`) carry no usage at all.

Four decisions in the PR threads (Q1 to Q5) removed token collection for Claude Code. What remained: `👥 N` from `subagent_stats.spawned` (already parsed), one icon constant, one count function used by the header and the Progress tab.

## Guidance
1. **Do not put a number under a usage label if it measures something else.** `totalTokens` shown as `↕` would read as consumption. If the harness gives no true usage, show the count only.
2. **Keep the protocol and TUI ready for tokens, and fill them only from harness data.** `↑`/`↓`/`↕` stay for harnesses that report tokens (GitHub Copilot today). Claude Code leaves them empty. Do not compute or add substitute numbers.
3. **Drop a field that adds nothing.** "No cost, so the count alone must be enough." Cost for subagents stays out until a harness gives it (`PositionUsage` has no field for it).
4. **Do not change the meaning of an existing group because a new fact appeared.** The `✨` description ("numbers include subagents") may be false for Claude Code. In the fixture, `usage` on `result` covers only the last turn, while `modelUsage` covers the whole run. The decision (Q5): do not change the text and do not change the display. Only collect the fact.
5. **Record a raw, masked fixture when data is uncertain.** `tests/fixtures/claude-code/subagents.ndjson` (one foreground and one background subagent, two `result` lines) plus a field description in `README.md`. Later work can rely on it instead of local session logs, which are a different format (only the `toolUseResult` object is the same, and this needed a recording to confirm).
6. **Mask paths and check for customer data before you commit a recording.** The fixture uses `/home/user` and `/tmp/project`.

## Why This Matters
A plausible but wrong figure in a cost view is worse than a missing figure: the operator trusts it. The same applies to a header total built from mixed forms (one step with tokens, one with a count only). The plan needed three review rounds to remove this, and the biggest rewrite came after the user questioned the meaning of one field.

## When to Apply
- A new harness field is mapped to a TUI usage figure.
- A plan states a fact about harness output without a recording to prove it. Make the recording the first task (U1).
- A decision from a PR thread conflicts with an earlier one (Q1 versus Q2). Ask once and name the conflict, and record which option is withdrawn.

## Examples
Before (plan v1): `👥 2 ↕84k` with `↕` from `totalTokens`.
After: `👥 2` for Claude Code; `👥 2 ↑80k ↓6k` only for data from a harness that reports tokens (covered by a test with hand-made data).

Count source order (A6, one function for the header and the Progress tab): `subagent_count` when above 0, otherwise `subagent_stats.spawned` when above 0. Background subagents count: `spawned: 2` with `requested.background: 1`.

## Related
- `docs/plans/2026-11-13-002-feat-zuzycie-subagentow-claude-code-plan.md`
- `docs/brainstorms/2026-11-13-zuzycie-subagentow-claude-code-requirements.md`
- `docs/solutions/design-patterns/tui-step-icon-mapping-lives-in-the-tui-not-the-protocol-2026-11-12.md` (one icon constant, one mapping)
