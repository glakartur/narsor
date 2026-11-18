---
date: 2026-11-17
topic: observer-queues-at-decision-gates
---

# Observer queues at decision gates: a script answers a gate like a person

## Summary

A `decision` gate can put its wait episode on an observer queue. Every
observer of that queue checks the episode on its own schedule. A script
answers the gate with the same contract as a `script` step (choice, reason,
variables). The first answer wins: the person's or a script's. Any exit from
the gate removes the episode from the queue before the next step can add a
new one. The `wait` step type, event types, event filters and
`context.event` are removed.

The owner manages observers, their scripts and their schedule (cron) in the
TUI and the WebUI. An observer runs in one of two modes: for the entries of
its queue only, or always on its schedule, also when no task waits.

---

## Problem Frame

In `wruw-change` (`review_requirements`, `review_plan`, `review_work`) and in
`workflows/compound-engineering-change.yaml` (`review_code`), the person
works on the merge request and then goes back to Narsor only to choose
`check_feedback`. An agent step then reads the merge request. The manual
choice carries no information that the merge request does not already
have.

The current `wait` step (`docs/dev/workflow-schema.md` §3.3.D) cannot
replace that click:

- A `wait` step instead of the gate removes the choices that do not come
  from the source (`revise_plan` with its note, `abandon`).
- A `wait` step before the gate still needs the manual choice after it.

The engine already treats an awaited event as the owner's choice of a way
out of a step (`docs/user/observers.md` §4). The gate is the right place
for it.

Two more problems of the current design:

- Event types and filters put decision logic into YAML (`review.approved`
  maps to a fixed label). The author of the workflow and the script should
  own that logic.
- A watch entry stays after a manual exit and is pruned only when the task
  ends. The script runs for an entry that can never matter.

---

## Actors

- **Workflow author** — puts a gate on a queue and gives the script its
  parameters.
- **Observer owner** — writes and approves the script and assigns the
  observer to a queue. Today the same person as the task owner.
- **Source observer** — an observer in mode `always`. It watches a source
  (Jira) and creates tasks, so it must run when no task exists.
- **Person at the gate** — can still choose manually at any time.
- **Observer script** — reads the source and answers the gate, or does
  nothing in this run.

---

## Key Flows

1. The runner enters the gate and reports the new wait episode in its
   snapshot, with the rendered `watch` block.
2. The control plane adds an entry `(task, episode, queue)` with the
   rendered `params`.
3. Each observer of the queue runs its script for the entry on its own
   schedule. The input has the params, the episode and the gate's choices
   and variables.
4. A script answers with `narsor-cli task decide`, or does not answer.
5. The first answer for the episode exits the gate. Later answers for the
   same episode are refused.
6. The runner reports the exit. The control plane removes the entry of the
   old episode, then adds the entry of the next gate, if any, in that
   order.

---

## Requirements

**Workflow schema**

- R1. A `decision` step (`by: human`) may have a `watch` block:

  ```yaml
  watch:
    queue: mr-review                         # plain name, no template
    params:                                  # rendered when the step starts
      mr_url: "{{ context.workspace.code.pr_url }}"
      label: "w CR"
  ```

- R2. `params` is a flat map of names to rendered text. Narsor does not
  interpret any value: not as a URL, a host or an object id.
- R3. `queue` is required in `watch`. `params` is optional. The validator
  refuses a templated queue name, and a `watch` block on a `decision` with
  `by: conductor` (the conductor always decides at once, so an entry could
  never matter).
- R4. The gate's `choices` and `variables` do not change. A gate with
  `watch` can still be answered manually with every choice.
- R5. The `wait` action type, `event_type`, `filter`, `watch.object`,
  `watch.event_types` and `context.event` are removed. Hard cutover, as in
  D64: tasks with an active `wait` step must finish before the update.

**Queues and observers**

- R6. A queue is an attribute of an observer
  (`narsor-cli observer create NAME --queue QUEUE …`), not a separate
  object. Several observers may share a queue.
- R7. A queue name is resolved in the namespace of the task's owner. An
  observer of another owner never sees the entry.
- R8. Each observer of the queue processes the queue's entries on its own
  schedule, independently of the other observers. There is no ordering
  between observers: "first wins" follows from the episode (R12).
- R9. When the queue has no active observer, the control plane writes a
  warning to the task history. The gate waits for the person.
- R10. `--allow-prefix`, `--max-object-len` and the event-type list of an
  entry are removed. The script owns all checks of its params.

**Script contract**

