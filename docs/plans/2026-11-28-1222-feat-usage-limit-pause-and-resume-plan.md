---
title: Pause on Subscription Usage Limit and Resume the Same Session - Plan
type: feat
date: 2026-11-28
topic: usage-limit-pause-and-resume
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Pause on Subscription Usage Limit and Resume the Same Session - Plan

## Goal Capsule

- **Objective:** When an agent step stops because the harness subscription has no tokens left, the task waits until the next usage window and then continues the same agent conversation, without a failed attempt in the history and without operator action.
- **Product authority:** The request of the project owner (this brainstorm). Claude Code is the required harness for v1. Other harnesses are context only.
- **Open blockers:** The questions in "Pytania otwarte dla użytkownika" change visible behaviour. A real Claude Code recording of an exhausted limit does not exist in the repo yet (R1); detection rules depend on it.

---

## Product Contract

### Summary

Narsor detects that a harness run ended because the subscription usage limit is exhausted. It does not count this as a failed attempt. It puts the step in a waiting state until the reset time that the harness reports (or retries at an interval when no reset time is known). Then it resumes the same harness session and the step continues.

### Problem Frame

Today a usage-limit stop is an ordinary harness failure. `ClaudeCodeStreamParser::finish` sets a free-text `failure`, the action error is `harness_failed`, and the retry policy starts a new attempt. Each retry gets a new `action_execution_id` and starts a new session, so the agent loses its context and the new attempt fails at once for the same reason. When the attempts run out, the execution goes to `Failed`, the task goes to `Error`, and an operator must restart the step after the window resets. The history keeps a chain of failed attempts that were not real failures.

A second problem: when the stopped step was itself a resumed session, the failure is classified as `resume_session_rejected`. The runner then asks the operator for a new session, or starts a new one at once when `restart_if_refused` is set. Both paths throw away the session that should continue.

The repo already parses `rate_limit_event` lines into usage limits (window, utilization, `resetsAt`, status `allowed` / `allowed_warning` / `rejected`) and shows them in the TUI and in `narsor usage`. Nothing uses this data to control execution.

### Key Decisions

- **A usage-limit stop is a separate outcome, not a failure.** It does not use an attempt of the retry policy and it does not go through the conductor's failure diagnosis. Governs R4, R5, R10.
- **Continue the existing harness session; never start a new one because of a usage limit.** This is the core of the request: the work done before the stop must not be lost. Governs R7, R8.
- **The reset time from the harness is preferred; periodic retry is the fallback.** The request names this order. Governs R6.
- **Detection is per harness and based on recorded real output.** Claude Code is required in v1. A harness without detection keeps today's behaviour. Governs R1, R2, R3.

### Requirements

**Detection**

- R1. The repo contains at least one real Claude Code stream-json recording (fixture) of a run that stops on an exhausted subscription limit, with a README note that describes the lines that signal it. The recording contains no customer data or secrets.
- R2. The Claude Code parser classifies a run as "usage limit reached" only from signals seen in R1 recordings (for example a `rate_limit_event` with status `rejected`, an error marker on the `result` or `assistant` line, or the limit message text). An ordinary error, an API error or a `max_turns` stop is not classified as a usage limit.
- R3. When the classification is "usage limit reached", the run carries the harness session id and, when the harness reports it, the reset time of the exhausted window.

**Waiting**

- R4. A usage-limit stop does not use an attempt of the step's retry policy and does not create a failed attempt in the execution history.
- R5. A usage-limit stop on a resumed session is not classified as `resume_session_rejected` and does not ask the operator for a new session.
- R6. The step waits until the reported reset time (plus a short safety margin). When no reset time is known, Narsor tries to resume at a fixed interval until the run no longer stops on the usage limit.
- R7. While it waits, the task shows a state that says the work waits for the usage limit, with the harness and the expected resume time when known. Execution and task status do not show `Failed` / `Error`.
- R8. The wait survives a restart of the runner or the server: after a restart, the waiting step still resumes the same session at the planned time.

**Continuation**

- R9. When the wait ends, Narsor resumes the same harness session (same session id, same working directory) with a short continuation prompt, and the step continues as the same attempt.
- R10. If the resumed session stops on the usage limit again, the step goes back to waiting (R6); this does not count as a failure.
- R11. If the resume fails for a reason that is not the usage limit, the step follows today's failure path (retry policy, then `Failed`).
- R12. The operator can cancel or stop a waiting step with the existing controls; a manual resume during the wait tries the same session at once.

### Key Flows

- F1. Limit reached during a step
  - **Trigger:** A Claude Code run of an agent step stops because the subscription has no tokens.
  - **Steps:** The parser classifies the run as usage limit (R2, R3); the runner records no failed attempt (R4) and puts the step in waiting (R6, R7); at the reset time it resumes the same session (R9); the step ends normally.
  - **Covered by:** R2–R4, R6, R7, R9
- F2. No reset time known
  - **Trigger:** As F1, but no reset time is available.
  - **Steps:** The step waits one retry interval; the resume stops on the limit again (R10) and the step waits again; a later resume succeeds.
  - **Covered by:** R6, R10

### Acceptance Examples

