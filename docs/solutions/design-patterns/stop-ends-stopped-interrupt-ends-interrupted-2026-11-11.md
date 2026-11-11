---
title: "Stop ends in Stopped; Interrupt ends in Interrupted — do not mix them"
date: 2026-11-11
category: design-patterns
module: orchestration / runner / tui
problem_type: design_pattern
component: backend
severity: medium
last_updated: 2026-11-11
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

`Stopped` after a pause or a stop can still hold a slot (decision Q5, option B).
`TaskRecord::holds_capacity` returns true for `Stopped` while `slot_hold_until`
is set. The hold ends at the wait deadline, on `ReleaseSlot` or on an archive.
A resume needs no new slot then. The field has `#[serde(default)]`, so old
records load without a hold. The panel offers `ReleaseSlot` for such a task.
For a `Running` task that waits for the owner, `ReleaseSlot` sends the pause
the wait policy would send, before its time (`TaskRecord::slot_release_pending`).
An earlier plan said "a stop frees the slot at the safe point". That was
replaced by the review decision; do not restore it.

## Compatibility: new variants have no serde fallback (decision)

`OperatorCommand::StopStep`, `OperatorCommand::SelectStep` and
`OwnerCommand::ReleaseSlot` have no `#[serde(other)]` fallback. An old runner
rejects them when it reads them. This differs on purpose from the rule
"give every wire enum a fallback" in
`docs/solutions/architecture-patterns/auto-resume-policy-for-lost-workloads-2026-10-11.md`.
The operator chose: update runners together with the controller. Upgrade order:
server first, then runners and controller together, then TUI clients. Rolling
the controller back after it stored records with these variants is not
supported. The order is in the plan and in `docs/operations/remote-backends.md`.
`TaskActionId` keeps `Unrecognized`, so an old TUI reads a new action safely.

Keep `OperatorCommand::Interrupt` and its TUI handling (`command_name`). The CLI
`task interrupt`, the server and the runner still produce it. It is not dead code.

## Related

- `docs/plans/2026-11-11-001-feat-akcje-taska-pauza-stop-panel-plan.md`
