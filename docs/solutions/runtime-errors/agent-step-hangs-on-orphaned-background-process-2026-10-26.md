---
title: An agent step hung after the final answer because a background process held the stdout pipe
date: 2026-10-26
category: runtime-errors
module: narsor-engine/agent-step
problem_type: runtime_error
component: tooling
symptoms:
  - "An agent step stays running after the agent gave its final answer, until timeout_secs ends it"
  - "The `claude` process is alive, or the stdout pipe has no EOF, because a `Bash` polling loop or `Monitor` task is still running"
  - "Child processes of the harness survive a time limit or a cancelled step"
root_cause: logic_error
resolution_type: code_fix
severity: high
tags: [agent-step, process-group, killpg, stdout-pipe, claude-code, monitor, progress, kill-on-drop, guard]
---

# An agent step hung after the final answer because a background process held the stdout pipe

## Problem

`spawn_and_wait` in `crates/narsor-engine/src/engine/actions/agent.rs` waited on `join!(stdout, stderr, child.wait())`. Nothing ended the process after the final answer. An agent that left a task in the background (an `until … sleep` loop in `Bash`, `Monitor`) kept the step open until the time limit. The agent also had no legal way to say "not finished yet" through `StructuredOutput`.

## Symptoms

- The step does not close after the answer with `choice`.
- An orphaned `bash` holds the write end of the stdout pipe. The reader never sees EOF, even when `claude` exits by itself.
- `kill_on_drop(true)` and `Child::kill()` reach only the leader (`claude`). The rest of the tree stays alive after a timeout or a cancel.

## What Didn't Work

- Waiting for `claude` to exit. The pipe stays open while any descendant lives.
- Killing only the child at the time limit. The first version signalled the group by hand in the timeout branch only; cancelling the future (`Drop`) still leaked the group.
- Blocking `Bash`. It would stop legitimate work. Only `ScheduleWakeup`, `CronCreate` and `Monitor` are blocked (`DEFERRING_TOOLS`).

## Solution

1. Start the harness as leader of its own group (`Command::process_group(0)`). Signal by number with `libc::killpg` (`process_group.rs`), because `wait` borrows the `Child`.
2. The stream parser reports the final answer (`HarnessStreamParser::final_answer`). The rule is one: `structured_response(value).is_ok()` and the `result` line is not an error. A `progress` object never replaces an earlier final answer.
3. After a final answer: wait a grace period (`FINAL_ANSWER_GRACE`), then SIGTERM, then SIGKILL, then drain the pipes. Treat the forced end as exit code 0 for `parser.finish`.
4. Own the group with a `GroupGuard`. It kills the group on `Drop` while armed, and it is disarmed once the group was dealt with. On timeout the guard is still armed, so it kills the rest of the group:

```rust
Err(_elapsed) => {
    // The waiting future is gone, so the child can be reaped for
    // good. The still armed guard kills the rest of the group.
    let _ = child.kill().await;
    return Ok(SpawnEnding::TimedOut);
}
```

5. Add a non-final answer `{"status":"progress","message":"…"}` to the output schema (flat object, no `anyOf`). A session that ends on `progress` gives `missing_output_label`. The engine reprompts once in the same session, then asks the conductor.

## Why This Works

A process group lives as long as one member does. `killpg` after the leader was reaped still reaches what the leader left behind, and `ESRCH` means there is nothing to end. The pipe closes when the last holder dies, so the readers finish. One guard owns every exit path (final answer, timeout, cancel), so no path needs its own kill call.

## Prevention

- Put group kill in one owner (`GroupGuard`), not in each exit branch. A second hand-written `killpg` in the timeout branch was dead weight once the guard existed (e58f7ff).
- When a process runs in its own group, Ctrl-C from the terminal no longer reaches its children. Plan the cleanup path in the same change.
- Keep one rule for "final answer" shared by the parser and `read_response`, so the two cannot disagree.
- Test with a session that ends on `progress` only (`a_progress_only_session_is_reprompted_once_then_goes_to_the_conductor`): two calls, the second one resumes the same session, then the conductor chooses.
- Killing after `awaiting_input` stays off (`KILL_AFTER_QUESTION = false`) until a test on a live `claude` confirms `--resume` after a kill. This is an open assumption, not a verified fact.
- Group kill works on Linux and macOS only.

## Related Issues

- `docs/plans/2026-10-26-001-feat-zamykanie-kroku-agenta-i-progress-plan.md`
- Commits 0450d98 and e58f7ff
