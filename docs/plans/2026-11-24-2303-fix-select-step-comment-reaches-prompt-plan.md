---
title: Select Step - The Comment Reaches the Prompt of the Chosen Step - Plan
type: fix
date: 2026-11-24
topic: select-step-comment-reaches-prompt
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Select Step - The Comment Reaches the Prompt of the Chosen Step - Plan

## Goal Capsule

- **Objective:** When the owner stops a task, chooses a step (*Wybierz krok*) and types a comment for the agent, the agent of that step gets the comment in its prompt. This is true in every frontend (TUI, Web UI, CLI) and on every path the server and the runner take.
- **Means:** Find the exact place where the comment is lost with a test that reproduces the owner's path (Stop, then a new runner launch, then `SelectStep` with a comment). Fix the runner and engine so that the comment is applied to the chosen step, or the command is refused loudly. Add the same capability to the CLI.
- **Product authority:** This plan covers the `SelectStep` operator command from the server offer to the agent prompt. On product behavior the Requirements (R-IDs) win.
- **Open blockers:** None. The assumptions in *Założenia do potwierdzenia* hold until the person changes them.
- **Execution profile:** One pull request. Expected changes: `crates/narsor-runner`, `crates/narsor-exec`, `crates/narsor-engine` (server checks), `src/cli` (new CLI option), tests, `docs/user/*`.
- **Stop conditions:** Stop and ask if the fix needs a breaking change of the wire format of `OperatorCommand::SelectStep`, or if the comment must stay for more than one run of the step.
- **Who finishes:** The implementer runs `cargo test --workspace` and hands the branch for review. Commit and push only when asked (AGENTS.md).

---

## Product Contract

### Summary

The comment that the owner types for an agent step in *Wybierz krok* must appear in the prompt of the first agent attempt of that step. If the system cannot deliver the comment, it must not start the step silently without it: it refuses the command with a reason that the owner sees. The fix is in the server and the runner, so the TUI and the Web UI get it without their own change. The CLI gets a way to choose a step with a comment.

### Problem Frame

The owner reports (TUI): Stop → `.` → choose a step manually → step → fill in the extra prompt field → the text does not reach the prompt of that step.

What the code does today (static trace, not yet reproduced by a test):

1. The server builds the *Wybierz krok* submenu (`step_menu`, `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs:274`). For an agent or profile step it adds the optional field `komentarz` and sends `OperatorCommand::SelectStep { target, note: "{{komentarz}}" }`. The action is offered only when `runner_state == Stopped`.
2. The TUI (`src/tui/task_app/actions.rs:941`) and the Web UI (`crates/narsor-web/ui/js/ui/actions.js`) fill the placeholder through `TaskActionSend::resolve` and post the command to `/control/v1/tasks/:id/operator-commands`.
3. The server queues the command for the next launch generation and resumes the task (`crates/narsor-engine/src/controller_protocol/service/operator/commands.rs:26`).
4. The new runner starts (`start_work`, `crates/narsor-runner/src/runner/mod.rs:1140`). For a cleanly closed session it calls `go_on()` **before** it reads the queued operator commands. If the execution is `Paused` or `Running`, `go_on()` starts a pass of the **old** current step at once (`resume_pass`).
5. Then the runner reads `SelectStep` (`crates/narsor-runner/src/runner/commands.rs:201`). If a pass is active, it refuses the command ("a step is running; interrupt it first"). If no pass is active, it moves the execution to the target step, writes the note with `leave_operator_note`, and starts a pass. If the note cannot be written, the runner only logs a warning and starts the step without it.
6. The first agent attempt takes the note from `metadata.engine.operator_recovery_note` and appends it to the prompt (`prepare_harness_attempt`, `crates/narsor-exec/src/engine/runner/step/harness_attempt.rs:87`).

Probable causes of the loss (to confirm with the reproducing test):

