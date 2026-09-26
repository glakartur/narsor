---
title: TUI runs the engine in the background instead of blocking the event loop
type: fix
status: completed
date: 2026-09-26
origin: docs/brainstorms/2026-09-26-tui-nieblokujace-uruchomienia-requirements.md
---

# TUI runs the engine in the background instead of blocking the event loop

## Summary

The four TUI requests that drive the engine (`StartNewTask`, `RunSelected`, `SubmitInput`,
`ApplyTransition`) stop awaiting `engine.run`/`resume`/`deliver_human_input` inline inside the
event loop's single `tokio::select!` branch. Each spawns that same call instead, guarded by a new
engine-owned "already running" set (mirroring the existing `pause_requests` set) so the same
execution can never be driven twice at once, and every outcome — including an engine-level
infrastructure failure that today only reaches the caller's `Result` — reaches the TUI exclusively
through the event stream it already consumes (`engine.subscribe()` / `App::on_engine_event`), with
no new result channel.

---

## Problem Frame

(see origin: `docs/brainstorms/2026-09-26-tui-nieblokujace-uruchomienia-requirements.md`) — the
event loop in `src/tui/mod.rs:180` awaits the chosen engine call to completion inside one
`tokio::select!` branch, so nothing else in that loop (redraw, keys, the event stream) is polled
again until it resolves. For the default `compound-engineering.yaml` workflow, whose first state
is a real agent action, this freezes the terminal for as long as that step takes.

---

## Requirements

- R1. Each of the four TUI requests that drive the engine synchronously today —
  `StartNewTask`, `RunSelected`, `SubmitInput`, `ApplyTransition` — spawns its engine call
  (`engine.run`, `engine.resume`, `engine.deliver_human_input`) instead of awaiting it inline
  inside the event loop's `tokio::select!` branch.
- R2. The event loop stays responsive (redraw, keys, live list updates) for the entire duration of
  a background engine call, regardless of how long it takes.
- R3. Multiple executions may be driven in the background at the same time from one TUI session.
- R4. The outcome of a background call — completion, failure, or an engine-level infrastructure
  error — reaches the TUI only through the existing event stream; no new result channel is added.
- R5. An `EngineError` that today is only returned to the caller also becomes an event, so a
  background call's infrastructure failure is observable without a return value.
- R6. Any terminal or error event for any execution on the list — not only the one currently
  selected — sets a bottom-bar notice naming that execution's id.
- R7. Starting or resuming an execution that is already being driven in the background is rejected
  with a notice instead of racing a second driver against the first.
- R8. The existing soft-pause mechanism (`pause_now` / `take_pause_request`) needs no change to
  keep working under background-driven execution.
- R9. Immediate/hard interrupt of an in-flight action is out of scope.

**Origin actors:** A1 (Operator), A2 (Engine)
**Origin flows:** F1 (start of a long-running first step), F2 (two executions driven at once),
F3 (rejecting a duplicate driver for the same execution)
**Origin acceptance examples:** AE1 (covers R1, R2), AE2 (covers R3), AE3 (covers R4, R5, R6),
AE4 (covers R7)

---

## Scope Boundaries

- Immediate/hard interrupt of an in-flight action (D33 kill) — out of scope; only the existing
  soft pause applies (R8, R9).
- The centrala/worker split, leases, `epoch` fencing, or an API transport
  (`docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`) — this plan is only an
  in-process step toward that direction, not an implementation of it.
- Offline work and central-process high availability — not applicable; no central process exists
  yet.

### Deferred to Follow-Up Work

- Whether `App` / `tui::mod::run` should track spawned `JoinHandle`s to wait for or cleanly
  abandon in-flight background runs on quit (`q`) — left open below (Open Questions) rather than
  decided here.

---

## Context & Research

### Relevant Code and Patterns

- `src/tui/mod.rs:180` — the single `tokio::select!` loop; nothing changes here under R4, since no
  new channel is added to it.
- `src/tui/app.rs`: `start_new_task` (~2463), `run_selected` (~2553), `submit_input` (~1897),
  `apply_transition` / `deliver_label` (~1946 / ~1971) — the four handlers to change.
  `manual_transition` / `force_transition` (~2007 / ~2023) do not call the engine loop and are out
  of scope.
- `src/tui/app.rs`: `on_engine_event` (~1389) and `apply_to_row` (~2671) already update any
  execution row by `event.workflow_execution_id`, not only the selected one — the mechanism R6
  extends already exists.
- `src/engine/runner.rs`: `pause_requests: Mutex<HashSet<ExecutionId>>` (~351) plus
  `lock_pause_requests` / `take_pause_request` (~1385, ~685) — the pattern the new "already
  running" guard mirrors.
