//! Tests for the TUI's state layer (U10, migrated to a REST(+SSE)-only
//! client in U7).
//!
//! The plan's execution note is explicit: test `app.rs`, not the rendering.
//! Frame snapshots are brittle and prove little, so what is asserted here is
//! what the interface *decides* — which executions it lists, what a key press
//! does to it, which transitions it offers, what it refuses to send and what it
//! writes when the operator forces a transition. The one thing taken from the
//! rendering half is [`views::layout`], because it is pure geometry and a
//! resize is nothing but running it against a different rectangle.
//!
//! # U7: a real daemon subprocess, not a shared `Arc<Engine>`
//!
//! Before this unit, every test here built one `Arc<Engine>` and handed it
//! both to `App` (as its live driver) and to itself (to seed state, drive the
//! workflow directly, and read back the result). `App` no longer holds an
//! `Engine` at all, so that sharing is gone: each test now spawns a real
//! `narsor daemon` subprocess (mirroring `tests/api_discovery.rs`'s own
//! pattern for driving one), over the same temporary root, and `App` talks to
//! it purely through `ApiClient`.
//!
//! The ordering that matters: whatever a test needs to *seed* directly on
//! disk (a specific execution record, a hand-written event log, an old-format
//! log fixture) has to happen **before** the daemon is spawned — once it is
//! up, it is the sole writer of that store (R4/R11), and a test process
//! writing to the same files concurrently would race exactly the corruption
//! the whole daemon architecture exists to prevent. A throwaway, in-process
//! `Engine` is still used for this — `Engine`/`ExecutionStore` are perfectly
//! fine constructions in a *test*, which is not `src/tui/` — but only ever
//! before `spawn_daemon` runs, never alongside it.
//!
//! # Nothing is written into the repository
//!
//! Every test gets its own temporary root under `/tmp`, which holds the store
//! and the workspace and is deleted when the test's [`TempRoot`] drops. The
//! fixture declares no `globals.workspace` and reaches its script through an
//! absolute path handed in as an input, so not even the script has a reason to
//! touch the checkout.

use std::fs;
use std::panic;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Once};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use futures::{Stream, StreamExt as _};
use serde_json::{json, Value};

use narsor::api::TransitionRequest;
use narsor::api::{ensure_daemon_running, ApiClient, ClientError};
use narsor::conductor::{
    Annotation, ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore,
    FailureOutcome, FailureReport,
};
use narsor::config::Flags;
use narsor::daemon::launch::set_exe_override_for_tests;
use narsor::domain::{
    AnsweredBy, ConductorPurpose, ConductorTrigger, InputCloseReason, ProblemKind,
};
use narsor::domain::{
    Event, EventData, ExecutionStatus, WorkflowDefinition, WorkflowExecution, Workspace,
};
use narsor::engine::actions::script::TRUNCATION_MARKER;
use narsor::engine::{
    ActionKind, ConductorAnalysis, Engine, HumanInputDelivery, InputContinuation, PendingHumanInput,
};
use narsor::orchestration::ControlPlane;
use narsor::persistence::conversation::ConversationSource;
use narsor::persistence::{CatalogPaths, DaemonInfo, ExecutionStore, StatePaths, WorkPaths};
use narsor::registry::{RepositoryEntry, RepositoryRegistry};
use narsor::tui::app::{
    event_detail_lines, event_line, task_position_line, App, Attention, EntryMap, FormRow,
    InputKind, Modal, NewTaskChoice, NewTaskStage, Panel, RepositoryField, Request, Screen,
    StreamStatus, TaskContent, TransitionMode, GATE_NEEDS_FORM_MESSAGE, OVERRIDE_PROVIDER,
    OVERRIDE_REASON,
};
use narsor::tui::chat::{ChatFocus, ChatItem, ProposalState};
use narsor::tui::transcript::{
    raw_display, DisplayMode, TranscriptEntry, TranscriptEntryId, TranscriptId, TranscriptState,
};
use narsor::tui::views;
use narsor::workflow::load_from_file;
use narsor::workflows::{builtin, WorkflowResolver};

#[path = "engine_support/mod.rs"]
mod engine_support;
mod environment_support;
mod remote_support;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate
/// — the same shape `tests/cli_smoke.rs` uses.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-tui-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Points [`narsor::daemon::launch::spawn_detached`] at the real, compiled
/// `narsor` binary instead of this test harness binary — see
/// `tests/api_discovery.rs`'s own copy of this helper for why.
fn init_exe_override() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        set_exe_override_for_tests(Path::new(env!("CARGO_BIN_EXE_narsor")));
        // Every daemon spawned from here inherits it: its conductor runs on
        // the fake harness, never a real `claude`.
        engine_support::fake_claude_first_on_path();
    });
}

/// A daemon subprocess spawned over one test's temporary root, killed and
/// reaped when it drops — the same lifecycle `tests/api_discovery.rs` gives
/// the daemons it spawns.
struct Daemon {
    pid: u32,
    /// The process, when this test spawned it itself rather than through
    /// `ensure_daemon_running` ([`Daemon::start_on_path`]): it is reaped on
    /// drop, so it never lingers as a zombie `kill(pid, 0)` still sees.
    child: Option<std::process::Child>,
}

impl Daemon {
    /// Spawns (or discovers) a daemon over `root` and returns a handle to it
    /// plus a client already talking to it.
    async fn start(root: &Path) -> (Self, ApiClient) {
        init_exe_override();
        let client = Self::discover(root).await;
        let info = DaemonInfo::read(&StatePaths::new(root).daemon_file())
            .expect("daemon.json exists once ensure_daemon_running returns");
        (
            Self {
                pid: info.pid,
                child: None,
            },
            client,
        )
    }

    /// Spawns a daemon over `root` whose `PATH` is `path` — the way a test
    /// gives its conductor a fake `claude` of its own, scripted with an
    /// answer, without touching the fake every other daemon of this process
    /// shares.
    async fn start_on_path(root: &Path, path: &str) -> (Self, ApiClient) {
        init_exe_override();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_narsor"))
            .arg("--root")
            .arg(root)
            .arg("daemon")
            .env("PATH", path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the daemon");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let client = loop {
            if let Ok(info) = DaemonInfo::read(&StatePaths::new(root).daemon_file()) {
                let client = ApiClient::new(info.addr, &info.token);
                if info.pid == child.id() && client.list_executions().await.is_ok() {
                    break client;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never came up"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        (
            Self {
                pid: child.id(),
                child: Some(child),
            },
            client,
        )
    }

    /// A fresh client for the daemon this handle already spawned — cheap: it
    /// just re-reads `daemon.json` and health-checks it, exactly what a
    /// second real TUI/CLI process does against an already-running daemon
    /// (AE5's own setup).
    async fn discover(root: &Path) -> ApiClient {
        let paths = StatePaths::new(root);
        let flags = Flags {
            root: Some(root.to_path_buf()),
            config: None,
        };
        ensure_daemon_running(&paths, &flags)
            .await
            .expect("ensure_daemon_running finds/spawns a daemon")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: a plain signal-send to a PID this handle's own spawn
        // reported, mirroring `tests/api_discovery.rs`'s `stop_and_reap`.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pid_alive(self.pid) {
            if std::time::Instant::now() >= deadline {
                unsafe {
                    libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
                }
                break;
            }
            if let Some(child) = &mut self.child {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing; it only probes existence/visibility.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// An `App` over a daemon this test already spawned — the REST-client
/// counterpart of the old `App::new(Arc::clone(&engine), …)`. Nothing is read
/// until [`App::refresh`], exactly as before.
async fn app_over(_daemon: &Daemon, root: &Path) -> App {
    let client = Daemon::discover(root).await;
    App::new(
        StatePaths::new(root),
        CatalogPaths::new(root),
        WorkPaths::new(root),
        client,
    )
}

/// The same, with the workflow search path confined entirely to `root` (no
/// `HOME`/`XDG_CONFIG_HOME` reach) — the REST-client counterpart of the old
/// `App::with_resolver`. Refreshes before returning, matching the old
/// `app_for`'s own behaviour.
async fn app_for(_daemon: &Daemon, root: &Path) -> App {
    let resolver = WorkflowResolver::with_user_dir(root.to_path_buf(), Some(user_workflows(root)));
    let client = Daemon::discover(root).await;
    let mut app = App::with_resolver(
        StatePaths::new(root),
        resolver,
        CatalogPaths::new(root),
        WorkPaths::new(root),
        client,
    );
    app.refresh().await;
    app
}

/// A plain, read-only store handle for setup and verification — legitimate in
/// a test (never in `src/tui/`), and always used either *before* a daemon
/// exists for this root, or read-only afterward.
fn store_of(root: &Path) -> ExecutionStore {
    ExecutionStore::new(root)
}

/// The directory the fixtures live in.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tui")
}

/// The workflow fixture: script state → human state → terminal.
fn definition_path() -> PathBuf {
    fixtures().join("pipeline.yaml")
}

/// The loaded fixture.
fn definition() -> WorkflowDefinition {
    load_from_file(definition_path()).expect("load the fixture")
}

/// The inputs the fixture needs.
fn inputs() -> Value {
    json!({
        "script_path": fixtures().join("prepare.sh").display().to_string(),
        "ticket": "ORC-7",
    })
}

/// Creates an execution the way `narsor run` does, definition path
/// included, without running it. Only ever called before a daemon exists for
/// `root` — see the module doc.
fn start(engine: &Engine, root: &Path) -> WorkflowExecution {
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace");

    let mut execution = engine
        .start(
            &definition(),
            inputs(),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start the execution");

    // The TUI resolves the definition exactly as `input` and `resume` do.
    execution.metadata = json!({
        "cli": { "definition_path": definition_path().display().to_string() }
    });
    engine
        .store()
        .save_execution(&execution)
        .expect("record the definition path");
    execution
}

/// An engine over a fresh store in `root`, for pre-daemon setup only.
fn engine_for(root: &Path) -> Engine {
    Engine::new(ExecutionStore::new(root))
}

/// Reloads `execution_id` directly off the store, retrying briefly until its
/// status matches `expected`. A plain file read — safe at any point, before
/// or after a daemon owns this root, since it never writes.
fn reload_until(root: &Path, execution_id: &str, expected: ExecutionStatus) -> WorkflowExecution {
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let store = store_of(root);
    loop {
        let execution = store
            .load_execution(execution_id)
            .expect("reload the execution");
        if execution.status == expected || std::time::Instant::now() >= deadline {
            return execution;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A boxed SSE stream, pinned so `.next()` can be called through a `&mut`
/// reference repeatedly.
type EventStream = Pin<Box<dyn Stream<Item = Result<Event, ClientError>> + Send>>;

/// Polls `GET /executions` until every one of `execution_ids` is no longer
/// `Running` — the authoritative wait every mutating call in this file falls
/// back on, exactly as `src/cli/commands.rs`'s `wait_until_stopped` does and
/// for the identical reason (documented in `src/tui/mod.rs`'s own module
/// doc): a bare SSE subscription has no replay, so a fast-completing fixture
/// can settle before a test's own stream ever gets around to delivering the
/// event that says so.
async fn wait_until_settled(client: &ApiClient, execution_ids: &[&str]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let listing = client
            .list_executions()
            .await
            .expect("list executions while waiting for them to settle");
        let all_settled = execution_ids.iter().all(|id| {
            listing
                .executions
                .iter()
                .find(|execution| &execution.id == id)
                .is_some_and(|execution| execution.status != ExecutionStatus::Running)
        });
        if all_settled {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {execution_ids:?} to settle"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// [`wait_until_settled`] for one execution, then folds in whatever `stream`
/// already delivered (best-effort — see [`wait_until_settled`]'s doc for why
/// this is not the primary mechanism) so a notice a live terminal event sets
/// (`notice_for`) is visible to a caller that wants to assert on it.
async fn drive_until_settled(
    app: &mut App,
    client: &ApiClient,
    stream: &mut EventStream,
    execution_id: &str,
) {
    drive_until_all_settled(app, client, stream, &[execution_id]).await;
}

/// Like [`drive_until_settled`], for more than one execution id at once.
async fn drive_until_all_settled(
    app: &mut App,
    client: &ApiClient,
    stream: &mut EventStream,
    execution_ids: &[&str],
) {
    wait_until_settled(client, execution_ids).await;
    while let Ok(Some(received)) =
        tokio::time::timeout(Duration::from_millis(200), stream.next()).await
    {
        app.on_stream(Some(received));
    }
}

/// Starts an execution and runs it until it stops on the `review` question —
/// a throwaway, in-process `Engine`, used only before a daemon exists for
/// `root` (see the module doc).
async fn paused_at_review(root: &Path) -> String {
    let engine = engine_for(root);
    let execution = start(&engine, root);
    let execution = engine
        .run(&definition(), &execution.id)
        .await
        .expect("run up to the human state");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("review"));
    execution.id
}

/// A key press, as the event loop would deliver it.
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// Types a whole string into the focused text field.
fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
}

/// The fixture the new-task modal starts: one required string, one boolean
/// with a default, one repository, and a human state to stop on.
fn new_task_definition_path() -> PathBuf {
    fixtures().join("new_task.yaml")
}

/// The fixture that fails validation on more than one count.
fn broken_definition_path() -> PathBuf {
    fixtures().join("broken.yaml")
}

/// The registry of a root, as the interface sees it.
fn registry_of(root: &Path) -> RepositoryRegistry {
    RepositoryRegistry::new(root)
}

/// Opens the repository form: `R` for the listing, `a` for the form.
fn open_repository_form(app: &mut App) {
    app.on_key(key(KeyCode::Char('R')));
    app.on_key(key(KeyCode::Char('a')));
    assert!(
        matches!(app.modal(), Modal::RepositoryForm(_)),
        "the repository form is open"
    );
}

/// Fills one row of the repository form, moving to it with Tab first.
fn fill_repository_field(app: &mut App, field: RepositoryField, value: &str) {
    let Modal::RepositoryForm(form) = app.modal() else {
        panic!("the repository form is open");
    };
    let steps =
        (field.index() + RepositoryField::ALL.len() - form.row) % RepositoryField::ALL.len();
    for _ in 0..steps {
        app.on_key(key(KeyCode::Down));
    }
    type_text(app, value);
}

/// Opens the new-task modal on a loaded definition.
fn open_new_task(app: &mut App, path: &Path) {
    app.on_key(key(KeyCode::Char('n')));
    // The modal opens on the pick list; the last row — reached by wrapping
    // backwards with ← — is the one that takes an arbitrary path.
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Enter));
    type_text(app, &path.display().to_string());
    app.on_key(key(KeyCode::Enter));
}

/// The index of one declared input in the new-task form.
fn input_row(app: &App, name: &str) -> usize {
    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task form is open");
    };
    form.parameters
        .iter()
        .position(|parameter| parameter.name == name)
        .unwrap_or_else(|| panic!("the fixture declares an input named '{name}'"))
}

/// Moves the new-task form's cursor onto one declared input.
fn focus_input(app: &mut App, name: &str) {
    let target = input_row(app, name);
    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task form is open");
    };
    let rows = form.parameters.len();
    let steps = (target + rows - form.row) % rows;
    for _ in 0..steps {
        app.on_key(key(KeyCode::Tab));
    }
}

/// The events an execution has on disk — a plain read, safe once a daemon
/// owns `root` (it never writes).
fn events_of(root: &Path, execution_id: &str) -> Vec<Event> {
    store_of(root)
        .event_log(execution_id)
        .read_all()
        .expect("read the event log")
}

// ---------------------------------------------------------------------------
// happy paths
// ---------------------------------------------------------------------------

/// The list mirrors the store, statuses included.
#[tokio::test(flavor = "multi_thread")]
async fn execution_list_reflects_the_store_with_its_statuses() {
    let root = TempRoot::new();
    let paused = paused_at_review(root.path()).await;

    // A second execution, parked in a terminal status by hand, so the list has
    // two different statuses to show.
    let engine = engine_for(root.path());
    let mut finished = start(&engine, root.path());
    finished.status = ExecutionStatus::Completed;
    finished.current_state = Some("done".to_string());
    engine.store().save_execution(&finished).expect("save");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert_eq!(app.executions().len(), 2);

    let paused_row = app
        .executions()
        .iter()
        .find(|row| row.id == paused)
        .expect("the paused execution is listed");
    assert_eq!(paused_row.status, ExecutionStatus::InputNeeded);
    assert_eq!(paused_row.current_state.as_deref(), Some("review"));
    assert_eq!(paused_row.workflow_id, definition().id);

    let finished_row = app
        .executions()
        .iter()
        .find(|row| row.id == finished.id)
        .expect("the finished execution is listed");
    assert_eq!(finished_row.status, ExecutionStatus::Completed);
    assert_eq!(finished_row.current_state.as_deref(), Some("done"));
}

/// A live event lands in the stream and takes the view with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_event_appends_to_the_stream_and_moves_the_view() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    let backfilled = app.events().len();
    assert!(backfilled > 0, "the backfill read the log");
    let last_sequence = app.events().last().expect("an event").sequence;

    let event = Event::new(
        execution_id.clone(),
        EventData::StateEntered {
            state_id: "done".to_string(),
        },
        last_sequence + 1,
    );
    app.on_stream(Some(Ok(event)));

    assert_eq!(app.events().len(), backfilled + 1);
    assert_eq!(app.event_cursor(), app.events().len() - 1);
    assert!(app.follows_stream());
    assert_eq!(app.stream_status(), &StreamStatus::Live);
}

/// `i` opens the form for a workflow gate too, not only for an agent's
/// question (R1) — the review's `select` offers exactly its outputs, so it is
/// the one choice of the label (KTD17), with no separate label row.
#[tokio::test(flavor = "multi_thread")]
async fn i_opens_the_gate_form_from_the_execution_list() {
    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert!(app.pending().is_some(), "the question is visible");
    assert_eq!(app.focus(), Panel::Executions);
    assert_eq!(app.on_key(key(KeyCode::Char('i'))), Request::None);

    let Modal::Input(form) = app.modal() else {
        panic!("the gate's form is open");
    };
    assert_eq!(form.kind, InputKind::Gate);
    assert_eq!(form.instruction, "Approve the prepared change?");
    assert_eq!(
        form.label_field,
        Some(0),
        "approval_status chooses the label"
    );
    assert_eq!(form.rows(), vec![FormRow::Field(0), FormRow::Field(1)]);
    assert_eq!(form.chosen_label().as_deref(), Some("approved"));
}

/// A declared transition resolves a workflow decision whose gate declares
/// no fields: there is nothing `t` could lose, so it is not sent to `i`.
#[tokio::test(flavor = "multi_thread")]
async fn selecting_a_declared_transition_resumes_the_execution() {
    let root = TempRoot::new();
    let execution_id = paused_at_confirm(root.path()).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('t')));
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::ApplyTransition);
    app.apply_transition().await;

    wait_until_settled(&client, &[&execution_id]).await;

    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::Completed);
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));
    assert!(!app.notice().expect("a notice").is_error);
}

