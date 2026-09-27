//! Shared scroll state and text sanitization for the TUI's read-only content
//! windows: the event detail window (U6) and, later, the conversation view
//! (U7). Both show a buffer of lines the operator did not write — a JSON
//! payload, an agent's raw stdout — and both need the same three things: a
//! cursor that survives the content growing, page/line/jump navigation with
//! the same keys (R3), and a guarantee that nothing hostile to a terminal (a
//! raw control character, an ANSI escape sequence) ever reaches the render
//! call (R10).
//!
//! # Why the state does not wrap text itself
//!
//! Wrapping is a function of terminal width, which the state layer does not
//! know — `tests/tui_state.rs` drives this module with no terminal at all.
//! [`ScrollState`] therefore tracks a cursor over the caller's own **logical**
//! lines, not over wrapped screen rows; the rendering side is the one that
//! turns a handful of visible logical lines into screen rows, and it only
//! ever does that for the slice it is about to draw. A conversation held in
//! full and wrapped up front the moment it grows to a few megabytes is
//! exactly the cost this split avoids.

/// How many logical lines a freshly opened window is assumed to show, before
/// the first real render tells it otherwise.
///
/// Page/jump navigation needs *some* step size the moment a window opens and
/// before a frame has been drawn, so state tests that never touch a terminal
/// still see a sensible page size. Nothing here depends on this being the
/// true render height.
pub const DEFAULT_VIEWPORT: usize = 20;

/// Vertical scroll position over a buffer of logical lines.
///
/// `offset` is always kept within `[0, total_lines.saturating_sub(viewport)]`
/// for whatever `total_lines` the caller passes to the method that moves it —
/// there is no way to scroll a window past its own content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    offset: usize,
    viewport: usize,
}

impl ScrollState {
    /// A fresh cursor at the top of the content.
    pub fn new() -> Self {
        Self {
            offset: 0,
            viewport: DEFAULT_VIEWPORT,
        }
    }

    /// The first visible logical line.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The number of logical lines a page jumps by.
    pub fn viewport(&self) -> usize {
        self.viewport
    }

    /// Records how many lines the viewport can show. Rendering calls this
    /// every frame — a resize is nothing more than a different number here.
    /// The offset is left untouched: it is re-clamped the next time a move
    /// method runs, which is the first point a content length is known.
    pub fn set_viewport(&mut self, height: usize) {
        self.viewport = height.max(1);
    }

    /// One line down, clamped so the content's last line stays reachable.
    pub fn line_down(&mut self, total_lines: usize) {
        self.move_by(1, total_lines);
    }

    /// One line up, clamped at the top.
    pub fn line_up(&mut self, total_lines: usize) {
        self.move_by(-1, total_lines);
    }

    /// A viewport's height down.
    pub fn page_down(&mut self, total_lines: usize) {
        let step = self.viewport.max(1) as isize;
        self.move_by(step, total_lines);
    }

    /// A viewport's height up.
    pub fn page_up(&mut self, total_lines: usize) {
        let step = self.viewport.max(1) as isize;
        self.move_by(-step, total_lines);
    }

    /// Jumps to the first line.
    pub fn to_top(&mut self) {
        self.offset = 0;
    }

    /// Jumps so the content's last line is the bottom of the viewport.
    pub fn to_bottom(&mut self, total_lines: usize) {
        self.offset = self.max_offset(total_lines);
    }

    /// The last legal offset for `total_lines` lines in the current viewport.
    fn max_offset(&self, total_lines: usize) -> usize {
        total_lines.saturating_sub(self.viewport.max(1))
    }

    fn move_by(&mut self, delta: isize, total_lines: usize) {
        self.clamp_to(delta, self.max_offset(total_lines));
    }

    fn clamp_to(&mut self, delta: isize, max_offset: usize) {
        let max_offset = max_offset as isize;
        let next = (self.offset as isize + delta).clamp(0, max_offset.max(0));
        self.offset = next as usize;
    }

