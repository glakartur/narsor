//! The full-screen terminal interface.
//!
//! A windowed, `lazygit`-shaped console: the alternate screen is taken over
//! entirely, bordered panels fill it, one of them holds focus, and a bar of
//! context-sensitive shortcuts sits at the bottom. Navigation is vim-flavoured
//! (`j`/`k`, `g`/`G`, `q`) and nothing depends on a mouse.
//!
//! There is one mode and one application, [`task_app::TaskApp`]: the tasks of
//! the control plane (`/control/v1`) of whichever backend the configuration
//! names — the local daemon, launched when none runs, or a remote server. This
//! module is the terminal runner state and the event loop that joins the
//! application to the terminal: keys and pastes in, control-plane events in,
//! a redraw out.
//!
//! # Why the terminal is restored twice over
//!
//! Raw mode, the alternate screen, the keyboard enhancement flags and
//! bracketed paste are terminal state that a crashing program does not clean
//! up by itself. An operator whose first panic leaves
//! them with an unusable console will not trust the tool again, so restoration
//! is arranged on two independent paths: [`TerminalGuard`] restores on every
//! exit from [`run`], normal or unwinding, and [`install_panic_hook`] restores
//! from inside the panic hook, before the default hook prints anything. Both
//! go through [`restore_terminal`], which is idempotent and counts its calls so
//! that a test can prove the path actually ran.
//!
//! # An unreachable server does not keep the interface closed
//!
//! The first read happens after the first draw, so the screen opens with
//! "łączę z …" and the application retries on its own schedule
//! ([`task_app::TaskApp::start_refresh_if_due`]). No read blocks the loop: it
//! runs as a job ([`task_app::background`]), the screen shows the loading
//! marker in the place of its data, and keys — quit among them — work
//! meanwhile. The event stream is opened by
//! [`task_app::EventFeed::ensure_open`] only once a read gave the application
//! a cursor, and resumes from it after every drop.

pub mod chat;
pub mod chooser;
pub mod connection;
pub mod conversation;
pub mod event_text;
pub mod failure_form;
pub mod form;
pub mod input_form;
pub mod keymap;
pub mod paths;
pub mod queues;
pub mod quiet_backend;
pub mod refresh;
pub mod screen;
pub mod scroll;
pub mod selection;
pub mod spinner;
pub mod stepped;
pub mod style;
pub mod task_app;
pub mod transcript;

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event as TerminalEvent,
    KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use tokio::sync::mpsc;

use crate::api::ApiClient;

use screen::Screen;
use style::Styles;
use task_app::{detail, layout, EventFeed, TaskApp};