/// The offered transitions are exactly the state's `outputs`.
#[tokio::test(flavor = "multi_thread")]
async fn permitted_transitions_are_exactly_the_declared_outputs() {
    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert_eq!(app.permitted_transitions(), vec!["approved", "rejected"]);

    app.on_key(key(KeyCode::Char('t')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert_eq!(menu.mode, TransitionMode::Outputs);
    assert_eq!(menu.entries(), ["approved", "rejected"]);
}

/// The transition menu says what each declared output means, where the
/// workflow gives a description — in the menu's model and on screen.
#[tokio::test(flavor = "multi_thread")]
async fn the_transition_menu_shows_output_descriptions() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    app.on_key(key(KeyCode::Char('t')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert_eq!(menu.description(0), Some("The change is ready to land."));
    assert_eq!(menu.description(1), None, "`rejected` declares none");

    let mut terminal = Terminal::new(TestBackend::new(160, 48)).expect("a test terminal");
    terminal
        .draw(|frame| views::render(frame, &mut app))
        .expect("draw the screen");
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains("approved — The change is ready to land."),
        "the description is drawn next to its label"
    );
}

// ---------------------------------------------------------------------------
// edge cases
// ---------------------------------------------------------------------------

/// An execution with no events is an empty stream, not a failure.
#[tokio::test(flavor = "multi_thread")]
async fn an_execution_without_events_shows_an_empty_stream() {
    let root = TempRoot::new();
    // Written straight to the store, so not a single event exists for it.
    {
        let store = store_of(root.path());
        let execution = WorkflowExecution::new(
            &definition(),
            inputs(),
            Workspace::local_folder(root.path().display().to_string()),
        );
        store
            .create_execution_dirs(&execution.id)
            .expect("create the directories");
        store
            .save_execution(&execution)
            .expect("save the execution");
        store
            .save_context(&execution.id, &execution.context)
            .expect("save the context");
    }

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert_eq!(app.executions().len(), 1);
    assert!(app.events().is_empty());
    assert_eq!(app.event_cursor(), 0);
    assert!(app.notice().is_none(), "an empty log is not an error");
}

/// Manual step selection is available even when the workflow did not opt into
/// the historical `allow_manual_override` escape hatch.
#[tokio::test(flavor = "multi_thread")]
async fn a_state_without_allow_manual_override_still_offers_step_selection() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;

    // `prepare` is the fixture's state that deliberately does not allow it.
    {
        let store = store_of(root.path());
        let mut execution = store
            .load_execution(&execution_id)
            .expect("load the execution");
        execution.current_state = Some("prepare".to_string());
        store.save_execution(&execution).expect("save");
    }

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert!(app.override_allowed());
    assert!(!app.override_targets().is_empty());

    app.on_key(key(KeyCode::Char('t')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert!(menu.override_allowed);
    assert_eq!(menu.mode, TransitionMode::Outputs);

    // `o` switches to the global step picker.
    app.on_key(key(KeyCode::Char('o')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is still open");
    };
    assert_eq!(menu.mode, TransitionMode::Override);
}

/// A resize preserves the arrangement of the panels.
#[test]
fn a_resize_preserves_the_panel_layout() {
    use ratatui::layout::Rect;

    for area in [
        Rect::new(0, 0, 80, 24),
        Rect::new(0, 0, 200, 60),
        Rect::new(0, 0, 60, 16),
    ] {
        let panels = views::layout(area);

        // The shortcut bar sits at the bottom and keeps its height.
        assert_eq!(panels.shortcuts.height, 3);
        assert_eq!(panels.shortcuts.bottom(), area.bottom());
        assert_eq!(panels.shortcuts.width, area.width);

        // The list is the left column, details above events on the right.
        assert_eq!(panels.executions.x, area.x);
        assert_eq!(panels.details.x, panels.executions.right());
        assert_eq!(panels.events.x, panels.details.x);
        assert_eq!(panels.events.y, panels.details.bottom());
        assert_eq!(panels.details.width, panels.events.width);

        // Nothing is lost and nothing overlaps.
        assert_eq!(
            panels.executions.width + panels.details.width,
            area.width,
            "the two columns fill the width"
        );
        assert_eq!(panels.executions.bottom(), panels.shortcuts.y);
        assert_eq!(panels.events.bottom(), panels.shortcuts.y);

        // The split stays proportional rather than drifting to a fixed size.
        let share = f64::from(panels.executions.width) / f64::from(area.width);
        assert!(
            (0.30..=0.40).contains(&share),
            "list width share was {share} at {area:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// error paths
// ---------------------------------------------------------------------------

/// An empty required field stops the gate's form before anything is sent:
/// the error is shown at the field, the focus moves to it, and the execution
/// stays exactly as paused as it was.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_required_field_stops_the_gate_form_at_the_field() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;
    let before = events_of(root.path(), &execution_id).len();

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::SubmitInput);
    app.submit_input().await;

    let Modal::Input(form) = app.modal() else {
        panic!("the form stays open");
    };
    assert_eq!(form.error_row, Some(FormRow::Field(1)), "at the comment");
    assert_eq!(form.current_row(), Some(FormRow::Field(1)), "focused there");
    assert!(form
        .error
        .as_deref()
        .is_some_and(|error| error.contains("'comment' is required")));
    assert!(app.notice().expect("a notice").is_error);
    assert!(
        screen_of(&mut app).contains("↳ Field 'comment' is required."),
        "the error is drawn under the field"
    );

    let store = store_of(root.path());
    let execution = store
        .load_execution(&execution_id)
        .expect("reload the execution");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert!(
        store.has_pending_input(&execution_id),
        "the question is still answerable"
    );
    assert_eq!(
        events_of(root.path(), &execution_id).len(),
        before,
        "nothing was written"
    );
}

/// Losing the channel shows an indicator instead of a frozen view.
#[tokio::test(flavor = "multi_thread")]
async fn losing_the_event_channel_shows_a_disconnection_indicator() {
    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(app.stream_status(), &StreamStatus::Live);

    // U7: the live source is an SSE connection, not an in-process broadcast
    // channel, so there is no `Lagged` case any more (see `on_stream`'s own
    // doc) — only the stream ending, either on a transport error or on `None`.
    app.on_stream(Some(Err(ClientError::Unavailable(
        "connection reset".to_string(),
    ))));
    assert_eq!(app.stream_status(), &StreamStatus::Disconnected);
    assert!(!app.stream_status().is_live());
    assert!(app.notice().expect("a notice").is_error);

    app.on_stream(None);
    assert_eq!(app.stream_status(), &StreamStatus::Disconnected);
    let indicator = app.stream_status().indicator();
    assert!(
        indicator.to_lowercase().contains("disconnected"),
        "the indicator names the disconnection: {indicator}"
    );
    assert!(app.notice().expect("a notice").is_error);
}

// ---------------------------------------------------------------------------
// integration
// ---------------------------------------------------------------------------

/// An override needs a confirmation and is recorded as forced.
#[tokio::test(flavor = "multi_thread")]
async fn an_override_requires_confirmation_and_is_recorded_as_forced() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;
    let before = events_of(root.path(), &execution_id).len();

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert!(app.override_allowed(), "`review` declares the flag");

    app.on_key(key(KeyCode::Char('t')));
    app.on_key(key(KeyCode::Char('o')));

    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert_eq!(menu.mode, TransitionMode::Override);
    // `prepare` is not reachable from `review` by any declared transition,
    // which is the whole point of an override.
    let target = menu
        .entries()
        .iter()
        .position(|entry| entry == "prepare")
        .expect("`prepare` is offered as a target");
    for _ in 0..target {
        app.on_key(key(KeyCode::Char('j')));
    }

    // Enter alone must not apply it.
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::None);
    let Modal::Confirm(confirm) = app.modal() else {
        panic!("a confirmation step stands between Enter and the write");
    };
    assert_eq!(confirm.from, "review");
    assert_eq!(confirm.target, "prepare");
    assert_eq!(
        events_of(root.path(), &execution_id).len(),
        before,
        "nothing is written before the confirmation"
    );

    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Request::ApplyTransition
    );
    app.apply_transition().await;

    let events = events_of(root.path(), &execution_id);
    assert!(events.len() > before, "the override was recorded");

    let forced = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::HumanTransitionSelected {
                from_state,
                to_state,
            } => Some((from_state.clone(), to_state.clone())),
            _ => None,
        })
        .expect("HumanTransitionSelected was emitted");
    assert_eq!(forced, ("review".to_string(), "prepare".to_string()));

    let annotation = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::DecisionMade {
                provider,
                reasoning,
                ..
            } if provider == OVERRIDE_PROVIDER => Some(reasoning.clone()),
            _ => None,
        })
        .expect("the transition is annotated as forced");
    assert_eq!(annotation.as_deref(), Some(OVERRIDE_REASON));

    assert!(
        events.iter().any(|event| matches!(
            &event.data,
            EventData::TransitionTaken { to_state, .. } if to_state == "prepare"
        )),
        "the transition itself is in the log"
    );

    let execution = store_of(root.path())
        .load_execution(&execution_id)
        .expect("reload the execution");
    assert_eq!(execution.current_state.as_deref(), Some("prepare"));
    assert_eq!(execution.status, ExecutionStatus::Paused);
    assert!(
        !store_of(root.path()).has_pending_input(&execution_id),
        "the abandoned question is no longer answerable"
    );
}

/// The terminal is restored on the way out and from the panic hook.
///
/// Entering the alternate screen is not possible here — the test harness has no
/// tty — so what is proved is that the restore path *runs*: once when called
/// directly, and once from inside a panic with the hook installed. That the
/// escape sequences then reach a real terminal is the part only a person at one
/// can confirm.
#[test]
fn the_terminal_is_restored_on_exit_and_on_panic() {
    let before = narsor::tui::restore_count();
    narsor::tui::restore_terminal();
    assert_eq!(
        narsor::tui::restore_count(),
        before + 1,
        "the ordinary exit path restores the terminal"
    );

    narsor::tui::install_panic_hook();
    let before_panic = narsor::tui::restore_count();

    let result = panic::catch_unwind(|| panic!("a crash inside the interface"));
    assert!(result.is_err(), "the panic really happened");

    assert!(
        narsor::tui::restore_count() > before_panic,
        "the panic hook restored the terminal before unwinding"
    );
}

/// The whole journey, driven through the state layer only.
///
/// From an execution the way `narsor run` creates one, every step after
/// that is a key press or the operation a key press asks for: run the script
/// state, open the generated form, fill it in, submit, and land in a terminal
/// state. Starting a *new* workflow is not part of the TUI — that stays with
/// `narsor run` — so this begins where the interface begins.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_drives_the_workflow_to_a_terminal_state_from_the_interface() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start(&engine, root.path())
    };

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert_eq!(app.focus(), Panel::Executions);
    assert_eq!(app.selected().expect("a selection").id, execution.id);
    assert_eq!(
        app.selected().expect("a selection").status,
        ExecutionStatus::Running
    );

    // `s` — run the script state; the workflow stops on the question.
    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;
    wait_until_settled(&client, &[&execution.id]).await;
    // Only the row's own status comes live off the stream (`apply_to_row`);
    // `pending()`/`variables()` are read on selection, so the test refreshes
    // them the way pressing `r` would.
    app.refresh().await;

    assert_eq!(
        app.selected().expect("a selection").status,
        ExecutionStatus::InputNeeded
    );
    assert_eq!(
        app.selected()
            .expect("a selection")
            .current_state
            .as_deref(),
        Some("review")
    );
    assert!(app.pending().is_some());
    assert!(
        app.variables()
            .iter()
            .any(|(name, value)| name == "ticket" && value == "ORC-7"),
        "the script's variable reached the context panel"
    );

    // `i` — answer the review: `approved` is preselected, the comment is
    // typed, and Enter sends it.
    app.on_key(key(KeyCode::Char('i')));
    app.on_key(key(KeyCode::Tab));
    type_text(&mut app, "looks good");
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::SubmitInput);
    app.submit_input().await;
    assert!(
        matches!(app.modal(), Modal::None),
        "an accepted answer closes the form"
    );
    wait_until_settled(&client, &[&execution.id]).await;

    let stored = reload_until(root.path(), &execution.id, ExecutionStatus::Completed);
    assert_eq!(stored.status, ExecutionStatus::Completed);
    assert_eq!(stored.current_state.as_deref(), Some("done"));

    let recorded = events_of(root.path(), &execution.id);
    assert!(recorded.iter().any(|event| matches!(
        &event.data,
        EventData::HumanInputReceived { input, output_label, .. }
            if input["comment"] == "looks good"
                && output_label.as_deref() == Some("approved")
    )));
    assert!(recorded
        .iter()
        .any(|event| matches!(&event.data, EventData::DecisionMade { .. })));
    assert!(recorded
        .iter()
        .any(|event| matches!(&event.data, EventData::WorkflowCompleted { .. })));

    // The interface catches up once asked, the same as pressing `r` would.
    app.refresh().await;
    assert_eq!(
        app.selected().expect("a selection").status,
        ExecutionStatus::Completed
    );

    // `q` ends the session.
    app.on_key(key(KeyCode::Char('q')));
    assert!(app.should_quit());
}

/// Writes a workflow with one deliberately slow `script` step (`sleep 0.3`),
/// so a test has a real window to dispatch into while it is still running.
fn write_slow_definition(root: &TempRoot, name: &str) -> (WorkflowDefinition, PathBuf) {
    let script_path = root.path().join(format!("{name}.sh"));
    fs::write(
        &script_path,
        "#!/bin/sh\nsleep 0.3\nprintf '{\"output\": \"done\"}\\n'\nexit 0\n",
    )
    .expect("write the slow script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path)
            .expect("stat the script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("make it executable");
    }

    let yaml = format!(
        r#"
name: {name}
version: 1
description: Fixture with one deliberately slow step.

states:
  prepare:
    description: A deliberately slow step.
    allow_manual_override: true
    action:
      type: script
      command: "{}"
      args: []
      exit_codes:
        0: done
    outputs:
      - done
    transitions:
      done: finished

  finished:
    description: Terminal.

terminal_states:
  finished: completed
"#,
        script_path.display()
    );
    let path = root.path().join(format!("{name}.yaml"));
    fs::write(&path, yaml).expect("write the definition");
    let definition = load_from_file(&path).expect("load the definition");
    (definition, path)
}

/// Starts an execution of `write_slow_definition`'s workflow, without running
/// it, recording the definition path the way the TUI's new-task modal does.
/// Pre-daemon setup only.
fn start_slow(engine: &Engine, root: &TempRoot, name: &str) -> WorkflowExecution {
    let (definition, definition_path) = write_slow_definition(root, name);
    let workspace_path = root.path().join(format!("{name}-workspace"));
    fs::create_dir_all(&workspace_path).expect("create the workspace");
    let mut execution = engine
        .start(
            &definition,
            json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start the execution");
    execution.metadata = json!({
        "cli": { "definition_path": definition_path.display().to_string() }
    });
    engine
        .store()
        .save_execution(&execution)
        .expect("record the definition path");
    execution
}

/// R1/R2: dispatching a request whose first step is deliberately slow returns
/// control long before that step finishes — the point of the daemon running
/// it in its own background task (U3/U7) rather than the caller awaiting it
/// inline.
/// `s` is an explicit recovery for a failed execution, not merely a synonym
/// for `/resume` (which only accepts running or paused executions).
#[tokio::test(flavor = "multi_thread")]
async fn s_recovers_a_failed_execution_with_a_fresh_attempt() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start(&engine, root.path())
    };

    let store = store_of(root.path());
    let mut failed = store.load_execution(&execution.id).expect("load execution");
    failed.status = ExecutionStatus::Failed;
    store.save_execution(&failed).expect("mark failed");

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(
        app.selected().expect("selection").status,
        ExecutionStatus::Failed
    );

    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;
    wait_until_settled(&client, &[&execution.id]).await;
    app.refresh().await;

    assert_eq!(
        app.selected().expect("selection").status,
        ExecutionStatus::InputNeeded,
        "the new script attempt reached the workflow's human checkpoint"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn i_is_unavailable_for_a_failed_execution() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start(&engine, root.path())
    };
    let store = store_of(root.path());
    let mut failed = store.load_execution(&execution.id).expect("load execution");
    failed.status = ExecutionStatus::Failed;
    store.save_execution(&failed).expect("mark failed");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert_eq!(app.on_key(key(KeyCode::Char('i'))), Request::None);
    assert!(!matches!(app.modal(), Modal::RecoveryComment(_)));
    assert!(app.notice().expect("a notice").is_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatching_a_slow_first_step_does_not_block_the_caller() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start_slow(&engine, &root, "tui-slow")
    };

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&execution.id).await;

    let started = std::time::Instant::now();
    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;
    let dispatch_elapsed = started.elapsed();

    assert!(
        dispatch_elapsed < Duration::from_millis(150),
        "dispatching returned in {dispatch_elapsed:?}, so it did not wait \
         on the 300ms step"
    );

    wait_until_settled(&client, &[&execution.id]).await;
    let stored = reload_until(root.path(), &execution.id, ExecutionStatus::Completed);
    assert_eq!(stored.status, ExecutionStatus::Completed);
}

/// R3: two different executions can be driven in the background from the
/// same TUI session at once, neither blocking on the other.
#[tokio::test(flavor = "multi_thread")]
async fn two_different_executions_run_in_the_background_at_once() {
    let root = TempRoot::new();
    let (first, second) = {
        let engine = engine_for(root.path());
        (
            start_slow(&engine, &root, "tui-slow-a"),
            start_slow(&engine, &root, "tui-slow-b"),
        )
    };

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    app.select_execution(&first.id).await;
    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;

    app.select_execution(&second.id).await;
    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;

    wait_until_settled(&client, &[&first.id, &second.id]).await;

    assert_eq!(
        reload_until(root.path(), &first.id, ExecutionStatus::Completed).status,
        ExecutionStatus::Completed
    );
    assert_eq!(
        reload_until(root.path(), &second.id, ExecutionStatus::Completed).status,
        ExecutionStatus::Completed
    );
}

/// R6, R7, R11: a second `resume` on the same execution while the first pass
/// is still running is rejected by the daemon's guard (U1/U3) with a `409`,
/// which the TUI's own `send_transition`/`deliver`/`run_selected` all turn
/// into a plain notice — instead of racing a second driver against the
/// first. Driven directly through two independent clients (rather than two
/// `App`s) so the race is deterministic (`tokio::join!`) instead of hoping a
/// background task wins a real timing race, the way the pre-U7 version of
/// this test had to.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_drive_of_the_same_execution_is_rejected_with_a_notice() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start_slow(&engine, &root, "tui-slow-guard")
    };

    let (daemon, client_a) = Daemon::start(root.path()).await;
    let client_b = Daemon::discover(root.path()).await;

    let (first, second) = tokio::join!(
        client_a.resume(&execution.id),
        client_b.resume(&execution.id)
    );
    let outcomes = [first, second];
    let accepted = outcomes.iter().filter(|result| result.is_ok()).count();
    let rejected = outcomes
        .iter()
        .filter(|result| matches!(result, Err(ClientError::Conflict)))
        .count();
    assert_eq!(accepted, 1, "exactly one driver wins the guard");
    assert_eq!(rejected, 1, "the other is refused with a 409/Conflict");

    wait_until_settled(&client_a, &[&execution.id]).await;
    let stored = reload_until(root.path(), &execution.id, ExecutionStatus::Completed);
    assert_eq!(
        stored.status,
        ExecutionStatus::Completed,
        "exactly one driver actually advanced the execution"
    );
    drop(daemon);
}

/// R6: a terminal event for an execution that is not the one currently
/// selected still sets a notice naming it — the mechanism is not gated on
/// selection.
#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_event_for_an_unselected_execution_still_sets_a_notice() {
    let root = TempRoot::new();
    let (watched, other) = {
        let engine = engine_for(root.path());
        let watched = start_slow(&engine, &root, "tui-slow-unselected");
        let other = start(&engine, root.path());
        (watched, other)
    };

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&watched.id).await;

    // Connected before dispatch, and the fixture's 300ms sleep gives this
    // stream a real window to actually be listening before the terminal
    // event fires — see `src/tui/mod.rs`'s module doc on why this ordering
    // is a best effort, not a guarantee, for a fixture with no such window.
    let mut stream: EventStream = Box::pin(client.stream_all());

    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;

    // Selection moves elsewhere before the background run finishes.
    app.select_execution(&other.id).await;
    assert_ne!(app.selected().expect("a selection").id, watched.id);

    drive_until_settled(&mut app, &client, &mut stream, &watched.id).await;

    let notice = app.notice().expect("a notice");
    assert!(
        notice.text.contains(&watched.id),
        "the notice names the execution that actually finished: {notice:?}"
    );
}

/// A manual/forced transition now goes through `client.transition` (U7),
/// which the daemon rejects with a `409` while a background pass already
/// holds the execution (U3) — the same denial the old client-side
/// `Engine::is_active` check used to produce, now sourced from the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_transition_is_refused_while_a_background_pass_is_driving_the_same_execution() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start_slow(&engine, &root, "tui-slow-override")
    };

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&execution.id).await;

    // Start the slow background pass; it now holds the guard for the whole
    // 300ms of the script.
    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;

    // No client-side visibility into `Engine::is_active` any more (U7): a
    // short sleep stands in for the old `while !engine.is_active(...)` poll,
    // trusting the 300ms script to still be running well past it.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Without refreshing, the cached selection still shows `prepare`, which
    // is exactly the state the background pass is stepping right now.
    app.on_key(key(KeyCode::Char('t')));
    app.on_key(key(KeyCode::Char('o')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert_eq!(menu.mode, TransitionMode::Override);
    let target = menu
        .entries()
        .iter()
        .position(|entry| entry == "finished")
        .expect("`finished` is offered as an override target");
    for _ in 0..target {
        app.on_key(key(KeyCode::Char('j')));
    }
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Request::ApplyTransition
    );
    app.apply_transition().await;

    assert!(
        app.notice().expect("a notice").is_error,
        "the forced transition is refused while the pass is live"
    );
    // The background pass is legitimately still writing its own events
    // concurrently, so a plain event count would be racy; what the refused
    // transition itself must never have written is a `HumanTransitionSelected`.
    assert!(
        !events_of(root.path(), &execution.id)
            .iter()
            .any(|event| matches!(&event.data, EventData::HumanTransitionSelected { .. })),
        "the refused forced transition wrote no HumanTransitionSelected"
    );

    wait_until_settled(&client, &[&execution.id]).await;
    let stored = reload_until(root.path(), &execution.id, ExecutionStatus::Completed);
    assert_eq!(
        stored.status,
        ExecutionStatus::Completed,
        "the background pass finished on its own, undisturbed"
    );
}

/// `i` works from the details panel as well, and the bar says what it does
/// there. While the form is open, the bar offers only keys `on_key_form`
/// handles: `j`/`k` would be typed into a text field, so they are not
/// advertised for moving between rows.
#[tokio::test(flavor = "multi_thread")]
async fn i_opens_the_gate_form_from_the_details_panel() {
    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus(), Panel::Details);
    assert!(app.shortcuts().contains(&("i", "answer")));

    app.on_key(key(KeyCode::Char('i')));
    assert!(matches!(app.modal(), Modal::Input(_)));
    assert!(app
        .shortcuts()
        .iter()
        .all(|(keys, _)| !keys.contains('j') && !keys.contains('k')));
}

// ---------------------------------------------------------------------------
// registering a repository
// ---------------------------------------------------------------------------

