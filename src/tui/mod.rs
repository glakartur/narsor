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
//! module is the terminal lifecycle and the event loop that joins the
//! application to the terminal: keys in, control-plane events in, a redraw out.
//!
//! # Why the terminal is restored twice over
//!
//! Raw mode and the alternate screen are process-global state that a crashing
//! program does not clean up by itself. An operator whose first panic leaves
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
//! ([`task_app::TaskApp::refresh_if_due`]). The event stream is opened by
//! [`task_app::EventFeed::ensure_open`] only once a read gave the application
//! a cursor, and resumes from it after every drop.

pub mod chat;
pub mod chooser;
pub mod connection;
pub mod conversation;
pub mod event_text;
pub mod failure_form;
pub mod input_form;
pub mod keymap;
pub mod refresh;
pub mod scroll;
pub mod selection;
pub mod style;
pub mod task_app;
pub mod transcript;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, Event as TerminalEvent, KeyEventKind, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::api::ApiClient;

use style::Styles;
use task_app::{detail, layout, EventFeed, TaskApp};

/// The terminal this interface draws on.
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

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

/// Leaves the alternate screen, disables raw mode and shows the cursor.
///
/// Every failure is deliberately swallowed: this runs on the way out, often
/// from a panic hook, and there is nowhere left to report to. It is safe to
/// call when no terminal was ever entered, which is what makes it usable from
/// a test.
pub fn restore_terminal() {
    if ENHANCEMENT_ARMED.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    keymap::set_modifiers_distinguished(false);
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
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
    terminal: Tui,
}

impl TerminalGuard {
    /// Takes over the screen: raw mode, alternate screen, hidden cursor.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        arm_modifier_reporting(&mut stdout);
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    /// The terminal to draw on.
    pub fn terminal(&mut self) -> &mut Tui {
        &mut self.terminal
    }
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

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Runs the interface over `client` until the operator quits.
///
/// Keys are read on a dedicated thread because `crossterm` reads block, and
/// are forwarded over a channel so the loop can wait on keys, control-plane
/// events and the next due read at once. `connection_name` is what the header
/// calls the server until the server says its own name.
pub async fn run(
    client: ApiClient,
    connection_name: String,
    refresh_interval: Option<Duration>,
) -> io::Result<()> {
    install_panic_hook();

    let styles = Styles::from_env();
    let mut app = TaskApp::new(client.clone(), connection_name, refresh_interval);
    app.detail_mut()
        .set_hyperlinks(detail::hyperlinks_supported(|name| {
            std::env::var(name).ok()
        }));
    let mut feed = EventFeed::new(client);

    let mut guard = TerminalGuard::enter()?;
    let stop = Arc::new(AtomicBool::new(false));
    let (keys_tx, mut keys_rx) = mpsc::unbounded_channel();
    let reader_stop = Arc::clone(&stop);
    let reader = std::thread::spawn(move || read_terminal_events(&keys_tx, &reader_stop));

    // The first read comes after the first draw: an unreachable server shows
    // "łączę z …" instead of a blank terminal.
    let mut first_read_pending = true;

    while !app.should_quit() {
        guard
            .terminal()
            .draw(|frame| layout::render(frame, &mut app, &styles, Instant::now()))?;

        if first_read_pending {
            first_read_pending = false;
            app.refresh().await;
            feed.ensure_open(&app);
            continue;
        }

        let wait = wait_until_next_due(&app, Instant::now());
        tokio::select! {
            incoming = keys_rx.recv() => match incoming {
                Some(TerminalEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    let request = app.on_key(key);
                    app.perform(request).await;
                }
                // A resize needs no state change: the next draw re-derives the
                // whole layout from the new area.
                Some(_) => {}
                None => break,
            },
            received = feed.next() => app.on_stream(received),
            _ = tokio::time::sleep(wait) => {}
        }

        feed.ensure_open(&app);
        app.refresh_if_due(Instant::now()).await;
        app.retry_outgoing_if_due(Instant::now()).await;
        app.sync_detail().await;
        feed.ensure_open(&app);
    }

    stop.store(true, Ordering::SeqCst);
    drop(guard);
    let _ = reader.join();
    Ok(())
}

/// How long the loop may sleep: until the next due read or resend, but no
/// longer than [`TICK`], so relative ages and the notification stay honest.
fn wait_until_next_due(app: &TaskApp, now: Instant) -> Duration {
    [app.next_refresh_due(), app.next_outgoing_due()]
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
