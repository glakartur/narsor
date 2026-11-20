---
date: 2026-11-20
topic: follow-mode-latest-utterance-and-bottom-anchor
---

# Follow Mode: Latest Utterance Expanded, Bottom-Anchored Conversation, Home/End

## Summary

In TUI follow mode (*Rozmowa* and *Przebieg*), the latest utterance shows in full and the view ends on its last line. When a newer utterance arrives, the previous auto-expanded one collapses back to its 5-line preview. The conversation stays anchored to its bottom line while the reply input grows or shrinks. Home/End jump to the start and end of the step's conversation. WebUI gets the same bottom anchor and Home/End.

---

## Problem Frame

The operator watches an agent work in follow mode. Today a long utterance shows only its first 5 lines (`SPEECH_PREVIEW_LINES`), so the newest — and usually most relevant — text is cut off. The operator must leave follow mode and expand it by hand to read it.

The "waiting for you" reply input grows while the operator types. The conversation viewport then shrinks. Outside follow mode, the transcript keeps its top line, so the bottom lines the operator was reading disappear under the input. In WebUI the follow effect reacts only to new entries, so a growing answer panel hides the end of the conversation even in follow mode.

Jumping to the start or end of a step's conversation works only with `g`/`G`. Operators expect Home/End.

---

## Requirements

**Latest utterance in follow mode (TUI: *Rozmowa* and *Przebieg*)**
- R1. In follow mode, the latest entry is shown expanded when it is an utterance (agent, operator, conductor speech) longer than the 5-line preview.
- R2. Tool cards, operation groups, subagent groups and other non-speech entries are never auto-expanded.
- R3. When a newer entry arrives in follow mode, the previously auto-expanded utterance collapses to its preview. An entry the operator expanded by hand stays expanded.
- R4. In follow mode, the cursor stays on the latest entry, and the view behaves as if the cursor stood just after the last line of content: the view ends on the last line of the latest entry. If the expanded entry is taller than the viewport, the view shows its end, not its header.
- R5. When the operator leaves follow mode (any navigation key or `f`), the auto-expanded utterance stays expanded. It does not collapse at that moment, so the view does not jump. The operator can collapse it by hand.
- R6. When follow mode is turned on again, R1–R4 apply to the current latest entry.

**Bottom anchor on reply-input resize (TUI)**
- R7. When the reply input grows by N lines, the conversation content moves up by N lines, so the view still ends on the same content line. When it shrinks by N lines, the content moves down by N lines. This applies in both follow and non-follow mode.
- R8. Outside follow mode, the resize does not move the cursor. The selected entry may scroll out of view above the top edge.

**Home/End (TUI)**
- R9. Home and End jump to the start and end of the conversation of the current step, as `g` and `G` do today. They apply wherever `g`/`G` move within a step's conversation (*Rozmowa* reader, *Przebieg* step conversation).
- R10. Home, End, `g` and `G` do not turn follow mode on. Follow mode is toggled only by `f`. Pressing them in follow mode leaves follow mode, as other navigation keys do.
- R11. The key help lists Home/End next to `g`/`G`.

**WebUI (*Rozmowa* and *Przebieg*)**
- R12. When the answer panel changes height, the conversation keeps its bottom visible line in place, in both follow and non-follow state. In follow state, the end of the conversation stays visible.
- R13. Home and End scroll the conversation to its start and end, unless focus is in a text input, textarea or other editable field. In an editable field the keys keep their native text-editing meaning.
- R14. WebUI keeps showing full utterance text. There is no 5-line preview and no row cursor.

---

## Acceptance Examples

- AE1. **Covers R1, R3.** Given follow mode and a latest agent utterance of 12 lines, all 12 lines show. When a new agent utterance arrives, the 12-line one shows its 5-line preview and the new one shows in full.
- AE2. **Covers R3.** Given the operator expanded utterance #4 by hand earlier, when new entries arrive in follow mode, #4 stays expanded.
- AE3. **Covers R2.** Given follow mode and a latest entry that is a tool card with long output, the card stays collapsed.
- AE4. **Covers R4.** Given follow mode, a 20-line viewport and a latest utterance of 40 lines, the view shows lines 21–40 of that utterance, with the cursor on it.
- AE5. **Covers R5.** Given follow mode with an auto-expanded latest utterance, when the operator presses ↑, follow mode ends and the utterance stays expanded.
- AE6. **Covers R7, R8.** Given non-follow mode with the view ending on line 120 and the cursor on an entry near the top, when the reply input grows by 3 lines, the view still ends on line 120 and the cursor stays on its entry, even if that entry leaves the view.
- AE7. **Covers R10.** Given non-follow mode, when the operator presses End, the view jumps to the end of the step's conversation and follow mode stays off.
- AE8. **Covers R13.** Given WebUI with focus in the answer textarea, Home moves the text caret, not the conversation.

---

## Success Criteria

- An operator in follow mode reads the full newest utterance without pressing any key, and typing a long reply never hides the line they were reading.
- A planner can implement each requirement without inventing behavior: expansion scope, collapse rule, follow-exit behavior, anchor rule and key semantics are all stated.

---

## Scope Boundaries

- No 5-line preview or row cursor in WebUI.
- No change to the conductor's chat tab or other readers (history, panel context, lists); only *Rozmowa* and *Przebieg*.
- No change to how `f` decides which step to follow.
- End does not become a "resume following" key.

---

## Key Decisions

- Auto-expand only speech: tool output would flood the view.
- Collapse only what was auto-expanded: the operator's manual expansions express intent and must survive.
- Leaving follow mode keeps the expansion: avoids a jump at the moment the operator starts reading.
- Bottom anchor in both modes: matches chat tools, and the reader's current line is at the bottom next to the input.
- Home/End are jump-only: following in *Rozmowa* can also switch to the newest step, so mixing it into End would surprise.
- WebUI keeps native keys in editable fields: hijacking Home/End there breaks text editing.

---

## Dependencies / Assumptions

- The TUI transcript reader is shared by *Rozmowa* and *Przebieg* (`src/tui/transcript/state.rs`), and the 5-line speech preview lives in `src/tui/conversation.rs`. WebUI uses one conversation component for both tabs (`crates/narsor-web/ui/js/ui/tabs/conversation.js`).
- The answer-panel resize in WebUI fires no scroll or entry-count event today, so the current follow effect does not react to it (verified by reading the component).

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R3][Technical] How to mark an expansion as automatic vs manual so R3 and R5 hold across refreshes, mode switches (rendered/chronology/raw) and grouped entries.
- [Affects R7][Technical] Where the TUI learns the reply-input height change (viewport change) and how the bottom anchor interacts with the existing top-line anchor used for width changes.
- [Affects R12][Technical] How WebUI detects the answer-panel height change without layout jitter.
