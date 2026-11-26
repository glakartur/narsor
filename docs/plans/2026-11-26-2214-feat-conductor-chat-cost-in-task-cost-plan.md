---
title: Conductor Chat Cost in the Task Cost - Plan
type: feat
date: 2026-11-26
topic: conductor-chat-cost-in-task-cost
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Conductor Chat Cost in the Task Cost - Plan

## Goal Capsule

- **Objective:** The cost and tokens that the conductor uses to answer the operator's chat messages are part of the task cost that every frontend shows, so the operator sees what a task really cost.
- **Means:** A task-level conductor bucket in the task's usage data, next to the per-position usage, that the task total adds up.
- **Product authority:** This plan covers only area 2 of `docs/bugs/NARSOR-1124-001.md` ("Koszt czatu z dyrygentem w koszcie taska"). Area 1 (multi-thread conductor chat) is not active scope; see How This Work Fits Together. The E2BIG fix in the same bug document is already done and is not part of this plan.
- **Open blockers:** None. The person answered the question about conductor subagents: they count in the task subagent counter (see Key Decisions and R8).

---

## Product Contract

### Summary

Every conductor invocation that has no timeline position (today: each reply to a chat message) records its cost and tokens in a task-level conductor bucket. The task cost line in the TUI, the Web UI and the CLI includes this bucket in the conductor part and in the combined total. Per-position usage does not change.

### Problem Frame

The operator can talk to the conductor of a task (the *Dyrygent* tab). Each reply is a real harness run that costs money. Today that cost disappears:

- A chat request has no episode (`crates/narsor-exec/src/conductor/store.rs`, `ConductorRequest::for_chat`).
- The reading tap returns nothing for a request without an episode (`crates/narsor-exec/src/conductor/invoke.rs`, `reading_tap`), so the tokens and the cost of the run are dropped.
- `apply_usage` (`crates/narsor-exec/src/engine/timeline_file.rs`) only counts readings keyed by an attempt of a timeline position, so there is nowhere to put a reading that belongs to the task and not to a position.

The task cost line (`src/client/task_usage_text.rs`) adds up positions only. A task where the operator talked a lot with the conductor shows a cost that is too low, with no sign that something is missing.

### Key Decisions

- **Task-level bucket, not attribution to a position.** Chosen over adding the chat cost to the current or last timeline position (option B in the bug document): option B loses the cost when no position exists, for example after the task ends, and it puts operator-driven cost on an agent step that did not cause it. Governs R1, R3.
- **The conductor's chat cost stays apart from the agents' cost.** The existing rule "conductor use is reported apart from agent use" (`src/client/task_usage_text.rs`, module doc) holds for the new bucket. Governs R4.
- **Conductor subagents follow the same rules as the subagents of agents that process task steps.** The harness cost of the conductor already includes the cost of its subagents. The subagent count and subagent tokens only show which part of that cost the subagents used. They do not add money. Decided by the person on the review of this plan. Governs R8.

### Requirements

**Recording**

- R1. Every completed conductor harness run that has no timeline position adds its tokens and its cost to a task-level conductor bucket of that task.
- R2. A failed or cancelled conductor run adds what the harness reported before it stopped, and adds nothing when the harness reported nothing.
- R3. The task-level bucket survives the end of the task and a server restart, the same as per-position usage.

**Showing**

- R4. The task cost that the TUI, the Web UI and `narsor-cli` show includes the task-level bucket in the conductor part (`⚡`) and in the combined total (`razem`).
- R5. Per-position usage (the usage of each timeline position) does not include the task-level bucket and does not change.
- R6. A task with no chat cost shows the same text as today.

- R8. The subagents of a conductor run that has no timeline position add to the task subagent count and subagent tokens, marked as the conductor's. They follow the same rules as the subagents of agents that process task steps. They add no money to the cost, because the conductor's cost already includes them.

**Compatibility**

- R7. A task whose usage data was written before this change reads without error and shows the same cost as today.