- `src/engine/runner.rs`: `record_engine_error` (~1166) already emits `EventData::Error` and fails
  the execution for one internal failure path (a decision-provider error, ~965) — the pattern R5
  generalizes to the engine's public entry points.
- `src/domain/event.rs`: `EventData::Error { message, context }` already exists and is already
  rendered by `event_line` in `src/tui/app.rs` (~2754) — no new event variant is needed.
- `tests/tasks.rs`: `engine_for` and related fixtures show the existing pattern for constructing
  an `Engine` over a temporary store for integration-style tests.
- `tests/tui_state.rs`: explicit project convention — "test `app.rs`, not the rendering" — plus the
  `TempRoot` helper and the `type: script` fixture idiom (`tests/fixtures/tui/pipeline.yaml`) for
  giving a workflow a deterministic, controllable external step in tests.

### Institutional Learnings

- None — `docs/solutions/` does not exist yet in this repository.

---

## Key Technical Decisions

- The "already running" guard is an `Engine`-owned `Mutex<HashSet<ExecutionId>>`, symmetric with
  the existing `pause_requests` set, released via a small RAII guard rather than a manual
  remove-on-every-exit-path: this keeps the check-and-insert atomic under one lock, which a
  separate check-then-insert would not.
- `EngineError` reaching the event stream reuses the existing `EventData::Error` shape and the
  `record_engine_error` emission pattern, generalized to wrap `run`, `resume`, and
  `deliver_human_input` at their entry points, rather than introducing a new event variant or
  rewiring every internal `?` propagation site inside `step_loop`.
