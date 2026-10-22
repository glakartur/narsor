---
title: "TUI reads and actions run as keyed background jobs with a loading marker"
date: 2026-10-22
category: design-patterns
module: tui/task_app
problem_type: design_pattern
component: frontend
severity: medium
applies_when:
  - "A TUI screen needs data from the server (read) or sends a request (action)"
  - "A new view, tab or key handler calls ApiClient from the event loop"
  - "A loading indicator or an in-flight guard is added to the TUI"
tags:
  - tui
  - async
  - background-jobs
  - spinner
  - event-loop
  - joinset
---

# TUI reads and actions run as keyed background jobs with a loading marker

## Context

The TUI called the server from the event loop and waited. The screen froze and keys, quit included, did not work until the call ended. Commit 86ef214 moved every read and action into a job. The rules below keep the next change from putting a blocking call back in the loop. The code is in `src/tui/task_app/background.rs`.

## Guidance

- **A job never touches `TaskApp`.** It owns an `ApiClient` clone, asks, and returns an `Outcome`: a closure over plain data. The loop collects it (`next_outcome`) and applies it (`on_outcome`). State changes only on the loop.
- **One job per `JobKey`.** `Background::start` returns false and starts nothing when the key runs. A failed job has left the map before its outcome is applied, so the next start is free.
- **Scope replaces scope.** When the selection or view leaves, abort the jobs of that scope (`abort_detail_jobs`, `abort_where`). An aborted job reports nothing. A read that was aborted changed nothing yet, so this is safe.
- **Jobs live in a `JoinSet`.** Dropping the application aborts them, so quitting never waits for a read.
- **The marker frame is application state.** The loop raises it on its tick only while `is_loading()`. A test sets the frame (`set_animation_frame`) and gets a stable render.
- **The loop waits on `next_outcome` only while `has_running()`.** The call returns `None` at once when no job is left.
- **Tests use `drain_jobs()`** to apply every outcome. Blocking entry points that keep their old shape use it too.

```rust
// shape of a read
let started = self.background.start(JobKey::Detail(id), async move {
    let data = client.read(...).await;
    Outcome::new(move |app| app.apply(data))   // plain data, no &mut TaskApp in the job
});
```

## Why This Matters

A job that mutates the app directly needs locks and risks half-applied state. A second start of the same read gives duplicate requests and a flicker of two answers. A job of a left scope that is not aborted writes old data over the new screen.

## When to Apply

Every new read or action in `src/tui/task_app/`. Add a `JobKey` variant for it, define its scope, and decide which events abort it.

## Examples

Tests for this are in `tests/tui_background.rs`. Related: [[tui-focus-stays-in-details-after-confirmed-action-2026-10-20]], [[tui-modal-confirmation-keyboard-contract]].
