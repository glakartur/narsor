---
title: "TUI started a task list read after every runner report (about once a second)"
date: 2026-10-22
category: logic-errors
module: tui/task_app
problem_type: logic_error
component: tooling
symptoms:
  - "The refresh marker in the task panel shows about once a second while a task runs, although tui_refresh_interval is 10 s"
  - "tui_refresh_interval is set and works, but the list is still read far more often than the interval"
root_cause: logic_error
resolution_type: code_fix
severity: medium
tags:
  - tui
  - refresh
  - event-stream
  - patch-row
  - runner-reported
  - debounce
---

# TUI started a task list read after every runner report (about once a second)

## Problem

`tui_refresh_interval` (10 s by default) worked, yet the task panel showed the refresh marker about once a second while a task was running. The setting looked broken. The cause was a different path: the event stream.

## Symptoms

- Refresh marker (`JobKey::Refresh`) visible about every 1.4 s on a running task.
- The periodic schedule itself fired every 10 s, as configured.

## What Didn't Work

Ruled out by reading the code, one by one (the TUI was not run; the finding is confirmed by the new tests):

- The Conductor tab poll (`READER_RETRY`, 2 s): it reads details, uses another job key and draws no marker.
- `STREAM_RECHECK_INTERVAL`: a server-side constant, not a list read in the TUI.
- `FIRST_RETRY` (1 s): runs only after a read error.

## Solution

`patch_row` in `src/tui/task_app/list.rs` returned `true` ("ask for a read") for every `TaskEventKind::RunnerReported`, because the runner's current step is not in the event. The runner reports about once a second, so each report started a debounced (400 ms) read, and the 10 s timer restarted after each read.

Now `patch_row` compares the fields the event can change and asks for a read only when one of them changed:

```rust
let status_changed = runner.execution_status != *execution_status;
let pending_changed = runner.pending_input.as_ref().map(|pending| &pending.id)
    != pending_input_id.as_ref();
// ... patch the row ...
let before = task.attention.clone();
replace_reported_attention(&mut task.attention, attention, &task.inputs);
status_changed || pending_changed || task.attention != before
```

A report that moves only `event_cursor` asks for no read. The runner's current step now shows with the periodic read, up to `tui_refresh_interval` late. README states this. A row without a runner still asks for a read.

## Why This Works

The row shows status, pending input and attention. The event carries all three, so the TUI can patch them at once. Only the step is missing, and the user asked for that to follow the interval. Comparing before and after turns "an event of this kind arrived" into "this event changed the row".

## Prevention

- When an event handler returns "needs a read", state which field the event cannot carry. Return `true` only for a real change in the fields that the row shows.
- Every kind of event that returns `_ => true` in `patch_row` is a possible hidden poll. Check the event rate of a kind before you let it ask for a read. A general cap on event-driven reads is deferred in the plan until another kind proves frequent.
- Tests in `tests/tui_state.rs`: a report that moves only the cursor asks for no read (10 reports in a row); a change of status, pending input or attention asks for a read; a row without a runner asks for a read.
- A periodic timer that restarts after every read hides event-driven reads. Find who calls `schedule.request` before you suspect the timer.

## Related Issues

- `docs/solutions/design-patterns/tui-reads-as-keyed-background-jobs-with-loading-marker-2026-10-22.md` (job keys and the loading marker)
- `docs/plans/2026-10-22-002-fix-tui-odczyt-listy-rzadziej-plan.md`
- `docs/brainstorms/2026-10-22-tui-interwal-odswiezania-requirements.md`
