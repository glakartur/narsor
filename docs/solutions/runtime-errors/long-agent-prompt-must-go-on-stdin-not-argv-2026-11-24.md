---
title: "A long agent prompt in argv fails with E2BIG; send it on stdin"
date: 2026-11-24
category: runtime-errors
module: narsor-exec/harness
problem_type: runtime_error
component: background_job
severity: high
applies_when:
  - "A harness adapter puts the prompt in a command-line argument"
  - "A prompt embeds data that grows with the task (snapshot, payload, workflow definition)"
  - "A new harness adapter is added"
symptoms:
  - "Conductor chat shows: failed to start the agent harness 'claude' ...: Argument list too long (os error 7)"
  - "Every human-gate summary fails; the failure is not retried"
root_cause: logic_error
resolution_type: code_fix
tags: [e2big, execve, MAX_ARG_STRLEN, stdin, claude-code, conductor, harness-adapter]
---

# A long agent prompt in argv fails with E2BIG; send it on stdin

## Problem
Linux limits **one** argv string to `MAX_ARG_STRLEN` (128 KiB), not only the total. A conductor prompt over that size made `execve` return `E2BIG` (os error 7).

## What is not obvious
- The conductor snapshot budget is 160 KiB (`crates/narsor-exec/src/conductor/snapshot.rs`) and is larger than the limit. "Whole" sections (workflow definition, request payload) are never trimmed. So a long task can always overflow.
- The conductor process runner (`run_harness_process`) used to pass `stdin_content = None`, so an adapter's stdin was dropped on that path. Agent steps already forwarded it.
- The error is recorded in the chat and not retried. Each gate adds a new error entry.

## Solution
The `claude-code` adapter sends a prompt over `LONGEST_PROMPT_ARGUMENT` (96 KiB) on stdin (`claude --print` reads stdin when no prompt argument exists). The conductor runner forwards `command.stdin`. Regression test: `a_prompt_too_long_for_one_argument_reaches_claude_on_stdin` in `tests/integration/conductor_invoke.rs`.

## Still open (PR #143)
- `codex`, `opencode`, `github_copilot` still put the prompt in argv and can hit the same error. `apply_extra_args` (`harness/mod.rs`) anchors extra args at the prompt, so `codex exec -` needs its own change.
- The test uses a fake harness. Real `claude` with a stdin prompt needs a check on a host with the CLI.

## Prevention
- Any new adapter: keep the prompt out of argv above a safe threshold, or use stdin/a file.
- Keep a prompt budget below the per-argument limit, or ensure the adapter handles larger prompts.