- R11. The script input (`contract_version: 2`) has, for a queue entry: the
  task id, the episode id and its start time, the step name, `params`, and
  the gate's choices (name, description, `requires_variables`) and
  variables (name, type, description). Whether and how the script uses the
  episode start time is up to the script.
- R12. The script answers with `narsor-cli task decide TASK --episode EP`
  and one of:
  - `--choice KEY [--reason TEXT] [--var NAME=VALUE …] [--var-file NAME=PATH …]`
  - `--data JSON` with `{choice, reason, variables}` (the D64 contract).

  There are no artifacts in this version: the answer carries the choice
  and variables only.
- R13. The control plane validates the answer as it validates the person's
  answer: the episode is the gate's current episode, the choice exists, the
  required variables are present, the variables are declared by the gate.
  A refusal is an error for the script (non-zero exit of `narsor-cli`).
- R14. Exit codes of a queue run: `0` the script answered or has nothing to
  report in this run, any other code is an error. The entry is never
  removed by an exit code (R16 removes it). An error is recorded per entry,
  as today, and does not move the observer to `error`.
- R15. The history of the task records who answered the gate: the person,
  or the observer with its run.

**Lifecycle of an entry**

- R16. Any exit from the gate (an answer of a script, a manual choice, a
  conductor decision) removes the episode's entry from the queue. The
  control plane removes it while it processes the runner snapshot that
  shows the exit, before it adds an entry for a later gate from the same
  snapshot.
- R17. Pause, `Interrupted`, auto-resume and a runner restart do not end
  the episode and do not remove the entry.
- R18. Before it runs a script for an entry, the control plane checks that
  the task still waits in that episode. If not, it removes the entry and
  does not run the script (a guard for R16).
- R19. Finished, archived and missing tasks lose their entries, as today.
- R20. Effects in the source (a label, a comment) are the responsibility of
  the workflow and the script. Narsor does not undo them.

**TUI and CLI**

- R21. The task view does not show the state of observer runs. Observer
  runs report nothing to the task or its runner. The task view shows at
  most one icon while the task's current episode has a queue entry. The
  control plane knows this from its own entries; the icon needs no
  report from a run.
- R22. The history (Przebieg) shows who answered the gate: the person, or
  `observer gitlab-mr` with its run (R15), and the reason the script gave.
  This is the existing answer event, not a new report.
- R23. A new **queues view** in the TUI and the WebUI, for the owner:
  - the list of the owner's queues, each with the number of entries;
  - for a queue: its observers (name, approved script version, state,
    interval, last run), and its entries (task, step, episode, start
    time, `params`);
  - for an entry: per observer, the processing state when one is
    available (last run time, last error of this entry). The control
    plane already keeps the newest error of each watched object; this
    becomes the newest error of each entry and observer.
  The view reads the control plane's observer store only.
- R24. The CLI has the same data: `narsor-cli observer queue list` and
  `narsor-cli observer queue show QUEUE`.

**Run mode and schedule**

