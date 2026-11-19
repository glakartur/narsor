---
date: 2026-11-18
topic: catalog-automations-and-queue-entry-exit-scripts
---

# Automations in the catalog, with entry and exit scripts for queues

## Summary

Automations and queues become files in the catalog, as workflow
definitions are. The catalog is the only source of their configuration. An
automation has a required `check` script and two optional scripts: `in`
runs when a task's gate episode enters a queue, `out` runs when it leaves.
When an automation has `in` or `out`, the gate step waits for them to
succeed. A failed `in` gives the step the status `failure` with retries; a
failed `out` is retried, then logged and ignored.

---

## Problem Frame

The observer queues of `docs/brainstorms/2026-11-17-observer-queues-at-decision-gates-requirements.md`
are implemented. Two problems remain.

**The source handling is split between the workflow and the observer.**
In AE1 of that document, a workflow step adds the label `w CR` to the merge
request, and the observer script waits until the reviewer removes it. The
workflow must know how the source works (a label, its name, the API), and
R20 leaves the cleanup to the workflow. Each workflow that uses the queue
repeats these steps, and a manual exit leaves the label on the merge
request.

**The configuration lives in the state of one installation.** Observers,
their script versions and their approvals are records in
`{state}/observers/`, created and edited through the CLI, the TUI and the
WebUI. A team cannot review an automation in a repository, share it, or
deploy it the way it shares workflow definitions in the catalog. The
long-term goal is a team repository that holds the whole catalog
(workflows, automations, queues), similar to Claude Code plugins.

---

## Actors

- A1. **Catalog author** — writes automations and queues as files, reviews
  them in a repository, and deploys them with the rest of the catalog.
- A2. **Workflow author** — puts a gate on a queue and gives it parameters.
  Does not know how the queue handles the source.
- A3. **Person at the gate** — can choose an exit manually at any time,
  also when the step has the status `failure`, and can retry the step.
- A4. **Narsor process user** — the user in whose context Narsor runs
  workflows. Queue processing runs in the same context.

---

## Key Flows

- F1. Merge request review with a label
  - **Trigger:** a task enters the gate `review_work` with
    `watch: {queue: mr-review, params: {mr_url}}`.
  - **Actors:** A2 (wrote the gate), A3, A4.
  - **Steps:** the control plane adds the queue entry and runs `in` of each
    automation of the queue that has one; `in` adds the label. When all
    `in` scripts succeed, the step is in progress and `check` runs on its
    schedule. The reviewer removes the label; `check` answers the gate.
    The gate runs `out` of each automation that has one (removing a label
    that is already gone is a success). When all `out` scripts succeed, the
    next step starts.
  - **Outcome:** the workflow has no label step; the label exists exactly
    while the task waits at the gate.
  - **Covered by:** R5, R6, R8–R11, R13.
- F2. A failing entry script
  - **Trigger:** `in` exits with an error (the source API is down).
  - **Actors:** A3, A4.
  - **Steps:** the gate step gets the status `failure`. Narsor retries
    `in` a limited number of times; each attempt is in the step's log, not
    a new step. A successful retry puts the step back in progress. While
    `in` has not succeeded, `check` does not run for the entry. The person
    can choose an exit manually, or retry the step from the beginning.
  - **Outcome:** a missing entry effect never looks like a reviewer's
    action.
  - **Covered by:** R12, R14, R15, R16.

---

## Requirements

**Catalog**

- R1. Automations and queues are files in the catalog, next to workflow
  definitions. Proposed layout (the exact file names are for planning):

  ```text
  {catalog}/automations/{name}/   -- one automation: its definition file and scripts
  {catalog}/queues/{name}.yaml    -- one queue: automations, mode, schedule
  ```

- R2. The catalog is the only source of automation and queue
  configuration. A queue in the catalog is active; there is no enable
  step, no approval of a script version and no per-owner copy. Like a
  workflow, a change in the catalog applies to the next run, not to a run
  in progress.
- R3. An automation definition has: a description, the `check` script
  (required), the `in` and `out` scripts (each optional), the params it
  reads, and the names of the secrets it needs.
- R4. A queue definition has: its name, the automations that serve it, the
  run mode (`on_episodes` or `always`, as R25 of 2026-11-17), the schedule
  (cron or interval, as R28 of 2026-11-17), and, for `always` queues that
  create tasks, the workflow mapping that `observer create` takes today.
  The schedule and the mode belong to the queue, so R26 of 2026-11-17
  (mode must match inside a queue) holds by construction.
