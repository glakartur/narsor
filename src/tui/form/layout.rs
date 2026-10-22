//! How a text is laid out in a field: the wrapped rows, where the cursor
//! falls in them, and which rows are in view.
//!
//! One function ([`wrap`]) is the only source for drawing, for the cursor's
//! place, for scrolling and for moving the cursor up and down the rows that
//! are drawn. Text that is not edited (descriptions) wraps through it too.

use super::buffer::char_byte_index;
use crate::tui::scroll::sanitize;

/// The columns before a field's text: the `> ` prompt, or its blank twin.
pub const PROMPT_WIDTH: usize = 2;

/// The columns kept free after the text, so that the cursor at the end of a
/// full row still has a cell inside the field.
pub const FIELD_RIGHT_MARGIN: usize = 1;

/// The columns a field's text may take in an area `area_width` wide.
pub fn text_width(area_width: usize) -> usize {
    area_width
        .saturating_sub(PROMPT_WIDTH + FIELD_RIGHT_MARGIN)
        .max(1)
}

/// How many cells a character takes. The one place that counts width: a
/// switch to `unicode-width` is a change here only.
pub fn cell_width(_character: char) -> usize {
    1
}

/// The width a field's text was last wrapped to; `0` before it was drawn.
/// Every value compares equal, so it never makes two fields differ.
#[derive(Debug, Clone, Default)]
pub struct WrapWidth(std::cell::Cell<usize>);

impl WrapWidth {
    /// Remembers the width of the last drawing.
    pub fn set(&self, width: usize) {
        self.0.set(width);
    }

    /// The width to wrap to now: the last drawn one, or no wrapping at all
    /// (the logical lines) before anything was drawn.
    pub fn get(&self) -> usize {
        match self.0.get() {
            0 => usize::MAX,
            width => width,
        }
    }
}

impl PartialEq for WrapWidth {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for WrapWidth {}

/// The first row of a field's window as last drawn, so that a field taller
/// than its window moves only when the cursor leaves it. Every value compares
/// equal, like [`WrapWidth`].
#[derive(Debug, Clone, Default)]
pub struct WindowRow(std::cell::Cell<usize>);

impl WindowRow {
    /// The first row drawn last time.
    pub fn get(&self) -> usize {
        self.0.get()
    }

    /// Remembers the first row of the last drawing.
    pub fn set(&self, row: usize) {
        self.0.set(row);
    }
}

impl PartialEq for WindowRow {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for WindowRow {}

/// A text laid out in rows of at most `width` cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    /// The rows, as drawn (a tab is already spaces).
    pub rows: Vec<String>,
    /// The first drawn character of each row, counting from the start of the
    /// drawn text (a `\n` takes one).
    pub starts: Vec<usize>,
    /// The row the cursor is on.
    pub cursor_row: usize,
    /// The cell of the cursor in that row.
    pub cursor_col: usize,
    /// For each character of the value, and one past its end: how many
    /// drawn characters come before it.
    pub offsets: Vec<usize>,
}

/// Lays `value` out in rows of `width` cells, the cursor at the character
/// `cursor`. With a `cursor`, a cursor at the end of a full row gets a row of
/// its own; with none, no such row is added.
pub fn wrap(value: &str, cursor: Option<usize>, width: usize) -> Wrapped {
    let width = width.max(1);
    let drawn = sanitize(value);
    let offsets = drawn_offsets(value);
    let cursor_at = cursor.map(|cursor| offsets[cursor.min(offsets.len() - 1)]);
    let mut rows = Vec::new();
    let mut starts = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    let mut consumed = 0;
    for line in drawn.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        let chunks = chunk_by_cells(&chars, width);
        let last = chunks.len() - 1;
        for (position, chunk) in chunks.iter().enumerate() {
            let length = chunk.len();
            if let Some(at) = cursor_at {
                let inside = at >= consumed && at < consumed + length;
                let at_line_end = position == last && at == consumed + length;
                if inside || at_line_end {
                    cursor_row = rows.len();
                    cursor_col = cells(&chunk[..at - consumed]);
                }
            }
            starts.push(consumed);
            rows.push(chunk.iter().collect::<String>());
            consumed += length;
        }
        // The cursor at the end of a full row needs a cell of its own.
        let needs_own_row =
            cursor_at == Some(consumed) && cells(chunks[last]) >= width && !chars.is_empty();
        if needs_own_row {
            cursor_row = rows.len();
            cursor_col = 0;
            starts.push(consumed);
            rows.push(String::new());
        }
        consumed += 1;
    }
    Wrapped {
        rows,
        starts,
        cursor_row,
        cursor_col,
        offsets,
    }
}