### Acceptance Examples

- AE1. **Covers R1, R4.** Given a task where the agents cost $1.00 and the conductor decided at gates for $0.10, when the operator sends one chat message and the conductor's reply costs $0.05, then the task line shows `razem $1.15` and the conductor part shows $0.15.
- AE2. **Covers R3.** Given a finished task, when the operator sends a chat message and the reply costs $0.05, then the task total grows by $0.05, and it is still there after a server restart.
- AE3. **Covers R5.** Given the same task as AE1, the usage of each timeline position is the same before and after the chat message.
- AE4. **Covers R2.** Given a chat reply that fails before the harness reports any usage, the task cost does not change.
- AE5. **Covers R6, R7.** Given a task saved before this change with no chat, the task line text is the same as before.

### Scope Boundaries

- Multi-thread conductor chat and `--resume` of conductor sessions are out of scope (area 1).
- No per-message or per-thread cost display in the chat itself. Deferred.
- No change to how gate decisions and failure handling of the conductor are counted; they keep their position-keyed readings.
- No budget limits or alerts on conductor cost. Deferred.

<!-- ce-section: work-relationships -->
### How This Work Fits Together

This plan covers area 2 of `docs/bugs/NARSOR-1124-001.md`: conductor chat cost in the task cost. The breakdown below is the current understanding, not a committed roadmap.

- Area 1: multi-thread conductor chat (a list of chats at the top of the *Dyrygent* tab, the leftmost option starts a new chat, a chat continues an earlier harness session).
  - Can proceed independently of this plan.
  - Shares the conductor's chat request path; if area 1 resumes harness sessions, its readings may become session totals instead of single-run deltas. Still to decide in area 1's own brainstorm.
  - Still to decide: whether cost is also shown per thread.

### Założenia do potwierdzenia

1. **This brainstorm covers area 2 (cost) and not area 1 (multi-thread chat).** The bug document holds two independent features. Cost is the smaller one and it fixes lost data, so it goes first. Area 1 needs its own brainstorm.
2. **Option A (task-level bucket) is chosen.** The bug document recommends it; option B loses cost when no position exists.
3. **Conductor subagents are added to the task subagent counter, marked as the conductor's.** The person decided this. The conductor's cost from the harness already includes its subagents' cost, so subagents show their part of the cost and do not add to the money total. The same rules apply to agents and subagents that process task steps.
4. **The bucket is for every conductor run without a position, not only chat.** Today only chat replies have no episode; a future trigger without an episode is then counted too, instead of being lost again.
5. **Readings of a chat reply are single-run totals,** because each chat invocation starts a new harness session (`resume_session: None`).

### Pytania otwarte dla użytkownika

None. Q1 (conductor subagents in the task subagent counter) is answered: option b. See R8.

### Outstanding Questions

**Deferred to Planning**

- How the bucket stores the conductor's subagent count and subagent tokens, and how the TUI, the Web UI and the CLI show them marked as the conductor's.
- Where the task-level bucket lives (in `timeline.json` next to `positions`, or in the readings store keyed by a task-level key) and how the protocol carries it to frontends.
- Whether a conductor request whose episode has no matching timeline position also loses its reading today, and if so, whether R1 covers it through the same bucket.

### Sources

- `docs/bugs/NARSOR-1124-001.md` — "Decyzje do podjęcia", item 2.
- `crates/narsor-exec/src/conductor/store.rs` — `ConductorRequest::for_chat` (`episode: None`).
- `crates/narsor-exec/src/conductor/invoke.rs` — `reading_tap`.
- `crates/narsor-exec/src/engine/timeline_file.rs` — `UsageReading::of_conductor`, `apply_usage`.
- `crates/narsor-protocol/src/api/timeline.rs` — `conductor_cost_usd` on position usage.
- `src/client/task_usage_text.rs` — `TaskUsage`, `combined_cost_usd`, `line`.