/// The form stores a valid entry, and the listing shows it afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn the_repository_form_stores_an_entry_that_the_listing_then_shows() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_repository_form(&mut app);
    fill_repository_field(&mut app, RepositoryField::Id, "acme-api");
    fill_repository_field(
        &mut app,
        RepositoryField::Remote,
        "https://example.test/acme.git",
    );
    fill_repository_field(&mut app, RepositoryField::DefaultBranch, "main");
    app.on_key(key(KeyCode::Enter));

    let stored = registry_of(root.path())
        .get("acme-api")
        .expect("read the registry")
        .expect("the entry was stored");
    assert_eq!(stored.remote, "https://example.test/acme.git");
    assert_eq!(stored.default_branch.as_deref(), Some("main"));
    // The id stands in for the name, exactly as `narsor repo add` does.
    assert_eq!(stored.name, "acme-api");

    let Modal::Repositories(list) = app.modal() else {
        panic!("the form gives way to the listing");
    };
    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].id, "acme-api");
    assert!(!app.notice().expect("a notice").is_error);
}

/// A duplicate id keeps the form open and leaves the stored entry alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_repository_id_keeps_the_form_open_and_changes_nothing() {
    let root = TempRoot::new();
    registry_of(root.path())
        .add(RepositoryEntry::new(
            "acme-api",
            "https://example.test/original.git",
        ))
        .expect("register the original");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_repository_form(&mut app);
    fill_repository_field(&mut app, RepositoryField::Id, "acme-api");
    fill_repository_field(
        &mut app,
        RepositoryField::Remote,
        "https://example.test/impostor.git",
    );
    app.on_key(key(KeyCode::Enter));

    let Modal::RepositoryForm(form) = app.modal() else {
        panic!("the form stays open so the operator can fix the id");
    };
    let error = form.error.as_deref().expect("the form reports the failure");
    assert!(
        error.contains("acme-api") && error.contains("already registered"),
        "the error names the collision: {error}"
    );
    assert!(app.notice().expect("a notice").is_error);

    let stored = registry_of(root.path())
        .get("acme-api")
        .expect("read the registry")
        .expect("the original is still there");
    assert_eq!(stored.remote, "https://example.test/original.git");
    assert_eq!(
        registry_of(root.path()).list().expect("list").entries.len(),
        1,
        "nothing was appended"
    );
}

/// An `ext::` remote is refused in the form and never reaches the registry.
#[tokio::test(flavor = "multi_thread")]
async fn an_ext_remote_is_refused_in_the_form_and_never_reaches_the_registry() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_repository_form(&mut app);
    fill_repository_field(&mut app, RepositoryField::Id, "hostile");
    // `ext::` makes git execute an arbitrary command; the provisioner's rules
    // refuse it, and the form has to say so before anything is stored.
    fill_repository_field(&mut app, RepositoryField::Remote, "ext::sh -c whoami");
    app.on_key(key(KeyCode::Enter));

    let Modal::RepositoryForm(form) = app.modal() else {
        panic!("the form stays open on a refused remote");
    };
    assert!(
        form.error.is_some(),
        "the refusal is shown in the form, not swallowed"
    );

    assert!(
        registry_of(root.path())
            .list()
            .expect("read the registry")
            .is_empty(),
        "nothing was written to the registry"
    );
    assert!(
        !registry_of(root.path()).dir().exists(),
        "the registry directory was not even created"
    );
}

/// A removal asks first, and only then removes.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_repository_needs_a_confirmation() {
    let root = TempRoot::new();
    registry_of(root.path())
        .add(RepositoryEntry::new(
            "acme-api",
            "https://example.test/a.git",
        ))
        .expect("register");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    app.on_key(key(KeyCode::Char('R')));
    app.on_key(key(KeyCode::Char('d')));
    let Modal::RepositoryRemove(confirm) = app.modal() else {
        panic!("a confirmation stands between d and the removal");
    };
    assert_eq!(confirm.id, "acme-api");
    assert_eq!(
        registry_of(root.path()).list().expect("list").entries.len(),
        1,
        "nothing is removed before the confirmation"
    );

    // Declining puts the listing back and leaves the entry alone.
    app.on_key(key(KeyCode::Char('n')));
    assert!(matches!(app.modal(), Modal::Repositories(_)));
    assert_eq!(
        registry_of(root.path()).list().expect("list").entries.len(),
        1
    );

    app.on_key(key(KeyCode::Char('d')));
    app.on_key(key(KeyCode::Char('y')));
    assert!(
        registry_of(root.path()).list().expect("list").is_empty(),
        "the confirmed removal went through"
    );
}

// ---------------------------------------------------------------------------
// the new-task pick list
// ---------------------------------------------------------------------------

/// The user workflow directory of a temporary root.
///
/// Nothing in this file ever lets the resolver read the environment, so the
/// developer's own `~/.config` is out of reach: every `App` built here is given
/// a resolver pointed at this directory.
fn user_workflows(root: &Path) -> PathBuf {
    let path = root.join("config/narsor/workflows");
    fs::create_dir_all(&path).expect("create the user workflow directory");
    path
}

/// `{root}/workflows`.
fn root_workflows(root: &Path) -> PathBuf {
    let path = root.join("workflows");
    fs::create_dir_all(&path).expect("create the root workflow directory");
    path
}

/// The rows of the new-task pick list, as labels.
fn pick_labels(app: &App) -> Vec<String> {
    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task modal is open");
    };
    form.choices.iter().map(NewTaskChoice::label).collect()
}

/// Moves the pick list's cursor onto the row naming one workflow.
fn pick_workflow(app: &mut App, name: &str) {
    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task modal is open");
    };
    let target = form
        .choices
        .iter()
        .position(|choice| choice.name() == Some(name))
        .unwrap_or_else(|| panic!("'{name}' is one of the rows: {:?}", pick_labels(app)));
    let steps = (target + form.choices.len() - form.choice) % form.choices.len();
    for _ in 0..steps {
        app.on_key(key(KeyCode::Right));
    }
}

/// The modal opens on every workflow the resolver reaches, each row naming its
/// source, plus the row that takes an arbitrary path.
#[tokio::test(flavor = "multi_thread")]
async fn the_new_task_list_is_built_from_the_resolver_and_names_each_source() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_for(&daemon, root.path()).await;

    app.on_key(key(KeyCode::Char('n')));

    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task modal is open");
    };
    assert_eq!(form.stage, NewTaskStage::Pick, "it opens on the list");

    let labels = pick_labels(&app);
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("compound-engineering [built-in]")),
        "the built-ins are offered with their source: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("open-mercato-sdlc [built-in]")),
        "both built-ins are offered: {labels:?}"
    );
    assert_eq!(
        labels.last().map(String::as_str),
        Some("Enter a path…"),
        "the last row is the arbitrary-path route: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.contains("compound engineering chain")),
        "a row carries the workflow's description: {labels:?}"
    );
}

/// `←` and `→` change the selection, and the selection is what is loaded.
#[tokio::test(flavor = "multi_thread")]
async fn left_and_right_move_the_selection_and_the_selection_is_what_is_loaded() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_for(&daemon, root.path()).await;

    app.on_key(key(KeyCode::Char('n')));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task modal is open");
    };
    assert_eq!(form.choice, 0);

    app.on_key(key(KeyCode::Right));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    assert_eq!(form.choice, 1, "→ moves forward");

    app.on_key(key(KeyCode::Left));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    assert_eq!(form.choice, 0, "← moves back");

    app.on_key(key(KeyCode::Left));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    assert_eq!(
        form.choice,
        form.choices.len() - 1,
        "← wraps round to the last row"
    );

    // Space and j/k are the same idiom as everywhere else in the interface.
    app.on_key(key(KeyCode::Char(' ')));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    assert_eq!(form.choice, 0, "space wraps forward");
    app.on_key(key(KeyCode::Char('j')));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    assert_eq!(form.choice, 1, "j moves forward");
    app.on_key(key(KeyCode::Char('k')));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    assert_eq!(form.choice, 0, "k moves back");

    // What was picked is what is loaded.
    pick_workflow(&mut app, "open-mercato-sdlc");
    app.on_key(key(KeyCode::Enter));

    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open on the input form");
    };
    assert_eq!(form.stage, NewTaskStage::Inputs);
    let definition = form.definition.as_ref().expect("the definition is loaded");
    assert_eq!(definition.name, "open-mercato-sdlc");
    let resolved = form.resolved.as_ref().expect("the source is remembered");
    assert_eq!(resolved.origin.label(), "built-in");
    assert_eq!(form.reference.as_deref(), Some("open-mercato-sdlc"));
}

/// A copy shadowing a built-in is one row, marked as the override.
#[tokio::test(flavor = "multi_thread")]
async fn an_override_appears_once_and_says_what_it_overrides() {
    let root = TempRoot::new();
    fs::write(
        root_workflows(root.path()).join("compound-engineering.yaml"),
        fs::read_to_string(new_task_definition_path()).expect("read the fixture"),
    )
    .expect("install a copy that shadows the built-in");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_for(&daemon, root.path()).await;
    app.on_key(key(KeyCode::Char('n')));

    let labels = pick_labels(&app);
    let rows: Vec<&String> = labels
        .iter()
        .filter(|label| label.starts_with("compound-engineering "))
        .collect();
    assert_eq!(rows.len(), 1, "one row, not two: {labels:?}");
    assert!(
        rows[0].contains("[root, overrides built-in]"),
        "the row says it is an override: {}",
        rows[0]
    );

    // And it is the override that loads, not the built-in.
    pick_workflow(&mut app, "compound-engineering");
    app.on_key(key(KeyCode::Enter));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open");
    };
    let resolved = form.resolved.as_ref().expect("the source is remembered");
    assert_eq!(resolved.origin.label(), "root");
    assert_ne!(
        resolved.source,
        builtin("compound-engineering")
            .expect("the built-in exists")
            .source,
        "the copy is what was loaded"
    );
}

/// The arbitrary-path route still starts a definition in no search location.
#[tokio::test(flavor = "multi_thread")]
async fn the_path_route_starts_a_definition_that_is_in_no_search_location() {
    let root = TempRoot::new();
    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_for(&daemon, root.path()).await;

    app.on_key(key(KeyCode::Char('n')));
    // The last row: ← wraps straight onto it.
    app.on_key(key(KeyCode::Left));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal is open");
    };
    assert_eq!(form.selected_choice(), Some(&NewTaskChoice::Path));

    app.on_key(key(KeyCode::Enter));
    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal is open");
    };
    assert_eq!(
        form.stage,
        NewTaskStage::Definition,
        "it switches to the text line"
    );

    type_text(&mut app, &new_task_definition_path().display().to_string());
    app.on_key(key(KeyCode::Enter));

    focus_input(&mut app, "ticket");
    type_text(&mut app, "ORC-42");
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::StartNewTask);
    app.start_new_task().await;

    assert!(matches!(app.modal(), Modal::None), "the task started");
    let listing = client.control_tasks().await.expect("list tasks");
    assert_eq!(listing.tasks.len(), 1, "one task was queued");
    assert_eq!(listing.tasks[0].workflow_id, "tui-new-task");
    assert!(
        app.notice()
            .is_some_and(|notice| notice.text.contains("Queued task")),
        "the path submission was acknowledged before execution admission"
    );
}

/// Esc on the text line returns to the list rather than closing the modal.
#[tokio::test(flavor = "multi_thread")]
async fn esc_on_the_path_line_returns_to_the_list() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_for(&daemon, root.path()).await;

    app.on_key(key(KeyCode::Char('n')));
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Esc));

    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal is still open");
    };
    assert_eq!(form.stage, NewTaskStage::Pick);

    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None), "Esc then closes it");
}

/// A local git repository a declared `git` workspace can be cloned from.
fn source_repository(root: &Path) -> PathBuf {
    let path = root.join("source-repo");
    fs::create_dir_all(&path).expect("create the source repository");
    git(&path, &["init", "-q", "-b", "main", "."]);
    fs::write(path.join("README.md"), "a repository to clone\n").expect("write a file");
    git(&path, &["add", "."]);
    git(
        &path,
        &[
            "-c",
            "user.email=tests@example.test",
            "-c",
            "user.name=tests",
            "commit",
            "-qm",
            "initial",
        ],
    );
    path
}

/// Runs `git` inside a directory, failing loudly.
fn git(path: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Starting a **built-in** from the pick list queues a task using its stable
/// catalogue identity rather than making the local UI manufacture an
/// execution first.
///
#[tokio::test(flavor = "multi_thread")]
async fn starting_a_picked_builtin_queues_its_catalogue_identity() {
    let root = TempRoot::new();
    let repository = source_repository(root.path());
    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_for(&daemon, root.path()).await;
    app.on_key(key(KeyCode::Char('n')));
    pick_workflow(&mut app, "compound-engineering");
    app.on_key(key(KeyCode::Enter));

    focus_input(&mut app, "feature_description");
    type_text(&mut app, "a thing worth building");
    focus_input(&mut app, "target_repo");
    type_text(&mut app, &repository.display().to_string());
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::StartNewTask);
    app.start_new_task().await;

    let listing = client.control_tasks().await.expect("list tasks");
    assert_eq!(
        listing.tasks.len(),
        1,
        "submission notice: {:?}",
        app.notice()
    );
    assert_eq!(listing.tasks[0].workflow_id, "compound-engineering");
    assert!(listing.tasks[0].launch_digest.len() > 10);
}

/// A definition that fails validation is refused with every violation.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_definition_is_refused_with_every_violation() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_new_task(&mut app, &broken_definition_path());

    let Modal::NewTask(form) = app.modal() else {
        panic!("the modal stays open on an invalid definition");
    };
    assert_eq!(
        form.stage,
        NewTaskStage::Definition,
        "no input form is built"
    );
    assert!(form.definition.is_none());
    assert!(
        form.violations.len() > 1,
        "every violation is listed, not only the first: {:?}",
        form.violations
    );
    assert!(
        form.violations
            .iter()
            .any(|violation| violation.contains("nowhere")),
        "the unknown transition target is reported: {:?}",
        form.violations
    );
    assert!(
        form.violations
            .iter()
            .any(|violation| violation.contains("undeclared_input")),
        "the unknown input reference is reported: {:?}",
        form.violations
    );

    assert!(
        store_of(root.path())
            .list_execution_ids()
            .expect("list the store")
            .is_empty(),
        "nothing was created"
    );
}

/// The input form is generated from the definition and enforces `required`.
#[tokio::test(flavor = "multi_thread")]
async fn the_input_form_is_built_from_the_definition_and_enforces_required_inputs() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_new_task(&mut app, &new_task_definition_path());

    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task form is open");
    };
    assert_eq!(form.stage, NewTaskStage::Inputs);
    let names: Vec<&str> = form
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    assert_eq!(names, vec!["ticket", "is_experimental", "repository"]);

    // `ticket` is required and nothing is typed into it.
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::StartNewTask);
    app.start_new_task().await;

    let Modal::NewTask(form) = app.modal() else {
        panic!("the form stays open so the operator can fill the field in");
    };
    let error = form.error.as_deref().expect("the form reports the failure");
    assert!(
        error.contains("ticket"),
        "the error names the input: {error}"
    );
    assert!(
        store_of(root.path())
            .list_execution_ids()
            .expect("list the store")
            .is_empty(),
        "no execution was created"
    );
}

/// A `repository` input offers the registered ids instead of a blank line.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_input_offers_the_registered_ids() {
    let root = TempRoot::new();
    let registry = registry_of(root.path());
    registry
        .add(RepositoryEntry::new(
            "zeta",
            "https://example.test/zeta.git",
        ))
        .expect("register zeta");
    registry
        .add(RepositoryEntry::new(
            "acme",
            "https://example.test/acme.git",
        ))
        .expect("register acme");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_new_task(&mut app, &new_task_definition_path());
    let row = input_row(&app, "repository");

    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task form is open");
    };
    let choices = form
        .choices(row)
        .expect("a repository input is a pick list, not a blank line");
    assert_eq!(choices, ["acme", "zeta"], "the registry's own order");
    assert_eq!(form.values[row], "acme", "it starts on the first id");
}

/// A boolean input is accepted into the queued task submission without
/// forcing the local execution-first UI to invent an execution id.
#[tokio::test(flavor = "multi_thread")]
async fn a_boolean_input_reaches_the_context_as_a_json_boolean() {
    let root = TempRoot::new();
    let (daemon, _client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_new_task(&mut app, &new_task_definition_path());
    focus_input(&mut app, "ticket");
    type_text(&mut app, "ORC-9");

    focus_input(&mut app, "is_experimental");
    let Modal::NewTask(form) = app.modal() else {
        panic!("the new-task form is open");
    };
    assert_eq!(
        form.choices(form.row).expect("a boolean is a pick list"),
        ["true", "false"]
    );
    assert_eq!(form.values[form.row], "false", "the default is respected");
    // ← steps onto `true`.
    app.on_key(key(KeyCode::Left));

    app.on_key(key(KeyCode::Enter));
    app.start_new_task().await;

    assert!(
        app.notice()
            .is_some_and(|notice| notice.text.contains("Queued task")),
        "creation acknowledges a task before an execution exists"
    );
    let task_id = app
        .queued_task()
        .expect("the submitted task remains selected")
        .task_id
        .clone();
    let task = ControlPlane::new(StatePaths::new(root.path()))
        .task(&task_id)
        .expect("read sealed task");
    assert_eq!(task.launch_spec.inputs["ticket"], json!("ORC-9"));
    assert_eq!(task.launch_spec.inputs["is_experimental"], json!(true));
}

/// A valid new task is immediately selected in the task-first screen without
/// inventing an execution row in the legacy execution-only panel.
#[tokio::test(flavor = "multi_thread")]
async fn starting_a_valid_new_task_is_queued_without_selecting_an_execution() {
    let root = TempRoot::new();
    // An unrelated execution, so "selected" means something.
    let existing = {
        let engine = engine_for(root.path());
        start(&engine, root.path())
    };

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(app.executions().len(), 1);

    open_new_task(&mut app, &new_task_definition_path());
    focus_input(&mut app, "ticket");
    type_text(&mut app, "ORC-11");
    app.on_key(key(KeyCode::Enter));
    app.start_new_task().await;

    assert!(
        matches!(app.modal(), Modal::None),
        "the modal closed on acknowledgement"
    );
    assert_eq!(
        app.executions().len(),
        1,
        "no queued execution placeholder exists"
    );
    assert_eq!(app.selected().expect("existing selection").id, existing.id);
    assert_eq!(app.focus(), Panel::Executions);
    assert!(app
        .notice()
        .is_some_and(|notice| notice.text.contains("Queued task")));

    let listing = client.control_tasks().await.expect("list queued tasks");
    let task = listing.tasks.first().expect("one durable task");
    assert_eq!(task.workflow_id, "tui-new-task");
    assert_eq!(task.priority, 0);
    assert_eq!(
        app.queued_task().map(|task| task.task_id.as_str()),
        Some(task.task_id.as_str()),
        "the acknowledgement selects its task identity"
    );
    let screen = screen_of(&mut app);
    assert!(screen.contains("Queued task"));
    assert!(screen.contains(task.task_id.as_str()));
}

/// A definition declaring `globals.workspace` starts from the modal, and its
/// workspace is provisioned — the interface no longer sends the operator to
/// `narsor run` for it (now: the daemon provisions it, on the other end of
/// `client.start_task`).
#[tokio::test(flavor = "multi_thread")]
async fn a_definition_declaring_a_workspace_starts_from_the_modal_and_is_provisioned() {
    let root = TempRoot::new();
    // The directory the declared `local` workspace is copied from.
    let source = root.path().join("source");
    fs::create_dir_all(&source).expect("create the source tree");
    fs::write(source.join("README.md"), "hello\n").expect("write readme");

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    open_new_task(&mut app, &fixtures().join("new_task_workspace.yaml"));
    focus_input(&mut app, "source");
    type_text(&mut app, &source.display().to_string());
    app.on_key(key(KeyCode::Enter));
    app.start_new_task().await;

    assert!(
        matches!(app.modal(), Modal::None),
        "the definition was accepted, not refused"
    );
    let notice = app.notice().expect("a notice");
    assert!(!notice.is_error, "and it started: {}", notice.text);

    let task = client
        .control_tasks()
        .await
        .expect("list queued tasks")
        .tasks
        .into_iter()
        .next()
        .expect("the task was queued");
    assert_eq!(task.workflow_id, "tui-new-task-workspace");

    // The runner receives the sealed local path only after scheduler
    // admission. Its backend-owned storage, rather than a synthetic legacy
    // execution, is the authoritative workspace location.
    let workspace = root
        .path()
        .join("local-controller")
        .join("storage")
        .join(task.task_id.as_str())
        .join("workspace");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !workspace.join("README.md").is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "local runner did not provision {}",
            workspace.display(),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        fs::read_to_string(workspace.join("README.md")).expect("read the copy"),
        "hello\n"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let projected = client
            .control_task(task.task_id.as_str())
            .await
            .expect("read task projection")
            .runner
            .and_then(|runner| runner.execution_cursor.execution_id);
        if projected.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "runner never projected an execution"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    app.refresh().await;
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::OpenTaskView);
    app.open_task_for_execution().await;
    assert!(matches!(app.screen(), Screen::Task(_)));
}