fn cells(chars: &[char]) -> usize {
    chars.iter().map(|&character| cell_width(character)).sum()
}

fn chunk_by_cells(chars: &[char], width: usize) -> Vec<&[char]> {
    if chars.is_empty() {
        return vec![chars];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, &character) in chars.iter().enumerate() {
        let wide = cell_width(character);
        if used + wide > width && index > start {
            chunks.push(&chars[start..index]);
            start = index;
            used = 0;
        }
        used += wide;
    }
    chunks.push(&chars[start..]);
    chunks
}

/// For each character of `value`, and one past the end: the drawn characters
/// before it. The drawing turns a tab into spaces, so the cursor counts what
/// the text before it draws as, not its characters.
fn drawn_offsets(value: &str) -> Vec<usize> {
    // An escape sequence swallows the characters after it, so a prefix has to
    // be drawn whole. Rare, and the only case that is not one pass.
    if value.contains('\u{1b}') {
        let count = value.chars().count();
        return (0..=count)
            .map(|character| {
                let byte = char_byte_index(value, character);
                sanitize(&value[..byte]).chars().count()
            })
            .collect();
    }
    const TAB_WIDTH: usize = 4;
    let mut offsets = Vec::with_capacity(value.len() + 1);
    let mut drawn = 0;
    let mut column = 0;
    offsets.push(0);
    for character in value.chars() {
        let width = match character {
            '\n' => {
                column = 0;
                1
            }
            '\t' => {
                let width = TAB_WIDTH - (column % TAB_WIDTH);
                column += width;
                width
            }
            other => {
                let width = sanitize(other.encode_utf8(&mut [0; 4])).chars().count();
                column += width;
                width
            }
        };
        drawn += width;
        offsets.push(drawn);
    }
    offsets
}

impl Wrapped {
    /// The character of the value nearest to the drawn position `drawn`.
    fn character_at(&self, drawn: usize) -> usize {
        self.offsets
            .iter()
            .position(|&offset| offset >= drawn)
            .unwrap_or(self.offsets.len() - 1)
    }

    /// The character the cursor lands on when it moves `delta` drawn rows
    /// up or down and keeps its cell. `None` at the first or the last row.
    pub fn vertical(&self, delta: isize) -> Option<usize> {
        let target = self.cursor_row as isize + delta;
        if target < 0 || target >= self.rows.len() as isize {
            return None;
        }
        let target = target as usize;
        let length = self.rows[target].chars().count();
        Some(self.character_at(self.starts[target] + self.cursor_col.min(length)))
    }

    /// Whether the cursor is on the first drawn row.
    pub fn on_first_row(&self) -> bool {
        self.cursor_row == 0
    }

    /// Whether the cursor is on the last drawn row.
    pub fn on_last_row(&self) -> bool {
        self.cursor_row + 1 >= self.rows.len()
    }
}

