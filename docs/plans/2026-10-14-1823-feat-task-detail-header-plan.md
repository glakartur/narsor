---
title: Task Detail Header - Plan
type: feat
date: 2026-10-14
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
---

# Goal Capsule

Make a task’s identity, current state, and selected detail tab easy to scan in the Ratatui detail view, including when the detail view is unfocused or `NO_COLOR` is set. Keep the existing task data, wording, semantic roles, navigation, and tab contents. Use the session-settled layout and tab-marker decisions below as the visual authority.

Stop when the shared live/history detail renderer has the three-row header, optional notice, two-row tabs strip, correctly placed link targets, and enough content space at the supported minimum terminal size.

## Product Contract

### Summary

The current two-line header mixes links, task state, and workflow metadata on one line. Long optional metadata can crowd out information operators need first. The selected tab is currently distinguished by bold and underline only; the user chose an additional dedicated marker line after seeing visual alternatives.

### Requirements

- **R1 — Identity first.** Put the task title and task ID on the first header row. The title may still be fitted to the available width.
- **R2 — State stays visible.** Put lifecycle state, current step or queue position, duration, and existing warnings on a dedicated second row. Keep each existing icon with its text label.
- **R2a — State-row overflow.** If the state row itself exceeds the available cells, keep the lifecycle icon and label plus any warning cue visible; shorten or omit duration first, then shorten secondary step/queue detail with an ellipsis. Do not move status content into the metadata row.
- **R3 — Metadata follows state.** Put available links and workflow/version metadata on a third row. Keep the existing notice as a separate optional row after the header; it remains visible and moves the tabs down.
- **R3a — Metadata-row overflow.** Keep link labels whole so each OSC 8 region can cover its displayed label. If the next link does not fit, omit it and following links; omit workflow/version detail when it does not fit after the retained links.
- **R4 — Tabs have a positional cue.** Keep the existing tab titles and dynamic follow indicators on the first tab row. Keep the selected title’s current bold and underline, and add a horizontal marker directly beneath the selected title on the second row. Use a heavy line when details have focus and a thin line when they do not, so focus remains distinguishable under `NO_COLOR`.
- **R4a — Tab-row overflow.** At narrow widths, first reduce padding between tab labels; if the row still overflows, omit the trailing follow indicator while keeping all tab titles and the selected title fully visible. The marker spans the selected title’s visible cell width only.
- **R5 — Preserve interaction and semantics.** Keep tab order, `[`/`]` navigation, content, focus behavior, existing semantic color roles, and `NO_COLOR` handling. Do not add a palette or change status wording.
- **R6 — Links remain accurate.** Hyperlink OSC 8 regions and their clickable hit spots must match the displayed link text after the row moves.
- **R7 — Shared views.** Live and historical task details use the same updated layout through the shared renderer.
- **R8 — Compact terminals remain usable.** At the supported 60×20 minimum, fullscreen details have 16 rows inside the app header/footer and detail frame. The three-row header, optional one-row notice, and two-row tabs consume at most 6 rows; the waiting panel is capped to leave at least one content row (9 panel rows with a notice, 10 without). Keep all regions non-overlapping.

### Success Criteria

- Status and warnings do not get displaced by links or workflow metadata.
- The selected tab is clear in focused and unfocused states, with color and under `NO_COLOR`.
- Link text and clickable regions stay aligned.
- Live and historical details render consistently, including with a notice and waiting panel at 60×20.

### Scope Boundaries

No changes to task data, lifecycle or workflow wording, links or link-menu behavior, tab contents, key bindings, global layout, color palette, or dependencies. No new tabs, status translation, or history-specific layout.

### Key Decisions

1. **Dedicated status row** *(user-directed; session-settled: user-directed — chosen over the mixed status-and-links row because the user selected sketch B and accepted its extra vertical space).* `Governs R1–R3`.
2. **Separate active-tab marker row** *(user-directed; session-settled: user-directed — chosen over the existing label-only treatment after the user selected a dedicated horizontal marker beneath the active tab).* `Governs R4`.

## Planning Contract

### Key Technical Decisions