- R5. Secret values are never in catalog files. Scripts get secrets the
  same way workflow processing does, in the context of the Narsor process
  user (A4).
- R6. Queue processing runs in the context of the Narsor process user. A
  task created by an `always` queue belongs to that user.
- R7. The catalog validator refuses: a queue that names a missing
  automation, an automation without `check`, a schedule that breaks the
  2026-11-17 rules, and secret values in a definition file.

**Entry and exit scripts**

- R8. When a gate episode enters a queue, the control plane runs `in` of
  each automation of the queue that has one, at once (not on the queue
  schedule). The input is the same as for `check` for this entry (params,
  task, episode, step, choices, variables).
- R9. The gate step is in progress only when every existing `in` has
  succeeded. `check` does not run for the entry before that.
- R10. When the gate exits (an answer of `check`, a manual choice, a
  conductor decision), the gate runs `out` of each automation of the queue
  that has one, at once. The next step starts only when every existing
  `out` has ended: succeeded, or failed after its last retry (R15).
- R11. The input of `out` also says how the episode ended: the choice, and
  who chose it (the person, an automation with its run, or the conductor).
- R12. A failed `in` gives the gate step the status `failure`. Narsor
  retries the script a limited number of times. Each attempt is in the
  step's log; no new step is created per attempt. A successful retry puts
  the step back in progress.
- R13. `in` and `out` must be idempotent. Narsor can run them again (R12,
  R16), and `out` can find the effect already gone (the reviewer removed
  the label).
- R14. While the step has the status `failure`, the person can choose any
  exit of the gate manually. A manual exit after a failed `in` still runs
  `out`.
- R15. A failed `out` is retried a limited number of times. When the last
  retry fails, Narsor records the problem in the task history (Przebieg)
  and continues to the next step. A failed `out` never gives the step the
  status `failure` and needs no action of the person.
- R16. The person can retry the gate step from the beginning. Narsor first
  runs `out` for the old episode, then starts a new episode and runs `in`.
- R17. An automation without `in` and `out` behaves as a queue does today:
  the gate is in progress at once and continues at once after an exit.

**Task end and pause**

- R18. When a task ends while it waits at a gate (cancel, archive, delete),
  Narsor runs `out` once in the background. It does not block the end of
  the task. A failure is recorded in the task history.
- R19. Pause, `Interrupted`, auto-resume and a runner restart do not run
  `in` or `out` (as R17 of 2026-11-17 for the entry).

**Views and CLI**

- R20. The task view shows the `failure` status of a gate step caused by
  `in` and the attempts of `in` and `out` in the step's log. This replaces R21 of
  2026-11-17 for these two scripts. `check` runs still report nothing to
  the task.
- R21. The queues view (TUI, WebUI, CLI) is read-only. It shows the queues
  and automations from the catalog, the entries, the runs and the errors
  from the state. Creating, editing, approving and deleting observers is
  removed (R31–R35 of 2026-11-17). Pause and resume of a queue are removed
  too; the catalog author removes the queue from the catalog instead.

**Cutover**

- R22. Hard cutover. Observers stored in `{state}/observers/`, their script
  versions and approvals, the `observer create/update/script/approve/
  pause/resume/delete` commands and the owner routes behind them are
  removed. The runtime state (entries, runs, errors, backoff) stays in the
  state directory, keyed by queue and automation. No migration of existing
  observers: the catalog author writes them again as files.

---

## Acceptance Examples

- AE1. **Covers R8, R9, R10, R13.** Queue `mr-review` has automation
  `gitlab-mr-label` with `in`, `check`, `out`. A task enters `review_work`.
  `in` adds `w CR`. The reviewer removes it. `check` answers
  `check_feedback`. `out` finds no label and exits `0`. The next step
  `check_work_feedback` starts.
- AE2. **Covers R12, R14.** `in` fails three times; the step shows
  `failure` and three attempts in its log. The fourth attempt succeeds; the
  step is in progress and `check` starts to run for the entry.
- AE3. **Covers R14.** `in` keeps failing. The person chooses
  `revise_plan` manually. `out` runs, then the next step starts.
- AE4. **Covers R10, R16.** The task loops `review_work` →
  `check_work_feedback` → `work_revision` → `review_work`. `out` of the
  first episode removes the label before `check_work_feedback` starts, so
  `in` of the second episode cannot run before it.