/// Esc leaves every new modal without writing anything.
#[tokio::test(flavor = "multi_thread")]
async fn esc_cancels_every_modal_without_writing_anything() {
    let root = TempRoot::new();
    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    // The repository form, half filled in.
    open_repository_form(&mut app);
    fill_repository_field(&mut app, RepositoryField::Id, "abandoned");
    fill_repository_field(
        &mut app,
        RepositoryField::Remote,
        "https://example.test/x.git",
    );
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None));
    assert!(
        registry_of(root.path())
            .list()
            .expect("read the registry")
            .is_empty(),
        "the abandoned form registered nothing"
    );

    // The listing.
    app.on_key(key(KeyCode::Char('R')));
    assert!(matches!(app.modal(), Modal::Repositories(_)));
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None));

    // The removal confirmation.
    registry_of(root.path())
        .add(RepositoryEntry::new(
            "kept",
            "https://example.test/kept.git",
        ))
        .expect("register");
    app.on_key(key(KeyCode::Char('R')));
    app.on_key(key(KeyCode::Char('d')));
    assert!(matches!(app.modal(), Modal::RepositoryRemove(_)));
    app.on_key(key(KeyCode::Esc));
    assert_eq!(
        registry_of(root.path()).list().expect("list").entries.len(),
        1,
        "the cancelled removal kept the entry"
    );

    // The new-task modal, with a loaded definition and a filled-in input.
    app.on_key(key(KeyCode::Esc));
    open_new_task(&mut app, &new_task_definition_path());
    focus_input(&mut app, "ticket");
    type_text(&mut app, "ORC-13");
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None));
    assert!(
        store_of(root.path())
            .list_execution_ids()
            .expect("list the store")
            .is_empty(),
        "the abandoned form created no execution"
    );
}

// ---------------------------------------------------------------------------
// U6: readable events and the event detail window
// ---------------------------------------------------------------------------

/// One instance of every [`EventData`] variant, with plausible field values.
///
/// Used to prove `event_line` has no leftover catch-all: a variant added here
/// without a matching hand-written arm would fail to compile, and this list
/// is the thing that would need updating to prove it — see
/// `event_line_has_no_debug_style_dump_for_any_variant`.
fn every_event_data_variant() -> Vec<EventData> {
    vec![
        EventData::WorkflowStarted {
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            inputs: json!({"ticket": "ORC-7"}),
        },
        EventData::WorkflowPaused {
            reason: "waiting on a human".to_string(),
        },
        EventData::WorkflowResumed,
        EventData::WorkflowCompleted { result: json!({}) },
        EventData::WorkflowFailed {
            error: "boom".to_string(),
        },
        EventData::WorkflowCancelled {
            reason: "operator".to_string(),
        },
        EventData::StateEntered {
            state_id: "prepare".to_string(),
        },
        EventData::StateExited {
            state_id: "prepare".to_string(),
            output_label: "success".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "a1".to_string(),
            state_id: "prepare".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionCompleted {
            action_execution_id: "a1".to_string(),
            output_label: "success".to_string(),
        },
        EventData::ActionFailed {
            action_execution_id: "a1".to_string(),
            error: "boom".to_string(),
        },
        EventData::ActionAttemptFailed {
            action_execution_id: "a1".to_string(),
            state_id: "prepare".to_string(),
            error: "boom".to_string(),
            retry_in_ms: 500,
        },
        EventData::ActionRetrying {
            action_execution_id: "a2".to_string(),
            retry_count: 1,
            state_id: Some("prepare".to_string()),
        },
        EventData::AgentSessionStarted {
            session_id: "sess".to_string(),
            harness: "claude_code".to_string(),
            action_execution_id: Some("a1".to_string()),
            resumed_session: None,
        },
        EventData::AgentSessionCompleted {
            session_id: "sess".to_string(),
            action_execution_id: Some("a1".to_string()),
            harness_session_id: Some("hs1".to_string()),
        },
        EventData::AgentSessionFailed {
            session_id: "sess".to_string(),
            error: "boom".to_string(),
            action_execution_id: Some("a1".to_string()),
            harness_session_id: None,
        },
        EventData::AgentMessage {
            session_id: "sess".to_string(),
            content: "hello there".to_string(),
            role: "assistant".to_string(),
            action_execution_id: Some("a1".to_string()),
        },
        EventData::DecisionRequested {
            provider: "claude".to_string(),
        },
        EventData::DecisionMade {
            provider: "claude".to_string(),
            output_label: "success".to_string(),
            reasoning: None,
        },
        EventData::HumanInputRequested {
            instruction: "approve?".to_string(),
            question: None,
            answer_tail: None,
            episode: None,
        },
        EventData::human_input_received(json!({"approval_status": "approved"})),
        EventData::HumanTransitionSelected {
            from_state: "review".to_string(),
            to_state: "done".to_string(),
        },
        EventData::ExternalEventReceived {
            event_type: "hook".to_string(),
            source: "github".to_string(),
            payload: json!({}),
        },
        EventData::TransitionTaken {
            from_state: "review".to_string(),
            to_state: "done".to_string(),
            output_label: "success".to_string(),
        },
        EventData::ArtifactCreated {
            artifact_id: "art1".to_string(),
            artifact_type: "file".to_string(),
            location: "artifacts/art1".to_string(),
        },
        EventData::Error {
            message: "boom".to_string(),
            context: json!({}),
        },
    ]
}

/// AE1: a `workflow_started` event with a dozen-plus inputs stays a single
/// line in the stream, and each input gets its own line of indented JSON in
/// the detail window.
#[test]
fn workflow_started_line_is_single_line_and_detail_expands_every_input() {
    let mut inputs = serde_json::Map::new();
    for index in 0..14 {
        inputs.insert(format!("field_{index}"), json!(format!("value-{index}")));
    }
    let event = Event::new(
        "exec-1".to_string(),
        EventData::WorkflowStarted {
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            inputs: Value::Object(inputs),
        },
        1,
    );

    let line = event_line(&event);
    assert_eq!(line.lines().count(), 1, "the stream line stays one line");

    let detail = event_detail_lines(&event);
    for index in 0..14 {
        let needle = format!("field_{index}");
        assert!(
            detail.iter().any(|line| line.contains(&needle)),
            "input {needle} is present in the detail window"
        );
    }
}

/// Edge case: `event_line` never leaks a Rust `Debug` dump, for any variant —
/// every hand-written arm in the function stays free of `{`, which a derived
/// `Debug` for a struct-like variant always contains (e.g. `AgentMessage {`).
#[test]
fn event_line_has_no_debug_style_dump_for_any_variant() {
    for (index, data) in every_event_data_variant().into_iter().enumerate() {
        let event = Event::new("exec-1".to_string(), data, index as u64);
        let line = event_line(&event);
        assert!(
            !line.contains('{'),
            "event_line leaked a Debug-style dump: {line:?}"
        );
    }
}

/// `Enter` in the Events panel opens the detail window on the selected
/// event, and `Esc` closes it with the selection preserved.
#[tokio::test(flavor = "multi_thread")]
async fn enter_opens_event_detail_and_esc_preserves_selection() {
    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert!(!app.events().is_empty(), "the backfill read some events");

    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus(), Panel::Events);

    let cursor_before = app.event_cursor();
    app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(app.modal(), Modal::EventDetail(_)),
        "Enter opened the detail window"
    );

    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None), "Esc closed it");
    assert_eq!(
        app.event_cursor(),
        cursor_before,
        "the selection survived the round trip"
    );
}

/// Scrolling in the detail window (`j`/`k`/`PgDn`/`G`) moves the offset and
/// never past the content's own bounds.
#[tokio::test(flavor = "multi_thread")]
async fn scrolling_the_event_detail_window_stays_within_content_bounds() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    // A payload large enough that the pretty-printed detail window has many
    // lines to scroll through.
    let mut context = serde_json::Map::new();
    for index in 0..200 {
        context.insert(format!("k{index}"), json!(index));
    }
    let last_sequence = app.events().last().expect("an event").sequence;
    let event = Event::new(
        execution_id,
        EventData::Error {
            message: "boom".to_string(),
            context: Value::Object(context),
        },
        last_sequence + 1,
    );
    app.on_stream(Some(Ok(event)));

    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Enter));

    let offset_of = |app: &App| match app.modal() {
        Modal::EventDetail(detail) => detail.scroll.offset(),
        other => panic!("expected the detail window to be open, got {other:?}"),
    };

    assert_eq!(offset_of(&app), 0);

    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(offset_of(&app), 1, "one line down");

    app.on_key(key(KeyCode::PageDown));
    let after_page = offset_of(&app);
    assert!(after_page > 1, "a page moves further than a line");

    app.on_key(key(KeyCode::Char('G')));
    let bottom = offset_of(&app);
    assert!(bottom >= after_page);

    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(offset_of(&app), bottom, "clamped at the last line");

    app.on_key(key(KeyCode::Char('g')));
    assert_eq!(offset_of(&app), 0, "g jumps back to the top");
}

/// Covers AE3: `c` on an event outside an agent conversation (here,
/// `human_input_received`) changes nothing on screen and names the reason in
/// the message bar.
#[tokio::test(flavor = "multi_thread")]
async fn c_on_an_unlinked_event_names_the_reason_and_opens_nothing() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    let last_sequence = app.events().last().expect("an event").sequence;
    let event = Event::new(
        execution_id,
        EventData::human_input_received(json!({"approval_status": "approved"})),
        last_sequence + 1,
    );
    app.on_stream(Some(Ok(event)));

    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus(), Panel::Events);
    assert!(matches!(
        &app.events().last().expect("an event").data,
        EventData::HumanInputReceived { .. }
    ));

    let request = app.on_key(key(KeyCode::Char('c')));
    assert_eq!(request, Request::None);
    assert!(matches!(app.modal(), Modal::None), "no screen change");

    let notice = app.notice().expect("a notice");
    assert!(notice.is_error);
    assert!(
        notice
            .text
            .contains("does not belong to an agent conversation"),
        "got: {}",
        notice.text
    );
}

/// Edge case: content with ANSI escapes and control characters reaches the
/// detail window with no raw control bytes left in it.
#[test]
fn event_detail_lines_strip_ansi_and_control_characters() {
    let event = Event::new(
        "exec-1".to_string(),
        EventData::Error {
            message: "boom\u{1b}[31mRED\u{1b}[0m\u{7}".to_string(),
            context: json!({"raw": "col1\tcol2\u{7}tail"}),
        },
        1,
    );

    let detail = event_detail_lines(&event);
    for line in &detail {
        assert!(
            !line.chars().any(|c| c.is_control() && c != '\n'),
            "a raw control byte survived sanitization: {line:?}"
        );
    }
    assert!(
        detail.iter().any(|line| line.contains("RED")),
        "the visible text around the escape codes is still there"
    );
}

/// Edge case: an `AgentMessage` whose content the executor truncated is
/// flagged in the detail window.
#[test]
fn truncated_agent_message_is_flagged_in_the_detail_window() {
    let content = format!("partial output{TRUNCATION_MARKER}");
    let event = Event::new(
        "exec-1".to_string(),
        EventData::AgentMessage {
            session_id: "sess".to_string(),
            content,
            role: "assistant".to_string(),
            action_execution_id: Some("a1".to_string()),
        },
        1,
    );

    let detail = event_detail_lines(&event);
    assert!(
        detail
            .iter()
            .any(|line| line.to_lowercase().contains("truncated")),
        "the truncation is flagged: {detail:?}"
    );
    assert!(
        detail.iter().any(|line| line.contains("press c")),
        "the window points at the c key: {detail:?}"
    );
}

// ---------------------------------------------------------------------------
// the full-screen task view (U7)
// ---------------------------------------------------------------------------

/// A minimal, directly-saved execution — the same shortcut `tests/api.rs`
/// takes, reused here so the task view has something real to open without
/// spawning an actual harness process. Only ever written before a daemon
/// exists for the root (see the module doc).
fn seeded_execution(id: &str, status: ExecutionStatus) -> WorkflowExecution {
    WorkflowExecution {
        id: id.to_string(),
        workflow_id: "implement-feature".to_string(),
        workflow_version: 1,
        status,
        current_state: Some("plan".to_string()),
        started_at: chrono::Utc::now(),
        completed_at: None,
        inputs: json!({}),
        context: narsor::domain::Context::new(),
        workspace: Workspace::local_folder("/tmp/workspace".to_string()),
        current_action_execution: None,
        metadata: json!({}),
    }
}

/// Seeds `store` with an execution whose `plan` step failed once and
/// succeeded on retry, each attempt with its own conversation record —
/// mirrors `tests/api.rs`'s fixture (AE2/AE7/F1/F3). Pre-daemon only.
fn seed_retried_agent_execution(store: &ExecutionStore, execution_id: &str) {
    store
        .save_execution(&seeded_execution(execution_id, ExecutionStatus::Completed))
        .expect("save execution");

    let log = store.event_log(execution_id);
    log.append_next(
        &execution_id.to_string(),
        EventData::WorkflowStarted {
            workflow_id: "implement-feature".to_string(),
            workflow_version: 1,
            inputs: json!({}),
        },
    )
    .expect("append WorkflowStarted");
    log.append_next(
        &execution_id.to_string(),
        EventData::StateEntered {
            state_id: "plan".to_string(),
        },
    )
    .expect("append StateEntered");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionStarted {
            action_execution_id: "attempt-1".to_string(),
            state_id: "plan".to_string(),
            action_type: "agent".to_string(),
        },
    )
    .expect("append ActionStarted 1");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionAttemptFailed {
            action_execution_id: "attempt-1".to_string(),
            state_id: "plan".to_string(),
            error: "boom".to_string(),
            retry_in_ms: 100,
        },
    )
    .expect("append ActionAttemptFailed");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionRetrying {
            action_execution_id: "attempt-2".to_string(),
            retry_count: 1,
            state_id: Some("plan".to_string()),
        },
    )
    .expect("append ActionRetrying");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionCompleted {
            action_execution_id: "attempt-2".to_string(),
            output_label: "success".to_string(),
        },
    )
    .expect("append ActionCompleted");

    let conversations = store.conversation_store(execution_id);
    let mut first = conversations.writer("attempt-1").expect("open writer 1");
    first
        .append_narsor("prompt: try the thing")
        .expect("append prompt 1");
    first
        .append(ConversationSource::Stdout, br#"{"type":"error"}"#)
        .expect("append stdout 1");
    first.finish().expect("finish writer 1");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt 1");

    let mut second = conversations.writer("attempt-2").expect("open writer 2");
    second
        .append_narsor("prompt: try again")
        .expect("append prompt 2");
    second
        .append(
            ConversationSource::Stdout,
            br#"{"type":"thread.started","thread_id":"thread-1"}"#,
        )
        .expect("append lifecycle event 2");
    second
        .append(
            ConversationSource::Stdout,
            br#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"done"}]}}"#,
        )
        .expect("append stdout 2");
    second.finish().expect("finish writer 2");
    conversations
        .close_attempt("attempt-2")
        .expect("close attempt 2");
}

/// The `Screen::Task` view, or a panic naming what was open instead.
fn task_view(app: &App) -> &narsor::tui::app::TaskView {
    match app.screen() {
        Screen::Task(view) => view,
        Screen::Main | Screen::Queued(_) => panic!("expected the task view to be open"),
    }
}

fn task_view_mut(app: &mut App) -> &mut narsor::tui::app::TaskView {
    app.task_view_mut().expect("task view")
}

/// Covers AE2/F1: selecting the second attempt's `action_failed`-adjacent
/// event and pressing `c` opens the task view on `plan #2`, with that
/// attempt's own conversation.
#[tokio::test(flavor = "multi_thread")]
async fn c_on_a_second_attempt_event_opens_its_position_and_conversation() {
    let root = TempRoot::new();
    seed_retried_agent_execution(&store_of(root.path()), "exec-1");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    let completed_sequence = app
        .events()
        .iter()
        .find(|event| matches!(event.data, EventData::ActionCompleted { .. }))
        .expect("the second attempt's ActionCompleted event")
        .sequence;
    // The default cursor after a refresh is the newest event, which here is
    // exactly attempt-2's `ActionCompleted` — the last event appended.
    assert_eq!(
        app.events()[app.event_cursor()].sequence,
        completed_sequence
    );

    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Tab));
    let request = app.on_key(key(KeyCode::Char('c')));
    assert_eq!(
        request,
        Request::OpenConversation {
            sequence: completed_sequence
        }
    );
    app.open_conversation_for_event(completed_sequence).await;

    let view = task_view(&app);
    assert_eq!(view.selected.as_deref(), Some("plan #2"));
    match &view.content {
        TaskContent::Conversation {
            entries,
            transcript,
            ..
        } => {
            assert!(
                entries.iter().any(|entry| entry.summary.contains("done")),
                "attempt-2's own conversation was loaded: {entries:?}"
            );
            let rendered = transcript
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>();
            assert!(
                rendered.iter().any(|line| line == "thread started"),
                "readable Codex lifecycle text is visible without expansion: {rendered:?}"
            );
            assert!(
                !rendered
                    .iter()
                    .any(|line| line.contains("system diagnostic")),
                "technical entries should not be replaced by diagnostic placeholders: {rendered:?}"
            );
        }
        other => panic!("expected a loaded conversation, got {other:?}"),
    }
}

/// `Enter`/space on the content pane (R10): the entry under the cursor
/// expands, and pressing it again on the same entry collapses it back.
#[tokio::test(flavor = "multi_thread")]
async fn expanding_a_conversation_entry_toggles_and_collapses_again() {
    let root = TempRoot::new();
    seed_retried_agent_execution(&store_of(root.path()), "exec-expand");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    let completed_sequence = app
        .events()
        .iter()
        .find(|event| matches!(event.data, EventData::ActionCompleted { .. }))
        .expect("the second attempt's ActionCompleted event")
        .sequence;
    app.open_conversation_for_event(completed_sequence).await;

    let entry_id = match &task_view(&app).content {
        TaskContent::Conversation { transcript, .. } => {
            transcript
                .anchor()
                .expect("attempt-2 has at least one entry")
                .entry_id
        }
        other => panic!("expected a loaded conversation, got {other:?}"),
    };
    assert!(
        !matches!(&task_view(&app).content, TaskContent::Conversation { transcript, .. } if transcript.is_expanded(&entry_id)),
        "entry should start collapsed"
    );

    // One `Tab` moves focus from the axis (where `c` leaves it selected) to
    // the content pane; `Enter` there toggles the entry under the cursor.
    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(&task_view(&app).content, TaskContent::Conversation { transcript, .. } if transcript.is_expanded(&entry_id)),
        "first Enter should expand the transcript entry"
    );
    if let TaskContent::Conversation { transcript, .. } = &task_view(&app).content {
        let expanded = transcript
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();
        assert!(expanded.iter().any(|line| {
            line.contains("message.content")
                || line.contains("event.type")
                || line.contains("workflow.event")
        }));
        assert!(!expanded
            .iter()
            .any(|line| line.contains("\"type\":\"assistant\"")));
    }

    app.on_key(key(KeyCode::Enter));
    assert!(
        !matches!(&task_view(&app).content, TaskContent::Conversation { transcript, .. } if transcript.is_expanded(&entry_id)),
        "second Enter should collapse the transcript entry"
    );

    app.on_key(key(KeyCode::Char('v')));
    assert!(
        matches!(&task_view(&app).content, TaskContent::Conversation { transcript, .. } if transcript.mode() == DisplayMode::Raw),
        "content focus switches the same transcript to diagnostic raw mode"
    );
    if let TaskContent::Conversation { transcript, .. } = &task_view(&app).content {
        let raw = transcript
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();
        assert!(raw
            .iter()
            .any(|line| line.contains("\"type\":\"assistant\"")));
    }
    assert!(
        app.shortcuts()
            .iter()
            .any(|(key, description)| *key == "v" && *description == "raw/rendered"),
        "the focused content bar documents raw mode"
    );
}

/// A continued harness session (D18) is named in the axis line, not just
/// carried silently on the `Position`.
#[tokio::test(flavor = "multi_thread")]
async fn a_continued_harness_session_is_named_in_the_position_header() {
    let root = TempRoot::new();
    let execution_id = "exec-resume";
    {
        let store = store_of(root.path());
        store
            .save_execution(&seeded_execution(execution_id, ExecutionStatus::Completed))
            .expect("save execution");

        let log = store.event_log(execution_id);
        log.append_next(
            &execution_id.to_string(),
            EventData::WorkflowStarted {
                workflow_id: "implement-feature".to_string(),
                workflow_version: 1,
                inputs: json!({}),
            },
        )
        .expect("append WorkflowStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::StateEntered {
                state_id: "plan".to_string(),
            },
        )
        .expect("append StateEntered");
        log.append_next(
            &execution_id.to_string(),
            EventData::ActionStarted {
                action_execution_id: "attempt-1".to_string(),
                state_id: "plan".to_string(),
                action_type: "agent".to_string(),
            },
        )
        .expect("append ActionStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::AgentSessionStarted {
                session_id: "sess-new".to_string(),
                harness: "claude_code".to_string(),
                action_execution_id: Some("attempt-1".to_string()),
                resumed_session: Some("sess-old".to_string()),
            },
        )
        .expect("append AgentSessionStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::ActionCompleted {
                action_execution_id: "attempt-1".to_string(),
                output_label: "success".to_string(),
            },
        )
        .expect("append ActionCompleted");
    }

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.open_task_for_execution().await;

    let view = task_view(&app);
    let position = view
        .timeline
        .positions
        .iter()
        .find(|position| position.label == "plan #1")
        .expect("plan #1 position");
    assert_eq!(
        position.resumed_harness_session.as_deref(),
        Some("sess-old")
    );
    assert!(
        task_position_line(position).contains("resumes"),
        "the position header should name the continued session: {}",
        task_position_line(position)
    );
}

