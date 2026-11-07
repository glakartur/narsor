---
title: Subagenci poza głównym wątkiem, odmowa uprawnienia nie zawiesza kroku z odpowiedzią - Plan
type: fix
date: 2026-11-07
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-plan-bootstrap
execution: code
---

# Subagenci poza głównym wątkiem, odmowa uprawnienia nie zawiesza kroku z odpowiedzią - Plan

## Goal Capsule

- **Objective:** In autopilot, a step whose agent finished with a valid answer goes on to the conductor or the next step. A permission denial that blocked nothing does not stop it. The operator can tell what the main agent said from what a subagent said, in the TUI and in the CLI.
- **Means:** The adapter of the harness marks subagents. The engine and the views read one harness-independent structure (KTD1, KTD2).
- **Authority:** This plan, then the settled decisions below. The old decision "a denial after recovery is not a success" (`classify.rs:114`, from #2) is reversed in part, on the user's explicit request (KTD3).
- **Execution:** One PR on a new branch from `main`. U1–U4 fix the defect. U5 is the view and can ship as a second PR if the first grows too big. `ce-work` finishes and ships.
- **Open blockers:** None.

---

## Product Contract

### Problem Frame

Task `task-326aba9d…` (autopilot) ran `ce-code-review`. A validator subagent called `Bash` (a git `core.fsmonitor` check). Claude Code denied it in print mode. The agent went on and ended with a valid answer (`{"choice":"findings", …}`, with a question for the operator). Narsor ignored that answer and suspended the step as a permission request (event 224). Permission requests skip the conductor by design, so autopilot did not help.

Chain, checked in the recording and in code:

1. The last `result` line has `permission_denials` for a call whose `parent_tool_use_id` is the subagent "Validate review findings". The line itself has `subtype: success` and `is_error: false`.
2. `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs:117` sets `is_error` when any denial exists. `claude_code.rs` (`accumulate`) then drops the answer (`answered = None`) and copies the denials of the last `result` line.
3. `crates/narsor-engine/src/engine/actions/agent.rs:463` checks denials before it reads the answer (`read_response`, `agent.rs:526`) and suspends with `permission: Some(..)`.
4. `crates/narsor-engine/src/engine/runner/rescue.rs:390` gives no conductor answer for a request with `permission`.

The conversation also shows nothing that tells subagent lines from main-agent lines, so the operator cannot see who wrote what. In the recording 255 `assistant` and 187 `user` lines carry `parent_tool_use_id`; the main agent has 58 and 35. All lines share one `session_id`, so it cannot mark a subagent.

### Key Decisions

- Narsor talks to the main agent only. A subagent's text, tool calls and denials do not take part in the analysis of the step. (session-settled: user-directed — chosen over analysing subagent content in the engine: "z punktu widzenia narsor rozmawiamy tylko z agentem".) Governs R2, R3.
- The adapter of the harness finds subagents. The runner, the engine and the views get unified structures, so other harnesses can fill them later. (session-settled: user-directed — chosen over parsing raw lines in the runner: "rozpoznanie robimy na poziomie harness… abyśmy mogli inne harness dostosować".) Governs R1.
- A subagent is recognised by `parent_tool_use_id`, not by `session_id`. (session-settled: user-directed — chosen over `session_id`; the recording shows one `session_id` for all lines, so the user's hypothesis does not hold in the data.) Governs R1.
- A denial that came before a valid final answer is only a log entry; the step goes on. (session-settled: user-directed — chosen over keeping the suspension: "to pytanie o dostępy możemy zignorować (wystarczy, że w logu jest umieszczone)".) Governs R3, R4.
- Subagent lines are marked and shown apart from the main thread, one collapsed row per subagent, with indented content under a header that names it. Separate switchable view is not built. (session-settled: user-approved — proposed with the trade-off, accepted: "potwierdzam" to the scope.) Governs R6, R7.
- The idea "flag whether the answer is valid when the step still suspends" goes to future ideas. (session-settled: user-directed — chosen over doing it now: "punkt 5 wpisz do pomysłów na przyszłość".) See Scope Boundaries.

### Requirements

- R1. A conversation entry and a permission denial from a subagent carry a harness-independent reference `SubagentRef { id, name }`. The main agent has none. Only the adapter sets it. The adapters of other harnesses leave it empty and behave as before.
- R2. A denial that belongs to a subagent never suspends the step and never changes `is_error`, the answer or the failure of the run.
- R3. A denial that belongs to the main agent suspends the step only when the run has no usable final answer. With a usable final answer the run completes.
- R4. Each ignored denial is recorded as the event `permission_denied_ignored` with the tool, the call id, a one-line summary, the reason when known, and the source (main agent or the subagent's name). The event does not suspend anything.
- R5. The event reads clearly in the TUI event view and in the CLI event text: one row with source, tool and the key parameter. The full data stays in the event details.
- R6. In the TUI conversation, entries of one subagent form one collapsed group with a header (name, number of elements). The expanded content is indented. Main-agent entries look as before.
- R7. The CLI conversation print marks each subagent line with the subagent's name and indents it.
- R8. A denial of the main agent without a usable answer still produces the permission request as today (regression).

### Scope Boundaries

**Non-goals.** Letting the conductor grant permissions in autopilot. A separate switchable view for one subagent's thread. Analysing subagent content in the engine.

**Deferred to Follow-Up Work.** Three ideas are written in `docs/follow-ups/2026-11-07-subagenci-i-odmowy-pomysly-na-przyszlosc.md` (already in the repo; U6 only links it from the docs): "flag whether the final answer is valid when the step suspends" (item 1 in that file; called "idea 5" in the earlier conversation), the conductor-grants-permissions non-goal, and the separate switchable view non-goal. The flag idea: when a step still suspends on a permission, check whether the text in `answer_tail` is a valid final answer and show the flag in the event and the TUI.

### Assumptions

- Print mode is the only mode where denials reach Narsor (as in #2). Not verified for other Claude Code modes.
- A nested subagent line carries the id of its nearest `Task`/`Agent` call. The plan shows it as one flat group under that call's name. Deeper nesting is not drawn.
- The recorded stream is from Claude Code 2.1.x (see `classify.rs`). The marker `parent_tool_use_id` is stable there, but the format of later versions is not checked.

---

## Planning Contract

### Key Technical Decisions

- **KTD1.** `SubagentRef` lives in `narsor-protocol` next to `ChatElement`. The mark is on the conversation **entry** (`ConversationEntry`, `ConversationEntryDto`), not on each `ChatElement` variant, because `parent_tool_use_id` belongs to the whole line and an entry is one line. New field is `#[serde(default, skip_serializing_if = "Option::is_none")]`, so stored conversations and older pages still read. (session-settled: user-directed — unified structure from the adapter, see Key Decisions.) Chosen over a field on each of the ~10 element variants: one field, one place, no change in every `match`.
- **KTD2.** The parser trait `HarnessStreamParser` gets a default method that returns the subagent of the last parsed line (`None` by default). `process.rs` calls it right after `on_stdout_line` and stores it with the entry. Only `claude_code` overrides it. Chosen over changing the return type of `on_stdout_line`: that would touch codex, opencode, github_copilot and copilot_sdk, which have nothing to say.
- **KTD3.** The engine decides in one function, in `agent.rs`, which denials block the step: denials without `subagent`, and only when the run has no usable final answer. `classify.rs:117` stops forcing `is_error` for denials. A denial before a usable answer becomes an ignored-denial event. (session-settled: user-directed — see Key Decisions; reverses part of #2.) A main-agent denial with only prose and no closing answer keeps the suspension (R8).
- **KTD4.** Denial attribution uses the stream, not the `result` line: the parser remembers `tool_use_id → parent_tool_use_id` from `assistant` `tool_use` blocks and `Task id → name` from `Task`/`Agent` calls. The name is the `description` of the call, then `subagent_type`, then the text "subagent". `PermissionDenial` gets `subagent` and a one-line `summary` (the existing `call_summary` of `tools.rs`).
- **KTD5.** Denials are collected over all `result` lines of a run (deduplicated by `tool_use_id`), not replaced by the last line. In the recording the denial sat in the last of 8 `result` lines, so "last wins" worked by chance. Verify in U2 whether a `result` repeats earlier denials; keep the dedup either way.
- **KTD6.** The view adds one **group layer above** the existing grouping. A `TranscriptEntry` has a single `grouping` slot (`src/tui/transcript.rs:149`), already used for tool-call operation cards and technical summaries, so it cannot also mark a subagent. The entry gets its own optional `subagent`, and the projection folds all entries with the same `SubagentRef.id` into one collapsed group, placed at the subagent's first entry. Lines of one subagent can be separated by main-agent lines (background subagents), so the group is keyed by id, not by contiguity. Inside the group the entries keep their own operation cards. The group is a **new projection pass that runs before** the existing contiguous grouping in `projected_entries` (`transcript.rs`, ~942-1160), because the existing summary cards are built only from contiguous runs. It has its own `ExpansionIdentity` keyed by `SubagentRef.id` and its own reveal snapshot. Members render through the existing per-entry projection, so operation cards survive inside the group. Entries that arrive later join the group at its fixed position (first entry). Only the summary text format of the header is reused, not the contiguous-run logic. No new widget.

### Technical Design

Directional only.

```
Claude Code stream line
  └─ adapter (claude_code): parent_tool_use_id? -> SubagentRef{id,name}
        ├─ entry mark  ──> persisted conversation ──> API DTO ──> TUI group / CLI prefix
        └─ denial mark ──> HarnessRun.permission_denials[].subagent
                               └─ agent.rs: blocking = no subagent AND no usable answer
                                     ├─ blocking     -> suspend (permission request, as today)
                                     └─ not blocking -> event permission_denied_ignored, run goes on
```

### Risks

- The change reverses part of #2. A real main-agent denial next to a valid answer is now only a log line. The event keeps it visible, and idea 5 and the existing suspension cover the case with no answer.
- The test fixture must be synthetic. The recording of `task-326aba9d…` holds code and review text of the company repository and does not go into the repo.

---

## Implementation Units

### U1. Contract: `SubagentRef`, entry mark, denial fields, event

**Goal:** One harness-independent shape for "this came from a subagent", and the event for an ignored denial.
**Requirements:** R1, R4. **Decisions:** KTD1, KTD4.
**Files:** `crates/narsor-protocol/src/chat.rs`, `crates/narsor-protocol/src/api.rs` (`ConversationEntryDto`), `crates/narsor-protocol/src/event.rs` (new `EventData::PermissionDeniedIgnored`, summary text near `event.rs:800`), `crates/narsor-engine/src/persistence/conversation.rs` (`ConversationEntry`, write and read), `crates/narsor-engine/src/engine/actions/harness/mod.rs` (`PermissionDenial`: `subagent`, `summary`), `crates/narsor-engine/src/api/projection.rs`.
**Approach:** Add the types and optional fields with serde defaults. The event holds `session_id`, `action_execution_id`, `tool_name`, `tool_use_id`, `summary`, `reason`, and `source` (main agent or `SubagentRef`).
**Test scenarios:**
- A conversation entry with `subagent` survives write and read; an entry written without the field reads as `None`.
- The DTO and the event serialize and deserialize; an event log without the new event type is not affected.
- `skip_serializing_if` keeps the main-agent entry byte-identical to today.
**Test files:** `crates/narsor-protocol/src/chat.rs` (unit), `tests/integration/` conversation persistence test next to existing ones (find with `grep -rn ConversationEntry tests`).
**Verification:** `cargo test -p narsor-protocol`, then the workspace build compiles all consumers.

### U2. Claude Code adapter marks subagents

**Goal:** The adapter turns `parent_tool_use_id` into `SubagentRef` for entries and denials.
**Requirements:** R1, R2. **Decisions:** KTD2, KTD4, KTD5.
**Dependencies:** U1.
**Files:** `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`, `claude_code/classify.rs`, `claude_code/tools.rs` (reuse `call_summary`), `crates/narsor-engine/src/engine/actions/harness/mod.rs` (trait default method), `crates/narsor-engine/src/engine/actions/agent/process.rs` (call the new trait method right after `on_stdout_line`), `crates/narsor-engine/src/engine/actions/agent/conversation.rs` (`record_stdout_line` and `ConversationSink::append` carry the optional `SubagentRef` into the stored entry; `append_narsor` and `append_outcome` pass `None`), new fixture `tests/fixtures/claude-code/subagent_denial.ndjson` and its note in `tests/fixtures/claude-code/README.md`.
**Approach:** Keep two maps in the parser. A line with non-null `parent_tool_use_id` marks its entry. Attribute each denial of a `result` line by its `tool_use_id`. Collect denials over the whole run, deduplicated. The existing filter at `claude_code.rs:306` (main-agent text only) uses the same helper, so there is one check for "is this a subagent line". Write the fixture by hand with invented content: a main agent, one background and one foreground subagent, a denied `Bash` call of the foreground one, a final `result` with `permission_denials` and a valid closing answer.
**Test scenarios:**
- Lines of a subagent give entries with `subagent.name` from the `Task` description; main-agent lines give `None`.
- A `Task` call without `description` falls back to `subagent_type`, then to the fixed text.
- A denial of a subagent call gets `subagent`; a denial of a main-agent call gets `None`.
- Two `result` lines that both list the same denial give one denial.
- A line of a nested subagent is attributed to its nearest `Task` call.
**Test files:** `tests/integration/claude_code_classification.rs` (structure only, per the repo rule: no prompt phrases; the fixture is read with `include_str!`, no process or file at run time).
**Verification:** `cargo test --test integration claude_code_classification`.

### U3. Engine rule: which denials block a step

**Goal:** A step with a usable final answer is not suspended by a denial; a blocked step still is.
**Requirements:** R2, R3, R4, R8. **Decisions:** KTD3 (KTD5 is done in U2).
**Dependencies:** U1, U2.
**Files:** `classify.rs` (remove `is_error |= !permission_denials.is_empty()` at line 117 and the comment above it), `crates/narsor-engine/src/engine/actions/agent.rs` (the check at line 463: one pure function decides which denials block; emit `PermissionDeniedIgnored` for the rest). `accumulate` in `claude_code.rs` needs no denial-specific error logic once line 117 is gone; collecting denials over all `result` lines is done in U2 (KTD5).
**Approach:** A **usable final answer** means: `run.failure` is `None` and the answer passes the same test the normal path uses (`final_answer_kind` for the answer shape, or `closing_answer` for the text). `structured_output` can survive from an earlier `result` line when the last line is an error, so the mere presence of an answer is not enough. A denial blocks when it has no `subagent` and the run has no usable final answer. Otherwise emit the event and continue to `read_response`. Keep `run.failure` and `reported_error` semantics for real errors.
**Execution note:** Test-first. Start with the failing case from the task, stated as input and output of the pure function: a run with a subagent denial and a valid closing answer → the denial is in the ignored list, the blocking list is empty.
**Test scenarios:** (input: in-memory `HarnessRun` with denials with or without `subagent`, answer usable or not, `run.failure` set or not; output: the split into blocking and ignored denials. Event emission and `Completed` are checked in the manual verification, not in unit tests.)
- Subagent denial + valid closing answer → ignored, nothing blocks.
- Main-agent denial + valid closing answer → ignored, source "main agent".
- Main-agent denial + only prose (no closing answer) → blocks (R8). The suspension itself stays pinned by `tests/integration/actions_agent.rs:1325`; adjust its input only if it carries a closing answer.
- Subagent denial + no answer → ignored; the denial never turns into a permission request.
- Main-agent denial + an error `result` line, with an answer kept from an earlier line (`run.failure` set) → blocks, so the permission request stays, not a harness error.
- A mix of one subagent and one main-agent denial without an answer → only the main-agent denial blocks.
- Autopilot: holds by construction (`answer_by_conductor` is reached only for a non-permission request); checked manually.
- `a_result_preserves_structured_permission_denials_and_correlation` (`claude_code_classification.rs:1126`): its `failure` and `RunResult` assertions are rewritten to the new contract (`run.failure` is `None`, `RunResult.is_error` is false). The denial-field and reason assertions stay.
**Test files:** unit tests of the pure blocking function in `agent.rs` (or its `agent/` module), fed `HarnessRun` values built in memory — no `FakeHarness`, which copies and runs `fake_harness.sh` as a child process and breaks the project rule. The existing `tests/integration/actions_agent.rs:1325` stays only as the R8 regression pin; add no new scenarios to it. `tests/integration/claude_code_classification.rs` (line 1126, see the last scenario above).
**Verification:** `cargo test --workspace` (unit tests of `agent.rs`), `cargo test --test integration actions_agent claude_code_classification`.

### U4. Readable event in the log

**Goal:** One clear row for an ignored denial in the TUI and CLI event views.
**Requirements:** R5. **Dependencies:** U1, U3.
**Files:** `src/tui/event_text.rs` (next to `AgentSessionFailed`, line ~198), the event summary in `crates/narsor-protocol/src/event.rs` (~800), `src/cli/commands.rs` if it formats events itself.
**Approach:** Row form: `odmowa pominięta · <agent | subagent nazwa> · Bash · <klucz>`. The source comes before the key parameter, and only the key parameter at the end is cut, so the source stays visible on a narrow terminal. The details block holds tool, ids, reason and the full input. Text is sanitized like other events (`scroll::sanitize`, `one_line`).
**Test scenarios:**
- A long key parameter at a narrow width still shows the source.
- Main-agent source and subagent source give different rows.
- A long command is cut to one line; control characters are removed.
- Missing reason and missing name do not leave empty brackets.
**Test files:** `tests/integration/tui_render.rs` or the existing event-text tests (find with `grep -rn event_text tests`).
**Verification:** `cargo test --test integration tui_`.

### U5. Conversation view: subagent groups

**Goal:** The operator sees subagents as separate, collapsed groups, in the TUI and the CLI.
**Requirements:** R6, R7. **Decisions:** KTD6. **Dependencies:** U1, U2.
**Files:** `src/tui/conversation.rs` (`classify` carries `subagent` into `TranscriptEvent`; `agent_transcript_entry` sets it on the entry and indents), `src/tui/transcript.rs` (the `grouping` API lives here: add the `subagent` field on `TranscriptEntry` and the fold by id in the projection, around `transcript.rs:672`), `src/tui/task_app/tabs/conversation.rs` (only if a key or label needs a change), `src/cli/commands.rs` (`conversation_entry_lines`: prefix and indent), `docs/user/tui.md`.
**Approach:** A subagent entry joins a group keyed by its `SubagentRef.id` (also across interleaved main-agent entries, KTD6), label from `name`. The group is collapsed by default and expands member by member. Tool-call cards of the subagent stay inside the group. The CLI keeps one line per element and adds `↳ [nazwa]` and a two-space indent. Raw mode stays byte for byte.
UX rules for the TUI group (good practice, none relies on color alone):
- **Keys and cue.** The header takes focus like an existing summary card and uses the same fold key. Expanding shows the members, each collapsed like a main-agent card. The header has an expand marker (`▸` collapsed, `▾` open, with a text fallback where the existing cards have one). The key is in the Rozmowa footer hint and in `docs/user/tui.md`.
- **Header form.** `<marker> <name cut to one line with an ellipsis> · <count>`; the count is never cut. The name goes through `scroll::sanitize` and `one_line`, also in the CLI prefix. A fallback name or a repeated name gets the subagent's order number (`subagent 2`).
- **State in text.** The header shows a state word (`trwa` or `zakończony`) and a failure count when a member is a failed or denied tool call. A group that gets new entries keeps its fold state, keeps the cursor on the same group, updates the count in place and does not take follow-tail focus from the main thread.
**Test scenarios:**
- The header shows the marker in both states; the long name, the repeated name and a name with control characters give a one-line header with the count visible.
- The state word and the failure count show in text; a new entry in a collapsed group changes the count and keeps the fold state and the cursor.
- Three entries of one subagent give one group; an entry of another subagent gives a second group; main-agent entries stay outside.
- A group header shows the name and the element count.
- Entries of one subagent separated by a main-agent entry still form one group, placed at the first entry.
- A subagent `ToolCall` and its `ToolResult` stay one operation card inside the group.
- An entry without `subagent` (old record, other harness) renders exactly as before.
- The CLI line of a subagent entry has the name prefix and indent; main-agent lines are unchanged.
**Test files:** `src/tui/conversation.rs` unit tests (structure only), `tests/integration/tui_render.rs`, `tests/integration/cli_task_text.rs` or the CLI conversation test.
**Verification:** `cargo test --workspace`; open the TUI on the synthetic fixture and look at the Rozmowa tab (`run.sh`).

### U6. Docs and future ideas

**Goal:** The behavior is documented and the future ideas are linked.
**Requirements:** scope boundary. **Dependencies:** U3, U5.
**Files:** `docs/follow-ups/2026-11-07-subagenci-i-odmowy-pomysly-na-przyszlosc.md` (already written with the plan; check it against the final names), `docs/user/agents.md` (denial rule, event), `docs/user/tui.md` (groups), `docs/dev/architecture.md` or `docs/dev/domain-model.md` (the parser contract and the event; find the right page with `grep -n "permission" docs/dev/*.md`), `docs/dev/building-and-testing.md` only if the fixture rule changes.
**Test expectation:** none — documentation.
**Verification:** Docs name the real event, field and file names from U1–U5.

---

## Verification Contract

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets`
- `cargo test --workspace`
- Tests use fakes and fixtures read with `include_str!`. No real files, locks or child processes at run time, and no assertions on prompt wording (project rules). New engine scenarios are unit tests of pure functions; the one existing `FakeHarness` test (`actions_agent.rs:1325`) stays as a pin and gets no new siblings.
- Manual: replay the synthetic fixture and check that the step completes, the log shows one ignored-denial row, and Rozmowa shows one collapsed subagent group.
- Repeat the original scenario if possible: a headless run of a review skill with a subagent that calls a denied tool must complete the step in autopilot.

## Definition of Done

- All R1–R8 hold; every unit's test scenarios exist and pass.
- The case from `task-326aba9d…` (subagent denial + valid answer) completes the step, and the conductor sees the answer.
- A main-agent denial without an answer still gives a permission request.
- No dead code from the removed `is_error` rule; the old comment at `classify.rs:114` is gone or rewritten.
- The fixture holds no real repository or company data.
- Docs and the follow-up file are updated.

## Sources & Research

- Recording and dump of `task-326aba9d0798483d8dc1e51a890eca8a` (kept outside the repo).
- Code: `crates/narsor-engine/src/engine/actions/agent.rs:463`, `harness/claude_code/classify.rs:117`, `harness/claude_code.rs` (`accumulate`, line ~306 and ~358), `engine/runner/rescue.rs:390`, `engine/runner.rs:3921`, `src/tui/conversation.rs`, `src/cli/commands.rs`.
- Earlier work: `docs/follow-ups/2026-10-18-odroczone-uprawnienia-harnessu-pozostale-prace.md` and the plan for deferred harness permissions (#2).
