//! The text of one field and the cursor in it.
//!
//! The cursor is an index in characters, not bytes, and `Home`/`End` work on
//! the logical line (between `\n`), not on the wrapped row.

use crate::tui::input_form::char_byte_index;

/// The value of a text field and where the cursor stands in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextBuffer {
    value: String,
    cursor: usize,
}

impl TextBuffer {
    /// A buffer holding `value`, the cursor after its last character.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    /// The text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The cursor, in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// How many characters the text has.
    pub fn len(&self) -> usize {
        self.value.chars().count()
    }

    /// Whether the text is empty.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Replaces the text; the cursor goes to its end.
    pub fn set(&mut self, value: impl Into<String>) {
        *self = Self::new(value);
    }

    /// Replaces the text and puts the cursor at `cursor`, kept inside it.
    pub fn restore(&mut self, value: String, cursor: usize) {
        self.value = value;
        self.cursor = cursor.min(self.len());
    }

    /// Puts the cursor at `cursor`, kept inside the text.
    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.len());
    }

    /// Inserts one character at the cursor.
    pub fn insert(&mut self, character: char) {
        self.insert_str(character.encode_utf8(&mut [0; 4]));
    }

    /// Inserts text at the cursor, the cursor after it.
    pub fn insert_str(&mut self, text: &str) {
        let at = char_byte_index(&self.value, self.cursor);
        self.value.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// Removes the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.remove(self.cursor - 1);
        self.cursor -= 1;
    }

    /// Removes the character under the cursor; at the end it does nothing.
    pub fn delete(&mut self) {
        if self.cursor < self.len() {
            self.remove(self.cursor);
        }
    }

    /// Moves the cursor one character left.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the cursor one character right.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    /// Moves the cursor to the start of its logical line.
    pub fn home(&mut self) {
        self.cursor = self.line_bounds().0;
    }

    /// Moves the cursor to the end of its logical line.
    pub fn end(&mut self) {
        self.cursor = self.line_bounds().1;
    }

    /// The first character of the logical line the cursor is on, and the
    /// position just after its last one (the `\n`, or the end of the text).
    pub fn line_bounds(&self) -> (usize, usize) {
        let mut start = 0;
        for line in self.value.split('\n') {
            let end = start + line.chars().count();
            if self.cursor <= end {
                return (start, end);
            }
            start = end + 1;
        }
        (start, start)
    }

    fn remove(&mut self, character: usize) {
        let start = char_byte_index(&self.value, character);
        let end = char_byte_index(&self.value, character + 1);
        self.value.replace_range(start..end, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str, cursor: usize) -> TextBuffer {
        let mut buffer = TextBuffer::new(value);
        buffer.set_cursor(cursor);
        buffer
    }

    #[test]
    fn delete_removes_the_character_under_the_cursor() {
        let mut buffer = at("abc", 1);
        buffer.delete();
        assert_eq!((buffer.value(), buffer.cursor()), ("ac", 1));
    }

    #[test]
    fn delete_at_the_end_changes_nothing() {
        let mut buffer = at("abc", 3);
        buffer.delete();
        assert_eq!((buffer.value(), buffer.cursor()), ("abc", 3));
    }

    #[test]
    fn home_and_end_stay_on_the_logical_line() {
        let mut buffer = at("ab\ncd", 4);
        buffer.home();
        assert_eq!(buffer.cursor(), 3);
        buffer.end();
        assert_eq!(buffer.cursor(), 5);
        let mut buffer = at("ab\ncd", 1);
        buffer.end();
        assert_eq!(buffer.cursor(), 2);
        buffer.home();
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn home_and_end_on_edge_cases() {
        let mut empty = at("", 0);
        empty.home();
        empty.end();
        assert_eq!(empty.cursor(), 0);
        // On the newline itself the line is the one that ends there.
        let mut buffer = at("ab\ncd", 2);
        buffer.home();
        assert_eq!(buffer.cursor(), 0);
        // After a trailing newline the line is empty.
        let mut buffer = at("ab\n", 3);
        buffer.home();
        assert_eq!(buffer.cursor(), 3);
        buffer.end();
        assert_eq!(buffer.cursor(), 3);
    }

    #[test]
    fn multibyte_text_keeps_its_boundaries() {
        let mut buffer = at("zażółć", 3);
        buffer.delete();
        assert_eq!(buffer.value(), "zażłć");
        buffer.home();
        assert_eq!(buffer.cursor(), 0);
        buffer.end();
        assert_eq!(buffer.cursor(), 5);
        buffer.backspace();
        assert_eq!(buffer.value(), "zażł");
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut buffer = at("abc", 0);
        buffer.backspace();
        assert_eq!((buffer.value(), buffer.cursor()), ("abc", 0));
    }

    #[test]
    fn insert_puts_text_at_the_cursor() {
        let mut buffer = at("ac", 1);
        buffer.insert('b');
        assert_eq!((buffer.value(), buffer.cursor()), ("abc", 2));
        buffer.insert_str("xy");
        assert_eq!((buffer.value(), buffer.cursor()), ("abxyc", 4));
    }
}