- AE5. **Covers R15.** `out` keeps failing (token expired). After the last
  retry the history records the problem and the next step starts. The
  label stays on the merge request.
- AE6. **Covers R18.** The task is cancelled while it waits at
  `review_work`. `out` runs once in the background; the task ends at once.
- AE7. **Covers R17.** Queue `jira-review` has an automation with `check`
  only. The gate behaves as today: no wait on entry or exit.
- AE8. **Covers R2, R7.** The catalog author adds a queue that names a
  missing automation. The validator refuses it; the other queues keep
  running.

---

## Success Criteria

- The `wruw-change` and `compound-engineering-change` workflows have no
  label steps: a gate names the queue and its params, and the label is on
  the merge request exactly while the task waits.
- A team shares an automation by copying (later: pulling) catalog files.
  No CLI or TUI step is needed after deploying the catalog.
- A failing source API at entry is visible on the task step, and the
  person can always leave the gate. A failing exit never stops the task.
- Planning needs no new product decision: optional scripts, failure
  behavior, cutover and views are set here.

---

## Scope Boundaries

- The "plugin" mechanism (a catalog from a separate repository, its
  versions and its fetching) is deferred. This document only sets a
  catalog layout that such a mechanism can use.
- No per-owner queues, no enable step and no script approval.
- No editing of automations or queues in the TUI or the WebUI.
- No hooks other than `in` and `out` (for example on pause or resume).
- No migration of observers stored in the state.

---

## Key Decisions

- **The catalog is the only source, like workflows.** A team reviews
  automations in a repository and deploys them with the catalog. Review in
  the repository replaces approval in Narsor. This reverses R31–R35 of
  2026-11-17 (management in the views), decided by the person.
- **Schedule and mode move from the observer to the queue.** 2026-11-17
  put the schedule on the observer because the queue was not an object. A
  queue file is now an object, so the rejected alternative of that
  document becomes the simple one (as its Key Decisions predicted). Two
  sources with different API limits get two queues.
- **`in` and `out` run inside the gate step, synchronously.** The next step
  waits for `out`, so `out` of one episode always ends before `in` of the
  next. Without that, a review loop could remove the label of the new
  episode.
- **Script failures are visible on the task.** The person must see why the
  automation does not work and must be able to leave the gate. This
  replaces R21 of 2026-11-17 for `in` and `out` only.
- **A failed `out` is logged and ignored.** Decided by the person: a stale
  label is a smaller problem than a failure flow for the exit.
- **`check` waits for `in`.** A missing label must not look like a
  reviewer's action.
- **`in` and `out` are optional.** A queue that only reads the source needs
  neither.
- **Task end does not wait for `out`.** Cancel and archive must stay
  immediate; `out` there is a best effort.

---

## Dependencies / Assumptions

- Verified: observers and versioned scripts are stored today in
  `{state}/observers/{id}/` (`crates/narsor-engine/src/observers/store.rs`),
  and the catalog holds workflows, the repository registry, backends and
  profiles (`crates/narsor-engine/src/environment/catalog.rs`). The TUI has
  a queues view with forms (`src/tui/queues/`).
- Verified: workflow secrets come from the environment or a mounted
  `.env` (`docs/user/configuration.md`). Observer secrets today come from
  `{secrets directory}/{owner}/{NAME}` (`docs/user/observers.md` §9). R5
  aligns automations with the workflow way.
- Assumption: the installation runs in the context of one user (A4). An
  installation with several owners would need the owner question reopened.
- Unverified assumption: a "retry step from the beginning" action for a
  gate step exists or is planned. A search of `docs/user/tui.md` did not
  find it. If it does not exist, R16 adds it.
- Assumption: the default number of retries and the backoff of `in` and
  `out` are a planning decision.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R12][Technical] Retry count, backoff, and the timeout of `in`
  and `out`, and whether the catalog can override them per automation.
- [Affects R8, R10][Technical] Where `in` and `out` run: in the observer
  worker (as `check` does today) or elsewhere, while keeping "at once" and
  the order of R10. "At once" needs a server-to-worker signal instead of
  the 10-second poll.
- [Affects R1, R2][Technical] How the control plane reads catalog changes
  (on each tick, like workflow lookup, or on change) and what happens to a
  queue removed while it has entries.
- [Affects R5][Needs research] How a script names a secret so that the
  same catalog works in a local daemon and in a Kubernetes deployment.