/// The Polish form of a noun after `count`: one, a few, many (for example
/// `["zadanie", "zadania", "zadań"]`).
pub(crate) fn plural(count: usize, forms: [&'static str; 3]) -> &'static str {
    match (count, count % 10, count % 100) {
        (1, _, _) => forms[0],
        (_, 2..=4, tens) if !(12..=14).contains(&tens) => forms[1],
        _ => forms[2],
    }
}

/// How long the key reader blocks before looking at the stop flag again.
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// How long the loop waits for something to happen before redrawing anyway.
///
/// A redraw on a timer keeps relative timestamps and a long-running action's
/// panel honest even when nothing arrives.
const TICK: Duration = Duration::from_millis(250);

/// How many times [`restore_terminal`] has run in this process.
static RESTORE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Whether the keyboard enhancement flags were pushed and still wait to be
/// popped; the terminal keeps them after the process ends if nobody pops.
static ENHANCEMENT_ARMED: AtomicBool = AtomicBool::new(false);

/// Whether bracketed paste was switched on and still waits to be switched
/// off; a shell would otherwise see its pastes wrapped in `200~` / `201~`.
static PASTE_ARMED: AtomicBool = AtomicBool::new(false);

/// Whether mouse reporting was switched on in this session. It stays set when
/// the operator switches the reporting off with a key: that change goes
/// through the screen's writer and may not have reached the terminal when the
/// process ends, so the restore path switches the reporting off whenever it
/// was on once.
static MOUSE_ARMED: AtomicBool = AtomicBool::new(false);

/// Switches on reporting of button presses and the wheel, in the SGR
/// encoding. Not drag (`?1002`) nor every move (`?1003`), which crossterm's
/// `EnableMouseCapture` also asks for: each move would be an event and a
/// redraw, and nothing here uses them.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";

/// Guards against chaining the panic hook onto itself.
static PANIC_HOOK: Once = Once::new();

/// Installs a panic hook that puts the terminal back before anything is
/// printed.
///
/// Chained, not replacing: the previously installed hook still runs, so the
/// usual panic message and `RUST_BACKTRACE` output survive — they just land on
/// a terminal that can display them. Installing twice is a no-op.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

/// Leaves the alternate screen, disables raw mode and bracketed paste, and
/// shows the cursor.
///
/// Every failure is deliberately swallowed: this runs on the way out, often
/// from a panic hook, and there is nowhere left to report to. It is safe to
/// call when no terminal was ever entered, which is what makes it usable from
/// a test.
pub fn restore_terminal() {
    restore_terminal_to(&mut io::stdout());
}

/// [`restore_terminal`], with the escape sequences written to `out` — the
/// seam a test reads them from.
pub fn restore_terminal_to(out: &mut impl Write) {
    if ENHANCEMENT_ARMED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if PASTE_ARMED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, DisableBracketedPaste);
    }
    if MOUSE_ARMED.swap(false, Ordering::SeqCst) {
        // Switches off every mouse mode, whichever was on.
        let _ = execute!(out, DisableMouseCapture);
    }
    keymap::set_modifiers_distinguished(false);
    let _ = disable_raw_mode();
    let _ = execute!(out, LeaveAlternateScreen, Show);
    RESTORE_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// How many times the restore path has run — the observable proof a test needs.
pub fn restore_count() -> usize {
    RESTORE_COUNT.load(Ordering::SeqCst)
}

/// An entered terminal that restores itself when it goes out of scope.
///
/// Covers the normal exit and the unwinding one; [`install_panic_hook`] covers
/// the panic that never reaches this frame.
pub struct TerminalGuard {
    /// `None` only while dropping: the screen's last writes must end before
    /// the terminal is restored.
    screen: Option<Screen>,
}

impl TerminalGuard {
    /// Takes over the screen: raw mode, alternate screen, hidden cursor, and
    /// bracketed paste where the terminal has it.
    ///
    /// A failure part way gives back what was already taken: no guard exists
    /// yet to do it on drop, so raw mode or bracketed paste would outlive the
    /// error.
    pub fn enter() -> io::Result<Self> {
        Self::take_over().inspect_err(|_| restore_terminal())
    }

    fn take_over() -> io::Result<Self> {
        let stdout = take_screen()?;
        let screen = Screen::new(stdout)?;
        Ok(Self {
            screen: Some(screen),
        })
    }

    /// The screen to draw on.
    pub fn screen(&mut self) -> &mut Screen {
        self.screen
            .as_mut()
            .expect("the screen lives as long as its guard")
    }
}

/// Raw mode, the alternate screen, a hidden cursor, and bracketed paste and
/// modifier reporting where the terminal has them.
fn take_screen() -> io::Result<Stdout> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    arm_bracketed_paste(&mut stdout);
    arm_modifier_reporting(&mut stdout);
    arm_mouse_capture(&mut stdout);
    Ok(stdout)
}

/// Switches mouse reporting on: buttons and the wheel only.
///
/// A best effort, as bracketed paste: a terminal that does not report the
/// mouse leaves the keys, which do everything the mouse does.
pub fn arm_mouse_capture(out: &mut impl Write) {
    if out.write_all(MOUSE_ON).and_then(|()| out.flush()).is_ok() {
        MOUSE_ARMED.store(true, Ordering::SeqCst);
    }
}

/// The bytes that switch mouse reporting on or off while the interface runs
/// (`P`). They go through the screen's writer, never straight to the terminal.
fn mouse_capture_bytes(on: bool) -> Vec<u8> {
    if on {
        return MOUSE_ON.to_vec();
    }
    let mut bytes = Vec::new();
    let _ = execute!(bytes, DisableMouseCapture);
    bytes
}

/// Asks the terminal whether it reports modifiers on keys that print as
/// `Enter`, and when it does, switches that reporting on: then `Shift+Enter`
/// and `Alt+Enter` arrive as themselves and can be a new line.
///
/// Asked once, at start. A terminal that does not answer, or answers with an
/// error, is a terminal without it: only `Ctrl+J` is promised then, and
/// nothing is left switched on that [`restore_terminal`] would have to undo.
fn arm_modifier_reporting(stdout: &mut Stdout) {
    let supported = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let armed = supported
        && execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    if armed {
        ENHANCEMENT_ARMED.store(true, Ordering::SeqCst);
    }
    keymap::set_modifiers_distinguished(armed);
}

