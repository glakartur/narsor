---
title: "Automatic resume of lost workloads: counted losses, capped free restarts, confirmed-loss gating, forward-compatible wire enums"
date: 2026-10-11
category: architecture-patterns
module: orchestration/resume
problem_type: architecture_pattern
component: background_job
related_components:
  - service_object
  - testing_framework
severity: high
applies_when:
  - "A supervisor restarts lost work on its own and must not loop forever"
  - "A restart of the observer (controller, lease holder) must be told apart from a failure of the work"
  - "A transition to 'lost' must wait for positive evidence, not for 'cannot tell'"
  - "Enums cross components that are upgraded at different times"
  - "A background loop issues the same commands a human owner can"
symptoms:
  - "Tasks stayed Interrupted after a runner or workload loss until the owner resumed them by hand"
  - "A controller crash loop would have resumed a task forever if restart losses were never counted"
root_cause: logic_error
resolution_type: code_fix
tags: [auto-resume, resume-policy, backoff, lease-epoch, crash-loop, serde-other, wire-compat, daemon-tick]
---

# Automatic resume of lost workloads: counted losses, capped free restarts, confirmed-loss gating, forward-compatible wire enums

## Context
Before plan `docs/plans/2026-10-10-003-feat-automatyczne-wznawianie-utraconych-zadan-plan.md`, a lost task stayed `Interrupted` until its owner pressed resume. A runner could be lost because its process died, its pod was evicted or its controller restarted. The decision is D58 in `ARCHITECTURE_DECISIONS.md`, with D59 covering the conversation read from storage. The control plane now resumes such tasks on the daemon's 5-second scheduler tick, for every backend.

The policy had to solve six problems:
- A runner that always crashes must not be retried forever.
- A controller restart says nothing about whether the task can run.
- A workload that may still be alive must never get a second runner on the same storage.
- Repeated passes, and owner commands racing with the policy, must not double-resume.
- New enum variants must not break older clients and controllers.
- On the local backend, a dead runner's children must not outlive it and race the relaunch.

Scope from the brainstorm (session history):
- Artur first asked only to read the conversation of an interrupted task, then reframed it: "the daemon should bring the task back to life".
- An early draft limited resume to local tasks. That came from mixing up two things: reading a conversation from disk is local-only (KTD4), but resume is one control-plane transition shared by all backends.
- A task killed in the middle of an agent action is still resumed. The repeated side effects are accepted.

## Guidance

### 1. Count a loss once, as the task enters `Interrupted`; reset after a stable run before any other branch
`ResumePolicy` (`src/orchestration/resume.rs`) is fixed:
- delays `[0, 30s, 2m, 10m, 30m]`, and the length of that list is the limit;
- a stability window of 10 minutes.

`count_loss` reads the remembered `last_loss` and either plans `next_attempt_at` or sets `exhausted`. The periodic pass only acts on attempts that are due (`TaskRecord::auto_resume_due`), so the counting rule and the timing are tested separately from the loop.

```rust
if loss.from == TaskLifecycle::PauseRequested { return; }   // the owner wanted a pause
if started_successfully {                                    // reset FIRST
    resume.consecutive_losses = 0;
    resume.uncounted_losses = 0;
}
if restarted && resume.uncounted_losses < policy.limit() {  // free, but capped
    resume.uncounted_losses += 1;
    resume.last_loss_uncounted = true;
    resume.next_attempt_at = Some(now);
    return;
}
resume.consecutive_losses = resume.consecutive_losses.saturating_add(1);
match policy.delay(resume.consecutive_losses) {
    Some(delay) => resume.next_attempt_at = Some(now + delay),
    None => resume.exhausted = Some(ResumeExhaustion { reason, at: now, announced: false }),
}
```

`Paused` is never auto-resumed. An owner's resume clears the counts.

### 2. Tell a restart apart from a failure by lease epoch, and cap the free path
A loss reported under a newer lease epoch than `TaskRecord.workload_epoch` was found by a controller that took over after the launch. The workload's own behaviour did not cause it, so it costs no attempt; `AutoResumed { attempt: 0, .. }` marks such a resume. These free losses are capped at `limit` in a row. Past the cap they count like any other loss, so a controller crash loop still ends in exhaustion.

### 3. Leave `Unknown` for `Interrupted` only on a confirmed loss
The controller's `confirm_loss` (`src/backends/controller.rs`) reports `Observation::LossConfirmed` only when both hold:
- the storage is `Present`;
- the workload is either absent or `Exited`.

A workload that is `Deleting`, or a Pod in phase `Unknown`, means "cannot tell", and nothing is resumed. Waiting for a confirmed loss costs latency, but it never puts two runners on one storage. When the control plane refuses a confirmation, the controller warns once per task (`OncePerTask`).

### 4. Make policy commands ordinary owner commands with a reserved key and an expected generation
```rust
self.command(task_id,
    CommandRequest::new(OwnerCommand::Resume, auto_resume_key(generation)) // "auto-resume:<gen>"
        .expecting_generation(generation)
        .issued_by(CommandActor::Policy))
```
- **Idempotent:** there is one key per interruption, so a later pass gets the replayed receipt and nothing new.
- **Owner wins a race:** if an owner command lands first, the policy gets `StaleGeneration` or `IllegalCommand` and steps aside quietly.
- **Not forgeable:** a client key starting with `auto-resume:` is rejected with 400 `reserved_idempotency_key`.
- **Same path as an owner:** the runner is still authorised the normal way, and history shows the right actor (`RunnerLaunchView.authorized_by_actor`).

