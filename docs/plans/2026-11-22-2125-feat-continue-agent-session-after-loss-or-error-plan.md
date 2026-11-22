---
title: "Continue the existing agent session after a workload loss or a step error - Plan"
type: feat
date: 2026-11-22
topic: continue-agent-session-after-loss-or-error
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Continue the existing agent session after a workload loss or a step error - Plan

## Goal Capsule

- **Objective:** When a task stops because its workload was lost or its step failed, the owner can write to the agent ("Napisz do agenta"). The agent continues the current step in the same harness session and keeps its context. When the session cannot be continued, the owner is told before sending that a new session starts.
- **Product authority:** Issue #140. Assumptions taken without the owner are listed in "Założenia do potwierdzenia".
- **Means:** The engine stores the harness session id as soon as the harness reports it. A recovery can resume a known session with the owner's text. The control plane offers the comment action in four more owner/runner states.
- **Open blockers:** None.

---

## Product Contract

### Summary

The comment action ("Napisz do agenta", `TaskActionId::Comment`) becomes available in four more states: `Paused`/`Interrupted`, `Error`/`Interrupted`, `Error`/`Unknown` and `Error`/`Stopped` with a `Failed` execution. The owner's text resumes the harness session of the interrupted or failed step. When no session can be resumed, the action stays available, its description warns that a new session starts, and the text reaches the agent as the operator recovery note.

### Problem Frame

Today the owner can write to the agent only in a pause with a stopped runner, or when the task waits on an agent question, an approval or a permission. After a workload loss or a failed step, the owner can only use "Uruchom od nowa" or "Wybierz krok". Both start a new agent session, so the agent loses what it did and learned in the step. Long steps are the most exposed: a pod loss late in a step throws away the most context.

Three gaps cause this (verified in the code):

- The offer, the send and the resume accept only runner state `Stopped` (`comment_reason`, `submit_comment` in `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs`; `resume_for_work` in `crates/narsor-engine/src/controller_protocol/service/operator/input.rs`). The lifecycle already allows `Resume` from `Interrupted` and `Unknown` (`command_effect` in `crates/narsor-engine/src/orchestration/lifecycle.rs`).
- The runner `recover` (`crates/narsor-runner/src/runner/mod.rs`) appends a pending comment to the recovery note, and `prepare_recover` (`crates/narsor-exec/src/engine/runner/lifecycle/resume.rs`) starts the action afresh.
- The session id of an in-flight attempt is stored only when the step completes or the harness finishes. A pod that dies mid-step leaves no record of it.

### Requirements

**Offer and send (control plane)**

- R1. The comment action is offered in these owner/runner states: `Paused`/`Interrupted`, `Error`/`Interrupted`, `Error`/`Unknown`, and `Error`/`Stopped` when the execution is `Failed`. The states where it is offered today stay unchanged.
- R2. The comment action stays refused in `Finished`, `Archived` and `Queued`, and in every state that is neither named in R1 nor offered today.
- R3. The offer (`comment_reason`) and the server check at send (`require_available`) give the same answer for every state.
- R4. A comment sent in an R1 state is stored as the pending resume message, and the task gets `OwnerCommand::Resume`, from `Interrupted` and `Unknown` as well as from `Stopped`.
- R5. The description of the action (`comment_effect`) says, for each R1 state, whether the agent continues its session or starts a new session that gets the text as a note.
- R6. The task summary tells the client whether the session of the current step can be resumed. When this is not known, the description uses the new-session warning.

**Session continuity (engine)**

- R7. The engine stores the harness session id of the running step in the execution metadata as soon as the harness reports it, before the step ends. The stored value is cleared when the step completes.
- R8. The step failure record stores the session id when it is known.
- R9. A recovery that carries an owner message resumes the known harness session of the current step with that text, instead of restarting the action. The step's output contract applies to the result, as for a reprompt.
- R10. Session lookup order for R9: the in-flight session of the current state, then the step failure record (R8), then the output rejection or rescue record, then `completed_agent_sessions` for the state. For a `Failed` execution the failure record is the source of the failed attempt's session, because a step failure clears the rejection and rescue records. The lookup must not fall through to an older session of the same state while the failure record holds one.
- R11. When no session is found, or the harness of the step cannot resume a session, the recovery keeps today's behavior: it restarts the action and gives the text as the operator recovery note.

**Delivery (runner)**

- R12. In `recover`, the runner passes a pending resume message to the continue-session recovery of R9, not as text appended to the recovery note. A `Failed` execution does not pass through `recover`: the relaunch is a clean launch, and the existing resume-message step refuses a `Failed` execution. The clean launch of a `Failed` execution must therefore also accept the pending message and hand it to the same continue-session recovery. Planning chooses the code shape.
- R13. Each message reaches the agent exactly once across runner restarts. The existing `resume-message:<key>` storage mark governs this.

### Key Flows

- F1. **Comment after a workload loss (session known).** Covers R1, R4, R7, R9, R12, R13.
  - **Trigger:** The task is `Error`/`Interrupted` (or `Paused`/`Interrupted`, `Error`/`Unknown`). The session id of the step was stored before the loss.
  - **Steps:** The owner opens "Napisz do agenta". The description says the agent continues its session. The owner sends the text. The control plane stores the resume message and sends `Resume`. A new runner starts, reconciles the execution and runs the continue-session recovery. The harness resumes the stored session with the text. The runner marks the message as given.
  - **Outcome:** The step goes on in the same session. The timeline shows the same `harness_session_id`.