    /// One item down in a list of `total_items` distinct, individually
    /// addressable items — a **selection cursor**, not a scroll viewport.
    ///
    /// Use this (never `line_down`) for a widget that picks one item out of
    /// a list — the task view's axis (one position per row) and its content
    /// pane's per-entry cursor (one conversation entry or event per row) —
    /// rather than scrolls through wrapped text. `line_down`'s clamp is
    /// `total_lines.saturating_sub(viewport)`, which collapses to `0` the
    /// moment the list is shorter than the viewport (true for almost every
    /// real timeline: a handful of positions against a pane many rows
    /// tall) and freezes the cursor at the first item forever. A selection
    /// cursor must be able to reach every index, `0` through
    /// `total_items - 1`, regardless of how much of the pane it fills;
    /// keeping the selected row on screen as it moves past what currently
    /// fits is the renderer's job (see `render_task_axis`/
    /// `render_task_content` in `src/tui/views.rs`), not this state's.
    pub fn select_down(&mut self, total_items: usize) {
        self.clamp_to(1, total_items.saturating_sub(1));
    }

    /// One item up — see [`Self::select_down`].
    pub fn select_up(&mut self, total_items: usize) {
        self.clamp_to(-1, total_items.saturating_sub(1));
    }

    /// A viewport's height of items down — see [`Self::select_down`].
    pub fn select_page_down(&mut self, total_items: usize) {
        let step = self.viewport.max(1) as isize;
        self.clamp_to(step, total_items.saturating_sub(1));
    }

    /// A viewport's height of items up — see [`Self::select_down`].
    pub fn select_page_up(&mut self, total_items: usize) {
        let step = self.viewport.max(1) as isize;
        self.clamp_to(-step, total_items.saturating_sub(1));
    }

    /// Selects the first item — see [`Self::select_down`].
    pub fn select_first(&mut self) {
        self.offset = 0;
    }

    /// Selects the last of `total_items` items — see [`Self::select_down`].
    pub fn select_last(&mut self, total_items: usize) {
        self.offset = total_items.saturating_sub(1);
    }
}

impl Default for ScrollState {
    fn default() -> Self {
        Self::new()
    }
}

/// Replaces control characters and ANSI escape sequences with a visible
/// substitute (R10), before anything from an event's payload or a
/// conversation entry reaches the terminal.
///
/// `\n` and `\t` are the exceptions: a newline stays a newline, and a tab
/// expands to spaces, so column-sensitive content (a JSON dump, a table)
/// does not depend on the viewer's own tab stops.
pub fn sanitize(text: &str) -> String {
    const TAB_WIDTH: usize = 4;

    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                out.push('\n');
                column = 0;
            }
            '\t' => {
                let width = TAB_WIDTH - (column % TAB_WIDTH);
                for _ in 0..width {
                    out.push(' ');
                }
                column += width;
            }
            '\u{1b}' => {
                // Swallow the whole escape sequence, not just the ESC byte,
                // so a CSI colour code or an OSC title-set does not leak its
                // parameter bytes as plain, otherwise-printable text.
                consume_escape_sequence(&mut chars);
                out.push(control_picture(ch));
                column += 1;
            }
            c if c.is_control() => {
                out.push(control_picture(c));
                column += 1;
            }
            c => {
                out.push(c);
                column += 1;
            }
        }
    }

    out
}

/// The visible stand-in for a control character — the Unicode "control
/// pictures" block for C0 controls and DEL, and the replacement character
/// for anything else `char::is_control` flags that this block does not name.
fn control_picture(c: char) -> char {
    let code = c as u32;
    if code < 0x20 {
        char::from_u32(0x2400 + code).unwrap_or('\u{FFFD}')
    } else if code == 0x7F {
        '\u{2421}'
    } else {
        '\u{FFFD}'
    }
}

/// Consumes the body of an ANSI escape sequence, up to and including its
/// terminator, assuming the caller already consumed the leading ESC byte.
fn consume_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek() {
        // CSI ("\x1b[..."): parameter/intermediate bytes, then a final byte
        // in the 0x40..=0x7E range (`m` for SGR colour codes, and others).
        Some('[') => {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        // OSC ("\x1b]..."): terminated by BEL, or by ESC '\' (ST).
        Some(']') => {
            chars.next();
            for c in chars.by_ref() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    break;
                }
            }
        }
        // Any other two-character escape (e.g. `ESC c` — full reset).
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

