---
date: 2026-11-20
topic: archive-task-with-run-snapshot
---

# Archive a Finished Task with Its Header and Run Snapshot

## Summary

The owner can archive a task only after its execution reaches a terminal state of its workflow. At that moment the control plane freezes a snapshot: the full task header and one header row per step (status, times, harness, model, effort, tokens, cost). The TUI and Web UI history and `narsor-cli task show` show this snapshot for an archived task.

---

## Problem Frame

Today *Archiwizuj* (archive) is offered in every task state ("archive wins", D66). Archiving deletes the workload and the task storage. The control plane keeps only the `TaskSummary` and the archive tombstone. In the history (`H` in the TUI, *History* in the Web UI) the *Przebieg*, *Rozmowa*, *Zdarzenia* and *Dyrygent* tabs only say that the storage was deleted (`src/tui/task_app/history.rs`).

So after archiving, the owner loses what the task did and what it cost: which steps ran, with which harness and model, how long they took, how many tokens they used and how much each one cost. The step data lives only in runner storage and is read through the runner (D63), so it disappears with the storage.

Because archive is also offered for running, stopped and failed tasks, the history mixes finished work with abandoned work, and a task can be archived before its run is complete.

---

## Requirements

**Archive availability**
- R1. The archive action is available only when the control plane knows that the task's execution reached a state that its workflow defines as terminal (`terminal_states`: completed, failed or cancelled).
- R2. In every other task state the archive action is unavailable, with a reason. The TUI panel, the Web UI and `narsor-cli task archive` respect the same rule, because the backend decides availability (D66).
- R3. A task that is stuck outside a terminal state (error, stopped, interrupted, exhausted resumes) reaches a terminal state through a forced manual transition, and only then can be archived. No separate delete action is added.

**What the snapshot keeps**
- R4. The snapshot keeps the full task header as the task details show it while the task is active: ticket, title, workflow and version, workspace rows (repository, branch, base branch, `+N −M`, PR), final status, and total cost.
- R5. The snapshot keeps one header row per step of the run, in run order, as the *Przebieg* tab shows it before the owner opens a step: step label and visit, action type, who decided, status, start and end time, error text, harness, model, effort, token counts, subagent counts, step cost and conductor cost.
- R6. The snapshot does not keep the detail inside a step (attempts, sessions), the conversation, the events or the conductor history. These go away with the storage, as today.

**Where the snapshot comes from**
- R7. The run data reaches the control plane together with the terminal state of the execution. The archive does not read task storage to build the snapshot.
- R8. The archive freezes the snapshot in the archived task record before the storage is deleted. The snapshot does not change after the archive.

**Showing archived tasks**
- R9. The TUI history shows, for an archived task, the snapshot header and the *Przebieg* tab with the step header rows. Steps cannot be opened.
- R10. The Web UI history shows the same header and step rows.
- R11. `narsor-cli task show` for an archived task shows the same header and step rows.
- R12. A task archived before this change has no frozen snapshot. The history and the CLI show the last run timeline its runner reported, which the record still holds (changed during planning). Only a task without any reported timeline shows the "storage deleted" text.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Given a task that waits for a human, when the owner opens the action panel, *Archiwizuj* is unavailable and shows the reason. `narsor-cli task archive` on that task is refused with the same reason.
- AE2. **Covers R3.** Given a task in error with exhausted resumes, when the owner forces a manual transition to the workflow's cancelled state, the archive becomes available, and the run snapshot includes the forced transition.
- AE3. **Covers R4, R5, R8, R9.** Given a completed task with three steps, two of them agent steps with known cost, when the owner archives it and opens it in the history, the header matches what the active task showed, and *Przebieg* lists the three steps with their harness, model, tokens and cost.
- AE4. **Covers R12.** Given a task archived before this change, when the owner opens it in the history, *Przebieg* shows the last timeline its runner reported; *Rozmowa*, *Zdarzenia* and *Dyrygent* say the storage was deleted.

---

## Success Criteria

- The owner can see, for any task archived after this change, what each step was and what it cost, without the task storage.
- The history contains only tasks whose workflow ended.
- Planning does not need to decide which fields the header and step rows hold or when archive is allowed.

---

## Scope Boundaries

- Keeping the run data in the control plane up to date while the task still works (before a terminal state) — a separate task.
- Keeping the conversation, events, conductor history or step detail after archive.
- A delete action for tasks outside a terminal state.
- Cost aggregation or export across archived tasks.

---

## Key Decisions

- Terminal state comes from the workflow definition: the workflow, not the runner state, says when a task's work is done.
- Stuck tasks go through a forced manual transition: forcing it is always possible, so no extra action is needed, and the run shows how the task ended.
- Step header rows only: they carry the cost and model information the owner needs; step detail would bring back the size and secret-exposure concerns of copying storage.
- The snapshot arrives with the terminal state, not by a read at archive time: no dependency on a read-only runner starting, no "incomplete" variant.

---

## Dependencies / Assumptions

- A forced manual transition to a terminal state is possible for every non-terminal task, including tasks in error without a live runner (confirmed by the user).
- Changes to the archive rule reverse part of D66 ("archiving is offered wherever the control plane accepts it"); planning records this as an architecture decision.
- Older clients receive the snapshot as new, optional data and keep showing archived tasks as today.
