---
title: "A step choice queued for the next launch must run before the first pass, and the step move and its note must be saved as one unit"
date: 2026-11-25
category: logic-errors
module: narsor-runner, narsor-exec, narsor-engine
problem_type: logic_error
component: development_workflow
severity: medium
applies_when:
  - "An operator command (SelectStep, Transition) is queued for the next launch generation of the runner"
  - "A runner start-up path calls go_on() or recover() and reads queued commands afterwards"
  - "A command must change the current step and attach text that only the new step may consume"
tags: [runner, select-step, operator-command, comment, note, start-up-order, atomic-save]
---

# Queued step move first, then the pass; move and note in one save

## Context

PR #145. The owner stops a task, chooses a step in *Wybierz krok*, types a comment, and the agent of that step never sees it. The TUI and Web UI send the comment correctly. The loss was in the runner and the engine.

## Root cause

1. **Order at start-up.** For a cleanly closed session `start_work` called `go_on()` before it read the queued commands. A task stopped at a safe point between steps is `Paused`, so `go_on()` started a pass of the old step. `SelectStep` then arrived, found an active pass and was refused. The 2026-11-11 plan already required "apply the command before the pass starts", but no test covered Stop -> relaunch -> `SelectStep`.
2. **Move first, note second.** `select_step` moved the step and then wrote the note. A failed note write was only logged, so the step started without the comment. After the move, a failed note write cannot undo it, so "no silent loss" cannot hold with this order.
3. **Untargeted note.** `operator_recovery_note` is consumed by whichever agent step runs next. A note sent for a non-agent target leaked into a later agent step.

## Guidance

- Keep the queued commands from the first start-up poll in a session field. In `start_work`, settle each queued step-moving command (`SelectStep`, `Transition`) through `settle_command` before any pass starts. Do not acknowledge there; the next `drive()` poll replays the stored verdict.
- If a queued move is refused, close the session at the safe point. Do not start a pass of the old step.
- Store the move and the note in one engine operation with one `save_execution`. Run every check that can refuse before the first write, so a failed save leaves the execution record unchanged.
- Bind the note to its target state (a separate metadata entry with state id and text). `prepare_harness_attempt` takes it only for that state, and a later manual transition to another state drops it. Do not reuse the untargeted key.
- Refuse a non-blank note for a non-agent target on the server (`note_not_allowed`), using the same step-kind helper as the offer, so the offer and the check cannot drift. See [[offer-and-consumer-must-share-one-predicate-comment-after-loss-2026-11-22]].
- Test the owner's path (stop at a safe point, relaunch with the command queued), not only a live runner. The old test `a_chosen_step_reopens_a_finished_workflow_and_carries_the_note` passed while the real path failed.

## Prevention

Any new queued operator command that changes execution state must be settled before `go_on()`/`recover()` in `start_work`. Add a start-up test with the execution `Paused` and the command queued.
