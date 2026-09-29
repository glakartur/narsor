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
//! # Events come from the daemon, not from an in-process engine (U7)
//!
//! The live stream used to be [`crate::engine::Engine::subscribe`] — an
//! in-process broadcast channel shared with an `Arc<Engine>` this module
//! constructed itself. U7 removes that construction entirely (R4): the only
//! engine this process ever touches now is the daemon's, reached over
//! [`crate::api::ApiClient`], discovered or transparently launched by
//! [`crate::api::ensure_daemon_running`] (AE2) exactly as every CLI command
//! already does (U6). The live stream is [`ApiClient::stream_all`] — a global
//! SSE connection — mixed into the same `tokio::select!` a broadcast receiver
//! used to occupy; the loop's shape does not change, only where its events
//! come from.
//!
//! # The SSE replay gap, and why this loop does not try to close it
//!
//! `stream_all()`'s underlying connection is lazy — building the stream value
//! does nothing over the wire; only the first `.next().await` actually opens
//! it — so there is no way to *guarantee* the connection is live before
//! [`App::refresh`] runs its own first `GET /executions` a few lines below.
//! This mirrors the exact gap `src/cli/commands.rs`'s `wait_until_stopped`
//! documents for `stream_execution`: a broadcast/SSE subscription has no
//! replay, so a subscriber that connects a moment too late silently misses
//! whatever already happened.
//!
//! This loop does not attempt to close that gap by synchronizing the SSE
//! handshake (there is no explicit "subscribed" acknowledgement to wait on,
//! short of racily waiting for the first keep-alive comment frame). Instead
//! it leans on the same fallback the CLI settled on: every mutating call this
//! interface makes ([`App::start_new_task`], [`App::run_selected`],
//! [`App::submit_input`], [`App::apply_transition`]) already re-reads the
//! authoritative state itself (`App::refresh`) right after the call returns,
//! rather than waiting on the stream to tell it what happened. So the one
//! window this gap can actually bite is the sliver between process startup
//! and the very first `refresh()` a few lines down — before the operator has
//! had a chance to select or act on anything — which is why the stream is
//! still built *before* that first `refresh()` call: whatever ordering
//! guarantee is available is taken, even though it is not a complete one.
//! Losing the connection later (the daemon restarting, the connection
//! dropping) is treated exactly as a closed broadcast channel used to be: a
//! permanent, visible "disconnected" indicator ([`app::StreamStatus`]) for
//! the rest of this process's life, not a silent reconnect-and-hope. Adding
//! an actual reconnect loop is a reasonable follow-up, but nothing in this
//! unit's test scenarios (AE5: two TUIs already up, watching the same
//! already-running execution) requires it.

pub mod app;
pub mod conversation;
pub mod scroll;
pub mod views;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use crate::persistence::{CatalogPaths, StatePaths, WorkPaths};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TerminalEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt as _;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::api::ensure_daemon_running;
use crate::config::Flags;

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
/// Keys are read on a dedicated thread because `crossterm` reads block, and
/// are forwarded over a channel so the loop can wait on keys and daemon
/// events at once. `state`/`flags` are exactly what
/// [`ensure_daemon_running`] needs to discover — or transparently launch
/// (AE2) — a daemon over the same state root this process was invoked
/// against; `App` then holds nothing but the [`crate::api::ApiClient`] that
/// call hands back (R4): no `Engine`, no `ExecutionStore`, constructed here
/// or anywhere else in this module.
pub async fn run(
    state: StatePaths,
    flags: Flags,
    catalog: CatalogPaths,
    work: WorkPaths,
) -> io::Result<()> {
    install_panic_hook();

    // D23 startup reconciliation ran here before U2, then moved to
    // `crate::cli::daemon::run_daemon` in U2 (R8): the daemon is the one
    // process that opens the store for write, so it is the one responsible
    // for reconciling whatever a previous, differently-killed holder left
    // behind. Until U7 this TUI still constructed its own
    // `Engine`/`ExecutionStore` regardless (dual ownership U2 deliberately
    // left in place, narrow in scope to what it asked for) — that
    // construction is what this unit removes for good.
    let client = ensure_daemon_running(&state, &flags)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;

    // Built before `app.refresh()`'s first read, not after — see the module
    // doc's "SSE replay gap" section for what this ordering does and does
    // not guarantee.
    let mut events = Box::pin(client.stream_all());

    let mut app = App::new(state, catalog, work, client);
    app.refresh().await;

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
                        Request::Refresh => app.refresh().await,
                        Request::SelectExecution => app.reload_selection().await,
                        Request::SubmitInput => app.submit_input().await,
                        Request::ApplyTransition => app.apply_transition().await,
                        Request::RunSelected => app.run_selected().await,
                        Request::RecoverSelected { comment } => app.recover_selected(comment).await,
                        Request::SaveOperatorNote { note } => app.save_operator_note(note).await,
                        Request::InterruptSelected => app.interrupt_selected().await,
                        Request::StartNewTask => app.start_new_task().await,
                        Request::OpenConversation { sequence } => {
                            app.open_conversation_for_event(sequence).await
                        }
                        Request::OpenTaskView => app.open_task_for_execution().await,
                        Request::TaskRefresh => app.refresh_task().await,
                        Request::TaskLoadSelected => app.load_task_position().await,
                        Request::LoadHarnessSelection => app.load_harness_selection().await,
                        Request::SetHarnessSelection { harness } => {
                            app.set_harness_selection(harness).await
                        }
                        Request::ResetHarnessSelection => app.reset_harness_selection().await,
                    }
                }
                // A resize needs no state change: the next draw re-derives the
                // whole layout from the new area.
                Some(_) => {}
                None => app.quit(),
            },
            received = events.next() => app.on_stream(received),
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