/// The first row to draw so that the cursor row is in a window of `height`
/// rows, given the row the window started at before (`scroll`).
pub fn window_start(cursor_row: usize, height: usize, scroll: usize) -> usize {
    let height = height.max(1);
    if cursor_row < scroll {
        cursor_row
    } else if cursor_row >= scroll + height {
        cursor_row + 1 - height
    } else {
        scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_count_what_is_drawn() {
        let offsets = drawn_offsets("a\tb\nc");
        assert_eq!(offsets, vec![0, 1, 4, 5, 6, 7]);
        let escaped = "x\u{1b}[31my";
        let last = *drawn_offsets(escaped).last().unwrap();
        assert_eq!(last, sanitize(escaped).chars().count());
    }

    #[test]
    fn a_large_value_wraps_at_once() {
        let value = "ab\t".repeat(100_000);
        let wrapped = wrap(&value, Some(value.chars().count()), 40);
        assert!(wrapped.rows.len() > 1000);
    }

    #[test]
    fn the_window_stays_while_the_cursor_is_in_it() {
        assert_eq!(window_start(5, 4, 3), 3);
        assert_eq!(window_start(2, 4, 3), 2);
        assert_eq!(window_start(9, 4, 3), 6);
    }

    #[test]
    fn long_text_wraps_after_width_cells() {
        let wrapped = wrap("abcdefgh", None, 3);
        assert_eq!(wrapped.rows, ["abc", "def", "gh"]);
    }

    #[test]
    fn text_in_a_form_of_forty_columns_ends_before_the_last_cell() {
        let width = text_width(40);
        assert_eq!(PROMPT_WIDTH + width, 39);
        let wrapped = wrap(&"x".repeat(100), None, width);
        assert!(wrapped.rows.iter().all(|row| row.chars().count() <= width));
    }

    #[test]
    fn cursor_at_the_end_of_a_full_row_gets_its_own_row() {
        let wrapped = wrap("abc", Some(3), 3);
        assert_eq!(wrapped.rows, ["abc", ""]);
        assert_eq!((wrapped.cursor_row, wrapped.cursor_col), (1, 0));
        let without = wrap("abc", None, 3);
        assert_eq!(without.rows, ["abc"]);
    }

    #[test]
    fn cursor_inside_moves_no_character() {
        let wrapped = wrap("abcdef", Some(2), 3);
        assert_eq!(wrapped.rows, ["abc", "def"]);
        assert_eq!((wrapped.cursor_row, wrapped.cursor_col), (0, 2));
    }

    #[test]
    fn empty_lines_are_empty_rows() {
        let wrapped = wrap("a\n\nb\n", Some(5), 5);
        assert_eq!(wrapped.rows, ["a", "", "b", ""]);
        assert_eq!((wrapped.cursor_row, wrapped.cursor_col), (3, 0));
    }

    #[test]
    fn a_tab_before_the_cursor_moves_it_four_cells() {
        let wrapped = wrap("\tx", Some(1), 20);
        assert_eq!(wrapped.rows, ["    x"]);
        assert_eq!(wrapped.cursor_col, 4);
    }

    #[test]
    fn width_zero_and_one_do_not_loop() {
        assert_eq!(wrap("ab", Some(1), 0).rows.len(), 2);
        assert_eq!(wrap("ab", Some(1), 1).rows.len(), 2);
    }

    #[test]
    fn the_window_keeps_the_cursor_row_in_view() {
        assert_eq!(window_start(9, 3, 0), 7);
        assert_eq!(window_start(1, 3, 5), 1);
        assert_eq!(window_start(6, 3, 5), 5);
    }

    #[test]
    fn up_and_down_walk_the_drawn_rows() {
        let wrapped = wrap("abcdefgh", Some(4), 3);
        assert_eq!((wrapped.cursor_row, wrapped.cursor_col), (1, 1));
        assert_eq!(wrapped.vertical(-1), Some(1));
        assert_eq!(wrapped.vertical(1), Some(7));
        let top = wrap("abcdefgh", Some(1), 3);
        assert_eq!(top.vertical(-1), None);
        assert!(top.on_first_row());
        let bottom = wrap("abcdefgh", Some(8), 3);
        assert!(bottom.on_last_row());
        assert_eq!(bottom.vertical(1), None);
    }

    #[test]
    fn home_and_end_agree_with_the_drawn_rows() {
        // A logical line over three drawn rows: the cursor of Home/End is on
        // the logical line, and the layout puts it on the row it lands on.
        let text = "abcdefghi\nxyz";
        let end_of_first = wrap(text, Some(9), 3);
        assert_eq!((end_of_first.cursor_row, end_of_first.cursor_col), (3, 0));
        let start = wrap(text, Some(0), 3);
        assert_eq!((start.cursor_row, start.cursor_col), (0, 0));
    }
}