### 5. Announce exhaustion exactly once
`ResumeExhaustion.announced` is flipped in the same mutation that emits `AutoResumeExhausted`, in `ControlPlane::announce_exhaustion`. The pass may call it on every tick.

### 6. Give every wire enum a fallback before shipping a new variant, and write down the upgrade order
`Observation`, `TaskEventKind` and `ConversationUnavailableReason` each end in `#[serde(other)] Unrecognized`. The control plane refuses `Unrecognized` in a report. The upgrade order is documented in D58/D59 and in `docs/operations/remote-backends.md` §9: control plane first, then controllers, then clients together with their server.

### 7. Kill a dead runner's orphaned process group before relaunch (local backend)
Local runners are spawned with `NARSOR_LOCAL_WORKLOAD_UID=<uid>`. `remove_workload` / `kill_marked_group` (`src/backends/local.rs`) send SIGKILL to `-pgid` only while a live, non-zombie member of that group carries the mark in `/proc/<pid>/environ`. Such a member proves the group is ours, and its pgid cannot be reused while it lives.

### 8. Expose the loop body and test it without a daemon
`pub fn run_periodic_pass(&ProtocolService)` in `src/cli/daemon.rs` runs `reschedule_all`, `auto_resume_pass` and `sweep_materials`. The default-run test `the_daemons_periodic_pass_resumes_a_due_interrupted_task` in `tests/controller_protocol.rs` calls it directly. Tests that start a real daemon are `#[ignore]`d because they leak processes. `recover()` adopts legacy `Interrupted` tasks from the event log (`loss_from_history`), and an unreadable log skips only that task.

The loss classifier (`LossState` / `loss_state`) also lives in `orchestration/resume.rs`, so the TUI and the CLI describe a lost task the same way.

## Why This Matters
Code review of the branch caught three real defects (fixes 637657f, d319ecf and neighbours):
- **The restart branch ran before the stability reset.** A long, stable run lost across a controller restart kept its old count, so the next quick loss continued from attempt 3 instead of attempt 1. The test is `a_restart_loss_after_a_stable_run_starts_the_count_anew`.
- **Free resumes had no cap.** A controller in a crash loop would have produced endless resumes with no delay and never exhausted. The tests are `a_controller_that_keeps_restarting_gets_only_so_many_free_resumes` and `six_restart_losses_in_a_row_do_not_exhaust_the_policy`.
- **New variants broke older decoders.** `LossConfirmed`, `AutoResumed`, `AutoResumeExhausted` and the new unavailable reason made an older TUI or CLI fail to decode a whole event or response. Raw-JSON tests now guard each enum.

Plan review added three more:
- **Unknown storage.** `Unknown` → `Interrupted` requires the storage to exist; otherwise it loops forever.
- **Exhaustion reason.** It lives in the task's own resume state, so a later report cannot overwrite it.
- **Daemon-restart test.** Its first design assumed a daemon restart kills local runners, which is false. That is what surfaced the orphaned process group.

Without the gating in §3, an "unknown" pod phase could start a second runner on storage that a live runner is still writing to.

## When to Apply
- **Supervisors that restart long-lived work automatically.** Use bounded backoff, a stability reset that comes first, and an exhaustion state that a human has to clear.
- **Fenced leases or epochs.** Tell "the observer changed" apart from "the work failed", but still bound the free path.
- **Automation that issues users' commands.** Reuse the command path with a reserved idempotency namespace and an expected generation instead of a side channel.
- **Enums serialized between components deployed on their own schedule.** Add the fallback before the first new variant ships.
- **Process supervisors that spawn process groups.** Tag the processes so cleanup can prove ownership before signalling.
- **Logic driven by a background loop.** Make the loop body a public function and test it directly (compare `recover()`, which had tests but was never called; auto memory [claude]).

## Examples

**`count_loss` ordering.** Before 637657f, simplified:
```rust
if record.workload_epoch.is_some_and(|epoch| loss.lease_epoch > epoch) {
    resume.next_attempt_at = Some(now);
    return;                                   // never counted, never capped; the reset below is skipped
}
resume.consecutive_losses = if started_successfully { 1 } else { resume.consecutive_losses.saturating_add(1) };
```
After: see §1. The reset happens first, and the free path is bounded by `uncounted_losses < limit`.

**Forward-compatible enum** (`src/orchestration/model.rs`):
```rust
    /// An observation of a newer version this one does not know. Never
    /// written: it only lets a client decode a newer server's event or
    /// summary instead of dropping it, and the control plane refuses it in
    /// a report.
    #[serde(other)]
    Unrecognized,
```
```rust
let observation: Observation =
    serde_json::from_value(serde_json::json!({ "kind": "workload_teleported" }))
        .expect("a newer observation decodes");
assert_eq!(observation, Observation::Unrecognized);
```

**Wiring tested without a daemon:**
```rust
let pass = narsor::cli::daemon::run_periodic_pass(&harness.service);
assert_eq!(pass.resumed, vec![TaskId::parse("task-1").unwrap()]);
```

## Related
- `ARCHITECTURE_DECISIONS.md`: D58 (automatic resume), D59 (conversation from storage), D23 (crash recovery, ancestor of `recover()`)
- `docs/operations/remote-backends.md` §9 (task lifecycle, upgrade order) and §10 (slots)
- `docs/brainstorms/2026-10-10-wznawianie-utraconych-zadan-requirements.md`
- `docs/plans/2026-10-10-003-feat-automatyczne-wznawianie-utraconych-zadan-plan.md`
- `docs/solutions/integration-issues/runner-private-home-hides-harness-login-2026-10-10.md`, a sibling learning about local runner launch