- Spawning discards the `JoinHandle`: the spawned task's only externally-visible effect is the
  events it emits along the way (success is already fully expressed by the state/terminal events
  the loop already emits; failure is now expressed by R5's event). No dedicated result channel is
  added, matching R4.
- The input/transition modal closes as soon as its request is dispatched (matching how
  `start_new_task` already behaves today), not after the background call resolves. This removes
  today's `submit_input` / `deliver_label` behavior of reopening the form with an inline
  `form.error` on an engine-level `Err` — that failure now surfaces only via the R6 notice. The
  client-side `form.validate()` error path, which runs before dispatch, is unaffected and stays
  synchronous.

---

## High-Level Technical Design

> This illustrates the intended approach and is directional guidance for review, not an
> implementation specification.

```text
# today, inside the App handler (e.g. start_new_task):
execution = tasks::start_task(...)?        # fast, local, unchanged
modal = None
outcome = engine.run(definition, execution.id).await   # <- blocks the whole event loop
notice = Notice::from(outcome)

# after this plan:
execution = tasks::start_task(...)?        # fast, local, unchanged
modal = None
spawn {
    guard = engine.try_begin(execution.id)?     # rejected -> notice, nothing spawned
    result = engine.run(definition, execution.id).await
    if result.is_err() { engine.note_background_failure(execution.id, &result) }
    # guard drops here regardless of outcome
}
# handler returns immediately; outcome (success or failure) arrives later as
# events on the existing stream, picked up by App::on_engine_event
```

The same shape applies to `run_selected` (`engine.resume`) and to `submit_input` /
`apply_transition`'s `deliver_label` (`engine.deliver_human_input`).

---

## Implementation Units

### U1. Engine-owned guard against driving the same execution twice

**Goal:** Prevent two concurrent drivers from advancing the same execution.

**Requirements:** R7

**Dependencies:** None

**Files:**
- Modify: `src/engine/runner.rs`
- Test: `tests/tasks.rs`

**Approach:**
- Add an `active_runs: Mutex<HashSet<ExecutionId>>` field to `Engine`, initialized alongside
  `pause_requests`.
- Add a small RAII guard type whose constructor does the check-and-insert under one lock and
  whose `Drop` removes the id, so acquisition and release cannot race each other.
- `run`, `resume`, and `deliver_human_input` acquire the guard first and hold it for their whole
  duration; a failed acquisition returns a new `EngineError` variant rather than proceeding.

**Patterns to follow:**
- `pause_requests` / `lock_pause_requests` / `take_pause_request` (`src/engine/runner.rs`).

**Test scenarios:**
- Happy path: acquiring the guard for an execution with no active driver succeeds, and the id is
  free again once the guard drops.
- Edge case: a second acquisition attempt for the same execution id while the first guard is still
  held is rejected.
- Edge case: after the first guard drops, a later acquisition for the same execution id succeeds.
- Integration: calling `engine.run` for an execution that is already being driven returns the new
  "already running" error immediately rather than proceeding to load or mutate the execution.

**Verification:**
- Two concurrent attempts to drive the same execution id never both proceed past acquisition; the
  loser gets the new error, not a race on the store.

---

### U2. Engine-level failures become events

**Goal:** Make an `EngineError` returned from a background call observable without its return
value.

**Requirements:** R5

**Dependencies:** None

**Files:**
- Modify: `src/engine/runner.rs`

**Approach:**
- Generalize the emission half of `record_engine_error` into a helper usable from outside
  `step_loop`.
- Wrap the public entry points `run`, `resume`, and `deliver_human_input` so that, on `Err`, they
  best-effort emit an `EventData::Error` against the execution id before returning the error —
  "best-effort" meaning a failure to emit never masks or replaces the original error.

**Patterns to follow:**
- `record_engine_error` (`src/engine/runner.rs`, ~1166).
- Existing `EventData::Error` emission sites: `src/tasks/mod.rs` (~837),
  `src/persistence/recovery.rs` (~188).

**Test scenarios:**
- Happy path: an `Err(EngineError)` returned by `run` for a failure that occurs after the
  execution id is known also produces a corresponding `EventData::Error` event on that execution's
  log.
- Edge case: a failure that occurs before any execution id can be resolved (e.g. the id itself is
  unknown) still returns the plain error, with no attempt to emit against a stream that cannot be
  identified.
- Edge case: the emission attempt itself fails — the original `EngineError` is still returned
  unchanged, not swallowed or replaced.

**Verification:**
- Every `EngineError` returned from `run`, `resume`, or `deliver_human_input` that carries a
  resolvable execution id has a matching `EventData::Error` on that execution's event log.

---

### U3. Spawn the four engine-driving requests

**Goal:** Stop blocking the event loop on the long-running part of each of the four requests.

**Requirements:** R1, R2, R3, R7 (consuming side)

**Dependencies:** U1

**Files:**
- Modify: `src/tui/app.rs` (`start_new_task`, `run_selected`, `submit_input`, `deliver_label`)

**Approach:**
- Each handler keeps its existing fast, local part unchanged (form validation, `tasks::start_task`,
  recording the human answer's local bookkeeping) and, in place of `.await`ing the long engine
  call, spawns it: `Arc::clone(&self.engine)` plus a cloned `WorkflowDefinition` and the execution
  id move into the spawned task, which drives the same `engine.run` / `resume` /
  `deliver_human_input` call as today.
- The spawned task's `JoinHandle` is discarded; its outcome travels only through the events it
  emits (U2 covers the failure case).
- The handler's own synchronous tail — closing the modal, any "started" bookkeeping — happens
  right after dispatch, not after the spawned call resolves.

**Technical design:** See High-Level Technical Design above.

**Patterns to follow:**
- The existing `Arc<Engine>` sharing already used for `engine.subscribe()`.
- `form.definition.clone()`, already used at `start_new_task` today.

**Test scenarios:**
- Happy path: dispatching `StartNewTask` for a workflow whose first step is deliberately slow (a
  `script` action, following the `type: script` fixture idiom already used in
  `tests/fixtures/tui/pipeline.yaml`) returns control to the caller before that step finishes.
- Happy path: dispatching two different executions' `RunSelected` / `SubmitInput` at once lets
  both proceed without either blocking on the other.
- Edge case: dispatching a request for an execution that `U1`'s guard rejects does not spawn a
  second driver, and the handler's synchronous tail reflects the rejection.

**Verification:**
- None of the four handlers holds an `.await` on the long engine call within its own execution
  path; a deliberately slow first step provably does not block the caller.

---

### U4. Notices for background outcomes, keyed by execution id

**Goal:** Make a background run's outcome visible to the operator regardless of what is currently
selected.

**Requirements:** R4, R6

**Dependencies:** U2, U3

**Files:**
- Modify: `src/tui/app.rs` (`on_engine_event`)

**Approach:**
- Extend `on_engine_event` so that, whenever the incoming event's data is `WorkflowCompleted`,
  `WorkflowFailed`, `WorkflowCancelled`, or `Error`, it sets `self.notice` naming
  `event.workflow_execution_id`, independent of whether that execution is currently selected.
  `apply_to_row` keeps updating row status exactly as it does today; this is an additional
  notice-setting step alongside it, not a replacement.

**Patterns to follow:**
- `apply_to_row` (`src/tui/app.rs`, ~2671), `status_name` (~2689), the existing `Notice::info` /
  `Notice::error` constructors.

**Test scenarios:**
- Happy path: a `WorkflowCompleted` event for an execution that is not currently selected still
  produces a notice naming that execution's id.
- Happy path: an `EventData::Error` event for any execution produces an error notice naming that
  execution's id and its message.
- Edge case: two such events for two different executions arrive back to back — the notice
  reflects the later one, matching the existing single-`notice`-field behavior.
- Edge case: a `WorkflowPaused` / `HumanInputRequested` event (reaching a human checkpoint, not a
  terminal or error outcome) does not produce a notice by itself.

**Verification:**
- `on_engine_event` produces the described notices without the affected execution needing to be
  selected.

---

### U5. Remove the inline modal-error path for backgrounded submissions

**Goal:** Keep `submit_input` / `deliver_label` consistent with the fact that they no longer
receive a synchronous engine-level `Result` to react to.

**Requirements:** R6 (replaces the prior inline-error UX for these two paths)

**Dependencies:** U3, U4

**Files:**
- Modify: `src/tui/app.rs` (`submit_input`, `deliver_label`)

**Approach:**
- Drop the `Err` branch's `form.error = Some(...)` / modal-reopening logic in both handlers, since
  that branch no longer exists once the engine call is spawned (U3). The pre-dispatch
  `form.validate()` error path, which is purely local and runs before anything is spawned, is
  left untouched.

**Patterns to follow:**
- The existing `form.validate()` error handling, which stays as-is.

**Test scenarios:**
- Happy path: submitting a form with a client-side validation error (e.g. an empty required
  field) still reopens the form with an inline error, unchanged.
- Edge case: submitting a well-formed form whose background delivery later fails engine-side no
  longer reopens the form — the operator instead sees the U4 notice.

**Verification:**
- Form-level validation errors remain synchronous and inline; engine-level failures no longer
  touch modal state.

---

## System-Wide Impact

- **Interaction graph:** Only `src/tui/app.rs` (four handlers plus `on_engine_event`) and
  `src/engine/runner.rs` (three public entry points plus the new guard) change.
- **Error propagation:** `EngineError` is still returned synchronously to any caller that awaits
  the engine directly; only the TUI's now fire-and-forget spawns lose that return value, which U2
  compensates for by making the same information reappear as an event.
- **State lifecycle risks:** a spawned task's `JoinHandle` is discarded; there is currently no
  mechanism to wait for or cancel it on TUI shutdown — see Open Questions.
- **Integration coverage:** U3 and U4 together need at least one scenario that proves the
  end-to-end path (dispatch → slow step → event → notice), not only the isolated per-unit tests.
- **Unchanged invariants:** the CLI (`src/cli/commands.rs`) continues to call
  `engine.run` / `resume` synchronously and is not touched by this plan — its blocking-by-design,
  foreground behavior is correct and out of scope. The existing soft-pause mechanism
  (`pause_now` / `take_pause_request`, R8) needs no code change; existing pause-related tests
  should keep passing unmodified as the confirmation of that.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Discarding the spawned task's `Result` could silently swallow a failure the type system used to force the caller to handle | U2 makes that information reappear as an event before the plan is considered complete; U2's verification explicitly checks this |
| A gap between "check if already running" and "mark as running" could let two drivers both proceed on the same execution | U1's guard performs the check-and-insert under a single lock (test-covered), not as two separate steps |
| Removing the inline modal-error path (U5) is a visible behavior change for anyone used to seeing engine errors inline | Explicitly confirmed as an accepted trade-off during brainstorming (see origin, Key Decisions) |
| A detached `tokio::spawn`ed task may still be running when the TUI process quits | Left as an open question below rather than silently assumed away |

---

## Open Questions

### Resolved During Planning

- Where does the "already running" guard live?: On `Engine`, mirroring `pause_requests` —
  confirmed during plan-time synthesis.
- How does an `EngineError` reach the event stream?: By generalizing the existing
  `record_engine_error` pattern at the three public entry points — confirmed during plan-time
  synthesis.
- Does the modal wait for the background result before closing?: No — it closes immediately at
  dispatch, matching `start_new_task`'s existing behavior — confirmed during plan-time synthesis.

### Deferred to Implementation

- [Affects U3][Technical] Whether `App` / `tui::mod::run` needs to track spawned `JoinHandle`s so
  that quitting the TUI can wait for or cleanly abandon in-flight background runs, versus leaving
  them fully detached.
- [Affects U1][Technical] The exact name and shape of the new "already running" `EngineError`
  variant — naming only; the acquisition behavior itself is settled.

---

## Sources & References

- **Origin document:**
  [docs/brainstorms/2026-09-26-tui-nieblokujace-uruchomienia-requirements.md](../brainstorms/2026-09-26-tui-nieblokujace-uruchomienia-requirements.md)
- Related brainstorm (future direction this plan steps toward):
  [docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md](../brainstorms/2026-09-25-rozproszony-narsor-requirements.md)
- Related code: `src/tui/mod.rs:180`, `src/tui/app.rs`, `src/engine/runner.rs`,
  `src/domain/event.rs`
