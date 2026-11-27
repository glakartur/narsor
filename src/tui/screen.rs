//! The screen: frames are drawn on the loop and written on their own thread.
//!
//! A terminal does not always read what it is sent. A window in the
//! background, a browser tab or a remote console under flow control can stop
//! reading for minutes; the pty buffer fills and a write blocks. Were the loop
//! to write, it would block with it: keys would pile up unseen and be applied
//! all at once when the terminal reads again. So the loop only draws: a frame
//! becomes bytes in memory, and a writer thread carries them to the terminal.
//!
//! While the writer is still busy with an earlier frame, the bytes of the
//! next frames wait and go together once it is free — `ratatui` diffs every
//! frame against the one before, so no frame may be dropped on its own. Past
//! [`MAX_PENDING`] the waiting bytes are dropped as a whole and the screen is
//! cleared and drawn again in full: the memory stays bounded however long the
//! terminal does not read.
//!
//! A mode change (mouse reporting on or off) goes through the same writer, so
//! its bytes never cut into a frame's, but it waits apart from the frames
//! ([`Screen::queue_control`]): a full redraw would not repeat it.

use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};
use tokio::sync::Notify;

use super::quiet_backend::QuietBackend;

/// How many bytes may wait for a terminal that does not read before they are
/// replaced by one full redraw.
pub const MAX_PENDING: usize = 256 * 1024;

/// The terminal the loop draws on: its output is collected in memory.
type FrameTerminal = Terminal<QuietBackend<CrosstermBackend<FrameSink>>>;

/// Draws frames without ever waiting for the terminal to read them.
pub struct Screen {
    terminal: FrameTerminal,
    pending: Arc<Mutex<Vec<u8>>>,
    control: Vec<u8>,
    writer: Writer,
}

impl Screen {
    /// A full screen writing to `out` from its own thread.
    pub fn new<W: Write + Send + 'static>(out: W) -> io::Result<Self> {
        Self::build(out, QuietBackend::new)
    }

    /// [`Self::new`] on a terminal of `size`, never asked for its own.
    #[cfg(test)]
    fn of_size<W: Write + Send + 'static>(out: W, size: ratatui::layout::Size) -> io::Result<Self> {
        Self::build(out, |backend| QuietBackend::new(backend).with_size(size))
    }

    fn build<W: Write + Send + 'static>(
        out: W,
        backend: impl FnOnce(CrosstermBackend<FrameSink>) -> QuietBackend<CrosstermBackend<FrameSink>>,
    ) -> io::Result<Self> {
        let pending = Arc::new(Mutex::new(Vec::new()));
        let sink = FrameSink(Arc::clone(&pending));
        let terminal = Terminal::new(backend(CrosstermBackend::new(sink)))?;
        Ok(Self {
            terminal,
            pending,
            control: Vec::new(),
            writer: Writer::spawn(out)?,
        })
    }

    /// Draws a frame and hands what changed to the writer, if it is free.
    /// Never waits for the terminal. An error is the writer's last failure,
    /// or the frame's own.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.writer.take_error()?;
        if self.writer.is_busy() && lock(&self.pending).len() > MAX_PENDING {
            lock(&self.pending).clear();
            // The terminal gets a cleared screen and the whole next frame,
            // not the diffs it missed.
            self.terminal.clear()?;
        }
        self.terminal.draw(render)?;
        self.hand_over();
        Ok(())
    }

    /// Signalled each time the writer finishes a write: the loop wakes then to
    /// hand over what waited meanwhile.
    pub fn written(&self) -> Arc<Notify> {
        Arc::clone(&self.writer.shared.written)
    }

    /// Waits up to `timeout` for the writer to finish what it was given.
    /// Whether it did.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        self.writer.wait_idle(timeout)
    }

    /// Queues bytes that change a terminal mode (mouse reporting) to go
    /// before the next frame. Unlike a frame's bytes they are never dropped
    /// for a terminal that does not read: a full redraw does not repeat them.
    pub fn queue_control(&mut self, bytes: &[u8]) {
        self.control.extend_from_slice(bytes);
        self.hand_over();
    }

    fn hand_over(&mut self) {
        let mut pending = lock(&self.pending);
        if (pending.is_empty() && self.control.is_empty()) || self.writer.is_busy() {
            return;
        }
        let mut batch = std::mem::take(&mut self.control);
        batch.append(&mut pending);
        self.writer.offer(&mut batch);
    }
}

