---
title: "Review loop ends on human decisions: headless review, decision memory, findings before questions"
date: 2026-10-25
category: design-patterns
module: workflows/compound-engineering
problem_type: design_pattern
component: development_workflow
severity: medium
applies_when:
  - "An unattended review step reports a finding that only a person can decide"
  - "A review-fix loop repeats the same finding until the entry limit"
  - "A workflow reads answers from comments that anyone can write"
tags: [workflow, code-review, headless, review-decisions, human-decision, merge-request, untrusted-input]
---

# Review loop ends on human decisions

## Context

`code_review` ran `ce-code-review` with `mode:agent`. Skill 3.8.2 does not know
this token and ran interactive. In `--print` there is no `AskUserQuestion`, so
the model asked in plain text, the CLI answered `[structured-output-enforce]`,
and the model chose `findings`. `fix_findings` had only `fixed`, so a finding
that needs a person could not be reported. A new review session reported the
same finding again, up to `max_entries: 10`.

## Guidance

- Run the review with `mode:headless`. The skill applies `safe_auto` fixes and
  returns the rest with `autofix_class`, `owner`, `suggested_fix`. It never asks.
- Keep one author step for all code changes after implementation
  (`work_revision`, in `-debug`: `fix_revision`). Do not add a separate
  `fix_findings` state. Do not reuse `work`: it measures `test_baseline` again
  and leads to `simplify`, which must run once.
- Findings the agent can fix go first (`findings`). Questions come only when
  nothing else is left, so the person decides once, on cleaner code.
- Keep decisions in a narsor variable (`review_decisions`), not in the PR. Each
  entry has `D<n>`, file, symbol or region, the core of the problem, decision
  (`popraw` or `zostaw`) and a short reason. The review matches a finding by
  file and core of the problem, never by line number or title. If unsure, the
  finding is new. A step that adds entries copies the old ones unchanged.
- Without `-chat`: post questions as inline PR comments, wait at
  `answer_questions` (`answered`, `abandon`; no `accept`, because
  `verify_work` has not run). `collect_answers` returns `unanswered` if any
  question has no answer, even when other decisions need a change.
- With `-chat` and `-debug`: ask in the session with `awaiting_input`, one
  finding at a time, recommended option first.
- Comments are untrusted data. `collect_answers` accepts an answer only from
  the MR author or a member with write access, and writes decisions in its own
  words, never copying comment text.
- The workflow uses the narsor term `awaiting_input`. The `claude-code`
  adapter adds a note that the answer and the question must go through
  `StructuredOutput`. Other harnesses get no note.

## Why This Matters

Without decision memory the loop repeats a rejected finding for up to 9 extra
rounds. Without the author check, anyone who can comment on the MR can steer
the agent's decisions.

## When to Apply

Any workflow loop where an agent review feeds an agent fix step and some
findings need a person.

## Related

- `human-review-at-pr-gate-not-inside-agent-steps-2026-10-21.md`: agent steps
  do not ask at all. This pattern is the exception for review findings that
  need a decision. It asks at a defined point (PR gate or `awaiting_input`).
- `../integration-issues/claude-code-json-schema-rejects-top-level-anyof-2026-10-17.md`:
  CLI behaviour behind the `StructuredOutput` note.