/// The over-linking U6 flagged: a script step's own `ActionStarted` carries
/// an `action_execution_id` (so the fast, event-only check lets it through),
/// but the real timeline says it is not an agent action, so `c` still opens
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn c_on_a_script_step_event_opens_nothing() {
    let root = TempRoot::new();
    let execution_id = "exec-script";
    {
        let store = store_of(root.path());
        store
            .save_execution(&seeded_execution(execution_id, ExecutionStatus::Completed))
            .expect("save execution");
        let log = store.event_log(execution_id);
        log.append_next(
            &execution_id.to_string(),
            EventData::WorkflowStarted {
                workflow_id: "implement-feature".to_string(),
                workflow_version: 1,
                inputs: json!({}),
            },
        )
        .expect("append WorkflowStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::StateEntered {
                state_id: "prepare".to_string(),
            },
        )
        .expect("append StateEntered");
        log.append_next(
            &execution_id.to_string(),
            EventData::ActionStarted {
                action_execution_id: "script-1".to_string(),
                state_id: "prepare".to_string(),
                action_type: "script".to_string(),
            },
        )
        .expect("append ActionStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::ActionCompleted {
                action_execution_id: "script-1".to_string(),
                output_label: "success".to_string(),
            },
        )
        .expect("append ActionCompleted");
    }

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    let started_sequence = app
        .events()
        .iter()
        .find(|event| matches!(event.data, EventData::ActionStarted { .. }))
        .expect("the script step's ActionStarted event")
        .sequence;

    app.open_conversation_for_event(started_sequence).await;

    assert!(
        matches!(app.screen(), Screen::Main),
        "a script step never opens the task view via c"
    );
    let notice = app.notice().expect("a notice");
    assert!(notice.is_error);
    assert!(
        notice
            .text
            .contains("does not belong to an agent conversation"),
        "got: {}",
        notice.text
    );
}

/// `Enter` on the execution list opens the task view at the timeline's
/// default position, and `Esc` returns to the execution list.
#[tokio::test(flavor = "multi_thread")]
async fn enter_on_an_execution_opens_the_task_view_and_esc_returns() {
    let root = TempRoot::new();
    seed_retried_agent_execution(&store_of(root.path()), "exec-1");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(app.focus(), Panel::Executions);

    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::OpenTaskView);
    app.open_task_for_execution().await;

    let view = task_view(&app);
    assert_eq!(
        view.selected.as_deref(),
        Some("plan #2"),
        "the last position is the default"
    );

    app.on_key(key(KeyCode::Esc));
    assert!(
        matches!(app.screen(), Screen::Main),
        "Esc returns to the main screen"
    );
}

/// Covers AE6: an execution whose agent session events predate
/// `action_execution_id` (an old-format log) shows "no conversation record"
/// plus the position's own events, not an error.
#[tokio::test(flavor = "multi_thread")]
async fn an_old_format_log_shows_no_conversation_record() {
    let root = TempRoot::new();
    let execution_id = "exec-old";
    {
        let store = store_of(root.path());
        store
            .save_execution(&seeded_execution(execution_id, ExecutionStatus::Completed))
            .expect("save execution");
        let log = store.event_log(execution_id);
        log.append_next(
            &execution_id.to_string(),
            EventData::WorkflowStarted {
                workflow_id: "implement-feature".to_string(),
                workflow_version: 1,
                inputs: json!({}),
            },
        )
        .expect("append WorkflowStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::StateEntered {
                state_id: "plan".to_string(),
            },
        )
        .expect("append StateEntered");
        log.append_next(
            &execution_id.to_string(),
            EventData::ActionStarted {
                action_execution_id: "attempt-1".to_string(),
                state_id: "plan".to_string(),
                action_type: "agent".to_string(),
            },
        )
        .expect("append ActionStarted");
        // An old-format log: the agent session events carry no
        // `action_execution_id` at all (`#[serde(default)]` from U1 means these
        // deserialize as `None`, which is what a pre-U1 log on disk would give).
        log.append_next(
            &execution_id.to_string(),
            EventData::AgentSessionStarted {
                session_id: "sess-1".to_string(),
                harness: "claude_code".to_string(),
                action_execution_id: None,
                resumed_session: None,
            },
        )
        .expect("append AgentSessionStarted");
        log.append_next(
            &execution_id.to_string(),
            EventData::ActionCompleted {
                action_execution_id: "attempt-1".to_string(),
                output_label: "success".to_string(),
            },
        )
        .expect("append ActionCompleted");
    }

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    app.open_task_for_execution().await;
    let view = task_view(&app);
    match &view.content {
        TaskContent::Events { message, events } => {
            assert_eq!(message, "no conversation record");
            assert!(
                !events.is_empty(),
                "the position's own events are still shown"
            );
        }
        other => panic!("expected the events fallback, got {other:?}"),
    }
}

/// The bug this test is named after: with only a handful of positions (far
/// fewer than the axis pane's height), `j`/`k` used to freeze the selection
/// at the first position forever, because the axis cursor reused
/// `ScrollState`'s scroll-viewport clamp (`total.saturating_sub(viewport)`,
/// which collapses to `0` here) instead of a selection cursor's
/// (`total - 1`). This drives the cursor across every position, in both
/// directions, and checks it actually lands on each one.
#[tokio::test(flavor = "multi_thread")]
async fn moving_the_axis_cursor_visits_every_position() {
    let root = TempRoot::new();
    seed_retried_agent_execution(&store_of(root.path()), "exec-axis-cursor");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.open_task_for_execution().await;

    // The default selection is the last position.
    assert_eq!(task_view(&app).selected.as_deref(), Some("plan #2"));

    app.on_key(key(KeyCode::Char('k')));
    app.load_task_position().await;
    assert_eq!(task_view(&app).selected.as_deref(), Some("plan #1"));

    // Moving up again must not move past the first position.
    app.on_key(key(KeyCode::Char('k')));
    app.load_task_position().await;
    assert_eq!(task_view(&app).selected.as_deref(), Some("plan #1"));

    app.on_key(key(KeyCode::Char('j')));
    app.load_task_position().await;
    assert_eq!(task_view(&app).selected.as_deref(), Some("plan #2"));

    // `g`/`G` jump to the first/last position directly.
    app.on_key(key(KeyCode::Char('g')));
    app.load_task_position().await;
    assert_eq!(task_view(&app).selected.as_deref(), Some("plan #1"));

    app.on_key(key(KeyCode::Char('G')));
    app.load_task_position().await;
    assert_eq!(task_view(&app).selected.as_deref(), Some("plan #2"));
}

// ---------------------------------------------------------------------------
// U1: shared transcript presentation state
// ---------------------------------------------------------------------------

fn transcript_entry(id: &str, lines: &[&str]) -> TranscriptEntry {
    TranscriptEntry::new(
        TranscriptEntryId::from(id),
        lines.iter().map(|line| (*line).to_string()).collect(),
        id.as_bytes().to_vec(),
    )
}

#[test]
fn transcript_scrolls_logical_lines_without_selecting_entries() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![transcript_entry(
        "one",
        &[
            "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "line 6", "line 7",
            "line 8", "line 9",
        ],
    )]);
    transcript.set_viewport(3);

    transcript.to_top();
    transcript.line_down();
    assert_eq!(transcript.scroll_offset(), 1);
    transcript.page_down();
    assert_eq!(transcript.scroll_offset(), 4);
    transcript.to_bottom();
    assert_eq!(transcript.scroll_offset(), 7);
    transcript.page_up();
    assert_eq!(transcript.scroll_offset(), 4);
    transcript.to_top();
    assert_eq!(transcript.scroll_offset(), 0);
}

#[test]
fn transcript_opens_at_end_and_preserves_an_older_anchor_on_append() {
    let mut empty = TranscriptState::new(TranscriptId::from("empty-attempt"));
    empty.load(Vec::new());
    assert!(empty.is_following_end());
    assert_eq!(empty.scroll_offset(), 0);
    empty.append(vec![transcript_entry("first", &["first line"])]);
    assert!(empty.is_following_end());

    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.set_viewport(2);
    transcript.load(vec![
        transcript_entry("one", &["one-a", "one-b"]),
        transcript_entry("two", &["two-a", "two-b"]),
    ]);
    assert_eq!(transcript.scroll_offset(), 3);
    assert!(transcript.is_following_end());

    transcript.append(vec![transcript_entry("three", &["three-a"])]);
    assert_eq!(transcript.scroll_offset(), 5);
    assert!(transcript.is_following_end());

    transcript.to_top();
    transcript.line_down();
    let anchor = transcript.anchor().expect("anchor while reading");
    assert_eq!(anchor.entry_id, TranscriptEntryId::from("one"));
    assert_eq!(anchor.logical_line, 1);
    assert!(!transcript.is_following_end());

    transcript.append(vec![transcript_entry("four", &["four-a"])]);
    assert_eq!(transcript.anchor(), Some(anchor));
    assert_eq!(transcript.scroll_offset(), 1);
}

#[test]
fn transcript_reanchors_across_detail_mode_resize_and_missing_entry() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.set_viewport(2);
    transcript.load(vec![
        transcript_entry("one", &["one-a", "one-b"]),
        transcript_entry("two", &["two-a", "two-b"])
            .with_technical_lines(vec!["detail-a".into(), "detail-b".into()]),
    ]);
    transcript.to_top();
    transcript.line_down();
    let anchor = transcript.anchor().unwrap();

    transcript.toggle_expanded(&TranscriptEntryId::from("two"));
    assert_eq!(transcript.anchor(), Some(anchor));
    transcript.set_mode(DisplayMode::Raw);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("one")
    );
    transcript.set_viewport(1);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("one")
    );

    transcript.replace_entries(vec![transcript_entry("two", &["replacement"])]);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("two")
    );
    assert_eq!(transcript.scroll_offset(), 0);
}

#[test]
fn transcript_expansion_is_scoped_to_its_conversation() {
    let entry = TranscriptEntryId::from("1");
    let mut first = TranscriptState::new(TranscriptId::from("attempt-a"));
    first.load(vec![
        transcript_entry("1", &["body"]).with_technical_lines(vec!["detail".into()])
    ]);
    first.toggle_expanded(&entry);
    assert!(first.is_expanded(&entry));

    first.set_conversation(TranscriptId::from("attempt-b"));
    assert!(!first.is_expanded(&entry));
    first.set_conversation(TranscriptId::from("attempt-a"));
    assert!(first.is_expanded(&entry));
}

#[test]
fn transcript_raw_display_is_terminal_safe_and_readable() {
    let raw = b"before\x1b[31m\0after\xff";
    let shown = raw_display(raw);
    assert!(!shown.contains('\x1b'));
    assert!(!shown.contains('\0'));
    assert!(!shown.starts_with("base64:"));
    assert!(shown.contains("before"));
    assert!(shown.contains("after"));
}

#[test]
fn transcript_separates_adjacent_entries() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![
        transcript_entry("one", &["first"]),
        transcript_entry("two", &["second"]),
    ]);

    let lines: Vec<String> = transcript
        .logical_lines()
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(lines, vec!["first", "", "second"]);
}

// ---------------------------------------------------------------------------
// AE2, AE5: daemon discovery and cross-process liveness (U7)
// ---------------------------------------------------------------------------

/// AE2: a TUI whose process starts with no daemon running discovers/launches
/// one transparently and sees the same execution list a `narsor` process
/// that already had one running would.
#[tokio::test(flavor = "multi_thread")]
async fn tui_starts_with_no_daemon_running_and_transparently_launches_one() {
    let root = TempRoot::new();
    let existing = {
        let engine = engine_for(root.path());
        start(&engine, root.path())
    };

    init_exe_override();
    let paths = StatePaths::new(root.path());
    assert!(
        !paths.daemon_file().exists(),
        "no daemon has been started for this root yet"
    );

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    assert_eq!(app.executions().len(), 1);
    assert_eq!(app.executions()[0].id, existing.id);
    assert!(
        paths.daemon_file().exists(),
        "a daemon is now discoverable for this root"
    );
}

/// AE5: two separate TUI processes (here, two independent `App`s, each with
/// its own `ApiClient` and its own SSE connection) connected to the same
/// daemon see the same live events for the same execution.
#[tokio::test(flavor = "multi_thread")]
async fn two_tuis_connected_to_the_same_daemon_see_the_same_live_events() {
    let root = TempRoot::new();
    let execution = {
        let engine = engine_for(root.path());
        start_slow(&engine, &root, "tui-ae5")
    };

    let (daemon, _) = Daemon::start(root.path()).await;
    let client_a = Daemon::discover(root.path()).await;
    let client_b = Daemon::discover(root.path()).await;

    let mut stream_a: EventStream = Box::pin(client_a.stream_execution(&execution.id));
    let mut stream_b: EventStream = Box::pin(client_b.stream_execution(&execution.id));

    client_a
        .resume(&execution.id)
        .await
        .expect("the first client starts the background pass");

    // Both streams, over independent connections, see the same terminal
    // event for the same execution. Polled concurrently (`tokio::join!`),
    // not one after the other: `stream_execution`'s connection is lazy (see
    // `src/tui/mod.rs`'s module doc), so awaiting `stream_a` to completion
    // first would only start opening `stream_b`'s connection once the
    // (~300ms) fixture may already have finished and the event already gone.
    let (a, b) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), stream_a.next()),
        tokio::time::timeout(Duration::from_secs(5), stream_b.next()),
    );
    let seen_a = a
        .expect("stream_a delivers before the timeout")
        .expect("stream_a yields an item")
        .expect("stream_a's item is a real event, not a transport error");
    let seen_b = b
        .expect("stream_b delivers before the timeout")
        .expect("stream_b yields an item")
        .expect("stream_b's item is a real event, not a transport error");

    assert_eq!(seen_a.workflow_execution_id, execution.id);
    assert_eq!(seen_b.workflow_execution_id, execution.id);

    wait_until_settled(&client_a, &[&execution.id]).await;
    drop(daemon);
}

// ---------------------------------------------------------------------------
// U5: daemon-backed default harness picker
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn harness_picker_uses_confirmed_snapshots_for_set_reset_and_errors() {
    let root = TempRoot::new();
    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;

    assert_eq!(
        app.on_key(key(KeyCode::Char('h'))),
        Request::LoadHarnessSelection
    );
    app.load_harness_selection().await;

    let initial = match app.modal() {
        Modal::HarnessSelection(picker) => picker.snapshot.clone().expect("loaded snapshot"),
        other => panic!("expected harness picker, got {other:?}"),
    };
    assert!(initial.supported.iter().any(|harness| harness == "codex"));

    app.set_harness_selection("codex".to_string()).await;
    match app.modal() {
        Modal::HarnessSelection(picker) => {
            let snapshot = picker.snapshot.as_ref().expect("set snapshot");
            assert_eq!(snapshot.effective, "codex");
            assert_eq!(snapshot.override_harness.as_deref(), Some("codex"));
        }
        other => panic!("expected harness picker, got {other:?}"),
    }

    app.reset_harness_selection().await;
    let confirmed_reset = match app.modal() {
        Modal::HarnessSelection(picker) => {
            let snapshot = picker.snapshot.clone().expect("reset snapshot");
            assert_eq!(snapshot.override_harness, None);
            assert_eq!(snapshot.effective, snapshot.configured);
            snapshot
        }
        other => panic!("expected harness picker, got {other:?}"),
    };

    app.set_harness_selection("not-a-harness".to_string()).await;
    match app.modal() {
        Modal::HarnessSelection(picker) => {
            assert_eq!(picker.snapshot.as_ref(), Some(&confirmed_reset));
            assert!(picker
                .error
                .as_deref()
                .is_some_and(|error| error.contains("rejected")));
        }
        other => panic!("failed mutation must leave picker open, got {other:?}"),
    }

    // Esc is read-only; reopening later will make a fresh daemon request.
    assert_eq!(app.on_key(key(KeyCode::Esc)), Request::None);
    assert!(matches!(app.modal(), Modal::None));
    assert_eq!(
        client.harness_selection().await.expect("snapshot"),
        confirmed_reset
    );
}

// ---------------------------------------------------------------------------
// Conductor plan U6: the `i` form for every pending input, and `t` on a gate
// ---------------------------------------------------------------------------

/// The question the agent before the gate left open (AE2).
const AGENT_QUESTION: &str = "Czy limit ma dotyczyć konta czy użytkownika?";

/// The whole screen as drawn, cell symbols in order.
fn screen_of(app: &mut App) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut terminal = Terminal::new(TestBackend::new(160, 60)).expect("a test terminal");
    terminal
        .draw(|frame| views::render(frame, app))
        .expect("draw the screen");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// The fixture shaped like the bundled `resolve_ambiguity` gate.
fn gate_definition_path() -> PathBuf {
    fixtures().join("gate.yaml")
}

/// Runs the gate fixture up to its first gate — before a daemon exists for
/// `root` — and, when given, records the question the agent step before it
/// left open, the way the engine carries it (U3).
async fn paused_at_gate(root: &Path, question: Option<&str>) -> String {
    let engine = engine_for(root);
    let definition = load_from_file(gate_definition_path()).expect("load the gate fixture");
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace");

    let mut execution = engine
        .start(
            &definition,
            json!({ "apply_path": fixtures().join("apply.sh").display().to_string() }),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start the execution");
    execution.metadata = json!({
        "cli": { "definition_path": gate_definition_path().display().to_string() }
    });
    engine
        .store()
        .save_execution(&execution)
        .expect("record the definition path");

    let execution = engine
        .run(&definition, &execution.id)
        .await
        .expect("run up to the gate");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        execution.current_state.as_deref(),
        Some("resolve_ambiguity")
    );

    if let Some(question) = question {
        let store = engine.store();
        let mut pending: PendingHumanInput = store
            .load_pending_input(&execution.id)
            .expect("read the pending input")
            .expect("the gate is pending");
        pending.request.question = Some(question.to_string());
        pending.request.answer_tail = Some("…so I stopped here.".to_string());
        store
            .save_pending_input(&execution.id, &pending)
            .expect("record the agent's question");
    }
    execution.id
}

/// Runs the gate fixture past its first gate to the second one, `confirm`,
/// which declares no fields.
async fn paused_at_confirm(root: &Path) -> String {
    let execution_id = paused_at_gate(root, None).await;
    let engine = engine_for(root);
    let definition = load_from_file(gate_definition_path()).expect("load the gate fixture");
    let execution = engine
        .deliver_human_input(
            &definition,
            &execution_id,
            &HumanInputDelivery::with_values(
                json!({ "ambiguity_decision": "answered", "ambiguity_answer": "B" })
                    .as_object()
                    .cloned()
                    .expect("an object"),
            ),
        )
        .await
        .expect("answer the first gate");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("confirm"));
    execution_id
}

/// Runs `agent_problem` until its agent step, which gives no output label,
/// becomes a problem (U5) — with, when given, the conductor's analysis
/// recorded on it the way U10 will.
async fn paused_on_problem(root: &Path, analysis: Option<ConductorAnalysis>) -> String {
    let store = store_of(root);
    let engine = Engine::new(store.clone()).with_executor(
        ActionKind::Agent,
        Arc::new(engine_support::LabellessAgentExecutor::answering(
            "Done, all tests pass.",
        )),
    );
    let definition_path = engine_support::fixtures().join("engine/agent_problem.yaml");
    let definition = load_from_file(&definition_path).expect("load the problem fixture");
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace");
    fs::copy(
        engine_support::fixtures().join("scripts/print_json.sh"),
        workspace_path.join("print_json.sh"),
    )
    .expect("copy the fixture script");

    let mut execution = engine
        .start(
            &definition,
            json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start the execution");
    execution.metadata = json!({
        "cli": { "definition_path": definition_path.display().to_string() }
    });
    store
        .save_execution(&execution)
        .expect("record the definition path");
    engine
        .run(&definition, &execution.id)
        .await
        .expect("run to the problem");

    let mut pending: PendingHumanInput = store
        .load_pending_input(&execution.id)
        .expect("read the pending input")
        .expect("the problem is pending");
    assert_eq!(pending.continuation, InputContinuation::ResolveOutput);
    if analysis.is_some() {
        pending.problem.as_mut().expect("a problem record").analysis = analysis;
        store
            .save_pending_input(&execution.id, &pending)
            .expect("record the analysis");
    }
    execution.id
}

/// The open input form.
fn input_form(app: &App) -> &narsor::tui::app::InputForm {
    match app.modal() {
        Modal::Input(form) => form,
        other => panic!("the input form is open, got {other:?}"),
    }
}

/// Moves the form's focus onto `row`.
fn focus_row(app: &mut App, row: FormRow) {
    for _ in 0..32 {
        if input_form(app).current_row() == Some(row) {
            return;
        }
        app.on_key(key(KeyCode::Down));
    }
    panic!("the form has no row {row:?}");
}

/// The form shows the gate's message, the agent's question above the fields
/// and the end of its answer (AE2, R7).
#[tokio::test(flavor = "multi_thread")]
async fn the_gate_form_shows_the_agents_question_above_the_fields() {
    let root = TempRoot::new();
    paused_at_gate(root.path(), Some(AGENT_QUESTION)).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));

    let form = input_form(&app);
    assert_eq!(form.question.as_deref(), Some(AGENT_QUESTION));
    let screen = screen_of(&mut app);
    let message = screen
        .find("The agent stopped on an ambiguity rather than guessing.")
        .expect("the gate's message is drawn");
    let question = screen.find(AGENT_QUESTION).expect("the question is drawn");
    let field = screen
        .find("ambiguity_decision*")
        .expect("the choice is drawn");
    assert!(
        message < question && question < field,
        "the question sits above the fields"
    );
    assert!(
        screen.contains("…so I stopped here."),
        "the answer's tail is drawn"
    );
}