1. **Retain the custom tab renderer** *(planning decision; inherits the user-directed decision in R4).* Keep the current per-span rendering because it carries per-tab follow indicators and focus-aware selected styles. Measure the selected label’s display-column range and draw its marker beneath that range. Use existing Ratatui span-width support for cell measurements; do not add a dependency. Ratatui’s `Tabs` widget supports custom highlight and divider styles, but switching widgets is unnecessary for this existing custom behavior. [Ratatui Tabs](https://docs.rs/ratatui/latest/ratatui/widgets/struct.Tabs.html).
2. **Derive layout heights from the new rows.** Treat the header as three rows plus an optional notice, and the tabs strip as two rows. Update area allocation and link-spot geometry together so content and the bottom panel use the remaining height safely.
3. **Apply row-specific overflow rules.** Use terminal display-cell widths consistently. Apply R2a to status, R3a to metadata and links, and R4a to tab labels and follow indicators; do not add a width-measurement dependency.

### Assumptions

- `render_view` is shared with historical details, so no history-only behavior is needed.
- The current terminal minimum is 60×20; the outer layout remains responsible for smaller-terminal handling.
- Existing semantic roles and `NO_COLOR` handling remain centralized in `src/tui/style.rs`; the marker uses line weight per R4 to show detail focus when the selected label’s bold and underline are identical in focused and unfocused `NO_COLOR` states.

## Implementation Units

### U1 — Reflow task header and add the active-tab marker

**Goal:** Apply the selected hierarchy while preserving existing state and interaction semantics.

**Requirements:** R1–R8; specifically the user-directed decisions governing R1–R4.

**Files:** `src/tui/task_app/detail.rs`; `tests/tui_render.rs`; `tests/tui_state.rs`.

**Approach:** Update header and area documentation, header row generation, and `HEADER_LINES`. Place links and workflow/version metadata on row three, keep notices on their own row, and move OSC 8 hit spots to the actual link row. Apply R3a when the metadata row overflows. Allocate two rows to tabs and recalculate content and waiting-panel areas without allowing overlap; at 60×20 retain one content row by capping the panel as described in R8. Measure tab label positions in terminal cells and render the selected marker below the title, using heavy/thin glyphs for focused/unfocused details and preserving semantic color roles. Apply R4a when the tab row overflows. Fit variable-width content by displayed cell width and apply R2a if the state row itself overflows. Since `render_view` serves both live and history detail views, update and cover the shared path.

**Test scenarios:**

- A running task with a long title, links, workflow/version, active step, and duration at a wide width shows identity, state, then metadata on the intended rows; state and warnings remain visible when optional metadata is long.
- A queued task and a task with a warning retain their current labels and icons on the dedicated state row.
- When the state row overflows at 60 columns, lifecycle and warning cues remain visible; duration is shortened or omitted before secondary step/queue detail.
- When the metadata row overflows at 60 columns, link labels remain whole and later links/workflow detail are omitted in order; visible links remain clickable over their full labels.
- If the tab row overflows at 60 columns, reduced padding retains all titles; any further overflow omits only the follow indicator, and the selected title plus marker remain fully visible and aligned.
- With each tab selected, the marker appears under that tab’s title, not under follow indicators; focused, unfocused, and `NO_COLOR` renderings remain distinguishable.
- OSC 8 output and link hit spots identify the visible link text on row three.
- A default-harness notice adds one row and keeps its existing text; history details show the same layout.
- At 60×20, including with a notice and bottom waiting panel, the detail inner area allocates 4 header rows, 2 tab rows, 9 waiting-panel rows, and at least 1 content row without overlap.
- Variable-width title, link, workflow, and tab labels do not misalign the marker or link hit spots.

**Verification:** Update the existing detail renderer cases in `tests/tui_render.rs` and area allocation case in `tests/tui_state.rs`; run the focused test filters listed in the Verification Contract.

## Verification Contract

- `cargo test --test tui_render task_app_detail_render` — validates header ordering, notice placement, tab marker, styles, links, and shared live/history rendering.
- `cargo test --test tui_state the_areas_leave_room_for_a_panel_at_the_bottom` — validates recalculated header/tab/content/panel geometry.
- Review the 60×20 cases and confirm the marker and link hit regions align with terminal display cells, including variable-width text.

## Definition of Done

- [ ] Header uses identity, dedicated status, and metadata rows; the existing optional notice remains visible.
- [ ] The active tab has its existing label treatment plus a correctly aligned marker row.
- [ ] Existing focus, semantic color, `NO_COLOR`, link, tab-navigation, and history behavior is preserved.
- [ ] Layout and link-coordinate coverage passes at wide and minimum supported sizes, including notice and waiting-panel combinations.
- [ ] Both focused verification commands pass, and the 60×20 display-cell alignment review is complete.

## Sources & Research

- Repository implementation: `src/tui/task_app/detail.rs` (`HEADER_LINES`, `areas`, shared render path, `tab_bar`, `status_parts`, and `link_spots`).
- Existing detail rendering cases: `tests/tui_render.rs::task_app_detail_render`.
- Existing area allocation case: `tests/tui_state.rs::the_areas_leave_room_for_a_panel_at_the_bottom`.
- Current Ratatui version is 0.30.2; its official `Tabs` API supports custom selected/highlight and divider styles. [API documentation](https://docs.rs/ratatui/latest/ratatui/widgets/struct.Tabs.html).
