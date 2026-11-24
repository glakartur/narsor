---
date: 2026-11-24
topic: unified-harness-request-and-conductor-context-files
---

# Unified harness request and conductor context files

## Summary

Every caller of an agent harness (the workflow's agent step, the conductor,
and future tools such as a script generator) uses one interface: it builds one
self-contained request object and calls `run`. The harness translates the
request into its own form, starts the agent (CLI or SDK), reads its output and
returns a unified result, the mirror image of today's stream parsing. On top
of this interface, the conductor's prompt holds only a core, and the other
snapshot sections go to context files, in full, that the conductor reads on
demand.

---

## Problem Frame

NARSOR-1124-001 (`docs/bugs/NARSOR-1124-001.md`): the conductor put its whole
snapshot (a 160 KiB budget plus unbounded "whole" sections) into one argv
argument. Over 128 KiB (`MAX_ARG_STRLEN`) `execve` failed with E2BIG, and every
human gate summary failed. #143 fixed it for `claude-code` only (prompt over
96 KiB on stdin). `codex`, `opencode` and `github_copilot` still put the prompt
in argv.

The bug was possible because callers know harness details. Today the agent
step (`crates/narsor-exec/src/engine/actions/session.rs`) and the conductor
(`crates/narsor-exec/src/conductor/invoke.rs`) each repeat the same steps:

- build a `HarnessRequest` whose `config` is a workflow-step config
  (`HarnessStepConfig`); the conductor fakes one (`conductor_config`);
- call `adapter.invocation(...)` to get `program`, `args` and `stdin`;
- call `apply_extra_args`;
- pick one of `run_harness_process`, `run_harness_invocation`,
  `run_harness_invocation_with_sdk`, and pass `stdin` by hand (the conductor
  path did not, which caused the bug);
- pick the stream parser; the agent step also reads the model and effort back
  out of the harness's argv (`flag_value(&invocation.args, &["--model", "-m"])`).

So a new harness capability (prompt on stdin, context files) needs a change in
every caller, and a new caller has to copy all of it. The prompt side has the
opposite problem: the conductor's snapshot carries every section in the prompt
whether the purpose needs it or not (`GenerateTitle` gets the whole workflow
YAML and the agent conversations), and the budget cuts the data the conductor
might need (long conversations, older events).

---

## Actors

- A1. Caller: the agent step, the conductor, or a future tool; it states what
  it wants from an agent, never how a harness is started.
- A2. Harness: one per agent product (`claude-code`, `codex`, `opencode`,
  `github_copilot`, `copilot_sdk`); it owns everything specific to that
  product, in both directions.
- A3. Conductor agent: the model run by the conductor; it reads the core in
  the prompt and the context files on demand.

---

## Requirements

**Unified harness interface**

- R1. One request type is the only input of a harness. It is self-contained
  and independent of workflow-step configuration. It carries at least: the
  prompt, the context files (name and content), the model tier, the effort,
  the session to continue (when the call continues a conversation), the
  working directory, the workspace access (read-only or writable), the
  writable and read-only directories, the answer schema, the timeout, and the
  operator's extra arguments for the harness.
- R2. One call (`run`) is the only way to use a harness. The harness
  translates the request, starts the agent (CLI process or SDK), feeds it the
  prompt, reads its output and returns a unified result: the text, the
  structured answer, the session id, the usage, and the failure (start error,
  timeout, harness error).
- R3. Live output reaches the caller through observers it passes to `run`
  (conversation elements, usage readings), in the unified form the stream
  parsers produce today. The caller never sees argv, stdin, the process or the
  SDK.
- R4. The facts the caller needs about the run (the model and effort actually
  used, the session id) come back in the result. No caller reads a harness's
  argv.
- R5. Every harness accepts a prompt of any size. Each harness chooses how
  (argv, stdin, file). No prompt size makes a harness fail to start.
- R6. Every harness makes the context files readable by the agent and tells
  the agent where they are. A harness that cannot give the agent access to
  files (sandbox, SDK without file tools) appends their content to the prompt
  instead.
- R7. The agent step and the conductor both use only this interface. All five
  harnesses move to it in the same change; the old request type and the three
  `run_harness_*` entry points are removed, with no period of two paths.
- R8. Common process mechanics (timeout, killing the process group, stdin
  feeding, conversation file writing) stay shared, as a helper the CLI
  harnesses use, not as code each harness copies.

**Conductor input**

- R9. The conductor's prompt holds only the core: the fixed instructions, the
  execution's status and current state with its outputs, the request with its
  payload, the operator message it answers (chat), the pending input, the
  annotations, the current step's local values, and the last agent answer when
  the purpose analyses it (choose an output, answer an agent's question,
  diagnose a failure), then the task and the answer contract.