impl Drop for Screen {
    /// Gives the writer a moment to finish, then lets it go: a terminal that
    /// does not read must not keep the process from ending.
    fn drop(&mut self) {
        self.writer.wait_idle(Duration::from_secs(1));
        self.writer.close();
    }
}

/// Collects what the terminal backend writes for a frame.
struct FrameSink(Arc<Mutex<Vec<u8>>>);

impl Write for FrameSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The thread that writes to the terminal.
struct Writer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    written: Arc<Notify>,
}

#[derive(Default)]
struct State {
    /// Bytes given to the thread, not yet taken.
    outgoing: Option<Vec<u8>>,
    /// From an offer until its bytes are written.
    busy: bool,
    closed: bool,
    error: Option<io::Error>,
}

impl Writer {
    fn spawn<W: Write + Send + 'static>(out: W) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            written: Arc::new(Notify::new()),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("narsor-tui-writer".to_string())
            .spawn(move || write_until_closed(out, &thread_shared))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn is_busy(&self) -> bool {
        lock(&self.shared.state).busy
    }

    /// Gives `bytes` to the thread unless it is busy. Whether it took them.
    fn offer(&self, bytes: &mut Vec<u8>) -> bool {
        let mut state = lock(&self.shared.state);
        if state.busy || state.closed {
            return false;
        }
        state.outgoing = Some(std::mem::take(bytes));
        state.busy = true;
        self.shared.changed.notify_all();
        true
    }

