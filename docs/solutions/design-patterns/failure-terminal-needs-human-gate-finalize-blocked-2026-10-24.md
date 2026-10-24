---
title: "A failure terminal that a person must confirm: finalize_blocked gate with retry and confirm_failure"
date: 2026-10-24
category: design-patterns
module: workflows/compound-engineering
problem_type: design_pattern
component: development_workflow
severity: medium
applies_when:
  - "An agent step answers blocked or timeout and the cause lies outside the run (tool, login, host rights)"
  - "A workflow ends in a failed terminal state and a person may still fix the cause"
  - "A gate was removed in one change and a later requirement needs it again"
tags: [workflow, human-gate, finalize, finalize_blocked, finalize_failed, retry, confirm_failure, draft, merge-request]
---

# A failure terminal that a person must confirm

## Context

In `workflows/compound-engineering.yaml` the `finalize` step takes the merge
request out of Draft. Commit `0da020f` removed the `finalize_blocked` gate:
`blocked` and `timeout` went straight to `finalize_failed`. The requirement
from PR #25 says that a person decides before the run fails. The cause of a
`blocked` answer is usually outside the run: `gh` or `glab` is missing, not
signed in, or the account has no right to leave Draft. A person can fix that
and try again. A direct jump to a failed terminal state removes this chance.

## Guidance

- Send `blocked` and `timeout` of `finalize` to a human gate `finalize_blocked`.
- The gate has two choices only:
  - `retry` goes back to `finalize`. There is no limit. The person controls the loop.
  - `confirm_failure` goes to `finalize_failed` (status `failed`).
- Do not add a third choice such as `left_as_draft` or `finish_as_draft`.
  A person who prefers to keep the Draft chooses `confirm_failure`. One
  failure state keeps the terminal states unambiguous.
- Keep the automatic loop (3 tries, read from the host after each change)
  inside the prompt of `finalize`. `retry_policy` does not repeat a `blocked`
  answer (see `WORKFLOW_SCHEMA.md`), so the manual repeat belongs to the gate.
- Update the `choices` descriptions of `finalize`: `blocked` and `timeout` say
  "a person decides at the finalize_blocked gate", not "the run fails".
- The engine shows the `reason` of the agent choice in the pending gate view
  (test `a_script_choosing_blocked_leads_to_a_gate_with_its_reason`). After a
  `timeout` the agent gave no reason. Say this in the gate message.
- Change these files together, because they describe the same graph:
  `workflows/compound-engineering.yaml`, `workflows/README.md`,
  `workflows/compound-engineering-flow.html`, `tests/workflows.rs`
  (`COMPOUND_STATES`, `COMPOUND_EDGES`), `tests/builtin_workflows.rs`.

## Why This Matters

- A person can fix the environment and continue the same run. Without the
  gate the person must start a new task.
- Documents and tests that still describe the earlier graph hide the real
  behavior. The README said "the run ends in `finalize_failed`" after the
  code needed a gate.
- Tasks that waited at an earlier `finalize_blocked` gate with the
  `finish_as_draft` choice may not resume after the update. Say this in the
  README.

## When to Apply

- Any agent step whose failure may be fixed by a person outside the run.
- Not for failures that the run cannot change (a closed merge request is also
  `blocked`, but the person still chooses `confirm_failure` quickly).

## Examples

```yaml
transitions:
  ready: finished
  blocked: finalize_blocked
  timeout: finalize_blocked
...
finalize_blocked:
  action: { type: human, ... }
  transitions:
    retry: finalize
    confirm_failure: finalize_failed
```

Pattern source for the gate: `review_learning` in the same file. Earlier
version of the gate: `git show 0da020f^:workflows/compound-engineering.yaml`.

## Related

- `docs/solutions/design-patterns/human-review-at-pr-gate-not-inside-agent-steps-2026-10-21.md`
- `docs/solutions/design-patterns/agent-step-allowlist-per-step-least-privilege-2026-10-21.md`
- Commits `04c31f7` (gate restored), `0da020f` (gate removed), plan
  `docs/plans/2026-10-24-001-fix-compound-finalize-zdjecie-draft-plan.md`
