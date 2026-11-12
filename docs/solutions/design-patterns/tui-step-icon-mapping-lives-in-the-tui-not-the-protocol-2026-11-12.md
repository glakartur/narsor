---
title: "TUI: the step-kind icon mapping lives in the TUI, not in the protocol crate"
date: 2026-11-12
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: low
applies_when:
  - "A new step kind (for example profile) needs an icon in the TUI"
  - "A display detail (icon, color, label) is computed in a shared protocol crate"
  - "Several TUI views must show the same icon for one step"
tags: [tui, step-icon, narsor-protocol, presentation, emoji, variation-selector, position-icon, cursor-icon]
---

# TUI: the step-kind icon mapping lives in the TUI, not in the protocol crate

## Context
The branch added `⚖️` for `profile` steps and changed the `script` icon from `📜` to `💻`. The plan (`docs/plans/2026-11-12-002-feat-tui-ikony-profile-i-script-plan.md`) kept the mapping `step_icon` in `crates/narsor-protocol/src/api/timeline.rs`, with `Position::icon` and `ExecutionCursor::icon` as wrappers. The first commit followed the plan. A later commit moved the mapping, because an icon is a display detail and not part of the API.

## Guidance
1. **One mapping function in the TUI.** `step_icon(action_type, decision_by)` is private in `src/tui/task_app/detail.rs`. It is the only place that maps a step kind to an icon.
2. **Two thin adapters for the two data sources.** `position_icon(&Position)` serves the timeline, the axis and the Conversation tab. `cursor_icon(&ExecutionCursor)` serves the task list, the task header and the step strip. No view builds its own `match`.
3. **The protocol crate carries data only.** `Position.action_type`, `Position.decision_by` and the cursor fields already carry `profile`. A new step kind needs no protocol change.
4. **Kinds without an icon return `None`.** `human`, `none`, `terminal` and an unknown type show the plain label. `profile` is not on this list. `decision_by` does not change the `profile` icon.

```rust
fn step_icon(action_type: Option<&str>, decision_by: Option<&str>) -> Option<&'static str> {
    match (action_type?, decision_by) {
        ("agent", _) => Some("✨"),
        ("script", _) => Some("💻"),
        ("profile", _) => Some("⚖️"),
        ("wait", _) => Some("✋"),
        ("decision", Some("conductor")) => Some("⚡🔀"),
        ("decision", Some("human")) => Some("🧑🔀"),
        ("decision", _) => Some("🔀"),
        _ => None,
    }
}
```

## Why This Matters
- Removing `step_icon`, `Position::icon` and `ExecutionCursor::icon` from `narsor-protocol` removed public functions that only the TUI used. A change of an icon now touches one crate.
- A view that builds its own icon can drift from the others. One function plus tests per view keeps them equal.
- `⚖️` is U+2696 with the variation selector U+FE0F. Terminals can draw its width differently than ratatui 0.30 (unicode-width) counts it. The step strip uses `cells(&step_name(icon, label))` for chip widths, so a width error is one column at most. A fallback is the form without the selector. Do not assume the width: keep a layout test with a `profile` step in a narrow terminal.

## When to Apply
- When you add or change an icon for a step kind.
- When you are tempted to put a display helper in `narsor-protocol` because several views need it. Put it in the TUI and share it inside the TUI.

## Examples
- Unit tests in `src/tui/task_app/detail.rs` (`step_icon_tests`) cover every kind, `profile` with `decision_by = human`, and the `None` cases.
- Render tests in `tests/integration/tui_render.rs` cover the task list, the task header, the axis and the step strip with a `profile` step.
- Check after a change of an icon: search `src`, `crates` and `tests` for the old icon. The branch left no `📜`. The old icon plan `docs/plans/2026-11-07-2215-feat-tui-ikony-krokow-plan.md` keeps it as history, with a note.

## Related
- `docs/brainstorms/2026-11-12-tui-ikony-typow-kroku-profile-i-script-requirements.md`
- `docs/plans/2026-11-12-002-feat-tui-ikony-profile-i-script-plan.md`
- `docs/solutions/design-patterns/tui-list-row-title-one-builder-for-screen-and-rows-2026-10-31.md`
- PR glakartur/narsor#101