/// Switches bracketed paste on, so a paste arrives as one event and not as
/// keys — a pasted new line would otherwise be an `Enter` that sends.
///
/// A best effort: a terminal that refuses is a terminal without it, where a
/// paste still comes as keys, and nothing is left for [`restore_terminal`]
/// to switch off.
pub fn arm_bracketed_paste(out: &mut impl Write) {
    if execute!(out, EnableBracketedPaste).is_ok() {
        PASTE_ARMED.store(true, Ordering::SeqCst);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        drop(self.screen.take());
        restore_terminal();
    }
}

/// Runs the interface over `client` until the operator quits.
///
/// Keys are read on a dedicated thread because `crossterm` reads block, and
/// are forwarded over a channel so the loop can wait on keys, control-plane
/// events and the next due read at once. Frames are written on another
/// thread too ([`screen`]): a terminal that stops reading never stops the
/// loop. `connection_name` is what the header
/// calls the server until the server says its own name.
pub async fn run(
    client: ApiClient,
    connection_name: String,
    refresh_interval: Option<Duration>,
) -> io::Result<()> {
    install_panic_hook();

    let styles = Styles::from_env();
    let mut app = TaskApp::new(client, connection_name, refresh_interval);
    app.set_spinner(spinner::Spinner::from_env());
    app.detail_mut()
        .set_hyperlinks(detail::hyperlinks_supported(|name| {
            std::env::var(name).ok()
        }));
    let mut feed = EventFeed::new(app.client().clone());

    let mut guard = TerminalGuard::enter()?;
    let written = guard.screen().written();
    let stop = Arc::new(AtomicBool::new(false));
    let (keys_tx, mut keys_rx) = mpsc::unbounded_channel();
    let reader_stop = Arc::clone(&stop);
    let reader = std::thread::spawn(move || read_terminal_events(&keys_tx, &reader_stop));

    // The first read comes after the first draw: an unreachable server shows
    // "łączę z …" instead of a blank terminal.
    let mut first_read_pending = true;
    // What the terminal was last told about mouse reporting: on since
    // `take_screen`.
    let mut mouse_on = true;

    while !app.should_quit() {
        guard
            .screen()
            .draw(|frame| layout::render(frame, &mut app, &styles, Instant::now()))?;

        if first_read_pending {
            first_read_pending = false;
            app.start_refresh();
            app.start_usage_if_due(Instant::now());
            feed.ensure_open(&app);
            continue;
        }

        let wait = wait_until_next_due(&app, Instant::now());
        tokio::select! {
            incoming = keys_rx.recv() => match incoming {
                Some(TerminalEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    let request = app.on_key(key);
                    app.dispatch(request).await;
                }
                // A paste is text for the field being typed in, never keys.
                Some(TerminalEvent::Paste(text)) => app.on_paste(&text),
                Some(TerminalEvent::Mouse(mouse)) => {
                    let request = app.on_mouse(mouse);
                    app.dispatch(request).await;
                }
                // A resize needs no state change: the next draw re-derives the
                // whole layout from the new area.
                Some(_) => {}
                None => break,
            },
            received = feed.next() => app.on_stream(received),
            // The terminal took the last frame: what was drawn meanwhile goes
            // with the next draw.
            _ = written.notified() => {}
            Some(outcome) = app.next_outcome(), if app.has_running() => app.on_outcome(outcome),
            _ = tokio::time::sleep(wait) => app.advance_animation(),
        }

        if app.mouse_captured() != mouse_on {
            mouse_on = app.mouse_captured();
            guard.screen().queue_control(&mouse_capture_bytes(mouse_on));
        }

        feed.ensure_open(&app);
        app.start_refresh_if_due(Instant::now());
        app.start_usage_if_due(Instant::now());
        app.retry_outgoing_if_due(Instant::now()).await;
        app.start_detail_reads();
        feed.ensure_open(&app);
    }

    stop.store(true, Ordering::SeqCst);
    drop(guard);
    let _ = reader.join();
    Ok(())
}

/// How long the loop may sleep: until the next due read (the list or the
/// usage limits) or resend, but no longer than [`TICK`], so relative ages and
/// the notification stay honest.
fn wait_until_next_due(app: &TaskApp, now: Instant) -> Duration {
    [
        app.next_refresh_due(),
        app.next_outgoing_due(),
        app.next_usage_due(),
    ]
    .into_iter()
    .flatten()
    .map(|due| due.saturating_duration_since(now))
    .fold(TICK, Duration::min)
}

/// Forwards terminal events until the loop asks it to stop.
fn read_terminal_events(sender: &mpsc::UnboundedSender<TerminalEvent>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match event::poll(KEY_POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(terminal_event) => {
                    if sender.send(terminal_event).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Ok(false) => {}
            Err(_) => return,
        }
    }
}