/// AE1: the operator answers `resolve_ambiguity` — `answered` is the choice,
/// the answer is typed — and the next step receives that text. The second
/// gate declares no fields, so `t` decides it as before.
#[tokio::test(flavor = "multi_thread")]
async fn the_gate_answer_reaches_the_next_step() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), Some(AGENT_QUESTION)).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    app.on_key(key(KeyCode::Char('i')));
    assert_eq!(input_form(&app).chosen_label().as_deref(), Some("answered"));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "używamy wariantu B");
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::SubmitInput);
    app.submit_input().await;
    assert!(
        matches!(app.modal(), Modal::None),
        "an accepted answer closes the form"
    );
    assert!(!app.notice().expect("a notice").is_error);

    wait_until_settled(&client, &[&execution_id]).await;
    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("confirm"));
    assert_eq!(
        execution.context.variables.get("applied_answer"),
        Some(&json!("używamy wariantu B")),
        "the step after the gate got the answer"
    );
    assert!(events_of(root.path(), &execution_id)
        .iter()
        .any(|event| matches!(
            &event.data,
            EventData::HumanInputReceived { input, output_label, .. }
                if input["ambiguity_decision"] == "answered"
                    && output_label.as_deref() == Some("answered")
        )));

    // `confirm` has no fields: `t` is not redirected there.
    app.refresh().await;
    app.on_key(key(KeyCode::Char('t')));
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::ApplyTransition);
    app.apply_transition().await;
    wait_until_settled(&client, &[&execution_id]).await;
    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));
}

/// KTD17: a `select` whose options are exactly the outputs is one choice
/// with the outputs' descriptions, and what is sent — the field's value and
/// `chosen_label` — cannot disagree.
#[tokio::test(flavor = "multi_thread")]
async fn a_select_matching_the_outputs_is_one_choice_with_descriptions() {
    let root = TempRoot::new();
    paused_at_gate(root.path(), None).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));

    let form = input_form(&app);
    assert_eq!(form.label_field, Some(0));
    assert!(
        !form.rows().contains(&FormRow::Label),
        "no second label picker"
    );
    let screen = screen_of(&mut app);
    assert!(
        screen.contains("● answered — the agent's question is answered; carry on with the work.")
    );
    assert!(
        screen.contains("○ replan — the plan is what is wrong; send the work back to planning.")
    );

    app.on_key(key(KeyCode::Right));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "the plan misses the limits");

    let form = input_form(&app);
    assert_eq!(form.chosen_label().as_deref(), Some("replan"));
    let delivery = form.validate().expect("a valid answer");
    assert_eq!(delivery.values["ambiguity_decision"], "replan");
    assert_eq!(delivery.chosen_label.as_deref(), Some("replan"));
    assert!(screen_of(&mut app).contains("● replan"));
}

/// AE14: `t` → `answered` on a gate with required fields does not move on —
/// it points to `i`, and the pending answer stays. Override mode is still
/// there as a deliberate escape.
#[tokio::test(flavor = "multi_thread")]
async fn t_on_a_gate_with_required_fields_points_to_i() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), None).await;
    let before = events_of(root.path(), &execution_id).len();

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    app.on_key(key(KeyCode::Char('t')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert_eq!(menu.current().map(String::as_str), Some("answered"));
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::None);
    let notice = app.notice().expect("a notice");
    assert!(notice.is_error);
    assert_eq!(notice.text, GATE_NEEDS_FORM_MESSAGE);
    assert!(
        matches!(app.modal(), Modal::Transition(_)),
        "the menu stays open"
    );

    // Even a caller that goes straight to the operation is refused.
    app.apply_transition().await;
    assert_eq!(
        app.notice().expect("a notice").text,
        GATE_NEEDS_FORM_MESSAGE
    );

    let store = store_of(root.path());
    assert_eq!(
        store.load_execution(&execution_id).expect("reload").status,
        ExecutionStatus::InputNeeded
    );
    assert!(
        store.has_pending_input(&execution_id),
        "the gate still waits"
    );
    assert_eq!(events_of(root.path(), &execution_id).len(), before);

    // A forced override stays available.
    app.on_key(key(KeyCode::Char('o')));
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(app.modal(), Modal::Confirm(_)));
}

/// `i` works from the task view, and the form is drawn over it; closing the
/// form leaves the operator in the task view.
#[tokio::test(flavor = "multi_thread")]
async fn i_opens_the_form_from_the_task_view() {
    let root = TempRoot::new();
    paused_at_gate(root.path(), Some(AGENT_QUESTION)).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::OpenTaskView);
    app.open_task_for_execution().await;
    assert!(matches!(app.screen(), Screen::Task(_)));
    assert!(app.shortcuts().contains(&("i", "answer")));

    app.on_key(key(KeyCode::Char('i')));
    assert_eq!(input_form(&app).kind, InputKind::Gate);
    assert!(
        screen_of(&mut app).contains(AGENT_QUESTION),
        "drawn over the task view"
    );

    // Keys go to the form, not to the task view.
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "jk");
    assert_eq!(input_form(&app).values[1], "jk");

    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None));
    assert!(
        matches!(app.screen(), Screen::Task(_)),
        "still in the task view"
    );
}

/// An agent's own question opens as it always did: a reply form, titled as
/// one, with the question as its message.
#[tokio::test(flavor = "multi_thread")]
async fn i_opens_the_reply_form_for_an_agents_question() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;
    {
        // Turn the pending review into the question an agent would raise.
        let store = store_of(root.path());
        let pending: PendingHumanInput = store
            .load_pending_input(&execution_id)
            .expect("read")
            .expect("pending");
        let mut request = pending.request.clone();
        request.instruction = "Which API version?".to_string();
        request.fields = vec![narsor::domain::HumanInputField {
            name: "answer".to_string(),
            field_type: narsor::domain::HumanInputType::Text,
            required: true,
            description: Some("Answer for the agent.".to_string()),
            options: Vec::new(),
        }];
        let question = PendingHumanInput::for_agent(execution_id.clone(), "review", request);
        store
            .save_pending_input(&execution_id, &question)
            .expect("save the agent's question");
    }

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert!(app.shortcuts().contains(&("i", "reply to agent")));
    app.on_key(key(KeyCode::Char('i')));

    let form = input_form(&app);
    assert_eq!(form.kind, InputKind::AgentQuestion);
    assert_eq!(form.rows(), vec![FormRow::Field(0)]);
    let screen = screen_of(&mut app);
    assert!(screen.contains("Reply to the agent"));
    assert!(screen.contains("Which API version?"));

    // Conversation text is an editor, not an append-only line: Alt+Enter
    // inserts a newline and arrows place the next character in the middle.
    app.on_key(key(KeyCode::Char('a')));
    app.on_key(key(KeyCode::Char('b')));
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    app.on_key(key(KeyCode::Char('c')));
    app.on_key(key(KeyCode::Char('d')));
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Char('X')));
    assert_eq!(input_form(&app).values[0], "ab\nXcd");
}

/// A server-side refusal keeps the form open with what was typed: here the
/// request was answered by someone else first, so the daemon says it is no
/// longer pending (`409`).
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_delivery_keeps_the_form_and_its_content() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), None).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "mine");
    let episode = input_form(&app).episode.clone();

    // A second client answers the same request first.
    client
        .deliver_input(
            &execution_id,
            HumanInputDelivery::with_values(
                json!({ "ambiguity_decision": "replan", "ambiguity_answer": "theirs" })
                    .as_object()
                    .cloned()
                    .expect("an object"),
            )
            .for_episode(episode),
        )
        .await
        .expect("the first answer is accepted");

    app.on_key(key(KeyCode::Enter));
    app.submit_input().await;

    let form = input_form(&app);
    assert_eq!(form.values[1], "mine", "nothing has to be typed again");
    assert!(form.error.is_some(), "the refusal is shown in the form");
    assert!(app.notice().expect("a notice").is_error);
}

/// A problem's form: the label choice with descriptions starts on the
/// conductor's proposal, its proposed `variables` are prefilled and the key
/// the label needs is marked; the rejected answer is shown. Sending it
/// settles the step with that output.
#[tokio::test(flavor = "multi_thread")]
async fn the_problem_form_offers_labels_and_prefilled_key_values() {
    let root = TempRoot::new();
    let execution_id = paused_on_problem(
        root.path(),
        Some(ConductorAnalysis {
            label: Some("implemented".to_string()),
            data: serde_json::Map::new(),
            variables: json!({ "summary": "all units done" })
                .as_object()
                .cloned()
                .expect("an object"),
            rationale: Some("The answer says every test passes.".to_string()),
        }),
    )
    .await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert!(app.shortcuts().contains(&("i", "resolve problem")));
    app.on_key(key(KeyCode::Char('i')));

    let form = input_form(&app);
    assert_eq!(form.kind, InputKind::Problem);
    assert_eq!(form.chosen_label().as_deref(), Some("implemented"));
    let problem = form.problem.as_ref().expect("the problem's part");
    assert_eq!(problem.variables.len(), 1);
    assert_eq!(problem.variables[0].key, "summary");
    assert_eq!(problem.variables[0].value, "all units done");
    assert!(form.is_required(EntryMap::Variables, "summary"));

    let screen = screen_of(&mut app);
    assert!(screen.contains("Resolve the problem"));
    assert!(screen.contains("● implemented — The feature is implemented."));
    assert!(screen.contains("○ needs_human_decision — A person has to decide first."));
    assert!(screen.contains("variables.summary* = all units done"));
    assert!(
        screen.contains("Done, all tests pass."),
        "the agent's answer"
    );
    assert!(
        screen.contains("The answer says every test passes."),
        "the analysis"
    );

    focus_row(&mut app, FormRow::Comment);
    type_text(&mut app, "checked by hand");
    app.on_key(key(KeyCode::Enter));
    app.submit_input().await;
    assert!(matches!(app.modal(), Modal::None), "accepted");

    wait_until_settled(&client, &[&execution_id]).await;
    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::Completed);
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(events_of(root.path(), &execution_id)
        .iter()
        .any(|event| matches!(
            &event.data,
            EventData::HumanInputReceived { output_label, variables, comment, .. }
                if output_label.as_deref() == Some("implemented")
                    && variables.as_ref().is_some_and(|v| v["summary"] == "all units done")
                    && comment.as_deref() == Some("checked by hand")
        )));
}

/// Switching a problem's label adds the keys the new label needs; an empty
/// required key stops the form at its entry, and filling it in lets the
/// answer through — `data.question` then reaches the gate the label leads to.
#[tokio::test(flavor = "multi_thread")]
async fn a_problem_label_needing_a_key_is_refused_until_it_is_given() {
    let root = TempRoot::new();
    let execution_id = paused_on_problem(root.path(), None).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));
    app.on_key(key(KeyCode::Tab));

    // Manual-output mode starts on the first label.
    assert_eq!(
        input_form(&app).chosen_label().as_deref(),
        Some("implemented")
    );
    focus_row(&mut app, FormRow::Label);
    app.on_key(key(KeyCode::Right));
    let form = input_form(&app);
    assert_eq!(form.chosen_label().as_deref(), Some("needs_human_decision"));
    let problem = form.problem.as_ref().expect("the problem's part");
    let question = problem
        .data
        .iter()
        .position(|entry| entry.key == "question")
        .expect("the key the label needs is offered");
    assert!(form.is_required(EntryMap::Data, "question"));

    app.on_key(key(KeyCode::Enter));
    app.submit_input().await;
    let form = input_form(&app);
    assert_eq!(
        form.error_row,
        Some(FormRow::Entry(EntryMap::Data, question))
    );
    assert!(form
        .error
        .as_deref()
        .is_some_and(|error| error.contains("data.question")));
    assert_eq!(
        store_of(root.path())
            .load_execution(&execution_id)
            .expect("reload")
            .status,
        ExecutionStatus::InputNeeded,
        "nothing was sent"
    );

    // The focus is on the entry; typing fills its value in.
    type_text(&mut app, "Account or user limit?");
    // A key of the operator's own, added from the add row.
    focus_row(&mut app, FormRow::AddEntry(EntryMap::Variables));
    type_text(&mut app, "scope");
    app.on_key(key(KeyCode::Right));
    type_text(&mut app, "account");
    app.on_key(key(KeyCode::Enter));
    app.submit_input().await;
    assert!(matches!(app.modal(), Modal::None), "accepted");

    wait_until_settled(&client, &[&execution_id]).await;
    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::InputNeeded);
    assert_eq!(
        execution.current_state.as_deref(),
        Some("resolve_ambiguity")
    );
    assert_eq!(
        execution.context.variables.get("scope"),
        Some(&json!("account"))
    );
    let gate: PendingHumanInput = store_of(root.path())
        .load_pending_input(&execution_id)
        .expect("read")
        .expect("the gate is pending");
    assert_eq!(
        gate.request.question.as_deref(),
        Some("Account or user limit?")
    );
}

// ---------------------------------------------------------------------------
// Conductor plan U7: non-destructive refresh, counters, colours and notices
// ---------------------------------------------------------------------------

/// Characterization (U7 execution note): a refresh keeps the selection, the
/// event cursor and the follow mode the operator left the stream in, and
/// appends what is new instead of reloading it from scratch.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_keeps_the_selection_the_event_cursor_and_the_follow_mode() {
    let root = TempRoot::new();
    let first = paused_at_review(root.path()).await;
    let second = paused_at_review(root.path()).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&second).await;
    let selected = app.selected_index();

    // The operator scrolls the stream up: reading, not following.
    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus(), Panel::Events);
    app.on_key(key(KeyCode::Char('k')));
    app.on_key(key(KeyCode::Char('k')));
    let cursor = app.event_cursor();
    let backfilled = app.events().len();
    assert!(!app.follows_stream());
    assert!(cursor + 1 < backfilled);

    app.refresh().await;

    assert_eq!(app.selected_index(), selected);
    assert_eq!(
        app.selected().map(|row| row.id.as_str()),
        Some(second.as_str())
    );
    assert_eq!(app.event_cursor(), cursor, "the cursor stays where it was");
    assert!(!app.follows_stream(), "the follow mode is kept");
    assert_eq!(app.events().len(), backfilled);

    // New events are appended; the cursor still does not move.
    client
        .deliver_input(
            &second,
            HumanInputDelivery::with_values(
                json!({ "approval_status": "approved", "comment": "fine" })
                    .as_object()
                    .cloned()
                    .expect("an object"),
            ),
        )
        .await
        .expect("answer the review");
    wait_until_settled(&client, &[&second]).await;
    app.refresh().await;

    assert!(app.events().len() > backfilled, "the new events are there");
    assert_eq!(app.event_cursor(), cursor);
    assert!(!app.follows_stream());
    let sequences: Vec<u64> = app.events().iter().map(|event| event.sequence).collect();
    let mut sorted = sequences.clone();
    sorted.dedup();
    assert_eq!(sequences, sorted, "nothing was appended twice");
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    let _ = first;
}

/// The row of one execution.
fn row_of<'a>(app: &'a App, execution_id: &str) -> &'a narsor::tui::app::ExecutionRow {
    app.executions()
        .iter()
        .find(|row| row.id == execution_id)
        .unwrap_or_else(|| panic!("{execution_id} is listed"))
}

/// The next sequence number of the selected execution's stream.
fn next_sequence(app: &App) -> u64 {
    app.events().last().map_or(1, |event| event.sequence + 1)
}

/// AE11: an open `i` form with typed content survives the automatic
/// refresh, while another task's status on the list is brought up to date.
#[tokio::test(flavor = "multi_thread")]
async fn the_automatic_refresh_keeps_an_open_form_and_updates_the_list() {
    let root = TempRoot::new();
    let other = paused_at_review(root.path()).await;
    let gate = paused_at_gate(root.path(), None).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert!(!app.refresh_due(std::time::Instant::now()));
    assert!(
        app.refresh_due(std::time::Instant::now() + Duration::from_secs(10)),
        "the built-in interval is ten seconds"
    );
    assert_eq!(app.on_key(key(KeyCode::Char('r'))), Request::Refresh);

    app.select_execution(&gate).await;
    app.on_key(key(KeyCode::Char('i')));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "wariant A");

    // Another window answers the other task.
    client
        .deliver_input(
            &other,
            HumanInputDelivery::with_values(
                json!({ "approval_status": "approved", "comment": "fine" })
                    .as_object()
                    .cloned()
                    .expect("an object"),
            ),
        )
        .await
        .expect("answer the review");
    wait_until_settled(&client, &[&other]).await;

    app.auto_refresh().await;

    let form = input_form(&app);
    assert_eq!(form.values[1], "wariant A", "what was typed is kept");
    assert!(form.closed.is_none(), "the gate is still pending");
    assert_eq!(input_form(&app).current_row(), Some(FormRow::Field(1)));
    assert_eq!(row_of(&app, &other).status, ExecutionStatus::Completed);
    assert_eq!(
        app.selected().map(|row| row.id.as_str()),
        Some(gate.as_str())
    );
}

/// AE12 (counters and notice): while the operator watches task A, task B
/// turns into a problem — the header counts it and the bar names B.
#[tokio::test(flavor = "multi_thread")]
async fn a_problem_on_another_task_is_counted_and_announced() {
    let root = TempRoot::new();
    let watched = paused_at_review(root.path()).await;
    let problem = paused_on_problem(root.path(), None).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&watched).await;

    let counts = app.attention_counts();
    assert_eq!((counts.questions, counts.problems), (1, 1));
    assert_eq!(counts.summary().as_deref(), Some("1 question · 1 problem"));
    assert_eq!(row_of(&app, &problem).attention(), Some(Attention::Problem));
    assert_eq!(
        row_of(&app, &watched).attention(),
        Some(Attention::Question)
    );
    assert!(screen_of(&mut app).contains("1 question · 1 problem"));

    // The stream announces B's problem while A is selected.
    app.on_stream(Some(Ok(Event::new(
        problem.clone(),
        EventData::ProblemRaised {
            state_id: "implement".to_string(),
            action_execution_id: "attempt".to_string(),
            kind: ProblemKind::Output,
            reason: "no output label".to_string(),
            episode: None,
        },
        1_000,
    ))));
    let notice = app.notice().expect("a notice");
    assert!(notice.text.contains(&problem), "{}", notice.text);
    assert!(notice.text.contains("problem"), "{}", notice.text);
    assert!(app.refresh_requested(), "the event asks for a refresh");
    assert_eq!(
        app.selected().map(|row| row.id.as_str()),
        Some(watched.as_str())
    );

    // A question on an unselected task is announced too.
    app.on_stream(Some(Ok(Event::new(
        problem.clone(),
        EventData::HumanInputRequested {
            instruction: "Which variant?".to_string(),
            question: None,
            answer_tail: None,
            episode: None,
        },
        1_001,
    ))));
    let notice = app.notice().expect("a notice");
    assert!(notice.text.contains(&problem) && notice.text.contains("Which variant?"));
}

/// A `Failed` execution with an active failure problem counts as a problem,
/// and a refresh does not forget what the stream said about it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failure_problem_from_the_stream_counts_and_survives_a_refresh() {
    let root = TempRoot::new();
    let store = store_of(root.path());
    let failed = seeded_execution("failed-with-problem", ExecutionStatus::Failed);
    store.save_execution(&failed).expect("seed");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(
        app.attention_counts().problems,
        0,
        "failed alone is no problem"
    );

    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::ProblemRaised {
            state_id: "plan".to_string(),
            action_execution_id: "attempt".to_string(),
            kind: ProblemKind::Failure,
            reason: "the harness did not start".to_string(),
            episode: None,
        },
        1,
    ))));
    assert_eq!(app.attention_counts().problems, 1);

    app.refresh().await;
    assert_eq!(app.attention_counts().problems, 1);
    assert!(screen_of(&mut app).contains("Problem — the harness did not start"));
}

