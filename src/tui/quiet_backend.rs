//! A backend that writes nothing for a frame that changed nothing.
//!
//! The loop redraws on a timer ([`super::TICK`]) so relative ages stay
//! honest. Plain `ratatui` sends bytes for every such frame even when no cell
//! changed: the colour and attribute resets after the cell diff, and the
//! cursor hide. Those bytes go to a terminal that may not read them — a
//! terminal window in the background, a browser tab, a remote console under
//! flow control. The pty buffer fills, the write blocks, and the whole loop
//! blocks with it: keys pile up unseen and are applied at once only when the
//! terminal reads again. A frame without changes stays off the wire, so an
//! idle screen leaves the terminal nothing to fall behind on.

use std::io;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Wraps `B` and skips the writes of a frame that changes nothing on screen:
/// an empty cell diff and a cursor already in the asked state.
#[derive(Debug)]
pub struct QuietBackend<B> {
    inner: B,
    /// What the cursor was last set to; `None` until the backend sets it.
    cursor_hidden: Option<bool>,
}

impl<B> QuietBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            cursor_hidden: None,
        }
    }
}

impl<B: Backend<Error = io::Error>> Backend for QuietBackend<B> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        if self.cursor_hidden == Some(true) {
            return Ok(());
        }
        self.inner.hide_cursor()?;
        self.cursor_hidden = Some(true);
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if self.cursor_hidden == Some(false) {
            return Ok(());
        }
        self.inner.show_cursor()?;
        self.cursor_hidden = Some(false);
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    use super::*;

    /// What reached the terminal, readable while the terminal owns the writer.
    #[derive(Clone, Default)]
    struct Wire(Rc<RefCell<Vec<u8>>>);

    impl Write for Wire {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn draw(terminal: &mut Terminal<QuietBackend<CrosstermBackend<Wire>>>, text: &str) {
        terminal
            .draw(|frame| frame.render_widget(text, frame.area()))
            .unwrap();
    }

    #[test]
    fn an_unchanged_frame_writes_nothing_and_a_changed_one_does() {
        let wire = Wire::default();
        let mut terminal = Terminal::with_options(
            QuietBackend::new(CrosstermBackend::new(wire.clone())),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 20, 2)),
            },
        )
        .expect("a fixed viewport needs no terminal");
        let written = || wire.0.borrow().len();

        draw(&mut terminal, "czekam");
        let after_first = written();
        assert!(after_first > 0, "the first frame reaches the terminal");

        draw(&mut terminal, "czekam");
        assert_eq!(
            written(),
            after_first,
            "a frame that changed nothing must not write to the terminal"
        );

        draw(&mut terminal, "gotowe");
        assert!(written() > after_first, "a change reaches the terminal");
    }
}