- F2. **Comment after a failed step.** Covers R1, R8, R9, R10, R12.
  - **Trigger:** The task is `Error`/`Stopped` and the execution is `Failed` (harness crash, timeout, or output contract failure).
  - **Steps and outcome:** As F1, except that the runner reaches the continue-session recovery through the clean launch path (R12), not through `recover`. The session comes from the step failure record (R8) of the failed attempt, because the failure clears the rejection and rescue records.
- F3. **Comment with no resumable session.** Covers R5, R6, R11.
  - **Trigger:** The harness died before it reported a session id, or the harness cannot resume.
  - **Steps:** The action is offered. The description warns that a new session starts. After sending, the recovery restarts the action. The agent gets the text as the operator recovery note.

### Acceptance Examples

- AE1. **Covers R1, R9.** A task is `Paused`/`Interrupted` after a pod loss during a requested pause. The owner sends "Dokończ testy". The agent of the interrupted step gets the text in the same harness session.
- AE2. **Covers R1, R8, R9.** A step fails its output contract and the task is `Error`/`Stopped`. The owner sends a correction. The agent gets it in the session of the failed attempt, and the output contract checks the new result.
- AE3. **Covers R5, R11.** The harness died before its `init` line. The action description says that a new session starts. After sending, the step restarts and the text is the operator recovery note.
- AE4. **Covers R13.** The runner restarts after it gave the message to the harness. The message is not given a second time.
- AE5. **Covers R2, R3.** For a `Finished` task and for a `Stopped` runner of a task that is neither paused nor failed, the action is not offered, and a send is refused with the same reason.

### Key Decisions

- **When no session can be resumed, offer the action with a warning instead of hiding it.** Reason: the owner can still give the agent direction, and the text is not lost. Decided in issue #140. Governs R5, R11.
- **The engine stores the session id early, in the execution metadata.** Reason: a pod loss mid-step otherwise leaves no record of the session. Governs R7.
- **The text goes through the recovery path, not as an answer to a waiting request.** Reason: the runner already carries pending comments into recovery and marks them once. Governs R9, R12, R13.

### Scope Boundaries

- `Finished` and `Archived` tasks: out of scope.
- No classification of the failure cause (for example a context overflow) to force a new session when an id is known. See Assumption 3.
- The `runner_alive` result for `HumanNeeded` + `Stopped` + `holds_slot` ("Stop" and "Przejście ręczne" offered for a stopped runner): a separate issue, out of scope.
- No change to "Uruchom od nowa" and "Wybierz krok". They still start a new session.

### Dependencies / Assumptions

- All five harnesses handle `resume_session`: claude_code, codex, github_copilot, copilot_sdk and opencode (`--session`, `crates/narsor-exec/src/engine/actions/harness/opencode.rs`). Each one parses a session id from its stream (claude_code `init`, codex `thread_id`, opencode `sessionID`, the copilot harnesses `session_id`). This answers the first open question of the issue.

### Outstanding Questions

**Deferred to Planning**

- How the harness reports the session id to the engine during the run: the parsers collect it into the run result at the end today. Planning picks the channel (a callback, a stream observer or an equivalent).
- The name and shape of the in-flight metadata key and of the snapshot flag of R6.
- How the continue-session recovery counts against the attempt budget of the step (see Assumption 4).

### Założenia do potwierdzenia

1. **Fallback when no session is resumable.** The action is offered with a new-session warning, as the issue decided. The text goes into the operator recovery note, as today.
2. **opencode can resume.** The code passes `--session <id>` and parses `sessionID`, so opencode is treated like the other harnesses. A harness that does not report a session id before the loss is handled as "no session" (R11).
3. **A failure caused by the session itself (for example a context overflow).** When a session id is known, the engine resumes it, with no special case for the failure cause. If the resumed session fails again, the task goes to `Error` again, and the owner can still use "Uruchom od nowa". This is the safest choice because it adds no new classification logic. A later issue can add the classification.
4. **Attempt budget.** The continue-session recovery is an owner recovery, like "Uruchom od nowa" today. It follows the same episode and attempt rules as an owner recovery. Planning confirms the details.
5. **What the task does after the step.** In `Paused`/`Interrupted`, the comment has the same effect as today's comment in a pause ("wznawia zadanie jednym krokiem"). In the `Error` states, the task continues as after an owner recovery.
6. **Unknown resumability.** When the client cannot tell whether the session can be resumed (for example an older runner without the flag), the description uses the new-session warning. A wrong warning is safer than a wrong promise.
7. **`Error`/`Stopped` needs a `Failed` execution.** An `Error`/`Stopped` task with an execution in another status does not get the action.
8. **The `runner_alive` finding** in the issue is a separate issue and is not part of this work.

### Pytania otwarte dla użytkownika

None. Each unclear point has a safe assumption in "Założenia do potwierdzenia".

### Sources / Research

- Issue #140 (problem, proposed change, acceptance criteria, minimum tests).
- `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs`: `comment_reason`, `comment_effect`, `submit_comment`, `runner_alive`.
- `crates/narsor-engine/src/controller_protocol/service/operator/input.rs`: `resume_for_work`.
- `crates/narsor-engine/src/orchestration/lifecycle.rs`: `command_effect` (`Resume` from `Stopped | Interrupted | Unknown`).
- `crates/narsor-runner/src/runner/mod.rs`: `recover`, `untaken_resume_message`, `resume_message_mark`.
- `crates/narsor-exec/src/engine/runner/failure/handoff.rs`: `Recovery`.
- `crates/narsor-exec/src/engine/runner/lifecycle/resume.rs`: `prepare_recover`.
- `crates/narsor-exec/src/engine/runner/human_input/operations.rs`: `leave_resume_message` (the same-session behavior wanted here).
- `crates/narsor-exec/src/engine/actions/harness/`: session id parsing and `resume_session` for each harness.
