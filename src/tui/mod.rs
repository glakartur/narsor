//! The full-screen terminal interface (U10).
//!
//! A windowed, `lazygit`-shaped console: the alternate screen is taken over
//! entirely, bordered panels fill it, one of them holds focus, and a bar of
//! context-sensitive shortcuts sits at the bottom. Navigation is vim-flavoured
//! (`j`/`k`, `g`/`G`, `q`) and nothing depends on a mouse.
//!
//! # The three layers
//!
//! * [`app`] — everything the interface *knows*: which executions exist, which
//!   one is selected, what its event stream holds, which modal is open and what
//!   the operator has typed into it. It never touches a terminal, which is why
//!   it is the layer `tests/tui_state.rs` drives.
//! * [`views`] — everything the interface *looks like*: the panel geometry and
//!   the widgets. Pure rendering, derived from [`app::App`].
//! * this module — the terminal lifecycle and the event loop that joins the
//!   two: keys in, engine events in, a redraw out.
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
//! # Events come from the engine, not from the disk
//!
//! The live stream is [`crate::engine::Engine::subscribe`] — the same seam an
//! SSE endpoint takes over later (D38). The event log is read only to backfill
//! the history of a newly selected execution. A `broadcast` receiver can fall
//! behind or be closed, and both show up in the interface as an explicit
//! indicator rather than as a view that quietly stops updating.

pub mod app;
pub mod views;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TerminalEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::engine::{AgentDefaults, Engine};
use crate::persistence::ExecutionStore;

use app::{App, Request};

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
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    /// The terminal to draw on.
    pub fn terminal(&mut self) -> &mut Tui {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Runs the interface until the operator quits.
///
/// Keys are read on a dedicated thread because `crossterm` reads block, and are
/// forwarded over a channel so the loop can wait on keys and engine events at
/// once. The engine is owned here and shared with [`App`] behind an [`Arc`].
pub async fn run(store: ExecutionStore, defaults: AgentDefaults) -> io::Result<()> {
    install_panic_hook();

    let engine = Arc::new(Engine::new(store).with_agent_defaults(defaults));
    let mut events = engine.subscribe();
    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    let mut guard = TerminalGuard::enter()?;

    let stop = Arc::new(AtomicBool::new(false));
    let (keys_tx, mut keys_rx) = mpsc::unbounded_channel();
    let reader_stop = Arc::clone(&stop);
    let reader = std::thread::spawn(move || read_terminal_events(&keys_tx, &reader_stop));

    while !app.should_quit() {
        guard.terminal().draw(|frame| views::render(frame, &app))?;

        tokio::select! {
            incoming = keys_rx.recv() => match incoming {
                Some(TerminalEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    match app.on_key(key) {
                        Request::None => {}
                        Request::Refresh => app.refresh(),
                        Request::SubmitInput => app.submit_input().await,
                        Request::ApplyTransition => app.apply_transition().await,
                        Request::RunSelected => app.run_selected().await,
                        Request::StartNewTask => app.start_new_task().await,
                    }
                }
                // A resize needs no state change: the next draw re-derives the
                // whole layout from the new area.
                Some(_) => {}
                None => app.quit(),
            },
            received = events.recv() => app.on_stream(received),
            _ = tokio::time::sleep(TICK) => {}
        }
    }

    stop.store(true, Ordering::SeqCst);
    drop(guard);
    let _ = reader.join();
    Ok(())
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
