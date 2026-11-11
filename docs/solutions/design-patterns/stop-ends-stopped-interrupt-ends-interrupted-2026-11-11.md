---
title: "Stop ends in Stopped; Interrupt ends in Interrupted — do not mix them"
date: 2026-11-11
category: design-patterns
module: orchestration / runner / tui
problem_type: design_pattern
component: backend
severity: medium
applies_when:
  - "A new operator action must cut running work and leave the task waiting for the owner"
  - "Someone proposes to reuse OperatorCommand::Interrupt as a user-facing stop"
---

# Stop ends in Stopped; Interrupt ends in Interrupted

## Context

The action panel has *Pause* and *Stop*. They look close, but the runner
states they lead to are different, and the rest of the system depends on that.

## Guidance

- **Pause** = `OwnerCommand::Pause`. The runner state goes `Running` →
  `PauseRequested` → `Stopped`. The work ends its step first.
- **Stop** = the same `OwnerCommand::Pause` first, then
  `OperatorCommand::StopStep`. The server sends them in this order
  (`submit_operator_command`). The runner cuts the pass and the loop closes the
  session at the safe point. The result is `Stopped` + `TaskState::Paused`.
  `StopStep` is accepted in `Running` and in `PauseRequested` (and only there).
  Having no pass to cut is a success, not an error.
- **Interrupt** (`OperatorCommand::Interrupt`) stays for old clients. It leaves
  the runner in `Interrupted` where auto-resume, loss counting
  and `not_running` apply. It also fails when no pass is active. Do not use it
  as a stop.

Why the order matters: when the runner dies while it cuts, the loss counts "from
`PauseRequested`". Nothing resumes the task by itself then. Without the pause
first, it would count as a lost task and auto-resume would start it.

`Stopped` does not hold a slot (`TaskRecord::holds_capacity`). A stop
therefore frees the slot at the safe point. "Free the slot" in the panel is for
a `Running` task that waits for the owner: it sends the pause the wait policy
would send, before its time (`TaskRecord::slot_release_pending`).

## Related

- `docs/plans/2026-11-11-001-feat-akcje-taska-pauza-stop-panel-plan.md`