/// AE13 (TUI): "wariant A" is typed, the conductor closes the input with
/// "wariant B" — the form says who answered and what, refuses to send, and
/// keeps "wariant A".
#[tokio::test(flavor = "multi_thread")]
async fn an_input_answered_by_the_conductor_closes_the_open_form() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), None).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "wariant A");
    let episode = input_form(&app).episode.clone();

    let sequence = next_sequence(&app);
    app.on_stream(Some(Ok(Event::new(
        execution_id.clone(),
        EventData::HumanInputReceived {
            input: json!({ "ambiguity_answer": "wariant B" }),
            answered_by: Some(AnsweredBy::Conductor),
            close_reason: Some(InputCloseReason::Answered),
            episode: Some(episode),
            output_label: Some("answered".to_string()),
            data: None,
            variables: None,
            comment: None,
        },
        sequence,
    ))));

    let form = input_form(&app);
    let closure = form.closed.as_ref().expect("the form is closed");
    let message = closure.message();
    assert!(message.contains("the conductor"), "{message}");
    assert!(message.contains("wariant B"), "{message}");
    assert_eq!(form.values[1], "wariant A");

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Request::None,
        "nothing is sent"
    );
    assert!(app.notice().expect("a notice").is_error);
    app.submit_input().await;
    assert!(
        matches!(app.modal(), Modal::Input(_)),
        "still open until Esc"
    );
    assert_eq!(
        store_of(root.path())
            .load_execution(&execution_id)
            .expect("reload")
            .status,
        ExecutionStatus::InputNeeded,
        "the daemon was never asked"
    );
    let screen = screen_of(&mut app);
    assert!(screen.contains("(closed)"));
    assert!(screen.contains("wariant B"));

    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.modal(), Modal::None));
}

/// Another window takes the gate's transition with `t`: at the next refresh
/// the open form says it was closed by a transition.
#[tokio::test(flavor = "multi_thread")]
async fn a_transition_from_another_window_closes_the_open_form_at_the_refresh() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), None).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "mine");

    client
        .transition(
            &execution_id,
            TransitionRequest {
                label: Some("replan".to_string()),
                target: None,
                force: false,
            },
        )
        .await
        .expect("the transition is accepted");
    wait_until_settled(&client, &[&execution_id]).await;

    app.auto_refresh().await;

    let form = input_form(&app);
    let closure = form.closed.as_ref().expect("the form is closed");
    assert_eq!(closure.reason, Some(InputCloseReason::Transition));
    assert!(
        closure.message().contains("transition"),
        "{}",
        closure.message()
    );
    assert_eq!(form.values[1], "mine");
}

/// An operator in another window answers first: the refresh closes the
/// form with that answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_from_another_window_closes_the_open_form_at_the_refresh() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), None).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));
    focus_row(&mut app, FormRow::Field(1));
    type_text(&mut app, "mine");

    client
        .deliver_input(
            &execution_id,
            HumanInputDelivery::with_values(
                json!({ "ambiguity_decision": "replan", "ambiguity_answer": "theirs" })
                    .as_object()
                    .cloned()
                    .expect("an object"),
            ),
        )
        .await
        .expect("the first answer is accepted");
    wait_until_settled(&client, &[&execution_id]).await;

    app.refresh().await;

    let form = input_form(&app);
    let message = form.closed.as_ref().expect("closed").message();
    assert!(message.contains("an operator"), "{message}");
    assert!(message.contains("theirs"), "{message}");
    assert_eq!(form.values[1], "mine");
}

/// With the stream gone, the periodic read still brings statuses up to date.
#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_stream_still_gets_statuses_from_the_periodic_read() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path())
        .await
        .with_refresh_interval(Some(Duration::from_secs(1)));
    app.refresh().await;
    app.on_stream(None);
    assert_eq!(app.stream_status(), &StreamStatus::Disconnected);

    client
        .deliver_input(
            &execution_id,
            HumanInputDelivery::with_values(
                json!({ "approval_status": "approved", "comment": "fine" })
                    .as_object()
                    .cloned()
                    .expect("an object"),
            ),
        )
        .await
        .expect("answer the review");
    wait_until_settled(&client, &[&execution_id]).await;

    assert!(app.refresh_due(std::time::Instant::now() + Duration::from_secs(1)));
    app.auto_refresh().await;
    assert_eq!(
        row_of(&app, &execution_id).status,
        ExecutionStatus::Completed
    );
    assert_eq!(app.attention_counts().summary(), None);
}

/// The automatic refresh leaves the task view's axis where the operator put
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn the_automatic_refresh_keeps_the_task_views_cursor() {
    let root = TempRoot::new();
    paused_at_review(root.path()).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::OpenTaskView);
    app.open_task_for_execution().await;
    app.on_key(key(KeyCode::Char('g')));
    app.load_task_position().await;
    let before = task_view(&app).axis_scroll.offset();
    let selected = task_view(&app).selected.clone();

    app.auto_refresh().await;

    assert!(matches!(app.screen(), Screen::Task(_)));
    assert_eq!(task_view(&app).axis_scroll.offset(), before);
    assert_eq!(task_view(&app).selected, selected);
}

/// R10: a question and a problem read differently — their own words in the
/// list and the details, their own colours.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_and_a_problem_have_their_own_colours_and_descriptions() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let root = TempRoot::new();
    let question = paused_at_gate(root.path(), None).await;
    let problem = paused_on_problem(root.path(), None).await;

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;

    let colour_of = |app: &mut App, needle: &str| {
        let mut terminal = Terminal::new(TestBackend::new(160, 60)).expect("a test terminal");
        terminal
            .draw(|frame| views::render(frame, app))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let cells = buffer.content();
        let wanted: Vec<String> = needle.chars().map(|c| c.to_string()).collect();
        let at = (0..cells.len())
            .find(|start| {
                wanted.iter().enumerate().all(|(offset, symbol)| {
                    cells.get(start + offset).map(|cell| cell.symbol()) == Some(symbol.as_str())
                })
            })
            .expect("the text is on screen");
        cells[at].fg
    };

    app.select_execution(&question).await;
    let screen = screen_of(&mut app);
    assert!(screen.contains("Question — the workflow asks"), "{screen}");
    assert!(screen.contains("input_needed (question)"));
    let question_colour = colour_of(&mut app, "input_needed (question)");

    app.select_execution(&problem).await;
    let screen = screen_of(&mut app);
    assert!(screen.contains("Problem — "), "{screen}");
    assert!(screen.contains("input_needed (problem)"));
    let problem_colour = colour_of(&mut app, "input_needed (problem)");

    assert_ne!(question_colour, problem_colour);
}

// ---------------------------------------------------------------------------
// The conductor in the interface (U13)
// ---------------------------------------------------------------------------

/// The key of the step failure `failed_at_prepare` records.
const FAILURE_KEY: &str = "attempt-1";

/// The error the failed step of `failed_at_prepare` failed with.
const FAILURE_ERROR: &str = "the script exploded";

/// An execution of the pipeline fixture failed at its first step, with the
/// step-failure record the engine leaves for the conductor (F3). Its
/// definition is resolvable and its script runs, so a recovery reaches the
/// `review` gate. Pre-daemon only.
fn failed_at_prepare(root: &Path) -> String {
    let engine = engine_for(root);
    let mut execution = start(&engine, root);
    execution.status = ExecutionStatus::Failed;
    execution.current_state = Some("prepare".to_string());
    execution.completed_at = Some(chrono::Utc::now());
    execution.metadata["engine"] = json!({
        "step_failure": {
            "state_id": "prepare",
            "action_execution_id": FAILURE_KEY,
            "episode": FAILURE_KEY,
            "error": FAILURE_ERROR,
            "side_effect_free": false,
        }
    });
    engine
        .store()
        .save_execution(&execution)
        .expect("record the failure");
    execution.id
}

/// The conductor's report of `failed_at_prepare`'s failure, as it leaves
/// it once it concluded. Pre-daemon only.
fn report_failure(root: &Path, execution_id: &str, outcome: FailureOutcome, reason: &str) {
    let conductor = ConductorStore::new(&StatePaths::new(root), execution_id);
    let state = ConductorState {
        failure: Some(FailureReport {
            key: FAILURE_KEY.to_string(),
            episode: FAILURE_KEY.to_string(),
            state_id: "prepare".to_string(),
            reason: reason.to_string(),
            outcome,
            concluded_at: chrono::Utc::now(),
        }),
        ..ConductorState::default()
    };
    conductor.save_state(&state).expect("save the report");
}

/// Appends `entry` to the chat of `execution_id`. Pre-daemon only.
fn seed_chat(root: &Path, execution_id: &str, entry: ChatEntry) -> ChatEntry {
    ConductorStore::new(&StatePaths::new(root), execution_id)
        .append_chat(entry)
        .expect("seed the chat")
}

/// A chat entry of the conductor's with `payload`.
fn conductor_entry(kind: ChatEntryKind, text: &str, payload: Value) -> ChatEntry {
    let mut entry = ChatEntry::new(ChatAuthor::Conductor, kind, text);
    entry.payload = Some(payload);
    entry
}

/// The open failure form.
fn failure_form(app: &App) -> &narsor::tui::failure_form::FailureForm {
    match app.modal() {
        Modal::Failure(form) => form,
        other => panic!("the failure form is open, got {other:?}"),
    }
}

/// Opens the task view of the selected execution and its chat, with the
/// history read.
async fn open_chat(app: &mut App) {
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::OpenTaskView);
    app.open_task_for_execution().await;
    assert_eq!(app.on_key(key(KeyCode::Char('c'))), Request::LoadChat);
    app.load_chat().await;
    assert!(task_view(app).chat.open);
}

/// Moves the chat's focus to the history and its selection onto the first
/// entry `wanted` accepts.
fn select_chat_entry(app: &mut App, wanted: impl Fn(&ChatItem) -> bool) {
    if task_view(app).chat.focus == ChatFocus::Draft {
        app.on_key(key(KeyCode::Tab));
    }
    app.on_key(key(KeyCode::Char('g')));
    for _ in 0..64 {
        let item = task_view(app).chat.selected_item().expect("an entry");
        if wanted(&item) {
            return;
        }
        app.on_key(key(KeyCode::Char('j')));
    }
    panic!("no chat entry matches");
}

/// A fake `claude` of this test's own answering every invocation of the
/// conductor with `response`, and the `PATH` that puts it first.
fn conductor_answering(root: &Path, response: Value) -> String {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create the bin directory");
    let temp = bin.join(format!(".claude.{}.tmp", uuid::Uuid::new_v4()));
    fs::copy(
        engine_support::fixtures().join("harness/fake_harness.sh"),
        &temp,
    )
    .expect("copy the fake harness");
    fs::rename(&temp, bin.join("claude")).expect("move the fake harness into place");
    let envelope = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "conductor-session",
        "result": response.to_string(),
    });
    fs::write(bin.join("stdout"), format!("{envelope}\n")).expect("write the answer");
    format!(
        "{}:{}",
        bin.display(),
        engine_support::fake_claude_first_on_path()
    )
}

/// AE12: while the conductor diagnoses task B's failure, the list says so at
/// B, the failure itself is not announced, and once it raises the problem
/// the header counts it and the bar names B. The task view's header says
/// what the conductor is doing too.
#[tokio::test(flavor = "multi_thread")]
async fn the_conductor_at_work_is_shown_and_its_problem_counted() {
    let root = TempRoot::new();
    let watched = paused_at_review(root.path()).await;
    let failed = seeded_execution("b-failed", ExecutionStatus::Failed);
    store_of(root.path())
        .save_execution(&failed)
        .expect("seed B");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&watched).await;
    let before = app.notice().cloned();

    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::WorkflowFailed {
            error: "the harness did not start".to_string(),
        },
        1,
    ))));
    assert_eq!(app.notice().cloned(), before, "the failure is held back");
    assert!(app.holds_failure_notice(&failed.id));

    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::ConductorStarted {
            invocation_id: "invocation-1".to_string(),
            trigger: ConductorTrigger::StepFailure,
            purpose: ConductorPurpose::DiagnoseFailure,
            episode: Some("attempt".to_string()),
        },
        2,
    ))));
    assert!(
        app.refresh_requested(),
        "the conductor's start asks for one"
    );
    assert_eq!(
        row_of(&app, &failed.id).conductor_line().as_deref(),
        Some("conductor: diagnosing the failure")
    );
    assert!(screen_of(&mut app).contains("◆ conductor: diagnosing the failure"));
    assert_eq!(row_of(&app, &watched).conductor_line(), None);

    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::ProblemRaised {
            state_id: "plan".to_string(),
            action_execution_id: "attempt".to_string(),
            kind: ProblemKind::Failure,
            reason: "the harness is not installed".to_string(),
            episode: Some("attempt".to_string()),
        },
        3,
    ))));
    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::ConductorFinished {
            invocation_id: "invocation-1".to_string(),
            outcome: narsor::domain::ConductorOutcome::Completed,
            error: None,
        },
        4,
    ))));
    assert!(!app.holds_failure_notice(&failed.id));
    let notice = app.notice().expect("a notice");
    assert!(
        notice.text.contains(&failed.id) && notice.text.contains("problem"),
        "{}",
        notice.text
    );
    assert_eq!(row_of(&app, &failed.id).conductor_line(), None);
    assert_eq!(
        app.attention_counts().summary().as_deref(),
        Some("1 question · 1 problem")
    );
    assert!(screen_of(&mut app).contains("1 question · 1 problem"));

    // The task view's header.
    app.select_execution(&failed.id).await;
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::OpenTaskView);
    app.open_task_for_execution().await;
    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::ConductorStarted {
            invocation_id: "invocation-2".to_string(),
            trigger: ConductorTrigger::Chat,
            purpose: ConductorPurpose::ReplyToChat,
            episode: None,
        },
        5,
    ))));
    assert!(screen_of(&mut app).contains("b-failed · conductor: answering the chat"));
}

/// A failure the conductor does not take up is announced after all, at the
/// refresh that shows nobody did.
#[tokio::test(flavor = "multi_thread")]
async fn a_failure_nobody_takes_up_is_announced_at_the_refresh() {
    let root = TempRoot::new();
    let failed = seeded_execution("c-failed", ExecutionStatus::Failed);
    store_of(root.path())
        .save_execution(&failed)
        .expect("seed C");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_stream(Some(Ok(Event::new(
        failed.id.clone(),
        EventData::WorkflowFailed {
            error: "the disk is full".to_string(),
        },
        1,
    ))));
    assert!(app.holds_failure_notice(&failed.id));

    tokio::time::sleep(narsor::tui::refresh::DEBOUNCE + Duration::from_millis(50)).await;
    app.refresh().await;
    assert!(!app.holds_failure_notice(&failed.id));
    let notice = app.notice().expect("a notice");
    assert!(notice.is_error);
    assert!(notice.text.contains("the disk is full"), "{}", notice.text);
}

/// AE4 (view): the problem form shows the agent's answers, the conductor's
/// analysis, each label with its description — and marks what the
/// conductor proposed until the operator edits it.
#[tokio::test(flavor = "multi_thread")]
async fn the_problem_form_shows_the_analysis_and_marks_the_proposed_values() {
    let root = TempRoot::new();
    let execution_id = paused_on_problem(
        root.path(),
        Some(ConductorAnalysis {
            label: Some("implemented".to_string()),
            data: serde_json::Map::new(),
            variables: json!({ "summary": "all units done" })
                .as_object()
                .cloned()
                .expect("an object"),
            rationale: Some("Both answers say every test passes.".to_string()),
        }),
    )
    .await;
    // The answer the conductor's own reprompt got, the way U10 adds it.
    let store = store_of(root.path());
    let mut pending: PendingHumanInput = store
        .load_pending_input(&execution_id)
        .expect("read the pending input")
        .expect("the problem is pending");
    let problem = pending.problem.as_mut().expect("a problem record");
    let mut second = problem.answers[0].clone();
    second.action_execution_id = "reprompt".to_string();
    second.answer_tail = Some("Still done: every test is green.".to_string());
    problem.answers.push(second);
    store
        .save_pending_input(&execution_id, &pending)
        .expect("record the second answer");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));

    let form = input_form(&app);
    let problem = form.problem.as_ref().expect("the problem's part");
    assert_eq!(problem.answers.len(), 2, "the answer and the reprompt's");
    assert!(form.is_proposed_label("implemented"));
    assert!(problem.variables[0].is_proposed());

    let screen = screen_of(&mut app);
    assert!(screen.contains("Conductor's analysis"), "{screen}");
    assert!(screen.contains("◆ proposed output: implemented"));
    assert!(screen.contains("Both answers say every test passes."));
    assert!(screen.contains("● implemented — The feature is implemented.  ◆ conductor's proposal"));
    assert!(screen.contains("○ needs_human_decision — A person has to decide first."));
    assert!(screen.contains("variables.summary* = all units done  ◆ conductor"));
    assert!(screen.contains("Done, all tests pass."), "the first answer");
    assert!(
        screen.contains("Still done: every test is green."),
        "the second answer"
    );

    // An edited value is the operator's own.
    focus_row(&mut app, FormRow::Entry(EntryMap::Variables, 0));
    type_text(&mut app, "!");
    let form = input_form(&app);
    assert!(!form.problem.as_ref().expect("problem").variables[0].is_proposed());
    assert!(!screen_of(&mut app).contains("all units done  ◆ conductor"));
}

/// AE16 (view): the gate's form shows the conductor's summary and its
/// recommendation, as advice — the fields are still the operator's.
#[tokio::test(flavor = "multi_thread")]
async fn the_gate_form_shows_the_conductors_summary_and_recommendation() {
    let root = TempRoot::new();
    let execution_id = paused_at_gate(root.path(), None).await;
    let pending: PendingHumanInput = store_of(root.path())
        .load_pending_input(&execution_id)
        .expect("read the pending input")
        .expect("the gate is pending");
    ConductorStore::new(&StatePaths::new(root.path()), &execution_id)
        .save_annotation(&Annotation {
            key: pending.episode_key().to_string(),
            summary: "The plan covers both variants; variant B is cheaper.".to_string(),
            recommendation: Some("answered: variant B".to_string()),
            created_at: chrono::Utc::now(),
        })
        .expect("save the summary");

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.on_key(key(KeyCode::Char('i')));

    let form = input_form(&app);
    let annotation = form.annotation.as_ref().expect("the summary");
    assert_eq!(
        annotation.recommendation.as_deref(),
        Some("answered: variant B")
    );
    assert!(
        !form.values.iter().any(|value| value.contains("variant B")),
        "the recommendation fills in nothing"
    );
    let screen = screen_of(&mut app);
    assert!(screen.contains("Conductor's summary (advisory"), "{screen}");
    assert!(screen.contains("The plan covers both variants; variant B is cheaper."));
    assert!(screen.contains("◆ recommendation: answered: variant B"));
}

/// KTD4: `i` on a failed step the conductor diagnosed shows the diagnosis,
/// with its confidence and rationale, and its proposal; a restart with a
/// note from the form starts the step again.
#[tokio::test(flavor = "multi_thread")]
async fn the_failure_form_shows_the_diagnosis_and_restarts_with_a_note() {
    let root = TempRoot::new();
    let execution_id = failed_at_prepare(root.path());
    let proposal = json!({
        "kind": "retry",
        "summary": "Restart the step once the ticket exists",
        "text": "the ticket exists now",
    });
    report_failure(
        root.path(),
        &execution_id,
        FailureOutcome::Diagnosed {
            diagnosis: "The ticket file is missing.".to_string(),
            proposals: vec![serde_json::from_value(proposal.clone()).expect("a proposal")],
        },
        "The ticket file is missing.",
    );
    seed_chat(
        root.path(),
        &execution_id,
        conductor_entry(
            ChatEntryKind::Message,
            "The ticket file is missing.",
            json!({
                "kind": "diagnosis",
                "episode": FAILURE_KEY,
                "state_id": "prepare",
                "diagnosis": "The ticket file is missing.",
                "confidence": "low",
                "rationale": "The script stops on a missing ticket.",
            }),
        ),
    );
    seed_chat(
        root.path(),
        &execution_id,
        conductor_entry(
            ChatEntryKind::Proposal,
            "Restart the step once the ticket exists",
            json!({
                "episode": FAILURE_KEY,
                "state_id": "prepare",
                "proposal": proposal,
                "confidence": "low",
                "rationale": "The script stops on a missing ticket.",
            }),
        ),
    );

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    assert_eq!(app.selected_attention(), Some(Attention::Problem));
    assert!(app.shortcuts().contains(&("i", "resolve failure")));
    assert_eq!(
        app.on_key(key(KeyCode::Char('i'))),
        Request::OpenFailureForm
    );
    app.open_failure_form().await;

    let form = failure_form(&app);
    assert_eq!(form.key, FAILURE_KEY);
    assert_eq!(
        form.diagnosis.as_deref(),
        Some("The ticket file is missing.")
    );
    assert_eq!(form.raw_error, None);
    assert_eq!(form.proposals.len(), 1);
    let screen = screen_of(&mut app);
    assert!(screen.contains("Resolve the failure"), "{screen}");
    assert!(screen.contains("Conductor's diagnosis:"));
    assert!(screen.contains("confidence: low"));
    assert!(screen.contains("why: The script stops on a missing ticket."));
    assert!(screen.contains("◆ Restart the step once the ticket exists"));

    // The note row follows the proposal.
    app.on_key(key(KeyCode::Tab));
    type_text(&mut app, "the ticket is there now");
    let request = app.on_key(key(KeyCode::Enter));
    assert_eq!(
        request,
        Request::RecoverFailure {
            execution_id: execution_id.clone(),
            episode: FAILURE_KEY.to_string(),
            comment: Some("the ticket is there now".to_string()),
        }
    );
    app.recover_failure(
        &execution_id,
        FAILURE_KEY,
        Some("the ticket is there now".to_string()),
    )
    .await;
    assert!(matches!(app.modal(), Modal::None), "the form closed");
    assert!(!app.notice().expect("a notice").is_error);

    wait_until_settled(&client, &[&execution_id]).await;
    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::InputNeeded);
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("review"));
}

