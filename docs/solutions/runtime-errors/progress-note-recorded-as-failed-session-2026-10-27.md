---
title: A progress note was recorded as a failed session, a rejected output and a reprompt
date: 2026-10-27
category: runtime-errors
module: narsor-engine/agent-step
problem_type: runtime_error
component: tooling
symptoms:
  - "A step log shows agent_session_failed, action_output_rejected and output_reprompted after an agent sent only a progress note"
  - "The error text is: the agent's last answer was a progress note, not a final answer"
  - "After 5 progress notes in a row the step goes to the conductor"
root_cause: logic_error
resolution_type: code_fix
severity: medium
tags: [agent-step, progress, claude-code, json-schema, structured-output, resume, background-tasks, conversation-numbering]
---

# A progress note was recorded as a failed session, a rejected output and a reprompt

## Problem

Task `task-fdb9e1ab73af4fa496da23c1c4e11d1d` (workflow `compound-engineering`) showed errors in the steps `brainstorm` and `plan`. The agent had only sent `{"status":"progress","message":"…"}`. Requirement R7 of the progress feature says a progress note does not end the step and does not change the workflow state.

## Symptoms

Engine events of the execution (`state/executions/*/events/`), steps `plan` (65–73) and `brainstorm` (7–14):

```
agent_message  →  agent_session_failed  →  action_output_rejected  →  output_reprompted
```

`agent_session_failed.error`: `no output label could be extracted from the agent's answer: the agent's last answer was a progress note, not a final answer`.

The same sequence repeated for each note. The `plan` step ended only because its third attempt carried `choice`. A sixth note in a row would have sent the step to the conductor (`MAX_PROGRESS_CONTINUATIONS = 5`).

## Root cause

1. Claude Code with `--json-schema` ends its turn after `StructuredOutput` with `progress`, **when nothing runs in the background**. The process exits by itself with code 0. The engine kills nothing here: `progress` does not set `final_answer` in `spawn_and_wait`.
2. `AgentExecutor::run` read the run without `choice` as a format error. It called `fail_session` and returned `MISSING_OUTPUT_LABEL` with a `progress_only` flag.
3. `OutputRejection::from_error` and `Engine::reprompt` in `runner.rs` turned that into `ActionOutputRejected` and `OutputReprompted`.
4. Fix #39 added the resume of the session (`--resume`), but it kept the "reject, then resume" path. Every note stayed an error in the log, and the limit of 5 stayed. #38 says there is no limit.

The session was never lost. The resume worked. The record was wrong.

## Investigation notes

The first diagnosis said "Claude Code always ends the session after the first `StructuredOutput`". That was wrong in part. The operator saw sessions that stayed alive after several `StructuredOutput` calls. An experiment with the real CLI showed that both are true.

Experiments (`claude` 2.1.311, `--print --verbose --json-schema <flat schema> --output-format stream-json`, model `haiku`, empty directory, `stdin` closed):

| Scenario | After `progress` | Result |
|---|---|---|
| No background work. Prompt: `progress`, then `Bash echo AFTER > after.txt`, then `choice` | `result` line, process exits with 0 after about 6 s | `after.txt` was not written. One `result` line. |
| Background `Bash` (`sleep 10; echo DONE > bg.txt`), then `progress`, then `choice` | `result` line, process stays alive | `bg.txt` was written. A new turn (second `system/init`) sent `choice`. Two `result` lines, 17 s. |
| Background subagent (`Agent`, `run_in_background`) that runs `sleep 12; echo SUB > sub.txt`, then `progress`, then `choice` | `result` line, process stays alive | `sub.txt` was written. A new turn sent `choice`. Two `result` lines, 23 s. |

Conclusions:

- Without background work the process exits after the first `StructuredOutput`. This was the case in `task-fdb9…`: the agent used only `Bash`, `Read`, `Write` and `Skill`. The process exited after about 0.8 s.
- With background work `claude` does not exit and does not kill the work. It waits, and a finished task starts a new turn. This is why one process can emit several `result` lines (R1 of the progress requirements).
- `progress` does not kill background work. Two places do: the engine after an answer with `choice` (grace period `FINAL_ANSWER_GRACE` = 15 s, then SIGTERM and SIGKILL to the process group), and the step timeout. An agent that sends `choice` while a subagent still works loses that subagent's work.