- R25. Every observer has a run mode:
  - `on_episodes`: the script runs only for entries of its queue. No entry,
    no run (R8, as today's watch-list mode).
  - `always`: the script runs on every tick of its schedule, with or
    without tasks, and has no entry in its input (as today's mode of
    scripts that create tasks).
- R26. A queue is required for every observer. All observers of one queue
  have the same mode: the control plane refuses a create or an edit that
  mixes modes. In effect the mode is a property of the queue, but the queue
  stays a name and not an object (R6).
- R27. When a gate's `watch.queue` names a queue whose observers are all
  `always`, the control plane writes a warning to the task history, as in
  R9. The gate waits for the person. An `always` run cannot answer a gate.
- R28. The schedule is a property of the observer (see Key Decisions). It is
  one of:
  - a cron expression of five fields (minute precision), evaluated in UTC;
  - an interval in seconds (the existing setting, kept for existing
    observers).

  Exactly one of the two is set. An expression with a seconds field, or
  one that can fire more often than every `MIN_INTERVAL_SECS` (60 s), is
  refused.
- R29. A tick that finds the observer's previous run still active, or the
  observer in failure backoff, does not start a run (one run at a time, as
  today). Backoff and the `error` state do not change.
- R30. `on_episodes` observers keep the round rule of today: at a tick the
  observer checks the entries present at that time, one entry per run. No
  entries at a tick: no run, no error.

**Managing observers in the TUI and the WebUI**

- R31. The queues view (R23) lets the owner manage observers, not only read
  them. In the TUI and the WebUI the owner can:
  - create an observer: name, source, queue, mode, schedule, first script;
  - edit queue, mode and schedule of an observer;
  - edit the script: a save makes a new version;
  - approve a version, pause, resume;
  - delete an observer.
- R32. A save never approves a version (existing rule). The view shows the
  approved version and the newest saved version, and marks the observer when
  they differ. Approval is a separate, explicit action and shows the script
  text before the owner confirms.
- R33. Delete asks for a confirmation that names the observer, because it
  removes the scripts, run errors and entries and cannot be undone.
- R34. Other observer settings that `observer create` accepts today (secret
  references, task limit, task-creating workflow mapping, allowlists
  removed by R10) are shown read-only in the view. The owner sets them in
  the CLI. A source observer made in the view therefore needs the CLI for
  its workflow mapping.
- R35. The CLI gets the same actions: `observer create` takes `--queue`,
  `--mode` and `--cron` (and keeps `--interval`), and a new
  `observer update NAME` changes queue, mode and schedule. The TUI, the
  WebUI and the CLI use the same owner routes. An owner sees and changes only
  their own observers (R7).

---

## Acceptance Examples

- AE1. `review_work` has `watch: {queue: mr-review, params: {mr_url, label}}`.
  A workflow step before the gate adds the label. The reviewer removes the
  label. The script sees this and runs
  `task decide … --choice check_feedback --reason "label removed"`. The
  gate exits to `check_work_feedback`; the entry is gone.
- AE2. The person chooses `revise_plan` manually while the entry is on the
  queue. The entry is removed. A script run that started before the
  removal answers with the old episode and is refused.
- AE3. The task loops `check_work_feedback` → `work_revision` → … →
  `review_work`. The new episode gets a new entry. The old entry is already
  gone, so no answer can reach the new episode from the old one.
- AE4. Two observers share the queue `mr-review` (GitLab and GitHub). Only
  the GitHub one answers, because only its script recognizes the URL. The
  GitLab script exits `0` without an answer each run.
- AE5. The task is paused while it waits at the gate. The entry stays. A
  script answer is queued and resumes the task, as an answer of the person
  does today.
- AE6. **Covers R25, R28.** Observer `jira-new` has queue `jira-intake`,
  mode `always`, cron `*/10 * * * *`. No task exists. The script runs at
  :00, :10, :20 … and creates tasks from new Jira issues.
- AE7. **Covers R25, R30.** Observer `gitlab-mr` has mode `on_episodes` and
  cron `*/5 * * * *`. At 10:00 no entry is on the queue: no run. At 10:05
  one entry is on the queue: one run for that entry.
- AE8. **Covers R26.** Queue `mr-review` has an `on_episodes` observer. The
  owner creates an `always` observer in the same queue. The control plane
  refuses it and names the mode of the queue.
- AE9. **Covers R28.** The owner enters `* * * * * *` or `*/30 * * * * *`.
  The view refuses it with the reason. `* * * * *` is accepted.
- AE10. **Covers R31, R32.** The owner edits the script of an active
  observer in the WebUI. Version 3 is saved. The schedule continues to run
  version 2, and the view marks "version 3 not approved" until the owner
  approves it.
- AE11. **Covers R27.** A gate has `watch: {queue: jira-intake}`, and
  `jira-intake` has only `always` observers. The task history has a warning
  and the gate waits for the person.

---

## Success Criteria

- A person at a review gate does not click `check_feedback`; the observer
  does it, and the history shows who did it.
- The owner sets up a queue, its script and its schedule in the TUI or the
  WebUI without a text editor on the controller host, and sees which version
  runs.
- A Jira-style source observer runs with no tasks.
- Planning needs no new product decision: modes, schedule rules and the
  limits of the view are set here.

---

## Scope Boundaries

- No separate queue object, and no schedule on the queue (see Key Decisions).
- No time zones other than UTC, no cron aliases (`@daily`) and no run on
  demand ("run now") in this version.
- No script dry run, no script diff view, no history of past versions beyond
  the list of saved versions.
- No view of every observer setting: secrets, limits and workflow mapping
  stay in the CLI (R34).
- No change to who may approve a script: the owner.

---

## Key Decisions

- **The schedule belongs to the observer, not to the queue.** Decided by
  the person (PR thread Q1). The request was a queue schedule; we argued
  against it, for these reasons:
  - The observer already has a schedule (`interval_secs`), a script with
    versions, an approval and a failure backoff. A queue schedule would
    need a second place for each of them, or a queue object (against R6).
  - Observers of one queue serve different sources (AE4: GitLab and
    GitHub). They have different API limits, so they need different
    periods.
  - R8 says each observer runs independently. One queue schedule would
    couple them: one slow script would hold the tick of the others.
  - A queue has no owner action of its own (create, approve, pause). The
    observer has all of them.

  The view groups observers by queue, so the owner still sees "the queue's
  schedule" as the list of its observers' schedules. If the person still
  wants one schedule per queue later, the cost is a queue object (name,
  mode, schedule), and R26 then becomes simpler.
- **The run mode, not the schedule, separates Jira from MR review.** One
  scheduler serves both. `always` runs on each tick. `on_episodes` runs
  only when entries exist. Both use the same cron setting.
- **The mode is set on the observer, and must match inside a queue (R26).**
  A mode on the queue alone would need a queue object. A free mix would make
  "no entry, no run" and "always run" meet in one queue with unclear
  results.
- **Cron plus the existing interval.** Existing observers keep working with
  no migration of their schedule. Cron adds the schedules an interval cannot
  give (every working day at 8:00).
- **The view manages, the CLI keeps the rare settings (R34).** This keeps
  the first version small. The view does not copy every option of
  `observer create`.
- **The gate, not a separate step type.** One mechanism for "a person or
  an automation decides". The manual way out always stays.
- **The script decides everything.** No allowlist of choices for scripts,
  no event-type mapping in YAML. The owner approves the script, so the
  owner is responsible for what it chooses.
- **Queue as an observer attribute.** No new entity to manage. Several
  sources can serve one gate.
- **The episode is the key.** It already exists for the gate
  (`pending_input_id`) and already makes the runner refuse a stale answer.
- **Narsor cleans only its own entries.** Source-side effects belong to the
  workflow and the script.

---

## Dependencies / Assumptions

Verified in code:

- An answer is queued in the control plane bound to the request id
  (`orchestration/runner_state.rs`, `queue_input`: `delivery.episode =
  pending_input_id`). The runner's engine refuses an answer for an older
  request. "First wins" already holds per episode.
- The control plane learns that a wait began from the runner snapshot
  (`runner_channel/runner_poll.rs`, `began_wait` → `register_wait_watch`).
  The same handler is the place for R16.
- Today entries are pruned only for ended tasks (`observers/schedule.rs`,
  `prune_watches`).

Assumptions:

- The schedule today is `interval_secs` on the observer (60 s to 30
  days). No cron support exists in the workspace manifests.
  (`crates/narsor-engine/src/observers/model.rs`, `Cargo.lock`)
- Today's modes already exist as `WatchMode::All` and
  `WatchMode::WatchList` (same file). R25 renames and keeps them.
- The owner routes of the observers have create, list, show, pause, resume,
  delete, script, approve and watch. A route that edits queue, mode or
  schedule was not found. (`crates/narsor-server/src/controller_protocol/observer_http.rs`;
  verified by a route search, not by a full read.)
- Neither the TUI (`src/tui`) nor the WebUI (`crates/narsor-web/ui`) has an
  observer view today.
- Safe assumptions taken without asking the person (change them if wrong):
  - Cron is in UTC, five fields.
  - A queue is required for every observer. Existing observers without a
    queue get one named after the observer.
  - An `always` run cannot answer a gate.
  - The view does not edit secrets, limits or workflow mapping.
- A queued answer of a script resumes a paused task through
  `resume_for_work`, as the person's answer does.
- `task decide` reports `delivered` when the answer is queued, not when
  the runner applied it. The script cannot know synchronously that it won;
  R16 and R18 make this harmless.

---

## Outstanding Questions

### Resolved

- Q1: the schedule lives on the observer, not on the queue (decided by the
  person; Key Decisions).
- `watch` applies only to `by: human` (R3).
- `task decide` carries no artifacts in this version (R12).
- TUI: no run state on the task, at most an icon; a separate queues view
  in the TUI and the WebUI, and the same data in the CLI (R21–R24).

### Deferred to Planning

- [Technical] Which cron parser to use, and how the next tick combines with
  failure backoff and a run still active (R29).
- [Technical] How the TUI edits a multi-line script (an external editor,
  or an in-app text area), and how the WebUI shows approved against newest
  version.
- [Technical] The owner route and wire type for editing queue, mode and
  schedule (R35), and the check of R26 under concurrent creates.
- [Technical] Migration of existing observers: queue name, mode from
  `watch_mode`, and the interval kept as the schedule.
- Migration of the observer store: existing watch lists, `watch_mode`,
  `effect: deliver_event`, and the example scripts
  (`github-pr-review.sh`, `gitlab-mr-review.sh`).
- Removal of the runner protocol parts for `wait` (`events` in the poll
  answer, `/runner/v1/events/{id}/ack`, `event_wait` in the snapshot), and
  the update order of runner and control plane.
- A new decision record in `docs/dev/architecture.md` that supersedes the
  `wait` part of the observers design.