- R10. The core sections have no size limit (R5 makes this safe).
- R11. Every other snapshot section goes to a context file, uncut: the full
  pinned workflow definition, the context variables, all events, the agent
  conversations of every state and attempt, the chat, and the workflow's
  artifact files (isolated conductor only). The byte budget and its halving no
  longer apply to these sections.
- R12. The prompt lists the context files with what each holds and its size,
  and tells the conductor to read a file when the core does not settle the
  decision, and to escalate when the files do not settle it either.
- R13. The context files of one invocation live in the conductor's scratch
  area of the task. Each invocation replaces the files of the previous one; no
  history of snapshots is kept.

---

## Acceptance Examples

- AE1. **Covers R5, R7.** Given a conductor on `codex` and a request whose
  payload is 200 KiB, when the conductor is invoked, the harness starts and
  the conductor answers; no argv argument is 128 KiB or more.
- AE2. **Covers R6.** Given a harness that cannot give the agent file access,
  when it runs a request with context files, the agent receives their content
  in the prompt and the run does not fail.
- AE3. **Covers R9, R11.** Given a task with 10 states and long agent
  conversations, when the conductor summarizes a human gate, the prompt holds
  the core only, and the conversations file holds every attempt's
  conversation uncut.
- AE4. **Covers R13.** Given two conductor invocations of one execution, one
  after the other, when the second starts, the scratch area holds only the
  second invocation's files.
- AE5. **Covers R1, R2, R4.** Given a new caller (a test double for a future
  tool), it can run any of the five harnesses using only the request type and
  `run`, without harness-specific code.

---

## Success Criteria

- No conductor or agent step fails to start because of prompt size, on any
  harness.
- A typical conductor invocation (gate summary, title) sends a much smaller
  prompt than today on harnesses that read files; the conductor can still see
  data the old budget cut.
- Changing what the conductor or a step sends to an agent (prompt text,
  context files) needs no change in harness code; adding a harness capability
  needs no change in callers.
- The planner can design the request type, the `run` contract and the
  migration of five harnesses and two callers from this document without
  inventing behavior.

---

## Scope Boundaries

- Conductor session continuation and chat threads (NARSOR-1124-001, decision 1).
- Conductor cost in the task cost (NARSOR-1124-001, decision 2).
- Moving prompt texts out of code into configuration. Prompts stay in code,
  but outside harness code.
- Context files for workflow agent steps declared in YAML. The interface
  supports them; the workflow schema does not expose them in this change.
- The future script generator itself; only the interface it would use.
- Keeping a history of conductor snapshots for audit.

---

## Key Decisions

- The harness does everything, not only translation (option A over a shared
  runner fed by a translated command): an SDK harness fits without a second
  path, and the caller never sees how the agent is started.
- Core without a size limit: simpler; R5 removes the size risk.
- Everything else uncut in files: serves all three goals at once (size limit,
  cost and time, fuller data).
- Inline fallback for harnesses without file access: the run always works;
  for those harnesses the cost saving is lost.
- All at once: no period with two harness paths, which is how the stdin gap
  of the conductor path arose.
- No snapshot history: after the next invocation, the operator can no longer
  see which data an earlier conductor decision was based on; only the
  decision's rationale in the event remains. Accepted.

---

## Dependencies / Assumptions

- Conductor invocations of one execution are serialized (one worker drains
  the queue, `crates/narsor-exec/src/conductor/actor.rs`), so replacing the
  scratch files per invocation is safe. Verified.
- The success criterion on cost is measured only on harnesses that read the
  files (R6 fallback).
- Unverified: each CLI (`codex`, `opencode`, `github_copilot`) can read a
  prompt from stdin or a file, and the `codex` read-only sandbox lets the
  agent read files outside the working directory.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R6, R13][Technical] Where the context files live for a harness
  that works read-only in the task workspace (`claude-code`, `codex`): the
  files must not go into the workspace; how each harness makes the scratch
  area readable (e.g. `--add-dir` for `claude`).
- [Affects R5][Needs research] How `codex`, `opencode`, `github_copilot` and
  `copilot_sdk` take a large prompt.
- [Affects R1, R2][Technical] How the extra-argument rules (`apply_extra_args`,
  today anchored at the prompt) and the `contract_note` / answer delivery map
  into the request and the harness.
- [Affects R9][Technical] The exact core per conductor purpose, and the file
  layout and format of the context files.
- [Affects R3, R8][Technical] Where the agent step's conversation file and the
  usage relay attach: observers of `run` or the shared helper.
