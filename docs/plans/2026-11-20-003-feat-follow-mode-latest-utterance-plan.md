---
title: "feat: Follow mode expands the latest utterance, bottom-anchored conversation, Home/End"
type: feat
status: completed
date: 2026-11-20
origin: docs/brainstorms/2026-11-20-follow-mode-latest-utterance-and-bottom-anchor-requirements.md
---

# feat: Follow mode expands the latest utterance, bottom-anchored conversation, Home/End

## Summary

Extend the shared TUI transcript reader with an opt-in "auto-expand latest utterance" policy that *Rozmowa* and *Przebieg* turn on, and switch its viewport-height handling from a top-line anchor to a bottom-line anchor. Add Home/End as aliases of `g`/`G` in the two step-conversation key contexts. In WebUI, keep the conversation's bottom line in place when its container changes height, and add Home/End to the existing global key handler, which already skips editable fields.

---

## Problem Frame

The origin document describes the pain: in follow mode the newest utterance is cut at 5 lines, a growing reply input hides the line being read, and Home/End do nothing (see origin: `docs/brainstorms/2026-11-20-follow-mode-latest-utterance-and-bottom-anchor-requirements.md`). Research added one plan-local fact: *Przebieg* refreshes its reader with a full replace, which in follow mode scrolls to the bottom but leaves the cursor on the previously selected entry, so R4 does not hold there today.

---

## Requirements

- R1–R6. Latest utterance in follow mode: auto-expand speech only, collapse only auto-expansions, keep the expansion when follow ends, cursor on the latest entry, view ends on its last line.
- R7–R8. Bottom anchor on viewport height change in both modes; the cursor does not move.
- R9–R11. Home/End as jump-only aliases of `g`/`G` in the step conversation; listed in key help.
- R12–R14. WebUI: bottom anchor on answer-panel resize; Home/End outside editable fields; full text kept.

**Origin acceptance examples:** AE1 (R1, R3), AE2 (R3), AE3 (R2), AE4 (R4), AE5 (R5), AE6 (R7, R8), AE7 (R10), AE8 (R13).

---

## Scope Boundaries

- No 5-line preview or row cursor in WebUI (origin).
- The conductor chat tab keeps its current behavior; it shares the renderer but does not opt in to auto-expansion.
- `f` keeps its current step-selection semantics; End does not resume following.
- Other readers (event details, panel context, history) are not changed, except that the bottom anchor lives in the shared transcript reader and therefore applies to every reader built on it.

---

## Context & Research

### Relevant Code and Patterns

- `src/tui/transcript/state.rs` — `TranscriptState`: `following_end`, `expanded` set keyed by `ExpansionIdentity`, `append` / `replace_entries` / `set_viewport` / `toggle_follow`, `anchor()` (top visible line) and `restore_after_change`, `lines_for` (where expanded detail lines are added).
- `src/tui/transcript/entry.rs` — `TranscriptEntry` with `rendered_lines` and `detail_lines` (`TranscriptDetailLine` carries a presentation).
- `src/tui/conversation.rs` — `agent_transcript_entry`: speech body over `SPEECH_PREVIEW_LINES` (5) is split into `detail_lines`, followed by technical details (e.g. `RunResult` structured output and usage JSON). The function is shared with the conductor's chat.
- `src/tui/scroll.rs` — `ScrollState` (`offset`, `viewport`, `set_offset`, `to_bottom`).
- `src/tui/task_app/tabs/conversation.rs` — *Rozmowa* readers per step (`readers` map, `replace_entries`, follow re-enabled after refresh), `Action::Top/Bottom` → `to_top/to_bottom`, render calls `set_viewport(height)` every frame.
- `src/tui/task_app/tabs/progress.rs` — `StepView` reader per step entry, refreshed via `replace_entries`.
- `src/tui/keymap.rs` — `g`/`G` bound to `Action::Top/Bottom` in `Context::StepConversation` and `Context::Reading`; `Context::Form` takes over while the reply field has focus.
- `crates/narsor-web/ui/js/ui/tabs/conversation.js` — `Conversation` component: `follow` state from scroll distance, `scrollIntoView` on entry count change, "↧ do końca" button. Used by both *Rozmowa* and *Przebieg*.
- `crates/narsor-web/ui/js/ui/app.js` — global `keydown` handler, guarded by `typing(event.target)`; `g`/`G` already move in the task list.
- `crates/narsor-web/ui-tests/*.test.mjs` — node tests over pure functions.

