---
title: "Task detail action footer visual hierarchy"
date: 2026-10-14
category: design-patterns
module: tui/task_app/detail
problem_type: design_pattern
component: frontend
severity: low
applies_when:
  - "A terminal detail view combines a header, tabs, and a bottom action or conversation panel"
  - "Action labels can be long enough to push status markers beyond the visible area"
tags:
  - tui
  - task-details
  - action-footer
  - visual-hierarchy
---

# Task detail action footer visual hierarchy

## Context

The task detail view had several competing separators and selection cues. The active tab was underlined as well as marked on its own row, the waiting panel had nested borders, and recommendation or irreversible markers followed action labels where they could be clipped. The earlier chooser design applied a shared bracket treatment broadly, but the detail footer needed a calmer hierarchy while other chooser surfaces still depended on that treatment.

## Guidance

Give the header, tabs, and bottom action panel clear boundaries with a single horizontal separator at each boundary. Use one selected-tab cue: keep the separate marker row and omit text underline. Keep the conversation or waiting panel visible when it has no focus; distinguish focus through semantic styling, not by removing its boundary. Add vertical space between its heading and content, and avoid stacking nested pane borders inside the footer.

In action lists, enclose only the selected action number in pipes (`| 1 | opis`). Put recommendation and irreversible indicators before the description so they remain visible even when the description is long. Apply this treatment through a detail-footer rendering path; preserve the established bracket selection in general chooser surfaces that share the same component.

## Why This Matters

Several independent borders and emphasis marks make a compact terminal view harder to scan. Status markers at the end of a long label are easy to lose, and hiding an unfocused panel makes available actions less discoverable. A dedicated footer style addresses those problems without changing the visual contract of unrelated chooser contexts.

## When to Apply

- A detail screen needs to show both content and persistent actions in a small terminal area.
- Selection and focus are currently communicated through several overlapping visual treatments.
- A shared renderer serves contexts with different layout needs; scope the new treatment to the relevant context.

## Examples

Prefer a selected action line and description like:

```text
| 1 | revise
◆ rekomendacja dyrygenta
Opis działania, który może być długi…
```

Keep one separator between the header and tabs, and one separator above the footer. The footer remains visible when unfocused, with additional space between its heading and contents. In other chooser contexts, retain their existing selection treatment unless those contexts are deliberately redesigned too.

## Related

- `docs/plans/2026-10-14-1823-feat-task-detail-header-plan.md`
- `docs/plans/2026-10-10-002-feat-tui-klamra-zaznaczenia-rozmowa-kroku-plan.md` (historical selection design)