- **H1 (most probable): the old step starts before the command.** A Stop that lands at a safe point between steps leaves the execution `Paused`. At the relaunch, `go_on()` runs the old step before `SelectStep` arrives, and `SelectStep` is then refused. If the owner chose the same step, the step runs again with its normal prompt and without the comment. The 2026-11-11 plan required the opposite ("Runner stosuje komendę przed ruszeniem przebiegu, żeby nie ruszył stary krok", `docs/plans/2026-11-11-001-feat-akcje-taska-pauza-stop-panel-plan.md:216`), and no test covers Stop → relaunch → `SelectStep`.
- **H2: a silent drop when the note is not written** (`commands.rs:215-221`).
- **H3: the note is overwritten** by another note written for the same execution between the write and the attempt (for example a resume comment), or the note is consumed by a different agent step (a note sent for a non-agent target stays in metadata and goes to the next agent step).
- **H4: a display gap.** The note is in the prompt sent to the harness, but the TUI conversation view does not show it, so the owner thinks it is lost. To check.

Existing test `a_chosen_step_reopens_a_finished_workflow_and_carries_the_note` (`crates/narsor-runner/.../session_tests.rs:2040`) covers only a live runner, not the owner's path.

### Key Decisions

- **The fix is in the runner, the engine and the server, not in the frontends.** The TUI and the Web UI already send the comment correctly. All frontends get the fix at once. Governs R1–R6.
- **A queued operator command that changes the step is applied before the runner starts any pass.** This restores the rule of the 2026-11-11 plan. Governs R2.
- **No silent loss.** When the comment cannot be delivered, the command is refused and the step does not move. Governs R3.
- **The step move and the note are stored as one unit.** Today the runner moves the step first and writes the note second (`select_step` in `crates/narsor-runner/src/runner/commands.rs`), with a comment that says this order is on purpose. That order cannot give R3: after the move, a failed note write cannot bring the step back. The change must make both succeed together or leave the execution untouched, with no note left on the old step (which would break R4 and R5). The implementer chooses the mechanism (for example one engine operation that does both under one save) and updates the old comment. Governs R3, R4, R5.
- **The comment stays one-shot.** It goes to the first agent attempt of the chosen step only and does not stay for later runs (Z10 of the 2026-11-11 plan). Governs R5.

### Requirements

**Delivery of the comment**

- R1. After `SelectStep { target, note }` with a non-blank note for an agent or profile step, the prompt of the first agent attempt of `target` contains the note. This is true for each start state of the stopped task: execution `Paused`, `Unknown`, `Failed`, `Completed`, `InputNeeded`, and a finished workflow.
- R2. When the runner starts with a queued `SelectStep` (or another queued operator command that moves the step), it applies the command before it starts a pass. The old current step does not start.
- R3. When the note cannot be written or kept for the target step, the runner refuses the command with a reason, and the execution stays at its old step. The owner sees the refusal in the frontend (the existing command verdict path).
- R4. The note goes only to the target step. No other agent step gets it, and another note written before the attempt does not replace it silently. If two texts meet (for example a resume comment and the select-step note), the agent gets both.
- R5. The note is consumed by the first attempt of the target step. A later run of the same step does not get it again.

**Validation**

- R6. The server refuses `SelectStep` with a non-blank note when the target step is not an agent or profile step, with a reason. This keeps the note from leaking into a later agent step.

**Frontends**

- R7. TUI and Web UI: no change in the form. They must show the refusal reason of R3 and R6 the same way as other refused commands.
- R8. CLI: `narsor-cli` gets a command (or option) to choose a step of a stopped task with an optional comment, which sends `SelectStep { target, note }`. It prints the refusal reason when the server refuses.
- R9. The conversation view (TUI *Rozmowa*, Web UI) shows the prompt that the agent got, including the note, so the owner can check the delivery. (Only if H4 is confirmed as a gap.)

**Documentation**

- R10. `docs/user/tui.md`, `docs/user/web.md` and the CLI help say where the comment goes (the prompt of the first attempt of the chosen agent step) and that it does not stay for later runs.

### Acceptance Examples

- AE1. (R1, R2) A task runs step `implement`. The owner presses Stop; the pause lands at a safe point before step `review`. The owner chooses step `implement` with the comment "use the new API". The runner starts; step `implement` runs once, and its prompt ends with the comment. Step `review` does not start before it.
- AE2. (R1) A task is stopped in the middle of an agent step (execution `Unknown`). The owner chooses another agent step with a comment. That step's prompt contains the comment.
- AE3. (R3) Writing the note fails. The command is refused with a reason; the task stays at its old step; no pass starts without the note.
- AE4. (R6) A client sends `SelectStep` with a note for a script step. The server refuses it with a reason.
- AE5. (R5) After AE1, the workflow comes back to `implement` by a normal transition. The prompt does not contain the comment.
- AE6. (R8) `narsor-cli task select-step <task> implement --note "use the new API"` gives the same result as AE1.