### Institutional Learnings

- `docs/solutions/design-patterns/tui-form-engine-events-and-cursor-style.md` — the reply input grows by `Growth` rows and owns Home/End inside a field. Because the transcript already receives its height every frame, the bottom anchor can live in `set_viewport` without listening to `FormEvent::Resized`; that also covers the row-limit case where no resize happens.
- `docs/solutions/design-patterns/tui-row-background-from-semantic-flag-after-render-2026-10-30.md` — entry properties travel as model flags, never detected from text; scroll math uses wrapped row counts.
- `docs/solutions/ui-bugs/tui-action-panel-refresh-stale-generation-and-lost-action-lock-2026-11-13.md` — keys that can be text must stay text while a field has focus.

---

## Key Technical Decisions

- **Auto-expansion is a reader policy, opt-in per reader**: *Rozmowa* and *Przebieg* turn it on; the conductor chat and other readers keep today's behavior. It keeps scope to the two views without forking the shared renderer.
- **The adapter marks speech continuation explicitly**: `agent_transcript_entry` records which detail lines continue the speech (a flag or count on the entry), and the reader reveals only those when auto-expanding. Technical details stay behind manual expansion. Never inferred from presentation or text.
- **Auto-expansion is tracked separately from manual expansion**: the reader keeps the identity of the single auto-expanded entry apart from the `expanded` set. A manual toggle on that entry converts it to a manual expansion (and clears the auto mark), so R3/R5 hold. Leaving follow keeps the auto mark until the next follow-driven change or a manual toggle.
- **Bottom anchor in `set_viewport`**: on a height change outside follow, keep `offset + viewport` constant (clamped); in follow, the existing scroll-to-bottom applies. Width changes keep the existing top-line anchor because a re-wrap is not a reveal/hide at the bottom.
- **Follow-mode refresh selects the newest entry**: `replace_entries` in follow mode selects the last projected entry, as `append` already does, which fixes R4 for *Przebieg*.
- **Home/End are key aliases only**: new bindings to the existing `Action::Top/Bottom` in `Context::StepConversation` and `Context::Reading`; `to_bottom` already leaves follow, matching R10.
- **WebUI bottom anchor via a container size observer**: when the conversation container's height changes, shift `scrollTop` by the height difference; in follow, scroll to the end. Pure offset math is extracted to a testable helper.
- **WebUI Home/End in the global handler**: active when task details are open on the conversation or progress tab; reuses the `typing` guard, so the answer textarea keeps native Home/End.

---

## Open Questions

### Resolved During Planning

- How the TUI learns the reply-input height change: it does not need an event; the render passes the viewport height every frame.
- Where to detect "speech": in the adapter (`agent_transcript_entry`), which already knows `Content::speech`.

### Deferred to Implementation

- Exact shape of the speech-continuation marker on `TranscriptEntry` (count of leading detail lines vs per-line flag).
- How the auto mark behaves when the display mode switches to Chronology or Raw and back: expected to persist and apply only in rendered/chronology card views; confirm against `projected_entries` and grouping.
- Whether the WebUI observer also needs to react to window resize separately, or whether the container observer covers it.

---

## High-Level Technical Design

*This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

| Event | Follow on | Follow off |
|---|---|---|
| New entry arrives (append / replace) | cursor → newest; auto mark moves to newest speech entry (none if non-speech); previous auto entry collapses; scroll to bottom | nothing auto-changes; top anchor kept |
| Viewport height changes | scroll to bottom | keep bottom line: `offset += old_viewport - new_viewport` (clamped) |
| Width changes | scroll to bottom | top anchor (unchanged) |
| Operator leaves follow (↑, PgUp, Home/End, `f`) | — | auto-expanded entry stays expanded |
| Manual toggle on the auto entry | auto mark → manual expansion state | same |
| `f` turns follow on | cursor → newest; auto mark set on newest speech entry; scroll to bottom | — |

---

## Implementation Units

### U1. Speech continuation marker in the conversation adapter

**Goal:** Entries produced from speech know which detail lines continue the speech text.

**Requirements:** R1, R2

**Dependencies:** none

**Files:**
- Modify: `src/tui/transcript/entry.rs`
- Modify: `src/tui/conversation.rs`
- Test: `src/tui/conversation.rs` (existing test module)

