---
title: "TUI: keys stall after a window focus switch - Plan"
type: fix
date: 2026-11-23
topic: tui-keys-stall-after-focus-switch
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# TUI: keys stall after a window focus switch - Plan

## Goal Capsule

- **Objective:** The `narsor` TUI reacts to each key on the next frame, also after the operator returns from another window while a task changed state (for example, to "waiting for a human decision"). Keys are never held back and then applied in a burst later.
- **Product authority:** The report from the operator (2026-11-23). Assumptions taken without the operator are listed in "Assumptions to confirm".
- **Means:** First find where the loop or the screen stops (diagnosis with a reproduction and the TUI log). Then remove that cause. Then add a guard so that a slow terminal or a slow call cannot stop key handling again.
- **Open blockers:** None. The root cause is not known yet; finding it is the first unit of work.
- **Execution profile:** One PR, Standard. Diagnosis first (`ce-debug`), then the fix.
- **Stop conditions:** Stop and report when the cause is in the terminal emulator or the compositor and no in-app mitigation (including the redraw on focus gain from assumption 6) removes it; the result is then the documented workaround from "Scope Boundaries". Stop and ask when the fix needs a change in key behavior (for example, dropping keys pressed during a stall).
- **Who finishes:** `ce-plan` adds the technical plan after the diagnosis; `ce-work` builds and verifies with `cargo test --workspace`.

---

## Product Contract

### Summary

The operator has focus on the task list. They switch to another application. While they are away, a task goes into the "waiting for a human decision" state. They come back to the Narsor window and press keys: nothing changes on the screen. They switch away and back again: suddenly the earlier keys take effect (for example, the highlight is now inside the task). The operator says this happens "quite often".

After the fix, the TUI reacts to keys at once in this situation. The screen always shows the result of the keys that the operator pressed.

### Problem Frame

The behavior "keys do nothing, then all apply together" means that the keys are not lost. They wait somewhere and are applied later. The code shows these facts (verified on 2026-11-23):

- A dedicated thread reads terminal events and sends them over a channel to the main loop (`src/tui/mod.rs`, `read_terminal_events`). So reading keys cannot block, but handling them can wait for the main loop.
- The main loop draws the full screen before it waits for the next event (`terminal.draw(...)` on a blocking `Stdout`). The loop also awaits some calls directly: `app.dispatch(request).await` and `app.retry_outgoing_if_due(now).await`.
- The TUI does not enable focus reporting (no `EnableFocusChange`). So the terminal sends no event to Narsor when the window gets or loses focus. A focus switch cannot wake the loop through its own input.
- The control-plane event that moves a task to "waiting" runs `on_stream`, then `sync_waiting` and `sync_overlay`, and the next draw shows new content (attention marks, the waiting form, possibly links drawn as OSC 8 hyperlinks in `detail.rs`).

Because a focus switch sends nothing to Narsor, the "unblock" on the second return most likely comes from outside the process: the terminal emulator starts to read the program output again, or repaints its window again. The candidate causes, for the diagnosis:

- **H1. The draw blocks on output.** The terminal does not read the pty while its window is unfocused or hidden. The write of a large frame (the new "waiting" content) fills the pty buffer, `draw` blocks, and the keys queue up in the channel. When the terminal reads again, the loop runs and applies all queued keys at once. This matches the symptom best.
- **H2. The screen does not repaint.** Narsor handles the keys and writes the frames, but the terminal shows an old image until a later focus change (a terminal or compositor repaint problem, or terminal state broken by an escape sequence such as an OSC 8 link).
- **H3. The loop awaits a slow call.** A call awaited in the loop (`dispatch`, `retry_outgoing_if_due`) waits for the server without a time limit. The focus switch only coincides with the end of the wait.

### Requirements

**Responsiveness**

- R1. After the operator returns to the Narsor window, each key changes the screen on the next frame, as when the window never lost focus. This holds also when one or more tasks changed state (to "waiting for a human decision" or another state) while the window had no focus.
- R2. No control-plane event, list refresh, background read or retry stops the handling of keys. Work that can take long (a server call, a large redraw) does not keep a key waiting longer than one frame period of the loop.
- R3. Keys are applied in the order the operator pressed them, and none is lost.
- R4. The screen after a key shows the state that this key produced. The operator never sees an older image while the state behind it already moved on.

**Diagnosability**

- R5. When the loop cannot handle keys for longer than a threshold, the TUI log records it: how long, and in which step the loop was (drawing, a named awaited call, or waiting for events). An operator can attach this log to a report. The TUI writes its log only when `RUST_LOG` is set (`docs/user/tui.md`, "Logging"), so the "Logging" paragraph of `docs/user/tui.md` also tells the operator to set `RUST_LOG` before they report a stall, and the stall entry uses a level that this setting lets through.

### Acceptance Examples

- AE1. **Covers R1, R3.** Focus is on the task list. The operator switches to another window. A task goes to "waiting for a human decision". The operator returns and presses `j`, then `Enter`. The highlight moves down one row on the first key, and the detail of that task opens on the second key. No second focus switch is necessary.
- AE2. **Covers R2.** A server call in the loop does not answer for 10 seconds. During this time, `j`/`k` still move the highlight on the task list, and `q` still quits.
- AE3. **Covers R5.** When the draw of one frame takes longer than the threshold, the TUI log has one entry with the duration and the step "draw".

### Scope Boundaries

- Only the `narsor` TUI (the `tui` command of `narsor-cli` and the `narsor` binary). No change to the server, the controller or the event protocol.
- No change to the key map or to what a key does.
- No new desktop notification, bell or window title when a task waits for a decision. (This is a possible later feature, not part of this fix.)
- Problems that are fully inside a terminal emulator or compositor are out of scope. A mitigation inside Narsor, such as the redraw on focus gain from assumption 6, is in scope. When the diagnosis finds such a cause and no in-app mitigation removes it, the result is a documented workaround in `docs/user/` and a note in the TUI log, not a change in Narsor.

### Key Decisions

- **Diagnose before the fix.** The root cause is not known; three causes (H1–H3) give the same symptom and need different fixes. The first unit reproduces the stall and finds the step where the loop stops (R5 helps here and stays as a permanent tool).
- **Keys pressed during a stall are applied, not dropped (R3).** Dropping keys would hide the symptom but lose operator input. The fix removes the stall itself, so that no burst occurs.

---

## Assumptions to confirm

1. **Terminal environment is unknown.** The report does not name the terminal emulator, the window manager or the compositor (the platform is Linux). The fix must not depend on a specific terminal. The reproduction in the diagnosis tries at least one common Wayland terminal and one X11 terminal. The operator can give their terminal and compositor to make the reproduction faster; this is helpful but does not block the plan.
2. **The "waiting for a human decision" state is a trigger, not the only cause.** The operator saw the stall with this state change. The requirements (R1, R2) cover any state change and any background work while the window has no focus.
3. **Focus on the task list is not a necessary condition.** The report says "focus on the task list". The fix covers every panel of the TUI; the task list is the reproduction case.
4. **Keys pressed during a short stall are kept (R3).** A stall shorter than one frame period stays invisible to the operator. Keys are not dropped after a long stall either; the fix aims at no long stall at all.
5. **Threshold of R5.** The log threshold is a fixed value chosen in planning (for example 500 ms), not a setting. It must not write log entries during normal work.
6. **Enabling focus reporting is allowed as a tool.** When the diagnosis shows that a redraw on focus return fixes H2, the TUI may enable terminal focus events and force a full redraw on focus gain. This is an internal change, the operator sees no new behavior except a correct screen.

## Open questions for the user

None.
