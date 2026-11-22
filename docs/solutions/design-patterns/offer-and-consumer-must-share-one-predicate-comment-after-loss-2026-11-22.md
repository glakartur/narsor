---
title: "Keep the control-plane offer and the runner's consumer in step: a comment accepted for a task the runner cannot use fails every later launch"
date: 2026-11-22
category: design-patterns
module: narsor-engine, narsor-runner
problem_type: design_pattern
component: development_workflow
severity: medium
applies_when:
  - "The control plane accepts and stores an input (a comment) that the runner consumes later"
  - "The accept rule is written from the task state and the consume rule from the execution status, in two crates"
  - "A stored input can outlive the state it was accepted in"
tags: [controller, runner, resume-message, comment, loss, offer, stale-summary]
---

# Keep the offer and the consumer in step

## Context

The branch for PR #141 lets the owner write a comment to a task after a lost workload or a failed step. The comment continues the agent session. Review decision D1 found a gap: a comment on a lost or Unknown task was accepted when its execution was `WaitingForEvent`, `Completed` or `InputNeeded`. The runner's `take_resume_message` refuses those statuses, so every later launch failed on the stored comment.

## Guidance

- Narrow the offer in the control plane. `stopped_by_loss_or_failure` in `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs` offers the comment for an interrupted or Unknown runner only while the execution is Running, Paused, Unknown or Failed. A task with no runner view keeps the offer and gets the new-session warning.
- Add a backstop in the runner for a stale summary. `go_on` in `crates/narsor-runner/src/runner/mod.rs` takes the comment only for Running or Paused, recovers a Failed execution with it, and for any other status keeps the comment stored, logs a warning and goes on with the launch.
- The consumer must never turn "cannot use this input" into a launch error. It keeps the input and goes on.

## Why This Matters

The offer reads a summary that can be old. Without the backstop, one stale summary leaves a stored comment that blocks the task on each launch. The fix is in commit 9f1297d on this branch.

## When to Apply

Any new input that the control plane stores for the runner to take later. Write the accept rule and the consume rule together, and test the statuses where they differ.
