---
title: "TUI action-form confirmation popup closed by a background refresh, so Enter needed several presses"
date: 2026-10-26
category: ui-bugs
module: tui/task_app
problem_type: ui_bug
component: tooling
symptoms:
  - "After about ten seconds on an action form, Enter does not confirm; the confirmation popup is gone or ignores the key"
  - "A second or third Enter is needed; a quick Enter after Enter works"
  - "A refusal to save (connection gate) shows only in the status bar, not in the panel"
  - "A double Enter can send the same decision twice"
root_cause: logic_error
resolution_type: code_fix
severity: medium
tags:
  - tui
  - confirmation
  - chooser
  - refresh
  - rebuilt
  - idempotency
  - waiting-panel
---

# TUI action-form confirmation popup closed by a background refresh, so Enter needed several presses

## Problem

The operator opens a workflow action form, waits, and presses Enter. The confirmation popup had been closed by a timeline refresh that arrived in between, so the key did not confirm. Issue #37.

## Symptoms

- Enter confirms only on the 2nd or 3rd press after a pause; fast Enter-Enter works (no rebuild between the presses).
- A refusal from `mutation_gate` went only to the status bar.
- The same decision could be sent twice.

## What Didn't Work

- Suspecting a slow `answer(...).await` in `TaskApp::dispatch` (H3). The screen stayed responsive and the backend was local.
- Suspecting a form switch from `Switcher::sync` (H1). It only fires on a real form change, not on a plain refresh.
- The plan wrote the code reading down as hypotheses and required failing tests first (diagnosis unit) before choosing a fix.

## Solution

Root cause: a refresh of a running task changes its row revision every few seconds. The timeline read then calls `WaitingPanel::reread` → `Entry::rebuild` → `Chooser::rebuilt`, which called `form.decline()` unconditionally. The popup closed, focus stayed in the field, and the next Enter just re-opened it.

1. `Chooser::rebuilt` keeps the popup open when the highlighted entry is unchanged and still sendable; otherwise it closes it. "Unchanged" is `ChooserItem::same_definition` (label, flags, field definitions; typed values excluded).
2. A value the conductor proposed on the earlier side is part of what was asked, so `ChooserField::same_definition` compares it. Changed or lost proposal → popup closes.
3. `Chooser::select` declines the popup when the index changes.
4. When a confirmation is closed by a rebuild, `Entry::error` says the options changed and to confirm again. When the form is replaced during typing or confirming, a warning says the draft stays.
5. `mutation_refusal()` returns the reason, and the panel shows it (`view.waiting.fail`).
6. `Entry::sending: Option<Submit>` blocks a second send of the same decision (`"Zapis trwa."`); `unprepare` clears it when the request is not sent. Do the same for comments.

```rust
let asked = self.form.decline();           // remember where the popup was
// ... rebuild items ...
if let Some(return_field) = asked {
    if unchanged && self.is_sendable() {
        self.form.ask_confirmation(return_field.min(self.field));
    }
}
```

## Why This Works

A refresh that changes nothing the operator is deciding on no longer destroys their confirmation state. A refresh that does change it still closes the popup, with a visible reason, so the operator never confirms something different from what they read.

## Prevention

- Any code path that rebuilds a view model under an open modal must decide explicitly: keep the modal (same content) or close it (changed content). Never decline by default.
- A "same content" comparison must include what the operator saw in the popup (proposed values), and exclude what they typed.
- Capture "was busy" (typing or confirming) before entries are replaced, not after (fix 167abd3).
- Every mutation refusal needs a visible place in the panel, not only the status bar.
- Tests: refresh with unchanged entry keeps the popup; changed entry, changed proposal, changed selection, lost proposal close it; a second identical send is refused until the first settles (`tests/tui_form.rs`, `tests/tui_panel.rs`).

## Related

- [TUI modal confirmation keyboard contract](../design-patterns/tui-modal-confirmation-keyboard-contract.md)
- [TUI list read on every runner report](../logic-errors/tui-list-read-on-every-runner-report-2026-10-22.md)
- `docs/plans/` plan "fix: Enter w oknie potwierdzenia formularza akcji"
- `src/tui/chooser.rs`, `src/tui/task_app/panel/state.rs`
