---
date: 2026-11-22
topic: refresh-task-environment
---

# Refresh the environment of an existing task

## Summary

A manual "refresh environment" action (CLI, TUI and web) resolves a task's
whole environment again from the current catalogue: `runtime.yaml`, the
profile, images, runtime bundle, tools, secret references and the task image.
The workflow stays as it was sealed. The action works on the local and the
Kubernetes backend, only while the task has no live workload. `task show` and
the TUI mark a task whose environment differs from the catalogue.

---

## Problem Frame

A task seals its environment once, at creation (KTD8 in
`docs/plans/2026-11-21-001-feat-local-docker-work-mode-plan.md`). A later
catalogue edit changes only tasks created after it. This protects a running
task from a bad catalogue edit, but it has two costs today:

- A task created before `task_image` existed in `runtime.yaml` can never run
  in the Docker work mode. Provisioning fails five times and gives up; the
  error tells the operator to create the task again. Archiving loses the
  task's progress, workspace and history. This happened to a real task the
  day this document was written.
- A new component or secret added to `runtime.yaml` (for example a CLI tool
  and its token) reaches only new tasks. A long task that needs the tool must
  be abandoned and started again.

Secret *values* already rotate without this work: the sealed environment holds
only secret names and delivery, and the controller reads the values from the
token store at every workload start. Component *versions* also already
refresh (the image rebuild driven by `version` scripts). What is frozen is the
definition: which components, which scripts, which secrets, which base, which
profile.

---

## Actors

- A1. Task owner: runs the refresh and reads the drift indicator.
- A2. Control plane: resolves the environment from the catalogue and stores it
  on the task.
- A3. Controller and runner: start the next workload from the refreshed
  environment.

---

## Key Flows

- F1. Refresh a stopped task
  - **Trigger:** A1 sees the drift indicator, or a provisioning error that
    names a missing task image, and runs the refresh.
  - **Actors:** A1, A2, A3
  - **Steps:** A1 pauses the task if it runs. A1 runs the refresh. A2 resolves
    the environment from the current catalogue, checks it, stores it and
    records an event with the difference. A1 resumes the task. A3 builds or
    pulls what the new environment needs and starts the workload on the
    task's existing storage.
  - **Outcome:** The task continues its workflow from where it stopped, with
    the new environment.
  - **Covered by:** R1, R2, R4, R5, R6, R7, R9

---

## Requirements

**The refresh action**
- R1. The owner can refresh the environment of one task from the CLI, the TUI
  and the web interface.
- R2. The refresh resolves every part of the environment again from the
  current catalogue: task image, base and step images, runtime bundle, tools,
  profile and secret references. The workflow definition and its version do
  not change.
- R3. The refresh works for tasks on the local backend and on the Kubernetes
  backend, with the same rules as task creation on that backend.
- R4. The refresh is refused while the task has a live workload. The message
  tells the owner to pause the task first.
- R5. The refresh is refused for an archived or finished task.
- R6. The refresh is all or nothing. If the catalogue is invalid or a
  referenced entry is missing, the refresh is refused with the reason, and the
  task keeps its previous environment unchanged. The refresh does not check
  the controller's token store (the control plane cannot see it): a secret
  missing from the store is reported at the next workload start by the
  controller's preflight, and the owner fixes it with `narsor-cli secret set`
  and resumes.
- R7. The refresh writes the new environment at once, without a preview or a
  confirmation step.
- R8. When the resolved environment equals the stored one, the refresh
  reports that nothing changed and records nothing.

**Audit**
- R9. A successful refresh records a task event that names who refreshed and
  the difference: added, removed and changed components, secrets, images,
  bundle, tools and profile, plus the old and new environment identity.

**Drift indicator**
- R10. `task show`, the TUI and the web task view show when a task's
  environment differs from what the current catalogue would resolve, and
  name the refresh action.
- R11. When the current catalogue cannot be resolved (invalid file, missing
  entry), the indicator says that the catalogue has a problem, not that the
  task drifted.

**Existing messages**
- R12. The provisioning error for a Docker-mode task without a task image
  points to the refresh action instead of "create the task again".

---

## Acceptance Examples