    fn take_error(&self) -> io::Result<()> {
        match lock(&self.shared.state).error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        while state.busy {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        true
    }

    /// Stops the thread. Joined only when it is not stuck in a write.
    fn close(&mut self) {
        let idle = {
            let mut state = lock(&self.shared.state);
            state.closed = true;
            self.shared.changed.notify_all();
            !state.busy
        };
        if let Some(thread) = self.thread.take() {
            if idle {
                let _ = thread.join();
            }
        }
    }
}

fn write_until_closed(mut out: impl Write, shared: &Shared) {
    loop {
        let bytes = {
            let mut state = lock(&shared.state);
            loop {
                if let Some(bytes) = state.outgoing.take() {
                    break bytes;
                }
                if state.closed {
                    return;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        let result = out.write_all(&bytes).and_then(|()| out.flush());
        let failed = {
            let mut state = lock(&shared.state);
            state.busy = false;
            let failed = result.is_err();
            if let Err(error) = result {
                state.error = Some(error);
                state.closed = true;
            }
            shared.changed.notify_all();
            failed
        };
        shared.written.notify_one();
        if failed {
            return;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Size;

    use super::*;

    /// A terminal that reads only while its gate is open.
    #[derive(Clone, Default)]
    struct GatedTerminal(Arc<Gate>);

    #[derive(Default)]
    struct Gate {
        /// Whether it reads, and what it read.
        state: Mutex<(bool, Vec<u8>)>,
        opened: Condvar,
    }

    impl GatedTerminal {
        fn set_reading(&self, reading: bool) {
            lock(&self.0.state).0 = reading;
            self.0.opened.notify_all();
        }

        fn received(&self) -> Vec<u8> {
            lock(&self.0.state).1.clone()
        }
    }

    impl Write for GatedTerminal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut state = lock(&self.0.state);
            while !state.0 {
                state = self.0.opened.wait(state).unwrap();
            }
            state.1.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn screen(terminal: &GatedTerminal, width: u16) -> Screen {
        Screen::of_size(terminal.clone(), Size::new(width, 1)).unwrap()
    }

    fn show(screen: &mut Screen, text: &str) {
        screen
            .draw(|frame| frame.render_widget(text, frame.area()))
            .unwrap();
    }

    /// What a one-row terminal shows after `bytes`: enough of ANSI for what
    /// the backend sends — cursor moves, line and screen clears, ASCII.
    fn shown(bytes: &[u8], width: usize) -> String {
        let mut row = vec![' '; width];
        let mut col = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
                let start = i + 2;
                let mut end = start;
                while !bytes[end].is_ascii_alphabetic() {
                    end += 1;
                }
                let params = std::str::from_utf8(&bytes[start..end]).unwrap();
                match bytes[end] {
                    b'H' => {
                        let column = params.split(';').nth(1).unwrap_or("1");
                        col = column.parse::<usize>().unwrap_or(1) - 1;
                    }
                    b'K' => row[col..].fill(' '),
                    b'J' if params == "2" => row.fill(' '),
                    _ => {}
                }
                i = end + 1;
            } else {
                if col < width {
                    row[col] = char::from(bytes[i]);
                }
                col += 1;
                i += 1;
            }
        }
        row.into_iter().collect::<String>().trim_end().to_string()
    }

    #[test]
    fn drawing_goes_on_while_the_terminal_does_not_read_and_it_catches_up_after() {
        let terminal = GatedTerminal::default();
        let mut screen = screen(&terminal, 20);

        // The terminal stops reading; the first write blocks in it.
        show(&mut screen, "czekam");
        for step in 0..50 {
            show(&mut screen, &format!("klawisz {step}"));
        }
        assert!(terminal.received().is_empty());

        terminal.set_reading(true);
        assert!(screen.wait_idle(Duration::from_secs(5)));
        // What waited goes with the next frame, which changes nothing more.
        show(&mut screen, "klawisz 49");
        assert!(screen.wait_idle(Duration::from_secs(5)));
        assert_eq!(shown(&terminal.received(), 20), "klawisz 49");
    }

    #[test]
    fn a_terminal_that_never_reads_costs_bounded_memory_and_gets_a_full_redraw() {
        let terminal = GatedTerminal::default();
        let mut screen = screen(&terminal, 200);
        // Half the line is drawn once and stays; the other half changes every
        // frame. The steady half reaches the terminal only in the bytes that
        // wait, so it survives their drop only through a full redraw.
        let frame = |step: usize| {
            let changing = if step.is_multiple_of(2) { "a" } else { "b" };
            format!("{}{}", "c".repeat(100), changing.repeat(100))
        };

        show(&mut screen, "start");
        let steps = 3 * MAX_PENDING / 100;
        for step in 0..steps {
            show(&mut screen, &frame(step));
        }

        terminal.set_reading(true);
        assert!(screen.wait_idle(Duration::from_secs(5)));
        show(&mut screen, &frame(steps - 1));
        assert!(screen.wait_idle(Duration::from_secs(5)));
        let received = terminal.received();
        assert!(
            received.len() < MAX_PENDING + 16 * 1024,
            "the terminal gets at most one batch, not every missed frame: {} bytes",
            received.len()
        );
        assert_eq!(shown(&received, 200), frame(steps - 1));
    }

    #[test]
    fn a_mode_change_queued_while_the_terminal_does_not_read_survives_the_dropped_frames() {
        const MODE: &[u8] = b"\x1b[?1000l";
        let terminal = GatedTerminal::default();
        let mut screen = screen(&terminal, 200);

        show(&mut screen, "start");
        screen.queue_control(MODE);
        for step in 0..3 * MAX_PENDING / 100 {
            show(&mut screen, &step.to_string().repeat(200));
        }

        terminal.set_reading(true);
        assert!(screen.wait_idle(Duration::from_secs(5)));
        show(&mut screen, "end");
        assert!(screen.wait_idle(Duration::from_secs(5)));
        let received = terminal.received();
        assert!(
            received.windows(MODE.len()).any(|window| window == MODE),
            "the mode change reached the terminal"
        );
    }
}