/// Wraps just the logical lines a viewport of `height` rows starting at
/// `offset` needs, at `width` columns — never the whole buffer, so a very
/// long conversation (U7) only ever pays to wrap the handful of rows an
/// operator can actually see.
pub fn wrap_visible(lines: &[String], width: usize, offset: usize, height: usize) -> Vec<String> {
    if height == 0 || offset >= lines.len() {
        return Vec::new();
    }
    let width = width.max(1);
    let mut window = Vec::with_capacity(height);

    'lines: for line in &lines[offset..] {
        for chunk in wrap_line(line, width) {
            if window.len() >= height {
                break 'lines;
            }
            window.push(chunk);
        }
    }

    window
}

/// Breaks one logical line into rows of at most `width` characters.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = line.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_newlines_and_expands_tabs() {
        assert_eq!(sanitize("a\nb"), "a\nb");
        assert_eq!(sanitize("a\tb"), "a   b");
    }

    #[test]
    fn sanitize_replaces_control_characters() {
        let out = sanitize("a\x07b");
        assert!(!out.contains('\u{7}'));
        assert_eq!(out, "a\u{2407}b");
    }

    #[test]
    fn sanitize_swallows_ansi_escapes() {
        let out = sanitize("\x1b[31mred\x1b[0m");
        assert!(!out.chars().any(|c| c.is_control()));
        assert!(out.contains("red"), "the visible text survives: {out:?}");
        assert!(
            !out.contains("[31m") && !out.contains("[0m"),
            "the escape's parameter bytes do not leak as plain text: {out:?}"
        );
    }

    #[test]
    fn scroll_state_clamps_within_bounds() {
        let mut scroll = ScrollState::new();
        scroll.set_viewport(5);
        scroll.line_up(100);
        assert_eq!(scroll.offset(), 0);

        scroll.to_bottom(12);
        assert_eq!(scroll.offset(), 7);

        scroll.line_down(12);
        assert_eq!(scroll.offset(), 7, "cannot scroll past the last line");

        scroll.to_top();
        scroll.page_down(12);
        assert_eq!(scroll.offset(), 5);
    }

    /// Reproduces the bug this test was written against: a selection
    /// cursor over a short list (fewer items than the viewport) must still
    /// be able to reach every index — `line_down`'s scroll-viewport clamp
    /// would freeze it at `0` here, since `3 <= viewport`.
    #[test]
    fn select_down_reaches_every_index_even_when_the_list_is_shorter_than_the_viewport() {
        let mut scroll = ScrollState::new(); // default viewport is 20
        assert_eq!(scroll.offset(), 0);

        scroll.select_down(3);
        assert_eq!(scroll.offset(), 1);
        scroll.select_down(3);
        assert_eq!(scroll.offset(), 2);
        // Clamped at the last index, not the viewport-relative one.
        scroll.select_down(3);
        assert_eq!(scroll.offset(), 2);

        scroll.select_up(3);
        assert_eq!(scroll.offset(), 1);
        scroll.select_up(3);
        assert_eq!(scroll.offset(), 0);
        scroll.select_up(3);
        assert_eq!(scroll.offset(), 0, "cannot select before the first item");
    }

    #[test]
    fn select_last_and_select_first_jump_to_the_list_bounds() {
        let mut scroll = ScrollState::new();
        scroll.select_last(5);
        assert_eq!(scroll.offset(), 4);
        scroll.select_first();
        assert_eq!(scroll.offset(), 0);
        // An empty list has no last index to jump to.
        scroll.select_last(0);
        assert_eq!(scroll.offset(), 0);
    }

    #[test]
    fn select_page_down_and_up_step_by_the_viewport_but_still_clamp_to_the_list() {
        let mut scroll = ScrollState::new();
        scroll.set_viewport(5);
        scroll.select_page_down(3);
        assert_eq!(scroll.offset(), 2, "clamped to the last of only 3 items");
        scroll.select_page_up(3);
        assert_eq!(scroll.offset(), 0);
    }
}
