---
title: "TUI: the server describes the task actions, the TUI draws them on one multi-step form"
date: 2026-11-13
category: design-patterns
module: tui/task_app
problem_type: design_pattern
component: frontend
severity: medium
applies_when:
  - "A new action of a task is added on the server"
  - "A client needs a name, a description, a submenu or fields for an action"
  - "A place of the TUI asks the operator to choose, fill in and confirm"
tags: [tui, actions, protocol, offer, multi-step-form, stepped, confirmation]
---

# TUI: the server describes the task actions, the TUI draws them on one multi-step form

## Context

Before this change the server sent every action with `available` and a reason,
and the TUI had its own list (`TaskAction::ALL`), its own names (`menu_text`)
and its own knowledge of which action opens a submenu. Each action had its own
window (`Menu`, `Transitions`, `Steps`, `Confirm`). A new action of the server
needed a change of the TUI. The plan is
`docs/plans/2026-11-13-001-feat-tui-panel-akcji-z-serwera-i-formularz-wielokrokowy-plan.md`.

## Guidance

1. **The server offers only what can be done now.** `offer()` in
   `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs`
   leaves out every action that `unavailable_reason` refuses. The reason is
   still the server's answer at the send (`require_available`,
   `action_unavailable`).
2. **The server describes each action whole.** `TaskActionOffer` has `label`,
   `description`, `irreversible`, `order`, `kind` (`execute` / `submenu`),
   `surface` (`panel` / `conversation`), `fields`, `children` and `send`. The
   new fields have defaults, so an older client and an older server still read
   the summary.
3. **The server names the command.** `send` is an owner command, an operator
   command or a comment, kept as JSON so a command unknown to the client does
   not break the summary. A string `{{field}}` in the command is the value of
   that field; a blank optional field is `null`
   (`TaskActionSend::resolve`). The TUI has no `match` on the action id.
4. **One control for choose / fill in / confirm.** `src/tui/stepped.rs` takes a
   tree of `StepEntry` (a leaf or a submenu) and returns one
   `SteppedOutcome`. It composes `Chooser` and the form engine; it does not
   copy their editing rules. It knows nothing of actions.
5. **`None` and an empty list are not the same.** `actions == None` (or a list
   that describes nothing, from an older server) is "brak danych z serwera".
   `Some(vec![])` is "brak dostępnych akcji".

## The limit

A client can send an action it does not know only when the command is of a
kind it knows (owner, operator, comment) and the command itself is one it can
read. An action that needs a new kind of command still needs a new client: the
TUI says "ta akcja wymaga nowszej wersji klienta" and sends nothing.

## Why This Matters

The rules of an action live in one place. A new action of the server shows up
in the TUI with its name, its fields and its confirmation, without a change of
the TUI. The operator sees only what can be done and goes through the same
three steps for every action.

## Related

- [TUI modal confirmation keyboard contract](tui-modal-confirmation-keyboard-contract.md)
- [TUI form engine: cursor as cell style, wrapped layout, confirmation and events](tui-form-engine-events-and-cursor-style.md)
- [TUI focus stays in the task details after a confirmed action](tui-focus-stays-in-details-after-confirmed-action-2026-10-20.md)
