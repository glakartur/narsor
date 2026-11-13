---
title: "TUI action panel: a refresh sent a stale generation, moved the highlight and turned typed letters into shortcuts"
date: 2026-11-13
category: ui-bugs
module: tui/task_app
problem_type: ui_bug
component: tooling
symptoms:
  - "The server rejects a confirmed action as stale_generation after a refresh while the panel was open"
  - "An action chosen again after Esc from the confirmation step is rejected as stale"
  - "After a refresh removed the open action, Enter then Enter sent another action (often Archive)"
  - "Letters typed into a field (o, m, j, k) worked as menu shortcuts after the action was lost"
root_cause: async_timing
resolution_type: code_fix
severity: medium
tags: [tui, action-panel, refresh, generation, stepped-form, rebuilt, key-lock]
---

# TUI action panel: a refresh sent a stale generation, moved the highlight and turned typed letters into shortcuts

## Problem

The action panel stays open while the task refreshes. The refresh changes the offer of the server. The panel then sent the wrong generation, kept a highlight on an old index, or read typed letters as menu keys. Pull request #103; the pattern itself is in [the server-offer pattern doc](../design-patterns/tui-panel-actions-from-server-offer-2026-11-13.md).

## Symptoms

- The server refuses a confirmed action with `stale_generation`, although the operator saw the task a moment before.
- Esc from the confirmation step, then the same action again: refused as stale.
- A refresh removes the open submenu or action. The highlight stays on the old index. Enter + Enter sends a different action.
- Typing in the field of the lost action: `o`, `m` open shortcuts, `j` / `k` unlock the panel.

## What Didn't Work

- Reading the generation from the refresh at send time only. A choice made after Esc then used the generation from the first choice.
- Unlocking the panel with `j` / `k` as well as arrows (commit cac71a9). These letters can be part of the text the operator still types, so the lock ended too early (fixed in 7c9638f).

## Solution

1. **Send the generation the operator saw at the choice** (155cd8c). The refresh does not change the generation in the request. The server rejects the action when the task changed after that point.
2. **Keep the generation of the last refresh in the panel** (a84c554). On the choose step the panel takes it before each key. A new choice after Esc has the generation of the offer on screen. A confirmation with no return to the choose step keeps the generation from the choice.
3. **Lock the choice when the refresh removes the action** (cac71a9). `SteppedForm::rebuilt` puts the highlight on the first entry and blocks Enter, digits and Tab. The panel does not take `o`, `m`, `Ctrl+↑` / `Ctrl+↓` while it is locked.
4. **Unlock only with keys that cannot be text** (7c9638f): `↓` / `↑` without modifiers, or `Esc`. `j` / `k` and the shortcuts stay blocked.

## Why This Works

A refresh is an event that arrives between two keys. Each key must use only what the operator saw when they made the choice, or what is on screen now. After a lost action, the state "choose" has no safe default, so the form accepts no choice until the operator makes a clear move that is not text.

## Prevention

- Test each refresh case with a state change between two keys: lost submenu, lost leaf, Esc then new choice, confirm without return.
- Treat every key that can enter text as text while a field can still be open.
- Open question Q4 (PR #103): `↓` / `↑` also move between fields in a form. The owner chose to keep this as it is.
- Known risk, not fixed here: in a locked panel `?` opens help and `q` opens the exit confirmation.

## Related

- [Confirmation popup closed by a background refresh](tui-confirmation-popup-closed-by-background-refresh-2026-10-26.md)
- [TUI server-offer pattern](../design-patterns/tui-panel-actions-from-server-offer-2026-11-13.md)