/// R24 (view): a conductor that cannot answer — the fake `claude` answers
/// nothing, twice — leaves the raw error, and the failure form shows it
/// with no analysis at all. Until it concludes, the list says the conductor
/// is on it.
#[tokio::test(flavor = "multi_thread")]
async fn the_failure_form_shows_the_raw_error_after_two_conductor_failures() {
    let root = TempRoot::new();
    let execution_id = failed_at_prepare(root.path());

    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let row = row_of(&app, &execution_id);
        if row.problem.is_some() {
            break;
        }
        assert_eq!(
            row.conductor_line().as_deref(),
            Some("conductor: diagnosing the failure"),
            "an owed diagnosis is shown before it concludes"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the conductor never concluded"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        app.refresh().await;
    }
    let state = client
        .conductor_state(&execution_id)
        .await
        .expect("read the conductor's state");
    assert_eq!(
        state.invocation_attempts.get(FAILURE_KEY).copied(),
        Some(2),
        "one invocation and its one repeat"
    );

    app.select_execution(&execution_id).await;
    assert_eq!(
        app.on_key(key(KeyCode::Char('i'))),
        Request::OpenFailureForm
    );
    app.open_failure_form().await;
    let form = failure_form(&app);
    assert_eq!(form.diagnosis, None);
    assert!(form.proposals.is_empty());
    let raw = form.raw_error.as_deref().expect("the raw error");
    assert!(raw.contains(FAILURE_ERROR), "{raw}");
    let screen = screen_of(&mut app);
    assert!(screen.contains("Error:"), "{screen}");
    assert!(!screen.contains("Conductor's diagnosis"));
}

/// F4, R28, R26: a message from the chat is answered with a proposal, which
/// is carried out once confirmed and is recorded so in the history — and
/// that history is there again after the interface restarts. A draft being
/// written survives the automatic refresh.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_proposal_is_confirmed_and_the_history_survives_a_restart() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;
    let path = conductor_answering(
        root.path(),
        json!({
            "decision": "reply",
            "message": "The change is prepared; I would approve it.",
            "confidence": "high",
            "rationale": "The script prepared ORC-7.",
            "proposals": [{
                "kind": "answer_input",
                "summary": "Approve the prepared change",
                "values": {"approval_status": "approved", "comment": "checked by the conductor"}
            }],
        }),
    );

    let (daemon, client) = Daemon::start_on_path(root.path(), &path).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    open_chat(&mut app).await;

    type_text(&mut app, "should I approve it?");
    app.auto_refresh().await;
    assert_eq!(task_view(&app).chat.draft, "should I approve it?");

    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::SendChat);
    app.send_chat().await;
    assert_eq!(task_view(&app).chat.draft, "", "the sent draft is cleared");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !task_view(&app)
        .chat
        .items()
        .iter()
        .any(|item| item.open_proposal().is_some())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the conductor never proposed: {:?}",
            task_view(&app).chat.entries
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        app.load_chat().await;
    }
    assert!(screen_of(&mut app).contains("(proposal, open)"));

    select_chat_entry(&mut app, |item| item.open_proposal().is_some());
    // Actions are deliberately unavailable while reading history; enter the
    // explicit action focus before confirming the selected proposal.
    app.on_key(key(KeyCode::Tab));
    assert_eq!(task_view(&app).chat.focus, ChatFocus::Action);
    let proposal_id = task_view(&app)
        .chat
        .selected_item()
        .and_then(|item| item.open_proposal().map(String::from))
        .expect("an open proposal");
    task_view_mut(&mut app).chat.action_target = Some(
        narsor::tui::chat::ChatActionTarget::Proposal(proposal_id.clone()),
    );
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Request::ConfirmProposal {
            execution_id: execution_id.clone(),
            proposal_id: proposal_id.clone(),
        }
    );
    app.confirm_proposal(&execution_id, &proposal_id).await;

    wait_until_settled(&client, &[&execution_id]).await;
    let execution = reload_until(root.path(), &execution_id, ExecutionStatus::Completed);
    assert_eq!(execution.status, ExecutionStatus::Completed);
    app.load_chat().await;
    let items = task_view(&app).chat.items();
    let confirmed = items
        .iter()
        .find(|item| item.entry.id == proposal_id)
        .expect("the proposal is in the history");
    assert_eq!(confirmed.proposal, Some(ProposalState::Executed));
    assert!(screen_of(&mut app).contains("(proposal, carried out)"));
    let history = task_view(&app).chat.entries.len();

    // A new interface reads the whole history again.
    drop(app);
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    app.select_execution(&execution_id).await;
    open_chat(&mut app).await;
    let chat = &task_view(&app).chat;
    assert_eq!(chat.entries.len(), history);
    assert!(chat
        .entries
        .iter()
        .any(|entry| entry.text == "should I approve it?"));
    assert!(screen_of(&mut app).contains("should I approve it?"));
}

/// KTD9 (view): a proposal about a gate that is no longer the pending one
/// is refused — the operator reads that the situation changed, nothing is
/// done, and the view is read again.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_proposal_is_refused_with_a_message() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;
    seed_chat(
        root.path(),
        &execution_id,
        conductor_entry(
            ChatEntryKind::Proposal,
            "Approve the prepared change",
            json!({
                "episode": "an-earlier-gate",
                "state_id": "review",
                "proposal": {
                    "kind": "answer_input",
                    "values": {"approval_status": "approved", "comment": "fine"}
                },
            }),
        ),
    );

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    open_chat(&mut app).await;
    select_chat_entry(&mut app, |item| item.open_proposal().is_some());
    app.on_key(key(KeyCode::Tab));
    assert_eq!(task_view(&app).chat.focus, ChatFocus::Action);
    let Request::ConfirmProposal {
        execution_id: id,
        proposal_id,
    } = app.on_key(key(KeyCode::Char('y')))
    else {
        panic!("y confirms the selected proposal");
    };
    assert_eq!(id, execution_id);
    app.confirm_proposal(&execution_id, &proposal_id).await;

    let notice = app.notice().expect("a notice");
    assert!(notice.is_error);
    assert!(
        notice.text.contains("The situation changed"),
        "{}",
        notice.text
    );
    assert!(matches!(app.screen(), Screen::Task(_)), "still in the view");
    assert!(task_view(&app).chat.open, "the chat stays open");
    assert_eq!(
        store_of(root.path())
            .load_execution(&execution_id)
            .expect("reload")
            .status,
        ExecutionStatus::InputNeeded,
        "nothing was done"
    );
    assert_eq!(
        row_of(&app, &execution_id).status,
        ExecutionStatus::InputNeeded
    );
}

/// R31 (view): undoing the conductor's choice of an output first asks, with
/// what it chose, how sure it was and why; `y` sends the undo.
#[tokio::test(flavor = "multi_thread")]
async fn undoing_a_decision_asks_with_its_confidence_and_rationale() {
    let root = TempRoot::new();
    let execution_id = paused_at_review(root.path()).await;
    seed_chat(
        root.path(),
        &execution_id,
        conductor_entry(
            ChatEntryKind::Decision,
            "Chose the output 'implemented' at 'implement'.",
            json!({
                "decision_id": "decision-1",
                "kind": "choose_output",
                "state_id": "implement",
                "output_label": "implemented",
                "confidence": "high",
                "rationale": "Both answers say every test passes.",
                "saved_at": chrono::Utc::now(),
            }),
        ),
    );

    let (daemon, _) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;
    app.refresh().await;
    open_chat(&mut app).await;
    let screen = screen_of(&mut app);
    assert!(screen.contains("u — undo this decision"), "{screen}");

    select_chat_entry(&mut app, |item| item.undoable_decision().is_some());
    app.on_key(key(KeyCode::Tab));
    assert_eq!(task_view(&app).chat.focus, ChatFocus::Action);
    assert_eq!(app.on_key(key(KeyCode::Char('u'))), Request::None);
    let undo = task_view(&app).chat.undo.clone().expect("the confirmation");
    assert_eq!(undo.decision_id, "decision-1");
    let screen = screen_of(&mut app);
    assert!(
        screen.contains("it chose the output 'implemented'"),
        "{screen}"
    );
    assert!(screen.contains("confidence: high"));
    assert!(screen.contains("why: Both answers say every test passes."));
    assert!(screen.contains("keeps its side effects"));

    // `n` keeps it, `y` asks for the undo.
    app.on_key(key(KeyCode::Char('n')));
    assert!(task_view(&app).chat.undo.is_none());
    app.on_key(key(KeyCode::Char('u')));
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Request::UndoDecision {
            execution_id: execution_id.clone(),
            decision_id: "decision-1".to_string(),
        }
    );
    // The daemon has no such decision saved: it refuses, and says so.
    app.undo_decision(&execution_id, "decision-1").await;
    assert!(app.notice().expect("a notice").is_error);
}

// ---------------------------------------------------------------------------
// The remote tasks mode (U8)
// ---------------------------------------------------------------------------

mod remote_tasks {
    use chrono::{Duration, TimeZone, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::ClientError;
    use narsor::controller_protocol::{ControlEventEnvelope, OwnerCommandRequest, QueueView};
    use narsor::domain::TaskId;
    use narsor::orchestration::{
        Observation, OwnerCommand, QueueReason, TaskEvent, TaskEventKind, TaskLifecycle,
    };
    use narsor::tui::remote::{
        age, command_outcome, queue_badge, status_badge, RemoteApp, RemoteModal, RemoteRequest,
        RemoteStream, Tone,
    };

    use super::remote_support::{report, snapshot, RemoteHarness};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press(app: &mut RemoteApp, code: KeyCode) {
        let request = app.on_key(key(code));
        app.perform(request).await;
    }

    fn texts(app: &RemoteApp) -> Vec<String> {
        app.selected_lines()
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    fn queue(reason: QueueReason) -> QueueView {
        QueueView {
            message: reason.describe(),
            reason,
            since: Utc::now(),
        }
    }

    #[test]
    fn each_queue_reason_has_its_own_badge_and_tone() {
        let waiting = queue_badge(&queue(QueueReason::WaitingForCapacity {
            position: 2,
            running: 3,
            limit: 3,
        }));
        assert_eq!(
            (waiting.text.as_str(), waiting.tone),
            ("#2 · 3/3", Tone::Normal)
        );
        let offline = queue_badge(&queue(QueueReason::ControllerOffline));
        assert_eq!(offline.tone, Tone::Warning);
        let unknown = queue_badge(&queue(QueueReason::BackendUnknown {
            detail: "gone".into(),
        }));
        assert_eq!(unknown.tone, Tone::Error);
        assert!(unknown.text.contains("gone"));
        let admitted = queue_badge(&queue(QueueReason::Admitted));
        assert_eq!(
            (admitted.text.as_str(), admitted.tone),
            ("starting…", Tone::Good)
        );
    }

    #[test]
    fn ages_are_short_and_never_negative() {
        let now = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
        assert_eq!(age(now, now - Duration::seconds(5)), "5s");
        assert_eq!(age(now, now - Duration::minutes(3)), "3m");
        assert_eq!(age(now, now - Duration::hours(2)), "2h");
        assert_eq!(age(now, now + Duration::seconds(30)), "0s");
    }

    #[test]
    fn a_full_backend_on_resume_is_backend_full_not_a_task_failure() {
        let outcome = command_outcome(
            OwnerCommand::Resume,
            &Err(ClientError::Refused {
                code: "backend_at_capacity".into(),
                message: "backend 'k8s' has no free capacity (3 of 3 slots in use)".into(),
            }),
        );
        assert_eq!(outcome.tone, Tone::Warning);
        assert!(outcome.text.starts_with("Backend full"), "{}", outcome.text);
        assert!(outcome.text.contains("keeps its state"));

        let other = command_outcome(
            OwnerCommand::Pause,
            &Err(ClientError::Refused {
                code: "illegal_command".into(),
                message: "no".into(),
            }),
        );
        assert_eq!(other.tone, Tone::Error);
    }

    /// AE3: choosing a default backend changes where *new* tasks go; the
    /// running task stays on its backend and keeps running.
    #[tokio::test]
    async fn the_default_backend_selector_affects_new_tasks_only() {
        let harness = RemoteHarness::start().await;
        harness.running("task-1", "k8s", snapshot(1, None)).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;
        assert_eq!(app.default_backend().unwrap().effective, "k8s");

        press(&mut app, KeyCode::Char('b')).await;
        let RemoteModal::DefaultBackend(picker) = app.modal() else {
            panic!("the picker is open");
        };
        assert_eq!(picker.choices[0], "k8s", "the effective backend first");
        for c in "backend-a".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(app.default_backend().unwrap().effective, "backend-a");
        let notice = app.notice().unwrap();
        assert!(
            notice.text.contains("existing tasks stay"),
            "{}",
            notice.text
        );

        let created = harness.create("task-2", None).await;
        assert_eq!(created.backend_id.as_str(), "backend-a");
        app.refresh().await;
        let first = &app.tasks()[0];
        assert_eq!(first.backend_id.as_str(), "k8s");
        assert_eq!(first.lifecycle, TaskLifecycle::Running);

        // Reset: the first row returns to the configured default.
        press(&mut app, KeyCode::Char('b')).await;
        app.on_key(key(KeyCode::Up));
        while !matches!(app.modal(), RemoteModal::DefaultBackend(p) if p.index == 0) {
            app.on_key(key(KeyCode::Up));
        }
        press(&mut app, KeyCode::Enter).await;
        let view = app.default_backend().unwrap();
        assert_eq!(
            (view.effective.as_str(), view.override_backend.is_none()),
            ("k8s", true)
        );
    }

    /// R14: a controller (and runner) that went quiet turns the last
    /// observation stale — shown with its age — while the client still
    /// reads the last durable state instead of an invented one.
    #[tokio::test]
    async fn a_disconnected_controller_shows_a_stale_last_known_state() {
        let harness = RemoteHarness::start().await;
        harness.running("task-1", "k8s", snapshot(1, None)).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;
        let live = texts(&app);
        assert!(live.iter().any(|l| l.contains("online")), "{live:#?}");
        assert!(!live.iter().any(|l| l.contains("stale")), "{live:#?}");

        harness.clock.advance(Duration::minutes(10));
        app.refresh().await;
        let task = &app.tasks()[0];
        assert_eq!(task.lifecycle, TaskLifecycle::Running, "never invented");
        let lines = texts(&app);
        assert!(lines.iter().any(|l| l.contains("offline")), "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Observed") && l.contains("stale")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Runner") && l.contains("ago") && l.contains("stale")),
            "{lines:#?}"
        );
        let badge = status_badge(task);
        assert_eq!(badge.tone, Tone::Warning);
        assert!(badge.text.contains("stale"), "{}", badge.text);
    }

    /// Pause, resume and archive go through the one owner API; the request
    /// names the task and nothing about where it runs.
    #[tokio::test]
    async fn pause_resume_and_archive_through_the_control_plane_only() {
        let harness = RemoteHarness::start().await;
        let running = harness.running("task-1", "k8s", snapshot(1, None)).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;

        assert_eq!(
            app.on_key(key(KeyCode::Char('p'))),
            RemoteRequest::Command(OwnerCommand::Pause)
        );
        app.perform(RemoteRequest::Command(OwnerCommand::Pause))
            .await;
        assert_eq!(app.tasks()[0].lifecycle, TaskLifecycle::PauseRequested);
        assert_eq!(app.notice().unwrap().tone, Tone::Good);

        // The runner reaches its safe point, the controller removes compute.
        let mut safe = snapshot(2, None);
        safe.safe_point = true;
        running.runner.snapshot(&safe).await.expect("safe point");
        running
            .controller
            .report(&report(
                "task-1",
                running.epoch,
                2,
                Observation::PausedAtSafePoint,
                Some(2),
            ))
            .await
            .expect("paused");
        press(&mut app, KeyCode::Char('r')).await;
        app.perform(RemoteRequest::OpenDetail).await;
        assert_eq!(app.tasks()[0].lifecycle, TaskLifecycle::Paused);
        let lines = texts(&app);
        assert!(
            lines.iter().any(
                |l| l.starts_with("Conversation unavailable: the task is paused")
                    && l.contains("last safe summary")
            ),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("attempt act-6")),
            "{lines:#?}"
        );
        // Reading it is refused locally, explicitly, without a relay call.
        press(&mut app, KeyCode::Char('c')).await;
        assert!(app.notice().unwrap().text.contains("unavailable"));
        // Pausing again is pointless and says so.
        assert_eq!(app.on_key(key(KeyCode::Char('p'))), RemoteRequest::None);

        press(&mut app, KeyCode::Char('u')).await;
        assert_eq!(app.tasks()[0].lifecycle, TaskLifecycle::Provisioning);

        assert_eq!(app.on_key(key(KeyCode::Char('a'))), RemoteRequest::None);
        assert!(matches!(app.modal(), RemoteModal::ConfirmArchive { .. }));
        press(&mut app, KeyCode::Char('y')).await;
        assert!(app.tasks()[0].lifecycle.is_archival());
        assert!(app.notice().unwrap().text.starts_with("archive accepted"));

        // What a command sends: the command, a key and a generation.
        let body = serde_json::to_value(OwnerCommandRequest {
            command: OwnerCommand::Pause,
            idempotency_key: "k".into(),
            expected_generation: Some(1),
        })
        .unwrap();
        let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["command", "expected_generation", "idempotency_key"]);
    }

    #[tokio::test]
    async fn priority_is_edited_for_queued_tasks_only() {
        let harness = RemoteHarness::start().await;
        harness.running("task-1", "k8s", snapshot(1, None)).await;
        // Another backend without a controller: task-2 stays queued.
        harness.create("task-2", Some("backend-b")).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;

        assert_eq!(app.on_key(key(KeyCode::Char('P'))), RemoteRequest::None);
        assert!(matches!(app.modal(), RemoteModal::None));
        assert!(app.notice().unwrap().text.contains("not queued"));

        assert!(app.select("task-2"));
        let queued = &app.tasks()[1];
        assert_eq!(queued.lifecycle, TaskLifecycle::Queued);
        let badge = status_badge(queued);
        assert_eq!(badge.tone, Tone::Warning, "{}", badge.text);
        assert!(badge.text.contains("controller offline"));

        app.on_key(key(KeyCode::Char('P')));
        assert!(matches!(app.modal(), RemoteModal::Priority { .. }));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Char('7')));
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(app.tasks()[1].priority, 7);
    }

    #[tokio::test]
    async fn stream_events_update_the_list_and_report_reconnects() {
        let harness = RemoteHarness::start().await;
        harness.create("task-1", Some("backend-b")).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;

        let event = |kind| ControlEventEnvelope {
            cursor: "c-9".into(),
            event: TaskEvent {
                sequence: 9,
                task_id: TaskId::parse("task-1").unwrap(),
                at: Utc::now(),
                lifecycle: TaskLifecycle::Queued,
                generation: 1,
                kind,
            },
        };
        app.on_stream(Some(Ok(event(TaskEventKind::QueueChanged {
            reason: QueueReason::WaitingForCapacity {
                position: 1,
                running: 3,
                limit: 3,
            },
        }))));
        assert_eq!(app.stream(), &RemoteStream::Live);
        assert_eq!(app.cursor(), Some("c-9"));
        assert_eq!(status_badge(&app.tasks()[0]).text, "queued · #1 · 3/3");
        assert!(!app.refresh_wanted());

        app.on_stream(Some(Ok(event(TaskEventKind::PriorityChanged {
            priority: 4,
        }))));
        assert_eq!(app.tasks()[0].priority, 4);

        app.on_stream(Some(Ok(event(TaskEventKind::InputsUpdated {
            updates: vec![],
        }))));
        assert!(
            app.refresh_wanted(),
            "what the list cannot apply is re-read"
        );

        app.on_stream(Some(Err(ClientError::Unavailable("refused".into()))));
        assert!(matches!(app.stream(), RemoteStream::Reconnecting(_)));
        app.on_stream(Some(Err(ClientError::Unauthorized)));
        assert!(matches!(app.stream(), RemoteStream::Ended(_)));
    }

    #[tokio::test]
    async fn the_remote_mode_draws_its_list_detail_and_modals() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let harness = RemoteHarness::start().await;
        harness
            .running("task-1", "k8s", snapshot(1, Some("episode-1")))
            .await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;

        let draw = |app: &mut RemoteApp| {
            let mut terminal = Terminal::new(TestBackend::new(160, 48)).expect("a test terminal");
            terminal
                .draw(|frame| narsor::tui::remote_views::render(frame, app))
                .expect("draw");
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let screen = draw(&mut app);
        assert!(screen.contains("Remote tasks"), "the list panel");
        assert!(screen.contains("awaiting input"), "the badge");
        assert!(screen.contains("default backend: k8s"));

        press(&mut app, KeyCode::Enter).await;
        app.on_key(key(KeyCode::Char('i')));
        let screen = draw(&mut app);
        assert!(screen.contains("Approve the plan?"), "the input form");
    }
}
