---
title: "TUI bracketed paste into text fields"
date: 2026-10-18
category: design-patterns
module: tui
problem_type: design_pattern
component: tooling
severity: medium
applies_when:
  - "A crossterm/ratatui TUI has text fields where Enter submits"
  - "Operators paste multi-line text (logs, prompts, paths) into the TUI"
  - "The TUI switches on terminal modes that must be switched off on exit, error and panic"
  - "Text from outside (paste, logs) is stored in a value that is drawn with a sanitising function"
tags:
  - tui
  - crossterm
  - bracketed-paste
  - paste
  - terminal-restore
  - escape-sequences
  - text-input
---

# TUI bracketed paste into text fields

## Context

Without bracketed paste, the terminal sends a paste as a stream of key events. Each pasted newline arrives as `Enter`, and `Enter` submits the form (`Chooser::on_fields_key` → `Action::Submit`). The rest of the paste then goes as keys to the next view and can start actions there (for example `q` opens the quit dialog). The fix is in commit `66be617` (plan: `docs/plans/2026-10-17-001-feat-tui-wklejanie-wieloliniowe-plan.md`).

Three parts of the work were not obvious from the plan and are the durable lessons:

1. The display-side escape stripper (`consume_escape_sequence` in `src/tui/scroll.rs`) is not safe for pasted data. The plan said to reuse it; the implementation had to write a separate, stricter one.
2. A tab kept in the value moves the drawn cursor, because the drawing expands `\t` to spaces but the cursor is a character index.
3. `TerminalGuard::enter` can fail after it has switched on raw mode or bracketed paste. No guard exists yet at that point, so `Drop` does not restore the terminal.

## Guidance

**Take the paste as one event, on its own path.** Send `EnableBracketedPaste` after `EnterAlternateScreen` and handle `Event::Paste(String)` in the event loop. Route it with a separate `on_paste` method, in the same order as `on_key` (quit dialog → help → overlay → focused form → panel → conductor). Do not turn the paste into key events: that brings back `\n` → `Enter`. The paste path never returns a submit and never closes a modal. With no text field in focus, it does nothing.

**Arm terminal modes as best effort, with an "armed" flag.** Follow the `ENHANCEMENT_ARMED` pattern in `src/tui/mod.rs`:

```rust
static PASTE_ARMED: AtomicBool = AtomicBool::new(false);

pub fn arm_bracketed_paste(out: &mut impl Write) {
    if execute!(out, EnableBracketedPaste).is_ok() {
        PASTE_ARMED.store(true, Ordering::SeqCst);
    }
}

pub fn restore_terminal_to(out: &mut impl Write) {
    if PASTE_ARMED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, DisableBracketedPaste);
    }
    // ... pop keyboard flags, disable raw mode, leave alternate screen
}
```

A failure to enable does not stop start-up; the terminal then sends keys as before. `restore_terminal` is shared by `Drop` and the panic hook, and `swap(false)` keeps it idempotent. If the mode is not switched off, the shell shows `200~` / `201~` around every later paste. The `restore_terminal_to(out)` seam lets a test read the escape sequences.

**Restore on a failed `enter`, too:**

```rust
pub fn enter() -> io::Result<Self> {
    Self::take_over().inspect_err(|_| restore_terminal())
}
```

**Sanitise pasted text to what could be typed.** One shared function (`pasted_text` in `src/tui/input_form.rs`) decides by field kind:

- remove escape sequences; `\r\n`, `\r`, NEL, U+2028 and U+2029 become one `\n`;
- remove other control characters except `\n` and `\t`;
- remove invisible format characters and Unicode tag characters (U+E0000..U+E007F), which the field does not draw — otherwise the sent value differs from what the operator sees;
- multi-line field keeps `\n`; single-line field turns each `\n` into a space; number field keeps only digits and `-`; `select` takes no paste.

Insert with one `insert_str` at the byte index of the cursor and move the cursor by `chars().count()`. A loop of single-char inserts is O(n²) for a long log.