**Approach:**
- Add the marker to `TranscriptEntry` (default: no continuation). `agent_transcript_entry` sets it for the `extra_body` lines it splits off; technical details after them are not marked.
- Non-speech elements (tool call/result, thinking, technical, session) never get the marker.

**Patterns to follow:** the existing `workflow_choice` / `clip_head` flags set by the adapter.

**Test scenarios:**
- Happy path: an agent text of 12 lines yields an entry with 5 body lines and 7 marked continuation lines.
- Edge case: a run result with 8 lines of text and usage JSON marks only the 3 text continuation lines, not the JSON lines.
- Edge case: a tool result with long content has no continuation marker.

**Verification:** existing preview test (`a_long_prompt_previews_five_lines_and_expands_the_rest_once`) still passes; new marker assertions pass.

---

### U2. Auto-expand policy in the transcript reader

**Goal:** In follow mode, the reader shows the newest speech entry in full and collapses the previous auto-expansion, without touching manual expansions.

**Requirements:** R1, R2, R3, R4, R5, R6

**Dependencies:** U1

**Files:**
- Modify: `src/tui/transcript/state.rs`
- Test: `src/tui/transcript/tests.rs`

**Approach:**
- Add an opt-in policy flag on `TranscriptState` (off by default) and the identity of the current auto-expanded entry.
- `lines_for` reveals marked continuation lines when the entry is the auto entry and not manually expanded; manual expansion shows all detail lines as today.
- On follow-driven changes (`load`, `append`, `replace_entries` while following, `toggle_follow` on): move the auto mark to the newest projected entry if it carries continuation lines, otherwise clear it.
- `toggle_expanded` on the auto entry converts it to a manual expansion state the operator sees as expanded-then-collapsed per toggle semantics (first toggle collapses it, since it is visibly expanded) and clears the auto mark.
- Leaving follow keeps the mark. Raw display mode ignores the auto mark.
- `replace_entries` while following selects the newest projected entry (fixes *Przebieg* R4).
- Scroll-to-bottom after the mark moves uses wrapped row counts, so an entry taller than the viewport shows its end.

**Execution note:** start with failing reader tests for AE1–AE5.

**Patterns to follow:** `restore_after_change` / `anchor` flow; `expanded` handling via `ExpansionIdentity`.

**Test scenarios:**
- Covers AE1. Following, a 12-line agent utterance shows all 12 lines; after a new utterance arrives, the first shows 5 lines and the new one shows in full.
- Covers AE2. An utterance expanded by hand stays expanded when new entries arrive while following.
- Covers AE3. Following, a newest tool card with long output stays collapsed and no earlier utterance stays auto-expanded.
- Covers AE4. Following, viewport 20, newest utterance 40 wrapped rows: the last visible row is its last row and the cursor is on it.
- Covers AE5. Following with an auto-expanded newest utterance, `line_up` ends follow and the utterance stays expanded.
- Edge case: a reader without the policy (conductor chat) keeps showing the 5-line preview for the newest utterance.
- Edge case: a refresh via `replace_entries` while following moves the cursor to the newest entry.
- Edge case: run-result continuation expands but its usage JSON stays hidden until a manual toggle.

**Verification:** all transcript tests pass; no change in behavior for readers without the policy.

---

### U3. Bottom anchor on viewport height change

**Goal:** A viewport height change keeps the last visible line in place outside follow, and the end in view in follow.

**Requirements:** R7, R8

**Dependencies:** none (can land before or after U2)

**Files:**
- Modify: `src/tui/transcript/state.rs`
- Test: `src/tui/transcript/tests.rs`

**Approach:**
- In `set_viewport`, outside follow: compute the new offset from the old offset plus the height difference, clamped to the content; do not call the top-line anchor restore and do not touch the selection.
- In follow: unchanged (scroll to bottom).
- Leave `set_width` on the top-line anchor.

**Test scenarios:**
- Covers AE6. Not following, view ends on row 120, cursor near the top: shrinking the viewport by 3 keeps the last visible row at 120 and the cursor on its entry.
- Edge case: growing the viewport back by 3 moves content down by 3 rows and ends on the same row.
- Edge case: shrinking near the top of the content clamps the offset without panicking or moving past the start.
- Edge case: following, viewport change keeps the last content row at the bottom.

**Verification:** existing anchor/width tests still pass.

---

### U4. Opt-in in *Rozmowa* and *Przebieg*

**Goal:** Both step-conversation views turn on the auto-expand policy for their readers.

**Requirements:** R1, R4, R6