- AE1. **Covers R2, R6, R12.** Given a paused local task created before
  `runtime.yaml` had `task_image`, and a valid `task_image` whose secrets are
  all in the token store, when the owner refreshes it and resumes it, the
  controller builds the task image and the task continues its workflow on its
  existing storage.
- AE2. **Covers R4.** Given a task whose agent step is running, when the owner
  runs the refresh, it is refused with a message to pause the task first, and
  the environment does not change.
- AE3. **Covers R6.** Given a paused task and a `runtime.yaml` with a syntax
  error, when the owner refreshes, the refresh is refused and names the
  problem; the task can still be resumed with its old environment. Given
  instead a valid `runtime.yaml` that adds a component whose secret is not in
  the token store, the refresh succeeds, and the next resume fails in
  provisioning with a message that names the missing secret; after
  `narsor-cli secret set` and another resume, the task starts.
- AE4. **Covers R8.** Given a paused task whose environment matches the
  catalogue, when the owner refreshes, the result is "nothing changed" and no
  event is recorded.
- AE5. **Covers R10, R11.** Given a task whose sealed environment lacks a
  component the catalogue now defines, `task show` marks the environment as
  differing from the catalogue. Given instead a `runtime.yaml` with a syntax
  error, `task show` reports a catalogue problem and no drift.

---

## Success Criteria

- A task like the one that failed with "this task has none" can be repaired
  and continued without archiving it.
- A component or secret added to `runtime.yaml` reaches a running task after
  pause, refresh and resume, with no other step.
- A bad catalogue edit never affects a task the owner did not refresh.
- The planner does not have to decide which parts of the environment refresh,
  when the action is allowed, or what happens on failure.

---

## Scope Boundaries

- No refresh of the workflow definition or its version: execution state is
  tied to the workflow, which is a separate and harder problem.
- No automatic refresh at resume or on a schedule.
- No preview, dry run or confirmation step.
- No undo and no history of environment versions. To go back, the owner fixes
  the catalogue and refreshes again; the old environment is visible only in
  the event's difference.
- No bulk refresh of many tasks at once.
- No refresh of a task while its workload runs (no automatic pause and
  resume).

---

## Key Decisions

- Manual with a drift indicator, not automatic: one bad catalogue edit must
  not stop every task at once, and the owner decides when a task's tools
  change.
- Refuse on a live workload: the agent never gets different tools in the
  middle of a step.
- Whole environment, both backends: one action covers the missing task image,
  new components and profile changes; a partial refresh would leave the owner
  guessing which part changed.
- No undo: recovery is "fix the catalogue, refresh again". Accepted risk: on
  Kubernetes a refreshed runtime bundle puts a newer runner on the task's
  existing storage; if that runner cannot read it, the task stays stopped
  until the catalogue is fixed.
- One action repairs tasks without a task image; there is no separate
  "fill in only what is missing" mode.

---

## Dependencies / Assumptions

- The runner keeps a copy of the launch spec on the task storage and checks
  it against the digest the controller pins
  (`crates/narsor-runner/src/runner/mod.rs`). A refreshed spec has a new
  digest, so the storage copy must follow it, or the runner refuses to start.
- The controller caches launch specs per task and generation
  (`crates/narsor-controller-runtime/src/launch_specs.rs`); a refresh must
  reach it like any other spec change of a new generation.
- Idempotent task creation compares the stored launch spec with a retried
  request (`same_creation` in
  `crates/narsor-engine/src/orchestration/control_plane.rs`); a refreshed task
  must still answer a retry of its original create request correctly.
- Assumption: the owner who may pause a task may also refresh it; network
  mode uses the same authorization as other task actions.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R2, R6][Technical] How the refreshed spec reaches the task storage
  and the controller without breaking digest pinning and fencing.
- [Affects R3][Needs research] Whether a newer runtime bundle can resume an
  execution started by an older runner on Kubernetes, and whether the
  refresh should warn when the bundle version changes.
- [Affects R4][Technical] Which task states count as "no live workload"
  (queued, paused, interrupted, waiting for input).
- [Affects R10][Technical] When the drift is computed (on each view or
  cached) and its cost for a long task list.
