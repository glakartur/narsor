---
title: "A usage-limit stop inside the engine's output-label reprompt must park the attempt, not fail the step"
date: 2026-11-28
category: logic-errors
module: narsor-exec/engine/runner/output
problem_type: logic_error
component: background_job
symptoms:
  - "A task paused for the usage limit during the output-label reprompt is failed instead of waiting for the reset"
  - "The first agent call parks and resumes, but the follow-up reprompt of the same attempt ends as a harness failure"
root_cause: logic_error
resolution_type: code_fix
severity: medium
tags: [usage-limit, reprompt, suspended-action, conductor, pause-resume]
---

# A usage-limit stop inside the engine's output-label reprompt must park the attempt, not fail the step

## Problem
Usage-limit pause and resume (PR #159) parks an attempt on a usage-limit stop and continues in the same session. A second agent call on the same attempt, the reprompt that asks for a missing output label, mapped `Suspended(AwaitingUsageLimit)` to `RepromptFailure::Failed`. That broke requirement R4: the step failed where it should wait.

## What Didn't Work
Treating every secondary agent call alike. The assumption "the exemption for conductor chats covers all reprompts" was wrong: it covers only the conductor's rescue.

## Solution
`reprompt_attempt` in `crates/narsor-exec/src/engine/runner/output/reprompt.rs` returns `RepromptEnd::AwaitingUsageLimit` when the caller is `RepromptedBy::Engine`. The engine then calls `park_for_usage_limit` for the same attempt. The continuation asks for the label again. For `RepromptedBy::Conductor` the stop stays `RepromptFailure::Failed`, because a conductor rescue has no attempt to continue. `rescue.rs` maps the new variant to `Flow::Stop`.

## Why This Works
Parking keys on the attempt and its session, not on the first call. The engine's reprompt has both. The conductor's has neither.

## Prevention
- Every new path that runs an agent call must handle `SuspendedAction::AwaitingUsageLimit` explicitly. Do not fold it into a generic failure arm.
- The regression test is in `tests/integration/engine_recovery.rs` (usage-limit stop during the reprompt).
- Plan docs under `docs/` use English headings (AGENTS.md); review caught Polish headings in the plan.