**Do not reuse a display escape stripper for data.** The display stripper consumes characters until it finds a terminator. On a lone or cut-off sequence in a paste, it eats real text: a CSI runs to the first byte in `@..~` (any letter), and an OSC without BEL/ST runs to the end of the paste. For pasted data, stop as early as possible:

- CSI: take only parameter/intermediate bytes (`' '..='?'`), then at most one final byte (`'@'..='~'`);
- OSC: stop at BEL, at ST (`ESC \`) or before a line break;
- other ESC: take at most one following `'@'..='_'` byte.

**Map the cursor through the same transform as the drawing.** When the value keeps `\t` but the drawing expands it, compute the drawn cursor from the drawn prefix:

```rust
fn drawn_cursor(field: &ChooserField) -> usize {
    let byte = char_byte_index(&field.value, field.cursor);
    sanitize(&field.value[..byte]).chars().count()
}
```

**A paste is an edit for the host state.** After a paste, do what a key edit does: clear `proposed` and `error` on the field, sync the draft (`sync_new_task_draft`, `sync_repository_draft`, `sync_draft`), and in the waiting panel clear `sent` / `key` / `error`. Without this, the pasted draft is lost when the view changes and an old validation error stays on screen.

## Why This Matters

- A paste that submits a half-written form or sends stray keys to other views can start real actions (send an answer, quit, confirm) that the operator did not want.
- Terminal modes that outlive the process break the operator's shell.
- ANSI codes and invisible characters from logs would go to the server in the submitted value.
- A shared helper that is correct for display output can be wrong for user data. The two have different failure costs: dropping a few drawn characters is harmless, dropping pasted text is data loss.

## When to Apply

- Adding any text input to a crossterm TUI, or a new host of `ChooserField` (add it to `TaskApp::on_paste` and test both "paste goes in" and "paste ignored when not typing").
- Adding any other terminal mode (mouse capture, focus events, keyboard flags): use the armed-flag pattern and restore it in `restore_terminal_to`.
- Reusing a sanitiser written for rendering on input or stored data.
- Storing a character in a value that the renderer draws with a different width.

## Examples

- Multi-line prompt field, paste `a\r\nb\r\nc` → value `a\nb\nc`, cursor at the end, nothing sent; `Enter` then submits as before.
- Single-line field, paste `ABC-123\n` → `ABC-123 ` (trailing space, by requirement AE4).
- Number field, paste `4a2\n` → `42`.
- Task list in focus, paste `q\nq\n` → no quit dialog, no change.
- Paste `x\x1b[ abc` (cut-off CSI) → the stripper takes `\x1b[ ` and `a` at most, not everything up to the next letter far away; OSC without a terminator stops at the line break.
- Field value `a\tb`, cursor after `b` → the drawn cursor is after `b`, not inside the expanded spaces.

Tests: `tests/tui_state.rs` (paste routing per host, sanitising, ignored states), `tests/tui_render.rs` (cursor after a 200-line paste and after `\t`), `tests/tui_terminal_restore.rs` (`\x1b[?2004l` written on restore, and only once). The restore test lives in its own test binary: it arms and disarms process-wide flags, and a parallel test or another test's panic hook would disarm them in between.

Limit: terminals or multiplexers without bracketed paste (tmux without `paste-buffer -p`, mosh) still send keys, and the original bug stays there. A timing heuristic was rejected in favour of the unambiguous `Paste` event.

## Related

- [TUI modal confirmation keyboard contract](tui-modal-confirmation-keyboard-contract.md) — a paste with a modal open is ignored and does not close or confirm it.
- `src/tui/mod.rs`, `src/tui/input_form.rs`, `src/tui/chooser.rs`, `src/tui/task_app/mod.rs`, `src/tui/task_app/waiting.rs`, `src/tui/scroll.rs` (`consume_escape_sequence`, `sanitize` — display side)
- Requirements: `docs/brainstorms/2026-10-17-tui-wklejanie-wieloliniowe-requirements.md`