### Scope Boundaries

- In scope: `SelectStep` delivery in the runner and engine, server validation, a CLI command, tests, user docs, and the conversation view only if H4 is a real gap.
- Out of scope: the override transition (*Przejście ręczne → Override: dowolny stan*). Its `powód` field stays audit only.
- Out of scope: new fields in the TUI or Web UI forms.
- Out of scope: a persistent comment for all later runs of the step.

### Założenia do potwierdzenia

- A1. The owner used *Wybierz krok* (the only step choice offered after Stop), not *Przejście ręczne → Override*. The override needs a live runner and is not offered after Stop. Its reason field stays audit only, as the documentation says.
- A2. The comment goes at the end of the step prompt under the existing heading "Operator recovery note:". A new heading or position is not necessary.
- A3. "Global" means: the fix is in the server and the runner (so TUI and Web UI get it), and the CLI gets the same capability (R8). The local daemon API (`TransitionRequest` in `crates/narsor-protocol/src/api.rs`) does not get a note field in this change.
- A4. A refusal (R3, R6) is safer than a step that starts without the comment, because the owner can send the command again.
- A5. The comment stays one-shot (Z10 of the 2026-11-11 plan).
- A6. H1 is the most probable cause, but the plan does not depend on it: the reproducing test decides, and R1–R5 must hold for all causes.
- A7. The CLI command name is `task select-step <task> <step> [--note <text>]`, next to `task transition` and `task restart-step --note`. The name can change in planning.

### Pytania otwarte dla użytkownika

None. Each unclear point has a safe assumption above.

### Outstanding Questions

- For planning: which of H1–H4 the reproducing test confirms; whether more than one applies.
- For planning: whether R9 is necessary (depends on H4).

### Sources / Research

- `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs:243-308` (`transition_menu`, `step_menu`), `:412` (`select_step_reason`).
- `crates/narsor-protocol/src/controller/task/input.rs:100-118` (`Transition`, `SelectStep`), `crates/narsor-protocol/src/controller/task/actions.rs:301-370` (`placeholder`, `resolve`).
- `crates/narsor-runner/src/runner/mod.rs:1140-1200` (`start_work`), `:1470-1566` (`recover`, `go_on`, `resume_pass`).
- `crates/narsor-runner/src/runner/commands.rs:100-223` (command dispatch, `select_step`).
- `crates/narsor-exec/src/engine/runner/human_input/operations.rs:240-301` (`leave_operator_note`, `leave_resume_message`), `:392-580` (`take_operator_transition`, `apply_manual_transition`).
- `crates/narsor-exec/src/engine/runner/step/harness_attempt.rs:56-95`, `:309-329` (note into the prompt).
- `src/tui/task_app/actions.rs:913-990`, `src/tui/stepped.rs`, `crates/narsor-web/ui/js/ui/actions.js`, `src/cli/remote.rs:372-401`.
- `docs/plans/2026-11-11-001-feat-akcje-taska-pauza-stop-panel-plan.md:108-111`, `:212-236`.
- `docs/user/tui.md:260-275`, `docs/user/web.md:112`, `docs/sources/narsor-owner-api-wire-reference.md:441-464`.

## Deferred / Open Questions

These entries are review items left for the author. They do not block implementation.

### From 2026-11-24 review

- **R2 does not say what happens when the first poll is slow or fails** — Requirements, R2 (P2, lite, confidence 75)

  If the runner does not receive the queued command in time, the old paused step can still start. `SelectStep` is then refused with "a step is running", which is the H1 failure that R2 should remove. In the clean-session start path, the wait for the first poll (`clean_launch_wait`) ends with a timeout, and `go_on()` runs anyway. The first poll also keeps only the launch part of the response and drops the queued commands.
  Change: choose one outcome for this window and write it in R2 — either the pass start waits for the queued commands, or the later `SelectStep` is refused with a clear reason. Also carry out any queued step-moving command from the first successful poll before `go_on()`.