- AE1. **Covers R2, R11.** Given a run that ends with `terminal_reason: api_error` and `API Error: 400`, when it is classified, then it is a normal failure and the retry policy applies.
- AE2. **Covers R4, R6, R9.** Given a step with `max_attempts: 3` and a run that stops on the usage limit with a reset in 2 hours, when the limit resets, then the same session id is resumed, the history shows one attempt, and no attempt is marked failed.
- AE3. **Covers R5.** Given a step that already runs with `resume_session` and stops on the usage limit, when it is classified, then no "start a new session" request goes to the operator and `restart_if_refused` does not start a new session.
- AE4. **Covers R8.** Given a step that waits for a reset at 15:00, when the runner restarts at 14:00, then at 15:00 the same session is resumed.

### Scope Boundaries

- Deferred: detection for codex, opencode and github-copilot (Copilot already reports `has_quota: false` as `Rejected`; it can follow later with its own recording).
- Deferred: holding back new steps or new tasks before they start when the limit is known to be exhausted (proactive scheduling from the usage data).
- Deferred: alarms or notifications about a waiting task outside the existing task state.
- Not included: overage / paid extra usage settings, and switching to another account or harness when the limit is reached.

### Dependencies / Assumptions

- The usage data flow from the earlier work (`docs/brainstorms/2026-10-31-limity-abonamentu-harnessow-requirements.md`) stays as is; this work can read it but does not change its display scope.
- Claude Code accepts `--resume <id>` on a session that stopped on the usage limit, and the conversation before the stop is kept. R1 recordings must confirm this.

### Założenia do potwierdzenia

- A1. Claude Code gives a reset time in the stream (`rate_limit_event.resetsAt` with status `rejected`, or the "resets …" text in the limit message). If only the text exists, a time without a date and with a local timezone is interpreted as the next such time in that timezone.
- A2. The fallback retry interval is 15 minutes, and the safety margin after a reported reset is 1 minute. Both are settings with these defaults.
- A3. The wait has no maximum length; a 7-day window can make a task wait for days. The operator can cancel it (R12).
- A4. Only the step that hit the limit waits. Other steps that hit the limit wait on their own; Narsor does not block steps that did not hit it (see the questions).
- A5. The waiting state uses the existing `Paused` task state with a reason, not a new task state, so that clients need small changes only (see the questions).
- A6. The continuation prompt is a short fixed text that tells the agent to continue the interrupted work; it is not the step's original prompt.
- A7. The R1 recording is made by the developer on a real account at a moment when the limit is exhausted, then cleaned of personal data; a synthetic line is not enough for R1.
- A8. A usage-limit stop in a conductor chat or other non-step conversation is out of scope for v1 and keeps today's behaviour.

### Pytania otwarte dla użytkownika

- Q1. (Requirements → Waiting, A4) When one step hits the limit, what happens to other steps and tasks on the same harness subscription?
  - Options: (a, recommended) Only the step that hit the limit waits; the others continue and wait on their own when they hit it. (b) All new agent steps on that harness wait until the reset, so they do not start and stop at once.
- Q2. (Requirements → Waiting, R7, A5) How should a waiting task look to the operator?
  - Options: (a, recommended) The existing `Paused` state with a reason "usage limit — resumes at HH:MM". (b) A new task state, for example `WaitingForUsage`, in the TUI, CLI and web. (c) The `Running` state with a note.
- Q3. (Requirements → Waiting, A3) Is there a maximum wait after which the task goes to the operator instead?
  - Options: (a, recommended) No maximum; the step waits until the reset and the operator can cancel it. (b) A maximum (for example 24 hours), then `HumanNeeded`.

### Outstanding Questions

**Resolve Before Planning**

- Q1–Q3 above.

**Deferred to Planning**

- The exact detection signals for Claude Code, after the R1 recording exists.
- Where the wait lives (runner timer vs. engine scheduling, for example next to `auto_resume.rs`) so that R8 holds.
- How the waiting attempt is stored so the history shows one attempt (R4).

### Sources / Research

- `crates/narsor-exec/src/engine/actions/harness/claude_code.rs` — `rate_limit_event` handling and `finish` failure rules.
- `crates/narsor-exec/src/engine/actions/harness/claude_code/usage.rs` — parsing of `rate_limit_info` (`resetsAt`, status).
- `crates/narsor-exec/src/engine/actions/session/process.rs` — `harness_failed` / `resume_session_rejected`.
- `crates/narsor-exec/src/engine/runner/step/action.rs`, `crates/narsor-exec/src/engine/runner/step/attempt.rs`, `crates/narsor-exec/src/engine/retry.rs` — retry loop, new session per attempt, refused-resume handling.
- `crates/narsor-engine/src/controller_protocol/service/scheduling/harness_usage.rs`, `auto_resume.rs` — in-memory usage state; auto-resume of lost workloads.
- `crates/narsor-engine/src/orchestration/model.rs` — execution status to task state mapping.
- `tests/fixtures/claude-code/README.md` — existing recordings; none shows an exhausted limit.
- `docs/brainstorms/2026-10-31-limity-abonamentu-harnessow-requirements.md` — earlier usage-display work and its scope boundaries.
