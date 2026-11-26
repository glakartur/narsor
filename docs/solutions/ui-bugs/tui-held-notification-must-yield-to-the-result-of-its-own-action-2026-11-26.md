---
title: A held TUI notification must yield to the outcome of its own action, and a transport error needs its cause chain
date: 2026-11-26
module: src/tui/task_app (notify.rs, problems.rs), crates/narsor-api-client
problem_type: ui-bug
tags: [tui, notifications, problem-log, reqwest, error-chain]
applies_when:
  - a notification is held so a later one cannot replace it
  - a failure is turned into text from a reqwest or other wrapped error
---

# Held notification vs. the outcome of its own action

## Problem
The problem log (`D` key) shows the dump path as a notification held for
the normal lifetime (6 s). The `Notifier` has one slot. Two requirements
conflicted: R6 (a later notification does not replace the path) and R9
(a failed write must show its reason). A second `D` that failed inside the
6 s was hidden behind the held path, so the operator saw a stale path for
a file that was never written.

A second, linked defect: the `reqwest` error's `Display` shows only the
top message. The cause chain (timeout, refused, DNS) was lost, so the log
could not tell a timeout from a refused connection.

## Rule
- A held notification blocks unrelated notifications only. The outcome of
  a newer run of the same action, failure included, replaces it at once.
  Unrelated notifications stay recorded in the problem log even when the
  bar does not show them.
- Capture the whole `source()` chain of a transport error where the error
  is created (`ApiClient::send`), not where it is shown. Later layers keep
  only the text.

## How to apply
When adding a "held" or "sticky" status text, list which events may
override it, and test the failing second run of the action.
Plan: `docs/plans/2026-11-26-1854-feat-tui-problem-log-plan.md` (KTD8).
