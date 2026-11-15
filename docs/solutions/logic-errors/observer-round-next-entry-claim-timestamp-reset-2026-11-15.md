---
title: "Observer round: the next entry must wait for a claim counted from the end of the previous run"
date: 2026-11-15
category: logic-errors
module: engine/observers/schedule
problem_type: logic_error
component: background_job
symptoms:
  - "A watch-list observer stays due for the whole round, so the worker can claim the next entry as soon as the previous run ends"
  - "The next entry keeps the run_due_since of the first claim, so it looks older than other observers' runs"
  - "A lost run (no result before the deadline) leaves the same old run_due_since for the next entry"
root_cause: logic_error
resolution_type: code_fix
severity: medium
tags: [observers, scheduler, claim, watch-round, run-due-since, fairness]
---

# Observer round: the next entry must wait for a claim counted from the end of the previous run

## Problem
A watch-list observer checks one entry per run (D18). A round holds the entries that were there when the run became due. While entries stay in the round, the observer stays due (`run_due_since` is set). The claim picks "the oldest due" observer. So the next entry of the round kept the old `run_due_since` of the first claim.

## Symptoms
- A long round with one observer always won against newer due observers.
- After a lost run, the next entry looked as old as the lost one.

## What Didn't Work
- Setting `run_due_since` only when the round starts (in `observer_pass`). The round has more than one claim, so the value goes stale after the first one.

## Solution
When a run ends (result, failed entry, or lost run), move `run_due_since` to `now` if the round still has entries. Do it only if the flag is set, so a finished round does not become due again.

```rust
// crates/narsor-engine/src/controller_protocol/service/observers/schedule.rs
record.next_run_at = Some(after(now, interval));
// The round's next entry waits for a claim from now, not from this run's claim.
if record.run_due_since.is_some() {
    record.run_due_since = Some(now);
}
```

The same change is in the lost-run path (`LOST_MESSAGE`).

## Why This Works
"Oldest due" sorts by `run_due_since`. The time that counts is the time since the observer could run again, not since the first claim of the round. After the reset, observers with one entry per round and observers with many entries share the worker in a fair order.

## Prevention
- Each state that is a "due since" timestamp needs a reset at every point where the run ends: result, failure, and loss. List the paths in the module header comment (as `schedule.rs` does).
- Guard the reset with `is_some()`: it keeps the "round finished" state (`None`) from turning into "due".
- Test: an observer with two entries and a second observer due between the two claims. The second observer must get its claim between them.

## Related Issues
- Plan: `docs/plans/2026-11-13-002-feat-obserwatory-zrodel-zewnetrznych-plan.md` (D18, single-watch script contract).
- Commit `83a3538`; PR glakartur/narsor#104.