**Dependencies:** U2

**Files:**
- Modify: `src/tui/task_app/tabs/conversation.rs`
- Modify: `src/tui/task_app/tabs/progress.rs`
- Test: `src/tui/task_app/tabs/conversation.rs`, `src/tui/task_app/tabs/progress.rs` (existing test modules)

**Approach:** enable the policy where the readers are created (`readers` map entries and the empty reader in *Rozmowa*; `StepView::new` in *Przebieg*). The conductor tab stays untouched.

**Test scenarios:**
- Integration: *Przebieg* step view following a running step, a refresh adds a long agent utterance → it renders expanded and the cursor is on it.
- Integration: *Rozmowa* following, a refresh adds a long utterance to the selected step's reader → it renders expanded.

**Verification:** existing tab tests pass.

---

### U5. Home/End key bindings

**Goal:** Home/End jump to the start and end of the step conversation without turning follow on.

**Requirements:** R9, R10, R11

**Dependencies:** none

**Files:**
- Modify: `src/tui/keymap.rs`
- Test: `src/tui/keymap.rs` (existing test module)

**Approach:** add `KeyCode::Home` → `Action::Top` and `KeyCode::End` → `Action::Bottom` in `Context::StepConversation` and `Context::Reading`, with help text next to `g`/`G`. `Context::Form` keeps the field's own Home/End.

**Test scenarios:**
- Covers AE7. In the step conversation context, End resolves to `Action::Bottom`; after it, the reader is not following.
- Edge case: with a reply field focused (`Context::Form`), Home/End do not resolve to `Top`/`Bottom`.

**Verification:** key help lists Home/End; keymap tests (including any conflict checks) pass.

---

### U6. WebUI bottom anchor and Home/End

**Goal:** The WebUI conversation keeps its bottom line when the answer panel changes height, and Home/End jump to its start/end outside editable fields.

**Requirements:** R12, R13, R14

**Dependencies:** none

**Files:**
- Modify: `crates/narsor-web/ui/js/ui/tabs/conversation.js`
- Modify: `crates/narsor-web/ui/js/ui/app.js`
- Modify: `crates/narsor-web/ui/js/text.js` (pure anchor helper)
- Test: `crates/narsor-web/ui-tests/text.test.mjs`

**Approach:**
- In `Conversation`, observe the scroll container's size. On a height change: in follow, scroll to the end; otherwise shift `scrollTop` by the old height minus the new height (helper computes the clamped value). Make sure the programmatic scroll does not flip `follow` off.
- Expose a way for the global handler to scroll the active conversation to start/end (e.g. a shared ref or a small custom event); in `app.js`, map `Home`/`End` when details are open on the conversation or progress tab. The existing `typing` guard keeps them native in the answer textarea and other fields.
- Full text rendering unchanged.

**Test scenarios:**
- Happy path (helper): old height 600, new 540, scrollTop 1000 → 1060; reverse change → back to 1000.
- Edge case (helper): result is clamped to `[0, scrollHeight - clientHeight]`.
- Covers AE8 (manual check): focus in the answer textarea, Home moves the caret, not the conversation.
- Manual check: answer panel grows while scrolled up — the last visible line stays; while following — the end stays visible.

**Verification:** `node --test` on `crates/narsor-web/ui-tests` passes; manual browser check of both tabs.

---

## System-Wide Impact

- **Shared reader:** the bottom anchor (U3) applies to every `TranscriptState` user, including the conductor chat; that is intended and consistent with chat behavior.
- **Terminal resize:** a terminal height change is a viewport change too, so it also keeps the bottom line.
- **Key help:** help and footer hints show Home/End for the step conversation.

---

## Risks & Dependencies

| Risk | Mitigation |
|---|---|
| Auto mark drifts with grouping/projection changes (subagent folding, raw mode) | Mark is keyed by the same `ExpansionIdentity` as manual expansion; tests cover mode switches during implementation |
| Programmatic scroll in WebUI resets `follow` through `onScroll` | Compute follow from distance after the shift; cover with manual check |
| Bottom anchor surprises readers that relied on top anchor on resize | Only height changes use it; width changes keep the top anchor |

---

## Sources & References

- **Origin document:** `docs/brainstorms/2026-11-20-follow-mode-latest-utterance-and-bottom-anchor-requirements.md`
- Related: `docs/brainstorms/2026-10-31-tui-wspolny-renderer-rozmowy-requirements.md`