The experiments used one subagent and 12 s of work. Many subagents and long work were not tested.

## Solution

`AgentExecutor::run_until_answered` (`crates/narsor-engine/src/engine/actions/agent.rs`) wraps `run`:

- A run that ends on a progress note and has a harness session ends as `agent_session_completed`, not `agent_session_failed`.
- The executor resumes that session at once (`--resume`, the workspace access of the step, prompt `PROGRESS_CONTINUATION`) inside the same action. The engine sees one action and no rejection.
- There is no limit on notes. The step timeout counts from the first run. A timeout reports the limit of the step, not what was left of it.
- A run that ends on a note with no harness session id stays a plain format error and goes the old way (reprompt, then conductor).

Other changes in the same fix:

- `ConversationWriter::create` continues the entry numbering of an existing file. Before, numbering started at 1 on each open. All runs of one step now write to one file, and the TUI follows the file by entry number (`from_entry`). Repeated numbers would hide entries.
- `PromptPurpose::Continuation` marks the resume prompt in the conversation. It does not look like a reprompt.
- `CONTRACT_NOTE` (`harness/claude_code.rs`) tells the agent that a progress note ends the turn when nothing runs in the background, and that each note costs a resumed session. The agent should send it only while it waits for a subagent or a background task.
- `progress_only` and `continuations` left `OutputRejection`. Old persisted rejections still load, because unknown fields are ignored.

## What didn't work or was ruled out

- Keeping the resume in `runner.rs` and only hiding the events. The events come from the executor (`fail_session`) and from the rejection path. Hiding them there would leave the log false in other consumers.
- A limit on notes as a safety net. The requirements (#38) state no limit. The step timeout is the guard.
- A new `PromptPurpose` was needed. `Reprompt` would label the resume as "fix your answer" in the conversation.

## Risks and open points

- With no limit, a series of notes lasts until `timeout_secs`. A step with no timeout can loop for as long as the agent sends notes. Each note costs a resumed session.
- Dropping `--json-schema` and reading JSON from plain text, which would remove the need for `progress`: done in D69 (`docs/plans/2026-10-27-0858-feat-agent-bez-json-schema-plan.md`). The step passes no schema, `progress` and the resume after it are gone. This document describes the state before that change.
- Memory note `claude-code-structuredoutput-ends-turn` was incomplete. The table above is the full picture.

## Prevention

Tests in `tests/actions_agent.rs` run the real `AgentExecutor` against the fake harness:

- `a_progress_note_ends_the_run_but_not_the_action`
- `a_progress_note_leaves_no_rejection_in_the_log` (the sequence of `task-fdb9…` at workflow level)
- `a_long_series_of_progress_notes_is_not_limited`
- `the_continuation_after_a_progress_note_keeps_the_workspace_access_of_the_step`
- `the_timeout_of_the_step_counts_from_its_first_run`
- `a_resumed_run_continues_the_numbering_of_the_conversation`

Why the old tests did not catch it: the #39 test used a fake executor in `session_tests.rs` that returned the progress note as an error. It checked that the step finished, not which events it wrote. Those two tests were removed, because the executor no longer returns a note as an error.

Also found: after #39, `tests/actions_agent.rs` did not compile on `main` (two `OutputRejection` literals lacked the new fields). Run `cargo test --workspace` before merge, not only the crate you changed.

## How to repeat the experiment

```sh
SCHEMA='{"type":"object","properties":{"choice":{"type":"string","enum":["done"]},"reason":{"type":"string"},"status":{"type":"string","enum":["awaiting_input","progress"]},"message":{"type":"string"}},"additionalProperties":false}'
claude --print --verbose --permission-prompts none --model haiku \
  --json-schema "$SCHEMA" --output-format stream-json --permission-mode acceptEdits \
  '<prompt of one scenario above>' < /dev/null > out.ndjson
```

Count `result` lines and look for a second `system/init` after the first `result`. Run it in an empty directory.

## Related

- `docs/brainstorms/2026-10-26-zamykanie-kroku-agenta-i-odpowiedz-progress-requirements.md` (R5–R9)
- `docs/solutions/runtime-errors/agent-step-hangs-on-orphaned-background-process-2026-10-26.md`
- PRs #38, #39
