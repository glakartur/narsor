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
mod task_app_support;

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
        // ... and may take a local workspace from the temporary directory the
        // tests' roots live in, and from nowhere else.
        std::env::set_var("NARSOR_LOCAL_WORKSPACE_ROOTS", std::env::temp_dir());
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
            // Fixture workspaces live under the test root, the one directory
            // a local task may take its workspace from.
            .env("NARSOR_LOCAL_WORKSPACE_ROOTS", root)
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
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    // A daemon that `ensure_daemon_running` spawned is this process's own
    // child and nobody reaps it: once it exits it stays a zombie, which
    // `kill(pid, 0)` still sees. A zombie has stopped running.
    !std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
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
    assert_ne!(task.rank, 0, "a task has a place in the queue");
    assert_eq!(
        app.queued_task().map(|task| task.task_id.as_str()),
        Some(task.task_id.as_str()),
        "the acknowledgement selects its task identity"
    );
    let screen = screen_of(&mut app);
    assert!(screen.contains("Queued task"));
    assert!(screen.contains(task.task_id.as_str()));
}

/// Parks a task on its human state, which keeps the local backend's only slot
/// busy, and returns once the scheduler has admitted it.
async fn hold_the_slot(client: &ApiClient) {
    use narsor::orchestration::TaskLifecycle;

    let inputs = json!({ "ticket": "HOLD" })
        .as_object()
        .cloned()
        .expect("an object");
    client
        .start_task(
            "slot-holder",
            &new_task_definition_path().display().to_string(),
            inputs,
        )
        .await
        .expect("queue the slot holder");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let task = client.control_task("slot-holder").await.expect("holder");
        if task.lifecycle == TaskLifecycle::Running {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the slot holder never started: {:?}",
            task.lifecycle
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Queues the `new_task` fixture through the form, as an operator would.
async fn queue_through_the_form(app: &mut App, ticket: &str) {
    open_new_task(app, &new_task_definition_path());
    focus_input(app, "ticket");
    type_text(app, ticket);
    app.on_key(key(KeyCode::Enter));
    app.start_new_task().await;
    assert!(app.queued_task().is_some(), "the queued task is on screen");
}

/// R3, R5, AE2: behind a full backend the task waits with its place in line
/// on screen. The task screen does not reorder it: that is `move`, not a key.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_waiting_behind_a_full_backend_shows_its_place_in_the_queue() {
    let root = TempRoot::new();
    let (daemon, client) = Daemon::start(root.path()).await;
    hold_the_slot(&client).await;
    let mut app = app_over(&daemon, root.path()).await;

    queue_through_the_form(&mut app, "ORC-21").await;
    let id = app.queued_task().unwrap().task_id.to_string();

    let screen = screen_of(&mut app);
    assert!(screen.contains("Status: queued"), "{screen}");
    assert!(screen.contains("Queue position: #2"), "{screen}");
    assert_eq!(client.control_task(&id).await.unwrap().rank, 2);
    assert!(
        !app.shortcuts().iter().any(|(key, _)| *key == "+/-"),
        "a queued task is no longer reordered by priority keys"
    );
    assert_eq!(app.on_key(key(KeyCode::Char('+'))), Request::None);
}

/// R8: the same screen follows the task from queued to running, names its
/// execution only once the runner has one.
#[tokio::test(flavor = "multi_thread")]
async fn the_task_screen_follows_the_lifecycle_once_admitted() {
    use narsor::orchestration::TaskLifecycle;

    let root = TempRoot::new();
    let (daemon, _client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;

    queue_through_the_form(&mut app, "ORC-22").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        app.refresh().await;
        let task = app.queued_task().expect("still on the task screen");
        let has_execution = task
            .runner
            .as_ref()
            .and_then(|runner| runner.execution_cursor.execution_id.as_ref())
            .is_some();
        if task.lifecycle == TaskLifecycle::Running && has_execution {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the task never reached a runner-backed execution: {:?}",
            task.lifecycle
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let screen = screen_of(&mut app);
    assert!(screen.contains("Status: running") || screen.contains("Status: running ·"));
    assert!(!screen.contains("Execution: not created yet"), "{screen}");
    let shortcuts = app.shortcuts();
    assert!(shortcuts.iter().any(|(_, what)| *what == "open execution"));
}

/// R7, F3, AE3: the create went through but its answer was lost, so the
/// operator submits again. The form's retained submission id makes that a
/// replay — the original task is selected and nothing is queued twice.
#[tokio::test(flavor = "multi_thread")]
async fn submitting_again_after_a_lost_acknowledgement_selects_the_original_task() {
    let root = TempRoot::new();
    let (daemon, client) = Daemon::start(root.path()).await;
    let mut app = app_over(&daemon, root.path()).await;

    open_new_task(&mut app, &new_task_definition_path());
    focus_input(&mut app, "ticket");
    type_text(&mut app, "ORC-24");
    app.on_key(key(KeyCode::Enter));

    // The attempt whose answer never arrived, made with exactly what the form
    // would have sent.
    let Modal::NewTask(form) = app.modal() else {
        panic!("the form is open");
    };
    let (submission_id, reference, inputs) = (
        form.submission_id.clone(),
        form.reference.clone().expect("a resolved reference"),
        form.inputs().expect("valid inputs"),
    );
    client
        .start_task(&submission_id, &reference, inputs)
        .await
        .expect("the lost attempt");

    app.start_new_task().await;

    assert!(
        matches!(app.modal(), Modal::None),
        "the replay was acknowledged, not refused"
    );
    assert_eq!(
        app.queued_task().map(|task| task.task_id.to_string()),
        Some(submission_id),
        "the original task is the one on screen"
    );
    assert_eq!(
        client.control_tasks().await.unwrap().tasks.len(),
        1,
        "no second task was queued"
    );
}

/// AE2/R2: a task belongs to the backend that accepted it. Connecting the
/// interface to another daemon neither moves it there nor shows it there.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_stays_with_the_backend_that_accepted_it_when_the_interface_connects_elsewhere() {
    use narsor::tui::remote::RemoteApp;

    let first_root = TempRoot::new();
    let second_root = TempRoot::new();
    let (first, first_client) = Daemon::start(first_root.path()).await;
    let (_second, second_client) = Daemon::start(second_root.path()).await;

    let mut app = app_over(&first, first_root.path()).await;
    queue_through_the_form(&mut app, "ORC-23").await;
    let id = app.queued_task().unwrap().task_id.to_string();

    let mut elsewhere = RemoteApp::new(second_client.clone());
    elsewhere.refresh().await;
    assert!(
        elsewhere.tasks().is_empty(),
        "the other backend does not show a task it never accepted"
    );
    assert!(matches!(
        second_client.control_task(&id).await,
        Err(ClientError::NotFound)
    ));

    let mut back = RemoteApp::new(first_client.clone());
    back.refresh().await;
    let task = back
        .tasks()
        .iter()
        .find(|task| task.task_id.as_str() == id)
        .expect("the task is still on its own backend");
    assert_eq!(task.backend_id.as_str(), "local");
    assert!(second_client
        .control_tasks()
        .await
        .unwrap()
        .tasks
        .is_empty());
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
        assert!(
            expanded
                .iter()
                .any(|line| line.starts_with("AGENT · Agent · ")),
            "the header names the role, the speaker and the date: {expanded:?}"
        );
        assert!(
            !expanded.iter().any(|line| line.contains('{')
                || line.contains("message.content")
                || line.contains("event.type")
                || line.contains("workflow.event")),
            "the expanded entry is readable text, not JSON or field names: {expanded:?}"
        );
    }

    app.on_key(key(KeyCode::Enter));
    assert!(
        !matches!(&task_view(&app).content, TaskContent::Conversation { transcript, .. } if transcript.is_expanded(&entry_id)),
        "second Enter should collapse the transcript entry"
    );

    // `v` shows the source of the selected entry only; `Ctrl+v` switches the
    // whole conversation.
    app.on_key(key(KeyCode::Char('v')));
    if let TaskContent::Conversation { transcript, .. } = &task_view(&app).content {
        assert_eq!(transcript.mode(), DisplayMode::Rendered);
        assert!(transcript.is_raw(&entry_id), "the selected entry is raw");
        let lines = transcript
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();
        assert!(lines
            .iter()
            .any(|line| line.contains("\"type\":\"assistant\"")));
        assert!(
            lines.iter().any(|line| line.contains("thread started")),
            "the other entries stay rendered: {lines:?}"
        );
    }
    app.on_key(key(KeyCode::Char('v')));
    app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    if let TaskContent::Conversation { transcript, .. } = &task_view(&app).content {
        assert_eq!(transcript.mode(), DisplayMode::Raw);
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
            .any(|(key, description)| *key == "v / Ctrl+v"
                && *description == "entry / conversation RAW"),
        "the focused content bar documents both RAW scopes"
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
fn transcript_cursor_moves_between_entries_not_wrapped_lines() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![
        transcript_entry("one", &["line 0", "line 1", "line 2", "line 3"]),
        transcript_entry("two", &["next entry"]),
    ]);
    transcript.set_viewport(3);

    transcript.to_top();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
    transcript.line_down();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("two"))
    );
    transcript.line_up();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
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
    let anchor = transcript.anchor().expect("anchor while reading");
    assert_eq!(anchor.entry_id, TranscriptEntryId::from("one"));
    assert_eq!(anchor.logical_line, 0);
    assert!(!transcript.is_following_end());

    transcript.append(vec![transcript_entry("four", &["four-a"])]);
    assert_eq!(transcript.anchor(), Some(anchor));
    assert_eq!(transcript.scroll_offset(), 0);
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
        TranscriptEntryId::from("two")
    );
    transcript.set_viewport(1);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("two")
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
    let raw = b"before\x1b[31m\0\r\tafter\xff";
    let shown = raw_display(raw);
    assert!(shown.contains("\\x1b"));
    assert!(shown.contains("\\x00"));
    assert!(shown.contains("\\r"));
    assert!(shown.contains("\\t"));
    assert!(!shown.contains('\r'));
    assert!(!shown.contains('\t'));
    assert!(shown.contains("before"));
    assert!(shown.contains("after"));
}

#[test]
fn transcript_keeps_entry_raw_separate_from_global_raw() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![transcript_entry("one", &["readable"])]);
    transcript.toggle_selected_raw();
    assert!(transcript.is_raw(&TranscriptEntryId::from("one")));
    assert_eq!(transcript.mode(), DisplayMode::Rendered);
    transcript.toggle_mode();
    assert_eq!(transcript.mode(), DisplayMode::Raw);
    transcript.toggle_selected_raw();
    assert!(
        transcript.is_raw(&TranscriptEntryId::from("one")),
        "global raw must not mutate hidden entry state"
    );
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
    // The form opens in conversation mode; the key–value rows belong to the
    // manual output.
    app.on_key(key(KeyCode::Tab));

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
    // The form opens in conversation mode; the key–value rows belong to the
    // manual output.
    app.on_key(key(KeyCode::Tab));

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
    async fn a_queued_task_without_a_controller_says_so_and_offers_no_priority() {
        let harness = RemoteHarness::start().await;
        // Another backend without a controller: task-2 stays queued.
        harness.create("task-2", Some("backend-b")).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;

        assert!(app.select("task-2"));
        let queued = &app.tasks()[0];
        assert_eq!(queued.lifecycle, TaskLifecycle::Queued);
        let badge = status_badge(queued);
        assert_eq!(badge.tone, Tone::Warning, "{}", badge.text);
        assert!(badge.text.contains("controller offline"));

        assert_eq!(app.on_key(key(KeyCode::Char('P'))), RemoteRequest::None);
        assert!(matches!(app.modal(), RemoteModal::None));
    }

    #[tokio::test]
    async fn a_move_in_the_stream_asks_for_the_list_to_be_read_again() {
        let harness = RemoteHarness::start().await;
        harness.create("task-1", Some("backend-b")).await;
        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;
        assert!(!app.refresh_wanted());

        app.on_stream(Some(Ok(ControlEventEnvelope {
            cursor: "c-2".into(),
            event: TaskEvent {
                sequence: 2,
                task_id: TaskId::parse("task-1").unwrap(),
                at: Utc::now(),
                lifecycle: TaskLifecycle::Queued,
                generation: 1,
                kind: TaskEventKind::Moved {
                    rank: 1,
                    order: vec![TaskId::parse("task-1").unwrap()],
                },
            },
        })));
        assert!(
            app.refresh_wanted(),
            "the server's order is read, not guessed"
        );
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

/// U7: the building blocks of the redesigned interface — key map, colour
/// roles, the chooser and the connection state. None of it needs a daemon.
mod foundations {
    use super::*;

    use narsor::tui::keymap::{
        self, bind, check_consistency, help_entries, hints, lookup, Action, Binding, Context, Key,
    };

    fn press(code: KeyCode) -> KeyEvent {
        key(code)
    }

    mod key_map {
        use super::*;

        /// The keys origin §3 gives the list context, plus the global ones.
        const LIST_KEYS: [&str; 13] = [
            "?", "q", "j", "k", "g", "G", "Space", "Enter", ".", "n", "z", "H", "R",
        ];

        fn help_keys(context: Context) -> Vec<String> {
            help_entries(context)
                .iter()
                .flat_map(|entry| entry.keys.split('/').map(str::to_string))
                .collect()
        }

        #[test]
        fn the_table_is_consistent() {
            assert_eq!(check_consistency(keymap::BINDINGS), Ok(()));
        }

        #[test]
        fn a_second_n_in_the_list_context_breaks_the_consistency_check() {
            let mut table: Vec<Binding> = keymap::BINDINGS.to_vec();
            table.push(bind(
                Context::List,
                Key::char('n'),
                Action::Quit,
                "",
                "a second meaning",
            ));
            let problems = check_consistency(&table).expect_err("a doubled key is refused");
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.contains("(lista, n)")),
                "{problems:?}"
            );
        }

        #[test]
        fn a_context_key_that_shadows_a_global_one_is_refused() {
            let mut table: Vec<Binding> = keymap::BINDINGS.to_vec();
            table.push(bind(
                Context::List,
                Key::char('q'),
                Action::Back,
                "",
                "shadow",
            ));
            assert!(check_consistency(&table).is_err());
        }

        #[test]
        fn list_help_names_every_key_of_the_origin_table() {
            let keys = help_keys(Context::List);
            for expected in LIST_KEYS {
                assert!(
                    keys.iter().any(|key| key == expected),
                    "{expected} is missing from the list help: {keys:?}"
                );
            }
            assert!(help_entries(Context::List)
                .iter()
                .all(|entry| !entry.description.is_empty()));
        }

        #[test]
        fn the_panel_bar_offers_tab_enter_alt_enter_and_esc() {
            let bar: Vec<String> = hints(Context::Panel)
                .into_iter()
                .map(|(keys, _)| keys)
                .collect();
            for expected in ["Tab", "Enter", "Alt+Enter", "Esc"] {
                assert!(
                    bar.iter().any(|keys| keys == expected),
                    "{expected}: {bar:?}"
                );
            }
        }

        #[test]
        fn help_agrees_with_the_handling_for_every_row() {
            for binding in keymap::BINDINGS {
                let event = binding.key.to_event();
                assert_eq!(
                    lookup(binding.context, &event),
                    Some(binding.action),
                    "({}, {}) must do what the table says",
                    binding.context.name(),
                    binding.key.display()
                );
                assert!(
                    help_keys(binding.context).contains(&binding.key.display()),
                    "({}, {}) must be in the help",
                    binding.context.name(),
                    binding.key.display()
                );
            }
        }

        #[test]
        fn global_keys_work_where_context_inherits_them_and_not_in_forms() {
            let help = press(KeyCode::Char('?'));
            assert_eq!(lookup(Context::List, &help), Some(Action::Help));
            assert_eq!(lookup(Context::Details, &help), Some(Action::Help));
            assert_eq!(lookup(Context::Form, &help), None, "a form types a ?");
            assert_eq!(lookup(Context::Form, &press(KeyCode::Char('q'))), None);
        }

        #[test]
        fn digits_pick_and_shift_is_ignored_for_letters() {
            assert_eq!(
                lookup(Context::Panel, &press(KeyCode::Char('3'))),
                Some(Action::Pick)
            );
            assert_eq!(keymap::digit_of(&press(KeyCode::Char('3'))), Some(3));
            assert_eq!(keymap::digit_of(&press(KeyCode::Char('0'))), None);
            let shifted_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
            assert_eq!(lookup(Context::List, &shifted_g), Some(Action::Bottom));
        }

        #[test]
        fn every_context_has_keys_and_the_origin_move_and_menu_shortcuts_exist() {
            for context in Context::ALL {
                assert!(
                    keymap::BINDINGS
                        .iter()
                        .any(|binding| binding.context == context),
                    "{} has no keys",
                    context.name()
                );
            }
            for (character, action) in [
                ('K', Action::MoveUp),
                ('J', Action::MoveDown),
                ('m', Action::MoveMode),
                ('p', Action::PauseResume),
                ('x', Action::Interrupt),
                ('t', Action::Transition),
                ('s', Action::Restart),
                ('a', Action::Archive),
                ('o', Action::OpenLink),
            ] {
                for context in [Context::List, Context::Details, Context::Menu] {
                    assert_eq!(
                        lookup(context, &press(KeyCode::Char(character))),
                        Some(action),
                        "{character} in {}",
                        context.name()
                    );
                }
            }
        }
    }

    mod chooser_state {
        use super::*;

        use narsor::tui::chooser::{
            Chooser, ChooserAction, ChooserField, ChooserFocus, ChooserItem,
        };

        fn alt_enter() -> KeyEvent {
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        }

        fn type_text(chooser: &mut Chooser, text: &str) {
            for character in text.chars() {
                chooser.on_key(press(KeyCode::Char(character)));
            }
        }

        fn review_chooser() -> Chooser {
            Chooser::new(vec![
                ChooserItem::new("approved", "Akceptuje wynik."),
                ChooserItem::new("revise", "Zwraca do agenta.")
                    .resuming("plan")
                    .recommended()
                    .with_field(ChooserField::multiline("comment").required()),
            ])
        }

        #[test]
        fn fields_keep_their_values_when_the_action_changes_and_comes_back() {
            let mut chooser = review_chooser().selecting(1);
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            assert_eq!(
                chooser.focus(),
                ChooserFocus::Fields,
                "required field: Enter goes to it"
            );
            type_text(&mut chooser, "migracja");
            chooser.on_key(alt_enter());
            type_text(&mut chooser, "orders");

            chooser.on_key(press(KeyCode::Tab));
            assert_eq!(chooser.focus(), ChooserFocus::Actions);
            chooser.on_key(press(KeyCode::Char('k')));
            assert_eq!(chooser.selected(), 0);
            chooser.on_key(press(KeyCode::Char('2')));
            assert_eq!(chooser.selected(), 1);

            assert_eq!(chooser.fields_of(1)[0].value(), "migracja\norders");
        }

        #[test]
        fn digit_beyond_the_list_changes_nothing() {
            let mut chooser = review_chooser();
            chooser.on_key(press(KeyCode::Char('3')));
            assert_eq!(chooser.selected(), 0);
            chooser.on_key(press(KeyCode::Char('2')));
            assert_eq!(chooser.selected(), 1);
            chooser.on_key(press(KeyCode::Char('3')));
            assert_eq!(
                chooser.selected(),
                1,
                "3 with two actions leaves the choice"
            );
            chooser.on_key(press(KeyCode::Char('9')));
            assert_eq!(chooser.selected(), 1);
        }

        #[test]
        fn enter_sends_an_action_without_required_fields() {
            let mut chooser = review_chooser();
            match chooser.on_key(press(KeyCode::Enter)) {
                ChooserAction::Submit(submission) => {
                    assert_eq!(submission.index, 0);
                    assert_eq!(submission.label, "approved");
                    assert!(submission.values.is_empty());
                }
                other => panic!("expected a submission, got {other:?}"),
            }
        }

        #[test]
        fn a_required_field_blocks_the_send_with_a_complaint_on_the_field() {
            let mut chooser = review_chooser().selecting(1);
            chooser.on_key(press(KeyCode::Enter));
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            assert!(chooser.fields_of(1)[0]
                .error()
                .is_some_and(|error| error.contains("comment")));

            type_text(&mut chooser, "poprawić");
            assert_eq!(chooser.fields_of(1)[0].error(), None, "typing clears it");
            match chooser.on_key(press(KeyCode::Enter)) {
                ChooserAction::Submit(submission) => {
                    assert_eq!(
                        submission.values,
                        vec![("comment".to_string(), "poprawić".to_string())]
                    );
                }
                other => panic!("expected a submission, got {other:?}"),
            }
        }

        #[test]
        fn typing_in_a_field_is_not_a_command_and_q_is_text() {
            let mut chooser = review_chooser().selecting(1);
            assert!(!chooser.is_typing());
            chooser.on_key(press(KeyCode::Enter));
            assert!(chooser.is_typing());
            type_text(&mut chooser, "q?j1");
            assert_eq!(chooser.fields_of(1)[0].value(), "q?j1");
            assert_eq!(chooser.selected(), 1, "j and 1 were text, not navigation");
        }

        #[test]
        fn an_unavailable_action_reports_its_reason() {
            let mut chooser = Chooser::new(vec![
                ChooserItem::new("pauza", "Wstrzymuje.").unavailable("zadanie jest w kolejce")
            ]);
            assert_eq!(
                chooser.on_key(press(KeyCode::Enter)),
                ChooserAction::Blocked("zadanie jest w kolejce".to_string())
            );
        }

        #[test]
        fn esc_collapses_and_leaves_the_draft() {
            let mut chooser = review_chooser().selecting(1);
            chooser.on_key(press(KeyCode::Enter));
            type_text(&mut chooser, "szkic");
            assert_eq!(chooser.on_key(press(KeyCode::Esc)), ChooserAction::Cancel);
            assert_eq!(chooser.fields_of(1)[0].value(), "szkic");
        }

        #[test]
        fn a_proposed_value_is_marked_until_it_is_edited() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("restart", "Od nowa.")
                .with_field(ChooserField::text("note").proposed("sprawdź testy"))]);
            assert!(chooser.fields_of(0)[0].is_proposed());
            chooser.on_key(press(KeyCode::Tab));
            chooser.on_key(press(KeyCode::Backspace));
            assert!(!chooser.fields_of(0)[0].is_proposed());
            assert_eq!(chooser.fields_of(0)[0].value(), "sprawdź test");
        }

        #[test]
        fn select_and_integer_fields_follow_the_form_rules() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("nowe", "Tworzy zadanie.")
                .with_field(ChooserField::select(
                    "repo",
                    vec!["narsor".to_string(), "other".to_string()],
                ))
                .with_field(ChooserField::integer("count").required())]);
            chooser.on_key(press(KeyCode::Tab));
            chooser.on_key(press(KeyCode::Right));
            assert_eq!(chooser.fields_of(0)[0].value(), "other");
            chooser.on_key(press(KeyCode::Right));
            assert_eq!(
                chooser.fields_of(0)[0].value(),
                "narsor",
                "wraps like the form's select"
            );
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "1a2-");
            assert_eq!(
                chooser.fields_of(0)[1].value(),
                "12-",
                "letters are not numbers"
            );
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            assert!(
                chooser.fields_of(0)[1].error().is_some(),
                "12- is not an integer"
            );
        }
    }

    mod connection_state {
        use narsor::tui::connection::{Connection, LinkState};
        use narsor::tui::style::Role;
        use std::time::{Duration, Instant};

        #[test]
        fn live_to_retrying_to_down_to_live() {
            let start = Instant::now();
            let mut connection = Connection::new("local");
            assert_eq!(connection.state(), LinkState::Connecting);
            assert!(connection.status(start).text.contains("łączę z local"));

            connection.on_data(start);
            let status = connection.status(start);
            assert_eq!(
                (status.icon, status.text.as_str(), status.role),
                ("●", "live", Role::Live)
            );
            assert_eq!(connection.mutation_gate(start), Ok(()));
            assert_eq!(connection.age_label(start), None);

            let dropped = start + Duration::from_secs(10);
            connection.on_failure(dropped, Some(Duration::from_secs(12)));
            let status = connection.status(dropped);
            assert_eq!(
                (status.icon, status.text.as_str(), status.role),
                ("◌", "ponawiam (12s)", Role::Attention)
            );
            assert_eq!(
                connection
                    .status(dropped + Duration::from_millis(2500))
                    .text,
                "ponawiam (10s)",
                "the countdown is real and rounds up"
            );
            let blocked = connection
                .mutation_gate(dropped + Duration::from_secs(30))
                .expect_err("not live: mutations are refused");
            assert!(blocked.reason.contains("ponawiam"), "{}", blocked.reason);
            assert!(
                blocked.reason.contains("stan sprzed 40s"),
                "{}",
                blocked.reason
            );

            let given_up = dropped + Duration::from_secs(40);
            connection.on_failure(given_up, None);
            let status = connection.status(given_up);
            assert_eq!(
                (status.icon, status.text.as_str(), status.role),
                ("✖", "brak połączenia", Role::Problem)
            );
            assert_eq!(
                connection.age_label(given_up),
                Some("stan sprzed 50s".to_string())
            );
            assert!(connection.is_stale());
            assert!(connection.mutation_gate(given_up).is_err());

            let back = given_up + Duration::from_secs(5);
            connection.on_data(back);
            assert!(connection.is_live() && !connection.is_stale());
            assert_eq!(connection.status(back).text, "live");
            assert_eq!(connection.mutation_gate(back), Ok(()));
            assert_eq!(connection.age_label(back), None);
        }

        #[test]
        fn ages_use_the_largest_short_unit() {
            let start = Instant::now();
            let mut connection = Connection::new("local");
            connection.on_data(start);
            connection.on_failure(start, None);
            for (seconds, expected) in [
                (5, "stan sprzed 5s"),
                (180, "stan sprzed 3m"),
                (7300, "stan sprzed 2h"),
            ] {
                assert_eq!(
                    connection.age_label(start + Duration::from_secs(seconds)),
                    Some(expected.to_string())
                );
            }
        }

        #[test]
        fn without_any_data_there_is_nothing_to_grey_or_age() {
            let now = Instant::now();
            let mut connection = Connection::new("remote");
            connection.on_failure(now, None);
            assert!(!connection.is_stale());
            assert_eq!(connection.age_label(now), None);
            assert!(connection.mutation_gate(now).is_err());
        }
    }

    mod style_roles {
        use narsor::tui::style::{no_color_requested, Role, Styles};
        use narsor::tui::transcript::TranscriptPresentation;
        use ratatui::style::{Color, Modifier, Style};

        const KINDS: [TranscriptPresentation; 7] = [
            TranscriptPresentation::Neutral,
            TranscriptPresentation::Operator,
            TranscriptPresentation::Agent,
            TranscriptPresentation::Conductor,
            TranscriptPresentation::System,
            TranscriptPresentation::Technical,
            TranscriptPresentation::Error,
        ];

        fn all_styles(styles: &Styles) -> Vec<Style> {
            let mut all: Vec<Style> = Role::ALL.iter().map(|role| styles.of(*role)).collect();
            for kind in KINDS {
                all.push(styles.chat_header(kind));
                all.push(styles.chat_body(kind));
            }
            all.push(styles.key_cap());
            all
        }

        #[test]
        fn no_color_is_set_and_not_empty() {
            assert!(!no_color_requested(None));
            assert!(!no_color_requested(Some("".into())));
            assert!(no_color_requested(Some("1".into())));
        }

        #[test]
        fn colour_mode_uses_only_the_sixteen_ansi_colours() {
            for style in all_styles(&Styles::new(false)) {
                for colour in [style.fg, style.bg].into_iter().flatten() {
                    assert!(
                        !matches!(colour, Color::Rgb(..) | Color::Indexed(_) | Color::Reset),
                        "{colour:?} is not one of the 16 ANSI colours"
                    );
                }
            }
        }

        #[test]
        fn no_color_mode_leaves_only_bold_dim_underline_and_reverse() {
            let allowed =
                Modifier::BOLD | Modifier::DIM | Modifier::UNDERLINED | Modifier::REVERSED;
            for style in all_styles(&Styles::new(true)) {
                assert_eq!((style.fg, style.bg), (None, None), "{style:?}");
                assert!(allowed.contains(style.add_modifier), "{style:?}");
            }
        }

        #[test]
        fn selection_is_reverse_video_in_both_modes_and_keeps_the_base() {
            for no_color in [false, true] {
                let styles = Styles::new(no_color);
                assert!(styles
                    .of(Role::Selected)
                    .add_modifier
                    .contains(Modifier::REVERSED));
                let selected = styles.selected(styles.of(Role::Attention));
                assert!(selected.add_modifier.contains(Modifier::REVERSED));
                assert!(
                    selected.add_modifier.contains(Modifier::BOLD),
                    "the base still speaks"
                );
            }
        }

        #[test]
        fn roles_follow_the_origin_table() {
            let styles = Styles::new(false);
            assert_eq!(styles.of(Role::Attention).fg, Some(Color::Yellow));
            assert!(styles
                .of(Role::Attention)
                .add_modifier
                .contains(Modifier::BOLD));
            assert_eq!(styles.of(Role::Problem).fg, Some(Color::Red));
            assert_eq!(styles.of(Role::Live).fg, Some(Color::Green));
            assert_eq!(styles.of(Role::Calm), Style::default());
            assert_eq!(styles.of(Role::Done).fg, Some(Color::DarkGray));
            assert_eq!(styles.of(Role::Focus).fg, Some(Color::Cyan));
            assert_eq!(styles.of(Role::Conductor).fg, Some(Color::Magenta));
            assert_eq!(styles.of(Role::Link).fg, Some(Color::Blue));
            assert!(styles
                .of(Role::Link)
                .add_modifier
                .contains(Modifier::UNDERLINED));
        }

        #[test]
        fn the_conversation_kinds_stay_distinguishable_in_colour() {
            let styles = Styles::new(false);
            let headers: Vec<Style> = [
                TranscriptPresentation::Operator,
                TranscriptPresentation::Agent,
                TranscriptPresentation::Conductor,
                TranscriptPresentation::Error,
            ]
            .into_iter()
            .map(|kind| styles.chat_header(kind))
            .collect();
            for (index, header) in headers.iter().enumerate() {
                assert!(headers[index + 1..]
                    .iter()
                    .all(|other| other.fg != header.fg));
            }
        }
    }
}

/// U8: the task application's list — stable order, stream patching, header
/// counters, the finished-tasks filter, moving queued tasks and the
/// connection. Most of it needs no server (a listing and events are handed
/// in); what talks to the API uses the served control plane of `RemoteHarness`.
mod task_app_list {
    use std::time::{Duration, Instant};

    use crossterm::event::KeyCode;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::{TcpListener, TcpStream};

    use narsor::api::{ApiClient, ClientError};
    use narsor::controller_protocol::MoveTaskRequest;
    use narsor::domain::ExecutionStatus;
    use narsor::orchestration::{
        AttentionKind, MoveTarget, Observation, QueueReason, TaskEventKind, TaskLifecycle,
    };
    use narsor::tui::connection::LinkState;
    use narsor::tui::task_app::list::{Attention, StateIcon};
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{layout, DetailTab, EventFeed, Focus, Request, TaskApp};

    use super::key;
    use super::remote_support::RemoteHarness;
    use super::task_app_support::{
        event, ids, listing, queued, task, titled, waiting, waiting_for_capacity, with_controller,
        with_runner,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(key(code))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn titles(app: &TaskApp) -> Vec<String> {
        app.list().rows().into_iter().map(|row| row.title).collect()
    }

    fn row(app: &TaskApp, title: &str) -> (Attention, StateIcon, String) {
        let row = app
            .list()
            .rows()
            .into_iter()
            .find(|row| row.title == title)
            .unwrap_or_else(|| panic!("no row {title}"));
        (row.attention, row.icon, row.label)
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn controlled(id: &str, backend: &str, lifecycle: &str, rank: u32) -> narsor::controller_protocol::TaskSummary {
        with_controller(task(id, backend, lifecycle, rank))
    }

    mod order {
        use super::*;

        /// Three tasks go queued → provisioning → running → done one after
        /// the other; the rows never move, icons and labels do.
        #[test]
        fn a_state_stream_never_reorders_the_rows() {
            let mut app = app();
            let queued_all = || {
                listing(
                    vec![
                        controlled("a", "local", "queued", 1),
                        controlled("b", "local", "queued", 2),
                        controlled("c", "local", "queued", 3),
                    ],
                    "c0",
                )
            };
            app.load(queued_all());
            let order = vec!["a", "b", "c"];
            assert_eq!(titles(&app), order);
            assert_eq!(row(&app, "b").2, "#2");

            let mut sequence = 1;
            for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
                sequence += 1;
                app.on_stream(event(
                    &format!("p{id}"),
                    id,
                    sequence,
                    TaskLifecycle::Provisioning,
                    TaskEventKind::ControllerReported {
                        observation: Observation::StartAccepted,
                        lease_epoch: 1,
                    },
                ));
                assert_eq!(titles(&app), order, "{id} provisioning");
                assert_eq!(row(&app, id).1, StateIcon::Provisioning);
                // Whoever waits behind moves up in the queue, not on screen.
                let waiting_behind = ["a", "b", "c"].iter().skip(index + 1).count();
                if waiting_behind > 0 {
                    let next = ["a", "b", "c"][index + 1];
                    assert_eq!(row(&app, next).2, "#1");
                }

                sequence += 1;
                app.on_stream(event(
                    &format!("r{id}"),
                    id,
                    sequence,
                    TaskLifecycle::Running,
                    TaskEventKind::ControllerReported {
                        observation: Observation::RunnerReady,
                        lease_epoch: 1,
                    },
                ));
                assert_eq!(titles(&app), order, "{id} running");
                assert_eq!(row(&app, id).1, StateIcon::Running);

                // The debounced read brings the runner's step.
                let mut tasks = app.list().tasks().to_vec();
                tasks[index] = with_runner(tasks[index].clone(), ExecutionStatus::Running, "implement");
                tasks[index].revision = sequence;
                app.load(listing(tasks, "read"));
                assert_eq!(row(&app, id).2, "implement");

                sequence += 1;
                app.on_stream(event(
                    &format!("d{id}"),
                    id,
                    sequence,
                    TaskLifecycle::Running,
                    TaskEventKind::RunnerReported {
                        incarnation: "pod-1".into(),
                        event_cursor: 9,
                        execution_status: ExecutionStatus::Completed,
                        pending_input_id: None,
                        attention: Vec::new(),
                    },
                ));
                assert_eq!(titles(&app), order, "{id} done");
                assert_eq!(row(&app, id), (Attention::None, StateIcon::Done, "done".into()));
            }
        }

        #[test]
        fn a_stale_event_does_not_undo_a_newer_read() {
            let mut app = app();
            let mut running = controlled("a", "local", "running", 1);
            running.revision = 7;
            app.load(listing(vec![running], "c7"));
            app.on_stream(event(
                "old",
                "a",
                5,
                TaskLifecycle::Queued,
                TaskEventKind::Created,
            ));
            assert_eq!(row(&app, "a").1, StateIcon::Running);
            assert_eq!(app.cursor(), Some("c7"), "a dropped event does not move the cursor");
            assert!(!app.refresh_wanted());
        }

        #[test]
        fn the_list_is_never_sorted_by_state() {
            let mut app = app();
            app.load(listing(
                vec![
                    controlled("done", "local", "running", 1),
                    controlled("queue", "local", "queued", 2),
                    controlled("fail", "local", "interrupted", 3),
                    controlled("run", "local", "running", 4),
                ],
                "c",
            ));
            assert_eq!(titles(&app), ["done", "queue", "fail", "run"]);
        }

        #[test]
        fn an_event_for_an_unknown_task_asks_for_a_debounced_read() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.on_stream(event("n", "new", 1, TaskLifecycle::Queued, TaskEventKind::Created));
            assert!(app.refresh_wanted());
            let due = app.next_refresh_due().expect("a refresh is due");
            assert!(due > Instant::now(), "debounced, not immediate");
        }

        #[test]
        fn queue_changes_and_answers_patch_the_row_without_a_read() {
            let mut app = app();
            let mut waiting_task = waiting(controlled("a", "local", "running", 1), AttentionKind::Question);
            waiting_task.runner = Some(narsor_runner());
            app.load(listing(vec![waiting_task, queued("b", 2)], "c"));
            assert_eq!(row(&app, "a").0, Attention::Question);

            let waiting_id = app.list().tasks()[0].attention[0].waiting_id.clone();
            app.on_stream(event(
                "i",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::InputQueued {
                    input_id: "in-1".into(),
                    pending_input_id: waiting_id,
                },
            ));
            assert_eq!(row(&app, "a").0, Attention::None);
            assert!(!app.refresh_wanted(), "an answer shows without a read");

            app.on_stream(event(
                "q",
                "b",
                2,
                TaskLifecycle::Queued,
                TaskEventKind::QueueChanged {
                    reason: QueueReason::ControllerOffline,
                },
            ));
            assert_eq!(row(&app, "b").2, "offline");
            assert!(!app.refresh_wanted());
        }

        fn narsor_runner() -> narsor::controller_protocol::RunnerView {
            super::super::task_app_support::runner(ExecutionStatus::Running, "review")
        }

        #[test]
        fn a_problem_wins_over_a_question_in_the_attention_column() {
            let mut app = app();
            let both = waiting(
                waiting(controlled("a", "local", "running", 1), AttentionKind::Question),
                AttentionKind::FailedStep,
            );
            app.load(listing(vec![both], "c"));
            assert_eq!(row(&app, "a").0, Attention::Problem);
            let counters = app.list().counters();
            assert_eq!((counters.questions, counters.problems), (0, 1));
        }
    }

    mod waiting_and_header {
        use super::*;

        #[test]
        fn space_walks_the_waiting_tasks_in_a_circle_and_the_rows_stand_still() {
            let mut app = app();
            app.load(listing(
                vec![
                    controlled("a", "local", "running", 1),
                    waiting(controlled("b", "local", "running", 2), AttentionKind::Question),
                    controlled("c", "local", "running", 3),
                    waiting(controlled("d", "local", "running", 4), AttentionKind::Problem),
                ],
                "c",
            ));
            let order = titles(&app);
            let mut visited = Vec::new();
            for _ in 0..5 {
                ch(&mut app, ' ');
                visited.push(app.list().selected_id().unwrap().to_string());
                assert_eq!(titles(&app), order, "the rows do not move");
            }
            assert_eq!(visited, ["b", "d", "b", "d", "b"]);
        }

        #[test]
        fn space_with_nobody_waiting_says_so() {
            let mut app = app();
            app.load(listing(vec![controlled("a", "local", "running", 1)], "c"));
            ch(&mut app, ' ');
            assert_eq!(app.list().selected_id(), Some("a"));
            assert!(note(&app).contains("nie czeka"));
        }

        #[test]
        fn the_header_counts_questions_and_problems_and_slots() {
            let mut app = app();
            app.load(listing(
                vec![
                    waiting(controlled("a", "local", "running", 1), AttentionKind::Approval),
                    waiting(controlled("b", "local", "running", 2), AttentionKind::ProvisioningFailed),
                    controlled("c", "local", "running", 3),
                    waiting_for_capacity(queued("d", 4), 1, 2, 3),
                ],
                "c",
            ));
            let counters = app.list().counters();
            assert_eq!((counters.questions, counters.problems), (1, 1));
            let slots = app.list().slots().expect("slots");
            assert_eq!((slots.used, slots.limit), (2, Some(3)));
        }

        #[test]
        fn slots_without_a_waiting_task_count_what_holds_compute() {
            let mut app = app();
            app.load(listing(
                vec![
                    controlled("a", "local", "running", 1),
                    controlled("b", "local", "paused", 2),
                    controlled("c", "local", "provisioning", 3),
                ],
                "c",
            ));
            let slots = app.list().slots().expect("slots");
            assert_eq!((slots.used, slots.limit), (2, None));
        }

        #[test]
        fn a_title_comes_from_display_and_falls_back_to_the_id() {
            let mut app = app();
            app.load(listing(
                vec![
                    titled(queued("a", 1), "Zmiana API\u{1b}[31m"),
                    queued("plain", 2),
                ],
                "c",
            ));
            let shown = titles(&app);
            assert_eq!(shown[1], "plain");
            assert!(!shown[0].contains('\u{1b}'), "{:?}", shown[0]);
            assert!(shown[0].starts_with("Zmiana API"));
        }
    }

    mod refresh_and_filter {
        use super::*;

        #[test]
        fn a_read_changes_neither_the_selection_nor_the_focus_nor_the_tab_nor_the_draft() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c1",
            ));
            ch(&mut app, 'j');
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Right);
            app.detail_mut().set_draft("b", "Uwzględnij migrację");
            assert_eq!(app.list().selected_id(), Some("b"));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(app.detail().tab(), DetailTab::Conversation);

            let mut moved = vec![
                controlled("a", "local", "running", 1),
                controlled("b", "local", "queued", 2),
                controlled("c", "local", "queued", 3),
            ];
            moved[1].revision = 4;
            app.load(listing(moved, "c2"));

            assert_eq!(app.list().selected_id(), Some("b"));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
            assert_eq!(app.detail().draft("b"), Some("Uwzględnij migrację"));
            assert_eq!(app.cursor(), Some("c2"));
        }

        #[test]
        fn z_hides_finished_tasks_keeps_the_order_and_a_second_z_brings_them_back() {
            let mut app = app();
            let done = |id: &str, rank| with_runner(controlled(id, "local", "running", rank), ExecutionStatus::Completed, "done");
            app.load(listing(
                vec![
                    queued("a", 1),
                    done("x", 2),
                    queued("b", 3),
                    done("y", 4),
                    queued("c", 5),
                ],
                "c",
            ));
            assert_eq!(titles(&app), ["a", "x", "b", "y", "c"]);
            ch(&mut app, 'z');
            assert_eq!(titles(&app), ["a", "b", "c"]);
            assert_eq!(app.list().hidden_count(), 2);
            assert_eq!(app.list().tasks().len(), 5, "hidden, not removed");
            ch(&mut app, 'z');
            assert_eq!(titles(&app), ["a", "x", "b", "y", "c"]);
        }

        #[test]
        fn hiding_the_selected_finished_task_moves_the_selection_to_the_row_below() {
            let mut app = app();
            let done = with_runner(controlled("x", "local", "running", 2), ExecutionStatus::Completed, "done");
            app.load(listing(vec![queued("a", 1), done, queued("b", 3)], "c"));
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("x"));
            ch(&mut app, 'z');
            assert_eq!(app.list().selected_id(), Some("b"));
        }

        #[test]
        fn a_selected_task_archived_from_the_cli_hands_the_selection_to_the_row_below() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2), queued("c", 3)], "c0"));
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("b"));

            app.on_stream(event(
                "arch",
                "b",
                2,
                TaskLifecycle::Archived,
                TaskEventKind::Created,
            ));
            assert_eq!(titles(&app), ["a", "c"]);
            assert_eq!(app.list().selected_id(), Some("c"));
            let text = note(&app);
            assert!(text.contains("b") && text.contains("zarchiwizowane") && text.contains("c"), "{text}");
        }

        #[test]
        fn archiving_the_last_row_selects_the_one_above() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c0"));
            ch(&mut app, 'G');
            app.load(listing(vec![queued("a", 1)], "c1"));
            assert_eq!(app.list().selected_id(), Some("a"));
            assert!(note(&app).contains("zaznaczono a"), "{}", note(&app));
        }

        #[tokio::test]
        async fn archived_through_the_api_the_selected_task_hands_over_on_the_next_read() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            assert_eq!(titles(&app), ["t1", "t2", "t3"]);
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("t2"));

            harness
                .owner
                .control_command(
                    "t2",
                    &narsor::controller_protocol::OwnerCommandRequest {
                        command: narsor::orchestration::OwnerCommand::Archive,
                        idempotency_key: "archive-t2".into(),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archive");
            app.refresh().await;
            assert_eq!(titles(&app), ["t1", "t3"]);
            assert_eq!(app.list().selected_id(), Some("t3"));
            assert!(note(&app).contains("t2"), "{}", note(&app));
        }
    }

    mod layouts {
        use ratatui::layout::Rect;

        use super::*;
        use layout::Screen;

        #[test]
        fn a_wide_terminal_splits_into_a_narrow_list_and_the_details() {
            let areas = layout::areas(Rect::new(0, 0, 120, 30), Focus::List);
            assert_eq!(areas.screen, Screen::Split);
            assert_eq!(areas.list.unwrap().width, layout::LIST_WIDTH);
            assert_eq!(areas.detail.unwrap().width, 120 - 2 - layout::LIST_WIDTH);
        }

        #[test]
        fn under_100_columns_the_list_and_the_details_take_turns_full_screen() {
            let on_list = layout::areas(Rect::new(0, 0, 90, 30), Focus::List);
            assert_eq!(on_list.screen, Screen::Full);
            assert!(on_list.list.is_some() && on_list.detail.is_none());
            let on_details = layout::areas(Rect::new(0, 0, 90, 30), Focus::Details);
            assert!(on_details.list.is_none() && on_details.detail.is_some());
            assert_eq!(layout::areas(Rect::new(0, 0, 100, 30), Focus::List).screen, Screen::Split);
            assert_eq!(layout::areas(Rect::new(0, 0, 99, 30), Focus::List).screen, Screen::Full);
        }

        #[test]
        fn under_60_by_20_there_is_only_a_request_for_room() {
            for (width, height) in [(50, 15), (59, 30), (100, 19)] {
                assert_eq!(
                    layout::areas(Rect::new(0, 0, width, height), Focus::List).screen,
                    Screen::TooSmall,
                    "{width}x{height}"
                );
            }
            assert_eq!(layout::areas(Rect::new(0, 0, 60, 20), Focus::List).screen, Screen::Full);
        }

        #[test]
        fn enter_goes_to_the_details_and_esc_comes_back_with_the_selection() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, 'j');
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.focus(), Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), Focus::List);
            assert_eq!(app.list().selected_id(), Some("b"));
        }
    }

    mod moving {
        use super::*;

        fn expect_move(request: Request) -> narsor::tui::task_app::list::MoveRequest {
            match request {
                Request::Move(request) => request,
                other => panic!("a move was expected, got {other:?}"),
            }
        }

        #[test]
        fn k_and_j_ask_to_go_before_the_row_above_and_after_the_row_below() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2), queued("c", 3)], "c"));
            ch(&mut app, 'j');
            let up = expect_move(ch(&mut app, 'K'));
            assert_eq!(up.target, MoveTarget::Before(ids(&["a"]).remove(0)));
            let down = expect_move(ch(&mut app, 'J'));
            assert_eq!(down.target, MoveTarget::After(ids(&["c"]).remove(0)));
            assert_eq!(down.generation, 1);
        }

        #[test]
        fn k_on_the_first_row_and_j_on_the_last_say_there_is_nowhere_to_go() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            assert_eq!(ch(&mut app, 'K'), Request::None);
            assert!(note(&app).contains("pierwsze"), "{}", note(&app));
            ch(&mut app, 'G');
            assert_eq!(ch(&mut app, 'J'), Request::None);
            assert!(note(&app).contains("ostatnie"), "{}", note(&app));
        }

        #[test]
        fn k_on_a_running_task_says_so_and_sends_nothing() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), controlled("b", "local", "running", 2)],
                "c",
            ));
            ch(&mut app, 'j');
            assert_eq!(ch(&mut app, 'K'), Request::None);
            assert_eq!(note(&app), "przesuń: zadanie nie jest w kolejce");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
            assert_eq!(ch(&mut app, 'm'), Request::None);
            assert!(app.list().moving().is_none(), "no move mode for a running task");
        }

        #[test]
        fn m_previews_the_place_and_enter_sends_after_for_a_task_moved_down() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3), queued("d", 4)],
                "c",
            ));
            ch(&mut app, 'm');
            ch(&mut app, 'j');
            ch(&mut app, 'j');
            assert_eq!(titles(&app), ["b", "c", "a", "d"], "the preview moves the row");
            assert_eq!(app.list().tasks()[0].task_id.as_str(), "a", "the data does not");
            assert_eq!(app.list().selected_id(), Some("a"));
            let request = expect_move(press(&mut app, KeyCode::Enter));
            assert_eq!(request.task_id, "a");
            assert_eq!(request.target, MoveTarget::After(ids(&["c"]).remove(0)));
            assert!(app.list().moving().is_none());
            assert_eq!(titles(&app), ["a", "b", "c", "d"], "nothing moves before the server says so");
        }

        #[test]
        fn moving_up_goes_before_the_row_it_takes_the_place_of() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c",
            ));
            ch(&mut app, 'G');
            ch(&mut app, 'm');
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(titles(&app), ["c", "a", "b"]);
            let request = expect_move(press(&mut app, KeyCode::Enter));
            assert_eq!(request.target, MoveTarget::Before(ids(&["a"]).remove(0)));
        }

        #[test]
        fn esc_and_an_unchanged_place_send_nothing() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, 'm');
            ch(&mut app, 'j');
            press(&mut app, KeyCode::Esc);
            assert!(app.list().moving().is_none());
            assert_eq!(titles(&app), ["a", "b"]);

            ch(&mut app, 'm');
            ch(&mut app, 'j');
            ch(&mut app, 'k');
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        }

        #[test]
        fn the_preview_stops_at_the_ends_of_the_queue() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, 'm');
            for _ in 0..5 {
                ch(&mut app, 'j');
            }
            assert_eq!(titles(&app), ["b", "a"]);
            for _ in 0..5 {
                ch(&mut app, 'k');
            }
            assert_eq!(titles(&app), ["a", "b"]);
        }

        #[test]
        fn a_move_is_refused_with_a_reason_while_the_connection_is_down() {
            let mut app = app();
            // Never heard from the server: connecting.
            app.on_stream(Some(Err(ClientError::Unavailable("refused".into()))));
            assert_eq!(app.connection().state(), LinkState::Connecting);
            assert!(!app.mutation_allowed());
            assert!(note(&app).contains("łączę"), "{}", note(&app));
        }

        #[tokio::test]
        async fn a_move_the_server_refuses_reloads_the_list_and_says_why() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            assert_eq!(titles(&app), ["t1", "t2", "t3"]);

            // Another client archives t1 behind the list's back.
            harness
                .owner
                .control_command(
                    "t1",
                    &narsor::controller_protocol::OwnerCommandRequest {
                        command: narsor::orchestration::OwnerCommand::Archive,
                        idempotency_key: "archive-t1".into(),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archive");

            let request = ch(&mut app, 'J');
            app.perform(request).await;
            let text = note(&app);
            assert!(text.contains("odrzucone") && text.contains("t1"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            assert_eq!(titles(&app), ["t2", "t3"], "the list was read again");
            assert_eq!(app.list().selected_id(), Some("t2"));
        }

        /// `m` `j` `j` `Enter` over the real API: a `move --after` goes out
        /// and the `Moved` event patches the list without a read.
        #[tokio::test]
        async fn a_moved_event_patches_the_list_without_reading_it_again() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            ch(&mut app, 'm');
            ch(&mut app, 'j');
            ch(&mut app, 'j');
            let request = press(&mut app, KeyCode::Enter);
            let Request::Move(move_request) = &request else {
                panic!("a move was expected");
            };
            assert!(matches!(move_request.target, MoveTarget::After(_)));
            app.perform(request).await;
            assert_eq!(titles(&app), ["t1", "t2", "t3"], "no read, no event yet");
            assert!(note(&app).contains("przesunięto za t3"), "{}", note(&app));

            let before_cursor = app.cursor().map(str::to_string);
            for _ in 0..5 {
                if titles(&app) == ["t2", "t3", "t1"] {
                    break;
                }
                let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("the Moved event arrives");
                app.on_stream(item);
            }
            assert_eq!(titles(&app), ["t2", "t3", "t1"]);
            assert_ne!(app.cursor().map(str::to_string), before_cursor);
            assert_eq!(app.list().selected_id(), Some("t1"));
            let ranks: Vec<u32> = app.list().tasks().iter().map(|task| task.rank).collect();
            assert_eq!(ranks, [1, 2, 3]);
            assert!(!app.refresh_wanted(), "a move is patched, not re-read");
        }

        /// The stream drops; whatever happens meanwhile is delivered when it
        /// resumes from the cursor of the last event applied.
        #[tokio::test]
        async fn a_dropped_stream_resumes_from_its_cursor_and_loses_no_move() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            let top = MoveTaskRequest {
                target: MoveTarget::Top,
                expected_generation: None,
            };
            // A first move, seen live.
            harness.owner.control_move_task("t3", &top).await.expect("move");
            for _ in 0..5 {
                if titles(&app) == ["t3", "t1", "t2"] {
                    break;
                }
                let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("the first Moved arrives");
                app.on_stream(item);
            }
            assert_eq!(titles(&app), ["t3", "t1", "t2"]);

            // The connection breaks; a second move happens while it is down.
            feed.close();
            assert!(!feed.is_open());
            harness.owner.control_move_task("t2", &top).await.expect("move");
            assert_eq!(titles(&app), ["t3", "t1", "t2"], "the app has not heard of it");

            feed.ensure_open(&app);
            for _ in 0..5 {
                if titles(&app) == ["t2", "t3", "t1"] {
                    break;
                }
                let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("the second Moved arrives after the resume");
                app.on_stream(item);
            }
            assert_eq!(titles(&app), ["t2", "t3", "t1"]);
        }
    }

    mod connection {
        use super::*;

        fn status(app: &TaskApp) -> String {
            let status = app.connection().status(Instant::now());
            format!("{} {}", status.icon, status.text)
        }

        #[test]
        fn the_stream_dropping_and_coming_back_changes_the_header_not_the_rows() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            assert_eq!(status(&app), "● live");

            app.on_stream(Some(Err(ClientError::Unavailable("reset".into()))));
            assert!(status(&app).starts_with("◌ ponawiam ("), "{}", status(&app));
            assert!(app.connection().is_stale());
            assert_eq!(titles(&app), ["a", "b"], "the data stays, grey");
            assert!(!app.mutation_allowed());
            assert!(note(&app).contains("ponawiam"), "{}", note(&app));

            app.on_stream(event(
                "e",
                "a",
                2,
                TaskLifecycle::Provisioning,
                TaskEventKind::Created,
            ));
            assert_eq!(status(&app), "● live");
            assert!(app.mutation_allowed());
        }

        #[test]
        fn a_stream_that_ends_for_good_is_no_connection_and_is_not_reopened() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.on_stream(Some(Err(ClientError::Unauthorized)));
            assert_eq!(status(&app), "✖ brak połączenia");
            assert!(app.stream_ended());
            let mut feed = EventFeed::new(offline_client());
            feed.ensure_open(&app);
            assert!(!feed.is_open());
        }

        #[tokio::test]
        async fn an_unreachable_server_opens_as_connecting_and_goes_live_without_a_restart() {
            // Reserve a port, and let nothing listen on it yet.
            let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = reserved.local_addr().unwrap();
            drop(reserved);

            let harness = RemoteHarness::start().await;
            harness.create("t1", None).await;
            let client = ApiClient::new(address, harness.handle.token());
            let mut app = TaskApp::new(client.clone(), "moj-serwer", None);

            app.refresh().await;
            assert_eq!(app.connection().state(), LinkState::Connecting);
            assert_eq!(status(&app), "◌ łączę z moj-serwer");
            assert!(app.list().tasks().is_empty());
            assert!(app.cursor().is_none());
            let mut feed = EventFeed::new(client);
            feed.ensure_open(&app);
            assert!(!feed.is_open(), "no cursor yet, so no stream");
            assert!(!app.mutation_allowed());

            // The server comes up on that address.
            let listener = TcpListener::bind(address).await.expect("bind the reserved port");
            let target = harness.handle.addr();
            tokio::spawn(async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        if let Ok(mut outbound) = TcpStream::connect(target).await {
                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                        let _ = inbound.shutdown().await;
                    });
                }
            });

            // Not due yet; due once the retry delay has passed.
            assert!(!app.refresh_if_due(Instant::now()).await);
            assert!(app.refresh_if_due(Instant::now() + Duration::from_secs(2)).await);
            assert_eq!(status(&app), "● live");
            assert_eq!(titles(&app), ["t1"]);
            assert_eq!(app.connection().name(), "local", "the server's own name replaces the configured one");
            feed.ensure_open(&app);
            assert!(feed.is_open());
            assert!(app.mutation_allowed());
        }
    }
}

mod task_app_detail {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ApiClient, ClientError, ConversationEntryDto, ConversationResponse};
    use narsor::controller_protocol::{
        ConversationUnavailableReason, ExecutionEventsResponse, ExecutionEventsSource, RunnerClient,
    };
    use narsor::domain::{Event, ExecutionStatus};
    use narsor::engine::{ConversationState, PositionStatus, Timeline};
    use narsor::orchestration::{Observation, QueueReason, TaskEventKind, TaskLifecycle};
    use narsor::persistence::conversation::ConversationSource;
    use narsor::tui::style::Styles;
    use narsor::tui::task_app::detail::{
        self, hyperlinks_supported, queue_text, readiness, short_age, unstarted_message,
        DetailRead, Readiness,
    };
    use narsor::tui::task_app::tabs::conversation::{AttemptRead, Attempts, Conversation};
    use narsor::tui::task_app::tabs::events::Availability;
    use narsor::tui::task_app::tabs::info;
    use narsor::tui::task_app::{DetailTab, Focus, TaskApp};
    use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};

    use super::remote_support::{report, snapshot, RemoteHarness};
    use super::task_app_support::{
        event, execution_events, listing, position, queued, task, timeline, titled, with_controller,
        with_runner,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ch(app: &mut TaskApp, character: char) {
        press(app, KeyCode::Char(character));
    }

    fn running(id: &str, rank: u32) -> narsor::controller_protocol::TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Running,
            "review",
        )
    }

    /// An app over running `a` and `b`, with the details of `a` open.
    fn details_app() -> TaskApp {
        let mut app = app();
        app.load(listing(
            vec![titled(running("a", 1), "NAR-1"), titled(running("b", 2), "NAR-2")],
            "c",
        ));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        app
    }

    fn read_timeline(app: &mut TaskApp, id: &str, revision: u64, steps: &[&str], current: &str) {
        let positions = steps
            .iter()
            .map(|state| {
                let status = if format!("{state} #1") == current {
                    PositionStatus::InProgress
                } else {
                    PositionStatus::Succeeded
                };
                position(state, status)
            })
            .collect();
        app.on_detail_read(
            id,
            revision,
            DetailRead::Timeline(Ok(timeline(id, positions, Some(current)))),
        );
    }

    fn selected_step(app: &TaskApp, id: &str) -> Option<String> {
        app.detail()
            .view(id)
            .and_then(|view| view.progress().selected_label().map(str::to_string))
    }

    mod progress {
        use super::*;

        #[test]
        fn follows_a_new_step_after_an_event() {
            let mut app = details_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            assert_eq!(selected_step(&app, "a").as_deref(), Some("review #1"));
            assert_eq!(app.detail().view("a").unwrap().progress().step_count(), Some((3, 3)));

            // The runner moves on: the event bumps the row's revision, which makes the
            // details due, and the new timeline takes the selection with it.
            app.on_stream(event("e", "a", 2, TaskLifecycle::Running, TaskEventKind::Created));
            assert!(app.detail().view("a").unwrap().progress().needs_read(2));
            read_timeline(&mut app, "a", 2, &["plan", "edit", "review", "test"], "test #1");
            assert_eq!(selected_step(&app, "a").as_deref(), Some("test #1"));
            assert_eq!(app.detail().view("a").unwrap().progress().step_count(), Some((4, 4)));
        }

        #[test]
        fn f_stops_following_and_the_cursor_stays_on_its_step() {
            let mut app = details_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            assert!(app.detail().view("a").unwrap().progress().follows());

            ch(&mut app, 'f');
            assert!(!app.detail().view("a").unwrap().progress().follows());
            read_timeline(&mut app, "a", 2, &["plan", "edit", "review", "test"], "test #1");
            assert_eq!(
                selected_step(&app, "a").as_deref(),
                Some("review #1"),
                "the cursor stays where it is"
            );

            ch(&mut app, 'f');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("test #1"), "following again picks the running step up");
        }

        #[test]
        fn moving_by_hand_ends_the_following_and_a_refresh_changes_neither() {
            let mut app = details_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("edit #1"));
            assert!(!app.detail().view("a").unwrap().progress().follows());

            let tasks = vec![titled(running("a", 1), "NAR-1"), titled(running("b", 2), "NAR-2")];
            app.load(listing(tasks, "d"));
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            assert_eq!(selected_step(&app, "a").as_deref(), Some("edit #1"));
            assert!(!app.detail().view("a").unwrap().progress().follows());
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            assert_eq!(app.focus(), Focus::Details);

            ch(&mut app, 'g');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            ch(&mut app, 'G');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("review #1"));
        }

        #[test]
        fn coming_back_to_a_task_finds_its_tab_and_cursor() {
            let mut app = details_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Right);
            assert_eq!(app.detail().tab(), DetailTab::Events);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Ok(ExecutionEventsResponse::Available {
                        events: execution_events(5),
                        has_more: false,
                        source: ExecutionEventsSource::Runner,
                    }),
                },
            );
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(app.detail().view("a").unwrap().events().cursor(), 2);

            // Away to b and back.
            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("b"));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.detail().tab(), DetailTab::Events, "the tab is the operator's, not the task's");
            assert!(app.detail().view("b").is_none_or(|view| view.events().events().is_empty()));
            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'k');
            press(&mut app, KeyCode::Enter);

            assert_eq!(app.list().selected_id(), Some("a"));
            assert_eq!(app.detail().view("a").unwrap().events().cursor(), 2);
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            assert!(!app.detail().view("a").unwrap().progress().follows());
        }

        #[test]
        fn a_failed_timeline_read_is_a_message_in_the_tab_and_the_rest_works() {
            let mut app = details_app();
            app.on_detail_read(
                "a",
                1,
                DetailRead::Timeline(Err(ClientError::Degraded {
                    code: "runner_timeout".into(),
                    message: "the runner did not answer".into(),
                })),
            );
            let view = app.detail().view("a").unwrap();
            assert_eq!(view.progress().failure(), Some("the runner did not answer"));
            assert!(app.connection().is_live(), "a 503 with a reason is not a lost connection");

            // The other tabs still work.
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Right);
            assert_eq!(app.detail().tab(), DetailTab::Info);
            let lines = info::lines(app.list().selected().unwrap(), &Styles::new(false), false);
            assert!(!lines.is_empty());

            // The next good read clears the message.
            read_timeline(&mut app, "a", 1, &["plan"], "plan #1");
            assert_eq!(app.detail().view("a").unwrap().progress().failure(), None);
        }
    }

    mod not_started {
        use super::*;

        fn flatten(lines: &[ratatui::text::Line<'_>]) -> String {
            lines
                .iter()
                .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[test]
        fn a_queued_task_says_it_has_not_started_and_info_shows_what_it_was_sealed_with() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            app.list_mut().select("b");
            let task = app.list().selected().unwrap().clone();
            let place = app.list().queue_position("b");
            assert_eq!(place, Some(2));
            assert_eq!(readiness(&task), Readiness::NotStarted);
            assert_eq!(
                unstarted_message(&task, place).as_deref(),
                Some("jeszcze nie wystartowało, #2 w kolejce")
            );

            let text = flatten(&info::lines(&task, &Styles::new(false), false));
            for expected in ["Zapieczętowane wejścia", "launch digest", "digest", "środowisko", "implement-feature v3"] {
                assert!(text.contains(expected), "{expected:?} in\n{text}");
            }
        }

        #[tokio::test]
        async fn nothing_is_read_for_a_task_without_an_execution() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.sync_detail().await;
            assert!(app.connection().is_live(), "no request was made, so none failed");
            assert!(app.detail().view("a").is_none_or(|view| view.progress().timeline().is_none()));
        }

        #[test]
        fn an_archived_task_says_its_storage_went_with_the_archiving() {
            let archived = task("a", "local", "archived", 1);
            assert_eq!(readiness(&archived), Readiness::Archived);
            assert_eq!(
                unstarted_message(&archived, None).as_deref(),
                Some("storage usunięty przy archiwizacji")
            );
            let provisioning = task("b", "local", "provisioning", 2);
            assert_eq!(
                unstarted_message(&provisioning, None).as_deref(),
                Some("jeszcze nie wystartowało, provisioning")
            );
            assert_eq!(unstarted_message(&running("c", 3), None), None);
        }

        #[test]
        fn the_queue_line_says_why_a_task_waits() {
            let waiting = super::super::task_app_support::waiting_for_capacity(queued("a", 1), 2, 2, 3);
            assert_eq!(queue_text(&waiting, Some(2)), "#2 · czeka na slot (2/3)");
            let mut offline = queued("b", 2);
            offline.queue = Some(narsor::controller_protocol::QueueView {
                reason: QueueReason::ControllerOffline,
                message: String::new(),
                since: offline.as_of - chrono::Duration::minutes(4),
            });
            assert_eq!(queue_text(&offline, Some(1)), "kontroler offline 4m");
            assert_eq!(queue_text(&queued("c", 3), Some(3)), "#3 · w kolejce");
        }
    }

    mod events_and_conversation {
        use super::*;

        fn open_tab(app: &mut TaskApp, tab: DetailTab) {
            while app.detail().tab() != tab {
                press(app, KeyCode::Right);
            }
        }

        #[test]
        fn a_failed_events_read_is_a_message_in_the_tab_and_not_a_lost_connection() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Events);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Err(ClientError::Degraded {
                        code: "runner_timeout".into(),
                        message: "the runner did not answer in time".into(),
                    }),
                },
            );
            let events = app.detail().view("a").unwrap().events();
            assert_eq!(events.failure(), Some("the runner did not answer in time"));
            assert!(app.connection().is_live());
            assert!(app.mutation_allowed());

            // An unreachable server is another matter.
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Err(ClientError::Unavailable("refused".into())),
                },
            );
            assert!(!app.connection().is_live());
        }

        #[test]
        fn unavailable_events_keep_the_reason_and_what_was_read_before() {
            let mut app = details_app();
            let available = |events| {
                Ok(ExecutionEventsResponse::Available {
                    events,
                    has_more: false,
                    source: ExecutionEventsSource::Runner,
                })
            };
            app.on_detail_read("a", 1, DetailRead::Events { after: 0, result: available(execution_events(3)) });
            app.on_detail_read(
                "a",
                2,
                DetailRead::Events {
                    after: 3,
                    result: Ok(ExecutionEventsResponse::Unavailable {
                        reason: ConversationUnavailableReason::RunnerUnreachable,
                        message: "the runner has not been heard from lately".into(),
                    }),
                },
            );
            let events = app.detail().view("a").unwrap().events();
            assert_eq!(events.events().len(), 3);
            assert!(matches!(
                events.availability(),
                Availability::Unavailable { reason: ConversationUnavailableReason::RunnerUnreachable, .. }
            ));
        }

        #[test]
        fn events_are_appended_after_the_last_sequence_and_the_cursor_follows_the_end() {
            let mut app = details_app();
            let page = |events: Vec<Event>, has_more| {
                Ok(ExecutionEventsResponse::Available {
                    events,
                    has_more,
                    source: ExecutionEventsSource::Storage,
                })
            };
            app.on_detail_read("a", 1, DetailRead::Events { after: 0, result: page(execution_events(3), true) });
            let events = app.detail().view("a").unwrap().events();
            assert!(events.has_more());
            assert_eq!(events.after(), 3);
            assert_eq!(events.cursor(), 2);
            app.on_detail_read("a", 1, DetailRead::Events { after: 3, result: page(execution_events(6)[3..].to_vec(), false) });
            let events = app.detail().view("a").unwrap().events();
            assert_eq!(events.events().len(), 6);
            assert_eq!(events.cursor(), 5, "a cursor on the last event stays on the last");
        }

        #[test]
        fn v_opens_the_detail_of_the_event_and_ctrl_v_the_raw_rows() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Events);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Ok(ExecutionEventsResponse::Available {
                        events: execution_events(3),
                        has_more: false,
                        source: ExecutionEventsSource::Runner,
                    }),
                },
            );
            ch(&mut app, 'v');
            assert!(app.detail().view("a").unwrap().events().is_detail_open());
            ch(&mut app, 'v');
            assert!(!app.detail().view("a").unwrap().events().is_detail_open());
            app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
            assert!(app.detail().view("a").unwrap().events().is_raw());
            ch(&mut app, 'g');
            assert_eq!(app.detail().view("a").unwrap().events().cursor(), 0);
        }

        fn entry(id: &str, text: &str) -> TranscriptEntry {
            TranscriptEntry::new(TranscriptEntryId::from(id), vec![text.to_string()], text.as_bytes().to_vec())
                .with_header(format!("AGENT · {id}"))
        }

        fn read(entries: Vec<TranscriptEntry>) -> AttemptRead {
            AttemptRead {
                entries,
                next_entry: 3,
                closed: true,
                truncated_tail: false,
                complete: true,
                no_record: false,
            }
        }

        #[test]
        fn the_conversation_is_every_attempt_in_the_order_of_the_steps() {
            let recorded = |state: &str| {
                let mut step = position(state, PositionStatus::Succeeded);
                step.conversation = ConversationState::Recorded;
                step
            };
            let positions = vec![recorded("plan"), position("script", PositionStatus::Succeeded), recorded("edit")];
            let mut attempts = Attempts::default();
            attempts.apply("act-plan", 1, 0, Ok(read(vec![entry("1", "plan one"), entry("2", "plan two")])));
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            assert!(conversation.is_loading(), "act-edit was not read yet");
            assert_eq!(conversation.transcript().entries().len(), 3, "a heading and two entries");

            attempts.apply("act-edit", 1, 0, Ok(read(vec![entry("1", "edit one")])));
            conversation.rebuild(&positions, &attempts);
            assert!(!conversation.is_loading());
            let lines: Vec<String> = conversation
                .transcript()
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect();
            let at = |needle: &str| lines.iter().position(|line| line.contains(needle)).unwrap();
            assert!(at("plan #1") < at("plan one") && at("plan two") < at("edit #1") && at("edit #1") < at("edit one"), "{lines:?}");

            attempts.apply("act-plan", 2, 3, Err("the runner did not answer".into()));
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.failure(), Some("plan #1: the runner did not answer"));
            assert_eq!(conversation.transcript().entries().len(), 5, "what was read stays");
        }

        #[test]
        fn keys_scroll_expand_and_switch_to_the_raw_view_of_the_conversation() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Conversation);
            // No transcript yet: keys are harmless.
            ch(&mut app, 'j');
            press(&mut app, KeyCode::PageDown);
            ch(&mut app, 'G');
            ch(&mut app, 'v');
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
        }
    }

    mod relay {
        use super::*;

        /// The runner's side of one relayed events read.
        async fn answer_events(runner: &RunnerClient, events: Vec<Event>, has_more: bool) {
            loop {
                let poll = runner.poll(None, 5).await.expect("poll");
                if let Some(request) = poll.execution_events.first() {
                    let page = narsor::controller_protocol::ExecutionEventsPage { events, has_more };
                    runner
                        .upload_execution_events(&request.request_id, &page)
                        .await
                        .expect("upload");
                    return;
                }
            }
        }

        /// The runner's side of one relayed conversation page.
        async fn answer_page(runner: &RunnerClient, page: ConversationResponse) {
            loop {
                let poll = runner.poll(None, 5).await.expect("poll");
                if let Some(request) = poll.conversation_pages.first() {
                    runner
                        .upload_conversation_page(&request.request_id, &page)
                        .await
                        .expect("upload");
                    return;
                }
            }
        }

        fn live_snapshot() -> narsor::orchestration::RunnerSnapshot {
            let mut recorded = position("plan", PositionStatus::Succeeded);
            recorded.conversation = ConversationState::Recorded;
            let current = position("review", PositionStatus::InProgress);
            let mut first = snapshot(1, None);
            first.timeline = Some(Timeline {
                default_position: Some(current.label.clone()),
                positions: vec![recorded, current],
                ..Timeline::default()
            });
            first
        }

        async fn app_over(harness: &RemoteHarness) -> TaskApp {
            let client = harness.owner.clone();
            let mut app = TaskApp::new(client, "remote", None);
            app.refresh().await;
            assert_eq!(app.list().tasks().len(), 1);
            press(&mut app, KeyCode::Enter);
            app
        }

        #[tokio::test]
        async fn the_timeline_of_a_remote_task_is_read_and_followed() {
            let harness = RemoteHarness::start().await;
            harness.running("task-1", "k8s", live_snapshot()).await;
            let mut app = app_over(&harness).await;

            app.sync_detail().await;
            let view = app.detail().view("task-1").unwrap();
            assert_eq!(view.progress().positions().len(), 2);
            assert_eq!(view.progress().selected_label(), Some("review #1"));
            assert!(app.connection().is_live());
        }

        #[tokio::test]
        async fn events_of_a_live_runner_travel_through_the_relay() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            let mut app = app_over(&harness).await;
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Right);
            assert_eq!(app.detail().tab(), DetailTab::Events);

            let (_, ()) = tokio::join!(
                app.sync_detail(),
                answer_events(&running.runner, execution_events(4), false)
            );
            let events = app.detail().view("task-1").unwrap().events();
            assert_eq!(events.events().len(), 4);
            assert_eq!(*events.availability(), Availability::Available(ExecutionEventsSource::Runner));
        }

        #[tokio::test]
        async fn a_remote_task_without_a_live_runner_says_why_in_the_events_tab() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            running
                .controller
                .report(&report(
                    "task-1",
                    running.epoch,
                    1,
                    Observation::WorkloadLost { state_known: true },
                    Some(1),
                ))
                .await
                .expect("lost");
            let mut app = app_over(&harness).await;
            assert_eq!(app.list().selected().unwrap().lifecycle, TaskLifecycle::Interrupted);
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::Right);

            app.sync_detail().await;
            match app.detail().view("task-1").unwrap().events().availability() {
                Availability::Unavailable { reason, message } => {
                    assert_eq!(*reason, ConversationUnavailableReason::Interrupted);
                    assert!(message.contains("runner is not running"), "{message}");
                }
                other => panic!("expected the reason, got {other:?}"),
            }
            assert!(app.connection().is_live());
        }

        #[tokio::test]
        async fn the_conversation_pages_of_a_recorded_step_come_through_the_relay() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            let mut app = app_over(&harness).await;
            press(&mut app, KeyCode::Right);
            assert_eq!(app.detail().tab(), DetailTab::Conversation);

            let page = ConversationResponse::Recorded {
                entries: vec![ConversationEntryDto {
                    entry_number: 1,
                    timestamp: chrono::Utc::now(),
                    source: ConversationSource::Narsor,
                    raw_base64: {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .encode(r#"{"event":"prompt","prompt":"Fix it"}"#)
                    },
                }],
                next_entry: 2,
                closed: true,
                truncated_tail: false,
            };
            let (_, ()) = tokio::join!(app.sync_detail(), answer_page(&running.runner, page));
            let view = app.detail().view("task-1").unwrap();
            let lines: Vec<String> = view
                .conversation()
                .transcript()
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect();
            assert!(lines.iter().any(|line| line.contains("plan #1")), "{lines:?}");
            assert!(lines.iter().any(|line| line.contains("Fix it")), "{lines:?}");
            assert!(view.attempts().get("act-plan").is_some());
        }
    }

    mod odds_and_ends {
        use super::*;

        #[test]
        fn ages_use_the_largest_whole_unit() {
            assert_eq!(short_age(-5), "0s");
            assert_eq!(short_age(45), "45s");
            assert_eq!(short_age(240), "4m");
            assert_eq!(short_age(3 * 3600 + 59), "3h");
            assert_eq!(short_age(2 * 86400), "2d");
        }

        #[test]
        fn hyperlinks_are_only_for_terminals_known_to_draw_them() {
            let env = |pairs: &'static [(&'static str, &'static str)]| {
                move |name: &str| {
                    pairs
                        .iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| (*value).to_string())
                }
            };
            assert!(hyperlinks_supported(env(&[("TERM_PROGRAM", "iTerm.app")])));
            assert!(hyperlinks_supported(env(&[("WT_SESSION", "x")])));
            assert!(hyperlinks_supported(env(&[("VTE_VERSION", "6800")])));
            assert!(hyperlinks_supported(env(&[("TERM", "xterm-kitty")])));
            assert!(!hyperlinks_supported(env(&[("VTE_VERSION", "4000")])));
            assert!(!hyperlinks_supported(env(&[("TERM", "xterm-256color")])));
            assert!(!hyperlinks_supported(env(&[("TERM", "dumb"), ("WT_SESSION", "x")])));
        }

        #[test]
        fn the_areas_leave_room_for_a_panel_at_the_bottom() {
            let area = ratatui::layout::Rect::new(10, 2, 80, 20);
            let plain = detail::areas(area, 0);
            assert_eq!(plain.header.height, 2);
            assert_eq!(plain.tabs.y, area.y + 2);
            assert_eq!(plain.content.y, area.y + 3);
            assert_eq!(plain.content.height, 17);
            assert_eq!(plain.panel, None);

            let with_panel = detail::areas(area, 6);
            assert_eq!(with_panel.content.height, 11);
            let panel = with_panel.panel.expect("a panel");
            assert_eq!((panel.y, panel.height), (area.y + 14, 6));
            assert_eq!(detail::areas(area, 40).content.height, 1, "the content keeps a row");
        }

        #[test]
        fn the_details_of_a_task_that_left_the_list_are_forgotten() {
            let mut app = details_app();
            read_timeline_for(&mut app, "b");
            assert!(app.detail().view("b").is_some());
            app.load(listing(vec![titled(running("a", 1), "NAR-1")], "d"));
            assert!(app.detail().view("b").is_none());
        }

        fn read_timeline_for(app: &mut TaskApp, id: &str) {
            app.on_detail_read(
                id,
                1,
                DetailRead::Timeline(Ok(timeline(id, vec![position("plan", PositionStatus::Succeeded)], None))),
            );
        }
    }
}

/// U10: the panel "Czeka na Ciebie". The first group characterises the form
/// semantics of the old `InputForm`, `ProblemForm` and `FailureForm`
/// (`3f3dab2`, `4ca1e73`, `2a9c6f2`, `9c6af7f`, `c7f3f0d`) on the new panel;
/// the rest is what the panel adds. Nothing here starts a daemon.
mod task_app_waiting {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::{json, Value};

    use narsor::api::{ApiClient, ClientError};
    use narsor::conductor::ConductorView;
    use narsor::controller_protocol::{ConductorReadResponse, ExecutionEventsSource};
    use narsor::engine::actions::human::HumanInputAction;
    use narsor::engine::PositionStatus;
    use narsor::orchestration::{AttentionKind, OperatorCommand, OwnerCommand, TaskEventKind, TaskLifecycle};
    use narsor::tui::chooser::ChooserFocus;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::waiting::{AnswerRequest, Submit};
    use narsor::tui::task_app::{Focus, Request, TaskApp};

    use super::remote_support::{snapshot, RemoteHarness};
    use super::task_app_support::{
        event, listing, position, task, timeline, titled, waiting_on, with_controller, with_pending,
        with_runner,
    };
    use narsor::domain::ExecutionStatus;

    fn app() -> TaskApp {
        TaskApp::new(ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn sent(request: Request) -> AnswerRequest {
        match request {
            Request::Answer(answer) => *answer,
            other => panic!("an answer was expected, got {other:?}"),
        }
    }

    fn delivery(request: Request) -> narsor::engine::HumanInputDelivery {
        match sent(request).submit {
            Submit::Input { delivery, .. } => delivery,
            other => panic!("an input was expected, got {other:?}"),
        }
    }

    fn running(id: &str, rank: u32) -> narsor::controller_protocol::TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::InputNeeded,
            "review_scope",
        )
    }

    /// An app whose only task `a` waits; the panel is open on its actions.
    fn opened(task: narsor::controller_protocol::TaskSummary) -> TaskApp {
        let mut app = app();
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        assert!(app.panel_focused(), "Enter on a waiting task opens its panel");
        app
    }

    fn chooser_of(app: &TaskApp) -> &narsor::tui::chooser::Chooser {
        app.detail().view("a").expect("details").waiting().chooser().expect("a panel")
    }

    fn selected_label(app: &TaskApp) -> String {
        chooser_of(app).selected_item().expect("an entry").label.clone()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        use narsor::tui::style::Styles;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(frame, app, &Styles::new(false), std::time::Instant::now())
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -- forms -------------------------------------------------------------

    /// A gate with a resume comment: only `revise` resumes the agent.
    fn gate(episode: &str) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Czy zakres zmian obejmuje też migrację danych?",
            "question": "Czy migracja wchodzi w zakres?",
            "fields": [{ "name": "comment", "type": "text", "required": true }],
            "outputs": [
                { "label": "approved" },
                { "label": "revise", "description": "Zakres wymaga poprawy.", "resume_comment_field": "comment" },
                { "label": "reject" }
            ],
            "resume_targets": { "revise": { "action_execution_id": "act-plan", "harness": "claude", "session_id": "s1" } },
            "episode": episode,
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn question(prefilled: Option<&str>) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Która wersja API?",
            "fields": [{ "name": "answer", "type": "text", "required": true }],
            "continuation": "resume_agent",
            "prefilled_answer": prefilled,
            "episode": "ep-q",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn approval(resumable: bool) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Zatwierdź wynik",
            "outputs": [{ "label": "reviewed", "description": "Wynik przeglądu." }],
            "continuation": "approve_output",
            "approval": {
                "action_execution_id": "act-7",
                "result": { "output_label": "reviewed", "data": { "files": 3 }, "variables": {}, "exit_code": null, "stdout": "", "stderr": "" },
                "agent_session_id": if resumable { json!("sess-1") } else { Value::Null }
            },
            "episode": "ep-a",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn problem(proposed: bool) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "implement",
            "action_execution_id": "act-9",
            "instruction": "Krok nie dał etykiety",
            "outputs": [
                { "label": "implemented", "description": "Funkcja gotowa." },
                { "label": "needs_human_decision", "description": "Najpierw decyzja człowieka." }
            ],
            "continuation": "resolve_output",
            "problem": {
                "reason": "Odpowiedź agenta nie zawiera etykiety.",
                "answers": [{ "action_execution_id": "act-9", "reason": "brak etykiety", "answer_tail": "Done, all tests pass." }],
                "requirements": [
                    { "label": "implemented", "data": [], "variables": ["summary"] },
                    { "label": "needs_human_decision", "data": ["question"], "variables": [] }
                ],
                "analysis": if proposed { json!({
                    "label": "implemented",
                    "data": {},
                    "variables": { "summary": "all units done", "files": 3 },
                    "rationale": "Odpowiedź mówi, że testy przechodzą."
                }) } else { Value::Null }
            },
            "episode": "ep-p",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn gate_app() -> TaskApp {
        opened(with_pending(running("a", 1), gate("ep-1")))
    }

    // -- characterisation of the form semantics --------------------------------

    #[test]
    fn a_resume_comment_is_required_only_for_the_output_that_resumes_the_agent() {
        // 3f3dab2: `approved` has no need of the comment, `revise ↩` does.
        let mut app = gate_app();
        // The state the agent returns to is known from the steps read.
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", vec![position("plan", PositionStatus::Succeeded)], None))),
        );
        let items = chooser_of(&app).items();
        assert!(!items[0].fields[0].required, "approved: the comment is optional");
        assert!(items[1].fields[0].required, "revise: the comment is required");
        assert!(!items[2].fields[0].required);
        assert_eq!(items[1].resumes_agent.as_deref(), Some("plan"), "↩ wraca do agenta: plan");
        assert!(items[0].resumes_agent.is_none());

        // The panel opens on the output that returns the work to the agent.
        assert_eq!(selected_label(&app), "revise");
        // Enter without the comment takes the operator to the field, sends nothing.
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "still refused: the comment is empty");
        assert!(chooser_of(&app).fields_of(1)[0].error().is_some());

        typed(&mut app, "Uwzględnij migrację");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.chosen_label.as_deref(), Some("revise"));
        assert_eq!(delivery.values["comment"], "Uwzględnij migrację");
        assert_eq!(delivery.episode.as_deref(), Some("ep-1"), "the answer names its request");
    }

    #[test]
    fn an_output_without_a_need_for_the_comment_is_sent_at_once() {
        let mut app = gate_app();
        ch(&mut app, '1');
        assert_eq!(selected_label(&app), "approved");
        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(request.summary, "odpowiedziano approved");
        let Submit::Input { pending_input_id, delivery } = request.submit else {
            panic!("an input");
        };
        assert_eq!(pending_input_id, "ep-1");
        assert_eq!(delivery.chosen_label.as_deref(), Some("approved"));
        assert!(delivery.values.is_empty(), "a blank optional field is left out");
    }

    #[test]
    fn a_select_that_offers_exactly_the_outputs_is_the_label_and_not_a_field() {
        // KTD17: the picked value is both the field and the chosen label.
        let form = json!({
            "execution_id": "exec-1",
            "state_id": "resolve_ambiguity",
            "action_execution_id": "act-1",
            "instruction": "Agent zatrzymał się na niejednoznaczności.",
            "fields": [
                { "name": "ambiguity_decision", "type": "select", "required": true, "options": ["answered", "replan"] },
                { "name": "ambiguity_answer", "type": "text" }
            ],
            "outputs": [{ "label": "answered" }, { "label": "replan" }],
            "episode": "ep-g",
            "requested_at": "2026-10-09T11:54:00Z"
        });
        let mut app = opened(with_pending(running("a", 1), form));
        let items = chooser_of(&app).items();
        assert_eq!(items.len(), 2, "one entry per output, no label row");
        assert!(items.iter().all(|item| item.fields.len() == 1 && item.fields[0].name == "ambiguity_answer"));
        ch(&mut app, '2');
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.chosen_label.as_deref(), Some("replan"));
        assert_eq!(delivery.values["ambiguity_decision"], "replan");
    }

    #[test]
    fn a_rejected_resume_gives_the_reply_back_in_the_field() {
        // 2a9c6f2: the text of an answer that could not be delivered returns.
        let mut app = opened(with_pending(running("a", 1), question(Some("wersja 2"))));
        assert_eq!(chooser_of(&app).items()[0].fields[0].value(), "wersja 2");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "a required field: Enter goes to it");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.values["answer"], "wersja 2");
        assert!(delivery.chosen_label.is_none(), "a question has no label");
    }

    #[test]
    fn a_question_is_answered_with_the_text_alone() {
        let mut app = opened(with_pending(running("a", 1), question(None)));
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "the answer is required");
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        typed(&mut app, "ab");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        typed(&mut app, "cd");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Left);
        typed(&mut app, "X");
        assert_eq!(chooser_of(&app).fields_of(0)[0].value(), "ab\nXcd", "an editor, not an append-only line");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.values["answer"], "ab\nXcd", "typed text goes out unchanged");
    }

    #[test]
    fn an_approval_returns_to_the_agent_by_default_and_accepts_on_request() {
        // 9c6af7f / 4ca1e73: accepting is explicit, returning needs a comment.
        let mut app = opened(with_pending(running("a", 1), approval(true)));
        let labels: Vec<_> = chooser_of(&app).items().iter().map(|item| item.label.clone()).collect();
        assert_eq!(labels, ["Akceptuj", "Zwróć agentowi"]);
        assert_eq!(selected_label(&app), "Zwróć agentowi", "nothing moves on by a stray Enter");

        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "the comment comes first");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "and is required");
        typed(&mut app, "popraw testy");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.action, Some(HumanInputAction::Resume));
        assert_eq!(delivery.comment.as_deref(), Some("popraw testy"));

        let mut app = opened(with_pending(running("a", 1), approval(true)));
        ch(&mut app, '1');
        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(request.summary, "zaakceptowano reviewed");
        let Submit::Input { delivery, .. } = request.submit else { panic!("an input") };
        assert_eq!(delivery.action, Some(HumanInputAction::Approve));
        assert_eq!(delivery.episode.as_deref(), Some("ep-a"));
    }

    #[test]
    fn an_approval_whose_session_cannot_be_resumed_offers_a_new_one() {
        let app = opened(with_pending(running("a", 1), approval(false)));
        let item = &chooser_of(&app).items()[1];
        assert_eq!(item.label, "Nowa sesja");
        assert!(item.description.contains("nie da się wznowić"));
    }

    #[test]
    fn a_problem_opens_on_a_message_to_the_agent_and_has_no_data_to_fill() {
        // c7f3f0d: problems start in conversation mode.
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        assert_eq!(selected_label(&app), "Napisz do agenta");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        typed(&mut app, "spróbuj jeszcze raz");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.agent_message.as_deref(), Some("spróbuj jeszcze raz"));
        assert!(delivery.chosen_label.is_none());
    }

    #[test]
    fn a_problem_offers_the_conductors_label_and_values_marked_until_edited() {
        let mut app = opened(with_pending(running("a", 1), problem(true)));
        assert_eq!(selected_label(&app), "implemented", "the recommendation is selected");
        let item = chooser_of(&app).selected_item().unwrap();
        assert!(item.recommended);
        let summary = &item.fields[0];
        assert_eq!(summary.name, "variables.summary");
        assert!(summary.is_proposed(), "◆ until the operator changes it");
        assert_eq!(summary.value(), "all units done");

        // Untouched, a proposal goes out as it was: the number stays a number.
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.chosen_label.as_deref(), Some("implemented"));
        assert_eq!(delivery.variables["summary"], "all units done");
        assert_eq!(delivery.variables["files"], json!(3));

        // Edited, the value is the operator's and the mark is gone.
        let mut app = opened(with_pending(running("a", 1), problem(true)));
        press(&mut app, KeyCode::Tab);
        typed(&mut app, "!");
        let field = &chooser_of(&app).selected_item().unwrap().fields[0];
        assert!(!field.is_proposed());
        assert_eq!(field.value(), "all units done!");
    }

    #[test]
    fn a_problem_label_needing_a_key_is_refused_until_it_is_given() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '2');
        assert_eq!(selected_label(&app), "needs_human_decision");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "data.question is required");
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let field = &chooser_of(&app).fields_of(1)[0];
        assert_eq!(field.name, "data.question");
        assert!(field.error().is_some_and(|error| error.contains("data.question")));

        typed(&mut app, "Konto czy użytkownik?");
        // A key of the operator's own, beside the ones the label needs.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "scope=account");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.chosen_label.as_deref(), Some("needs_human_decision"));
        assert_eq!(delivery.data["question"], "Konto czy użytkownik?");
        assert_eq!(delivery.variables["scope"], "account");
    }

    #[test]
    fn a_key_given_twice_or_without_a_value_is_refused_at_the_panel() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '1');
        press(&mut app, KeyCode::Tab);
        typed(&mut app, "x");
        press(&mut app, KeyCode::Down);
        typed(&mut app, "x=1");
        press(&mut app, KeyCode::Down);
        typed(&mut app, "summary=again");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let error = app.detail().view("a").unwrap().waiting().error().expect("refused").to_string();
        assert!(error.contains("variables.summary"), "{error}");

        // A line without `=` is refused too.
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '1');
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "bez znaku");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert!(app.detail().view("a").unwrap().waiting().error().is_some());
    }

    #[test]
    fn the_conductors_summary_of_a_gate_recommends_an_output_with_the_diamond() {
        let pending: narsor::engine::PendingHumanInput = serde_json::from_value(gate("ep-1")).unwrap();
        let mut panel = narsor::tui::input_form::InputPanel::new(&pending, &|_| None);
        let annotation: narsor::conductor::Annotation = serde_json::from_value(json!({
            "key": "ep-1", "summary": "brak planu migracji", "recommendation": "reject",
            "created_at": "2026-10-09T12:00:00Z"
        }))
        .unwrap();
        panel.annotate(&annotation);
        assert_eq!(panel.items()[panel.selected()].label, "reject");
        assert!(panel.items()[2].recommended);
        assert!(panel.context().iter().any(|line| line.text.contains("◆ dyrygent: brak planu migracji")));

        // A summary of another request changes nothing.
        let mut other = narsor::tui::input_form::InputPanel::new(&pending, &|_| None);
        let mut stranger = annotation.clone();
        stranger.key = "ep-2".to_string();
        other.annotate(&stranger);
        assert!(other.items().iter().all(|item| !item.recommended));
    }

    // -- the panel ------------------------------------------------------------

    #[test]
    fn a_failed_step_confirms_the_conductors_proposal() {
        let failed = waiting_on(running("a", 1), AttentionKind::FailedStep, "fk1", Some("test"));
        let mut app = opened(failed);
        read_conductor(&mut app, conductor(true));
        let labels: Vec<String> = chooser_of(&app).items().iter().map(|item| item.label.clone()).collect();
        assert!(labels[0].starts_with("Potwierdź: Uruchom krok"), "{labels:?}");
        assert!(labels[1].starts_with("Odrzuć: Uruchom krok"));
        assert_eq!(labels[2], "Restart kroku z notatką");
        assert!(chooser_of(&app).items()[0].recommended, "◆ the conductor's");
        assert_eq!(chooser_of(&app).selected(), 0, "the recommendation is selected");

        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(
            request.submit,
            Submit::Operator(OperatorCommand::ConfirmProposal { proposal_id: "p1".to_string() })
        );
        assert_eq!(request.generation, 1);
        assert!(!request.key.is_empty());
    }

    #[test]
    fn a_failed_step_rejects_or_restarts_with_a_note_naming_its_failure() {
        let failed = waiting_on(running("a", 1), AttentionKind::FailedStep, "fk1", Some("test"));
        let mut app = opened(failed);
        read_conductor(&mut app, conductor(true));
        ch(&mut app, '2');
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RejectProposal { proposal_id: "p1".to_string() })
        );

        ch(&mut app, '3');
        press(&mut app, KeyCode::Tab);
        typed(&mut app, "użyj nowszego toolchaina");
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RestartStep {
                note: Some("użyj nowszego toolchaina".to_string()),
                failure_key: Some("fk1".to_string()),
            })
        );
    }

    #[test]
    fn a_failed_step_without_a_conductor_shows_the_raw_error_and_restarts() {
        let failed = waiting_on(running("a", 1), AttentionKind::FailedStep, "fk1", Some("test"));
        let mut app = opened(failed);
        let mut failing = position("test", PositionStatus::Failed);
        failing.error = Some("cargo test: exit 101".to_string());
        app.on_detail_read("a", 1, DetailRead::Timeline(Ok(timeline("a", vec![failing], None))));
        read_conductor_response(
            &mut app,
            Ok(ConductorReadResponse::Unavailable {
                reason: narsor::controller_protocol::ConversationUnavailableReason::RunnerUnreachable,
                message: "brak harnessu".to_string(),
            }),
        );
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("dyrygent niedostępny: brak harnessu"), "{text}");
        // The raw error comes from the timeline once the entry is built again.
        assert_eq!(chooser_of(&app).items().len(), 1, "only the restart");
        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(
            request.submit,
            Submit::Operator(OperatorCommand::RestartStep { note: None, failure_key: Some("fk1".to_string()) })
        );
    }

    #[test]
    fn a_proposal_from_the_chat_is_confirmed_or_rejected() {
        let waiting = waiting_on(running("a", 1), AttentionKind::ConductorProposal, "p1", Some("test"));
        let mut app = opened(waiting);
        read_conductor(&mut app, conductor(false));
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("dyrygent proponuje: Uruchom krok od nowa"), "{text}");
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::ConfirmProposal { proposal_id: "p1".to_string() })
        );
        ch(&mut app, '2');
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RejectProposal { proposal_id: "p1".to_string() })
        );
    }

    #[test]
    fn a_failed_provisioning_starts_again_or_archives_after_the_confirmation_window() {
        let failed = waiting_on(
            task("a", "local", "interrupted", 1),
            AttentionKind::ProvisioningFailed,
            "provisioning-1",
            None,
        );
        let mut app = opened(failed);
        assert_eq!(selected_label(&app), "Uruchom od nowa");
        assert_eq!(sent(press(&mut app, KeyCode::Enter)).submit, Submit::Owner(OwnerCommand::Start));

        ch(&mut app, '2');
        assert!(chooser_of(&app).items()[1].irreversible);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "archiving asks first, in the shared window"
        );
        let confirmation = app
            .overlay()
            .and_then(|overlay| overlay.confirmation())
            .expect("the confirmation window");
        assert!(confirmation.heading().contains("Archiwizuj"));
        let text = screen(&mut app, 120, 40);
        assert!(
            text.contains("Usuwa workload i storage") && text.contains("Tego nie da się cofnąć"),
            "{text}"
        );
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "Enter is not the answer to a y / n window"
        );
        assert!(app.overlay().is_some());
        ch(&mut app, 'n');
        assert!(app.overlay().is_none());
        assert!(
            app.panel_focused(),
            "n took the question back, it did not fold the panel"
        );
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "asks again");
        assert_eq!(
            sent(ch(&mut app, 'y')).submit,
            Submit::Owner(OwnerCommand::Archive)
        );
    }

    #[test]
    fn two_waitings_are_counted_and_the_bracket_keys_move_between_them() {
        let both = with_pending(with_pending(running("a", 1), gate("ep-1")), question(None));
        let mut app = opened(both);
        assert_eq!(app.detail().view("a").unwrap().waiting().len(), 2);
        assert_eq!(app.detail().view("a").unwrap().waiting().current_index(), 0);
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("1/2"), "{text}");
        ch(&mut app, ']');
        assert_eq!(app.detail().view("a").unwrap().waiting().current_index(), 1);
        assert!(screen(&mut app, 120, 40).contains("2/2"));
        ch(&mut app, '[');
        assert_eq!(app.detail().view("a").unwrap().waiting().current_index(), 0);
    }

    #[test]
    fn a_refresh_while_typing_keeps_the_draft_and_the_place() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "Uwzględnij");
        assert!(app.detail().has_drafts());
        let waiting = with_pending(running("a", 1), gate("ep-1"));
        app.load(listing(vec![titled(waiting.clone(), "NAR-12")], "d"));
        app.on_stream(event("e", "a", 2, TaskLifecycle::Running, TaskEventKind::Created));
        app.load(listing(vec![titled(waiting, "NAR-12")], "e"));
        assert!(app.panel_focused());
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "Uwzględnij");
        assert!(app.detail().draft("a").is_some_and(|draft| draft.contains("Uwzględnij")));
        typed(&mut app, " migrację");
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "Uwzględnij migrację");
    }

    #[test]
    fn esc_folds_the_panel_and_opening_it_again_brings_the_draft_back() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "szkic");
        press(&mut app, KeyCode::Esc);
        assert!(!app.panel_focused());
        assert!(app.detail().view("a").unwrap().waiting().is_collapsed());
        assert!(screen(&mut app, 120, 40).contains("Tab rozwiń"));
        assert!(app.detail().has_drafts(), "the draft stays");

        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused() && !app.detail().view("a").unwrap().waiting().is_collapsed());
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "szkic");
        // From the list, Enter does the same.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::List);
        press(&mut app, KeyCode::Enter);
        assert!(app.panel_focused());
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "szkic");
    }

    #[test]
    fn a_waiting_that_goes_away_while_typing_folds_the_panel_and_keeps_the_draft() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "moja odpowiedź");
        // Answered from the CLI: the row no longer waits.
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
        assert!(!app.panel_focused(), "the focus left the panel");
        let panel = app.detail().view("a").unwrap().waiting();
        assert!(panel.is_empty());
        assert_eq!(panel.released().len(), 1);
        assert!(panel.released()[0].text.contains("moja odpowiedź"));
        assert!(app.detail().has_drafts());
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("moja odpowiedź") && text.contains("skopiuj"), "{text}");
        assert!(app.notifier().latest().unwrap().text.contains("oczekiwanie zniknęło"));

        // The box is dismissed on purpose.
        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        press(&mut app, KeyCode::Esc);
        assert!(!app.detail().has_drafts());
        assert!(!app.detail().view("a").unwrap().waiting().is_present());
    }

    #[test]
    fn sending_without_a_connection_is_blocked_with_a_reason_and_keeps_the_draft() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "tekst");
        app.on_stream(Some(Err(ClientError::Unavailable("down".to_string()))));
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let note = app.notifier().latest().expect("a reason").clone();
        assert!(note.text.contains("zablokowana"), "{}", note.text);
        assert_eq!(note.severity, Severity::Warning);
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "tekst");
    }

    #[test]
    fn quitting_with_an_unsent_draft_asks_first() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "q?");
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "q?", "q and ? are text while typing");
        assert!(!app.should_quit());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        ch(&mut app, 'q');
        assert!(!app.should_quit(), "the draft makes q ask");
        assert!(app.notifier().latest().unwrap().text.contains("niewysłany szkic"));
        ch(&mut app, 'q');
        assert!(app.should_quit());

        let mut app = gate_app();
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        ch(&mut app, 'q');
        assert!(app.should_quit(), "no draft, no question");
    }

    #[test]
    fn tab_in_the_details_goes_to_the_panel_and_a_task_that_waits_on_nothing_says_so() {
        let mut app = app();
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "c"));
        press(&mut app, KeyCode::Enter);
        assert!(!app.panel_focused());
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        assert!(app.notifier().latest().unwrap().text.contains("na nic nie czeka"));

        let mut app = gate_app();
        press(&mut app, KeyCode::Esc);
        assert!(!app.panel_focused() && app.focus() == Focus::Details);
        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
    }

    #[test]
    fn untyped_keys_of_the_panel_are_the_keymaps() {
        let mut app = gate_app();
        assert_eq!(app.key_context(), narsor::tui::keymap::Context::Panel);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.key_context(), narsor::tui::keymap::Context::Form, "typing inherits no global key");
        ch(&mut app, '?');
        assert!(!app.is_help_open());
        press(&mut app, KeyCode::Tab);
        ch(&mut app, '?');
        assert!(app.is_help_open());
    }

    fn conductor(with_failure: bool) -> ConductorView {
        let proposal = json!({
            "proposal_id": "p1",
            "text": "Uruchom krok od nowa",
            "at": "2026-10-09T12:00:00Z",
            "record": {
                "episode": if with_failure { "fk1" } else { "ep-x" },
                "state_id": "test",
                "proposal": { "kind": "retry", "summary": "Uruchom krok od nowa", "text": "z nowszym toolchainem" },
                "confidence": "high",
                "rationale": "Błąd wygląda na przejściowy."
            },
            "state": "pending"
        });
        let mut view = json!({ "available": true, "harness": "claude", "proposals": [proposal] });
        if with_failure {
            view["failure"] = json!({
                "key": "fk1",
                "episode": "fk1",
                "state_id": "test",
                "reason": "cargo test nie przeszło",
                "outcome": { "kind": "diagnosed", "diagnosis": "Testy padają przez toolchain.", "proposals": [] },
                "concluded_at": "2026-10-09T12:00:00Z"
            });
        }
        serde_json::from_value(view).expect("a conductor view")
    }

    fn read_conductor(app: &mut TaskApp, view: ConductorView) {
        read_conductor_response(
            app,
            Ok(ConductorReadResponse::Available {
                conductor: Box::new(view),
                source: ExecutionEventsSource::Runner,
            }),
        );
    }

    fn read_conductor_response(app: &mut TaskApp, result: Result<ConductorReadResponse, ClientError>) {
        app.on_detail_read("a", 1, DetailRead::Conductor(result));
    }

    // -- against a served control plane -----------------------------------------

    fn approval_snapshot(sequence: u64) -> narsor::orchestration::RunnerSnapshot {
        let mut snapshot = snapshot(sequence, None);
        let form: narsor::engine::PendingHumanInput = serde_json::from_value(approval(true)).unwrap();
        snapshot.pending_input = Some(narsor::orchestration::PendingInputRef {
            id: "ep-a".to_string(),
            attempt_id: None,
            form,
        });
        snapshot.attention = vec![narsor::orchestration::AttentionItem {
            kind: AttentionKind::Approval,
            state_id: Some("review_scope".to_string()),
            since: chrono::Utc::now(),
            waiting_id: "ep-a".to_string(),
        }];
        snapshot
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn space_enter_accept_enter_sends_returns_to_the_list_and_says_so() {
        let harness = RemoteHarness::start().await;
        let _running = harness.running("NAR-12", "local", approval_snapshot(1)).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;

        ch(&mut app, ' ');
        let request = press(&mut app, KeyCode::Enter);
        assert_eq!(request, Request::None);
        assert!(app.panel_focused());
        ch(&mut app, '1');
        let chooser = app.detail().view("NAR-12").unwrap().waiting().chooser().unwrap();
        assert_eq!(chooser.selected_item().unwrap().label, "Akceptuj");
        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;

        assert_eq!(app.focus(), Focus::List, "after sending, the focus is on the list");
        let note = app.notifier().latest().expect("a notification");
        assert_eq!(note.text, "NAR-12: zaakceptowano reviewed");
        assert_eq!(note.severity, Severity::Success);
        let summary = harness.owner.control_task("NAR-12").await.unwrap();
        assert_eq!(summary.inputs.len(), 1, "the answer was queued for the runner");
        assert_eq!(summary.inputs[0].pending_input_id, "ep-a");
        assert!(!app.detail().has_drafts());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_keeps_the_panel_and_the_text_and_reads_the_task_again() {
        let harness = RemoteHarness::start().await;
        let running = harness.running("NAR-12", "local", approval_snapshot(1)).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "popraw testy");

        // The runner moved on: the request the panel shows is gone.
        running.runner.snapshot(&snapshot(2, None)).await.expect("the runner moved on");
        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;

        let note = app.notifier().latest().unwrap();
        assert_eq!(note.severity, Severity::Error);
        assert!(note.text.starts_with("NAR-12: nie wysłano"), "{}", note.text);
        // The task was read again: nothing waits any more, and the text is kept.
        let panel = app.detail().view("NAR-12").unwrap().waiting();
        assert!(panel.is_empty());
        assert!(panel.released()[0].text.contains("popraw testy"), "{:?}", panel.released());
        assert!(app.detail().has_drafts());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_label_from_the_workflow_cannot_put_control_characters_on_the_bar() {
        let harness = RemoteHarness::start().await;
        let mut form = gate("ep-1");
        form["outputs"][0]["label"] = json!("ap\u{1b}[31mpr\u{202e}oved\u{7}");
        let mut snapshot = snapshot(1, None);
        let pending: narsor::engine::PendingHumanInput = serde_json::from_value(form).unwrap();
        snapshot.pending_input = Some(narsor::orchestration::PendingInputRef {
            id: "ep-1".to_string(),
            attempt_id: None,
            form: pending,
        });
        snapshot.attention = vec![narsor::orchestration::AttentionItem {
            kind: AttentionKind::Question,
            state_id: Some("review_scope".to_string()),
            since: chrono::Utc::now(),
            waiting_id: "ep-1".to_string(),
        }];
        let _running = harness.running("NAR-12", "local", snapshot).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        ch(&mut app, '1');
        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;
        let text = app.notifier().latest().expect("sent").text.clone();
        assert!(text.contains("approved") || text.contains("oved"), "{text}");
        assert!(
            !text.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "no control or bidi characters reach the bar: {text:?}"
        );
    }
}

/// U11: the action menu, the confirmations and the commands of the operator.
mod task_app_actions {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ApiClient, ClientError};
    use narsor::controller_protocol::{OperatorCommandView, RunnerClient, TaskSummary};
    use narsor::domain::ExecutionStatus;
    use narsor::orchestration::{
        AttentionKind, OperatorCommand, OperatorCommandStatus, OwnerCommand, OwnerRef,
        TaskEventKind, TaskLifecycle,
    };
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::actions::{
        availability, refusal_reason, verb, ActionRequest, ActionSend, Availability, Confirmed,
        Overlay, TaskAction,
    };
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{EventFeed, Request, TaskApp};

    use super::remote_support::{snapshot, RemoteHarness};
    use super::task_app_support::{
        displayed, event, listing, queued, task, titled, waiting_on, with_controller,
        with_runner, with_transitions,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn running(id: &str, rank: u32) -> TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Running,
            "review",
        )
    }

    fn finished(id: &str, rank: u32) -> TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Completed,
            "done",
        )
    }

    fn lifecycle(word: &str) -> TaskSummary {
        with_controller(task("a", "local", word, 1))
    }

    fn loaded(task: TaskSummary) -> TaskApp {
        let mut app = app();
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        app
    }

    fn act(request: Request) -> ActionRequest {
        match request {
            Request::Act(request) => *request,
            other => panic!("an action was expected, got {other:?}"),
        }
    }

    fn confirmation(app: &TaskApp) -> &narsor::tui::task_app::actions::Confirmation {
        app.overlay()
            .and_then(Overlay::confirmation)
            .expect("the confirmation window")
    }

    fn unavailable(task: &TaskSummary, action: TaskAction) -> String {
        match availability(task, action) {
            Availability::Unavailable(reason) => reason,
            Availability::Available => panic!("{action:?} should not be available"),
        }
    }

    fn available(task: &TaskSummary, action: TaskAction) -> bool {
        availability(task, action).is_available()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(
                    frame,
                    app,
                    &narsor::tui::style::Styles::new(false),
                    std::time::Instant::now(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -- the table of origin §3 --------------------------------------------

    mod availability_table {
        use super::*;

        #[test]
        fn move_is_for_queued_tasks_only() {
            assert!(available(&queued("a", 1), TaskAction::Move));
            for word in ["running", "paused", "interrupted", "provisioning"] {
                assert_eq!(
                    unavailable(&lifecycle(word), TaskAction::Move),
                    "zadanie nie jest w kolejce",
                    "{word}"
                );
            }
        }

        #[test]
        fn pause_for_running_resume_for_paused_and_nothing_else() {
            let running = running("a", 1);
            assert!(available(&running, TaskAction::PauseResume));
            assert_eq!(verb(&running, TaskAction::PauseResume), "pauza");
            let paused = lifecycle("paused");
            assert!(available(&paused, TaskAction::PauseResume));
            assert_eq!(verb(&paused, TaskAction::PauseResume), "wznowienie");
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::PauseResume),
                "zadanie jest w kolejce"
            );
            assert_eq!(
                unavailable(&lifecycle("provisioning"), TaskAction::PauseResume),
                "zadanie jest przygotowywane"
            );
            assert!(unavailable(&lifecycle("interrupted"), TaskAction::PauseResume)
                .contains("uruchom je od nowa"));
            assert!(unavailable(&finished("a", 1), TaskAction::PauseResume).contains("zakończone"));
            assert!(unavailable(&lifecycle("pause_requested"), TaskAction::PauseResume)
                .contains("wstrzymywane"));
        }

        #[test]
        fn interrupt_needs_a_running_step_and_a_runner_that_answers() {
            assert!(available(&running("a", 1), TaskAction::Interrupt));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Interrupt),
                "zadanie jest w kolejce"
            );
            assert!(unavailable(&lifecycle("paused"), TaskAction::Interrupt).contains("wznów"));
            assert!(unavailable(&finished("a", 1), TaskAction::Interrupt).contains("zakończone"));
            assert_eq!(
                unavailable(&lifecycle("running"), TaskAction::Interrupt),
                "runner jeszcze się nie zgłosił"
            );
            let mut silent = running("a", 1);
            silent.runner.as_mut().unwrap().freshness = narsor::controller_protocol::Freshness::Stale;
            assert!(unavailable(&silent, TaskAction::Interrupt).contains("nie odpowiada"));
        }

        #[test]
        fn a_transition_needs_exits_or_an_override() {
            let exits = [("approved", "ship", Some("Wypuszczamy."))];
            assert!(available(
                &with_transitions(running("a", 1), "review", &exits, None),
                TaskAction::Transition
            ));
            assert!(available(
                &with_transitions(running("a", 1), "review", &[], Some(&["plan"])),
                TaskAction::Transition
            ));
            assert!(unavailable(
                &with_transitions(running("a", 1), "review", &[], None),
                TaskAction::Transition
            )
            .contains("nie ma wyjść"));
            assert!(unavailable(&running("a", 1), TaskAction::Transition)
                .contains("serwer nie podał wyjść"));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Transition),
                "zadanie jest w kolejce"
            );
        }

        #[test]
        fn start_again_is_for_interrupted_tasks_and_a_failed_provisioning() {
            assert!(available(&lifecycle("interrupted"), TaskAction::Restart));
            assert!(available(&lifecycle("unknown"), TaskAction::Restart));
            let failed = waiting_on(
                lifecycle("provisioning"),
                AttentionKind::ProvisioningFailed,
                "p-1",
                None,
            );
            assert!(available(&failed, TaskAction::Restart));
            assert!(unavailable(&running("a", 1), TaskAction::Restart).contains("działa"));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Restart),
                "zadanie jest w kolejce"
            );
            assert!(unavailable(&lifecycle("paused"), TaskAction::Restart).contains("wznów"));
        }

        #[test]
        fn archive_is_for_finished_and_interrupted_tasks() {
            assert!(available(&finished("a", 1), TaskAction::Archive));
            assert!(available(&lifecycle("interrupted"), TaskAction::Archive));
            let failed = waiting_on(
                lifecycle("provisioning"),
                AttentionKind::ProvisioningFailed,
                "p-1",
                None,
            );
            assert!(available(&failed, TaskAction::Archive));
            for blocked in [running("a", 1), queued("a", 1), lifecycle("paused")] {
                assert!(
                    unavailable(&blocked, TaskAction::Archive).contains("zakończone lub przerwane"),
                    "{:?}",
                    blocked.lifecycle
                );
            }
        }

        #[test]
        fn a_link_needs_a_value() {
            assert_eq!(
                unavailable(&running("a", 1), TaskAction::OpenLink),
                "workflow nie deklaruje linków"
            );
            let unresolved = displayed(running("a", 1), "NAR-12", &[("Jira", "—", None)]);
            assert_eq!(
                unavailable(&unresolved, TaskAction::OpenLink),
                "linki zadania nie mają jeszcze wartości"
            );
            let unsafe_scheme = displayed(
                running("a", 1),
                "NAR-12",
                &[("Ewil", "x", Some("javascript:alert(1)"))],
            );
            assert!(!available(&unsafe_scheme, TaskAction::OpenLink));
            let ok = displayed(
                running("a", 1),
                "NAR-12",
                &[("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12"))],
            );
            assert!(available(&ok, TaskAction::OpenLink));
        }

        #[test]
        fn every_row_of_the_origin_table_has_a_key_an_action_and_a_rule() {
            for (key, action) in [
                ('m', TaskAction::Move),
                ('p', TaskAction::PauseResume),
                ('x', TaskAction::Interrupt),
                ('t', TaskAction::Transition),
                ('s', TaskAction::Restart),
                ('a', TaskAction::Archive),
                ('o', TaskAction::OpenLink),
            ] {
                assert_eq!(action.key(), key);
                assert!(TaskAction::ALL.contains(&action));
            }
            assert!(TaskAction::Interrupt.needs_confirmation());
            assert!(TaskAction::Archive.needs_confirmation());
            assert!(!TaskAction::PauseResume.needs_confirmation());
            assert!(!TaskAction::OpenLink.changes_state());
        }
    }

    // -- the menu ----------------------------------------------------------

    mod menu {
        use super::*;

        #[test]
        fn dot_on_a_running_task_lists_the_actions_and_greys_out_the_move() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            let Some(Overlay::Menu(_)) = app.overlay() else {
                panic!("the menu should be open");
            };
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let by_label = |needle: &str| {
                chooser
                    .items()
                    .iter()
                    .find(|item| item.label.contains(needle))
                    .unwrap_or_else(|| panic!("no entry {needle}"))
            };
            assert_eq!(by_label("wstrzymaj").unavailable, None);
            assert_eq!(by_label("przerwij krok").unavailable, None);
            assert!(by_label("przerwij krok").label.contains('!'));
            assert_eq!(
                by_label("przejście ręczne").unavailable.as_deref(),
                Some("serwer nie podał wyjść bieżącego stanu"),
                "no exits known, so the entry is greyed with the reason"
            );
            assert_eq!(
                by_label("przesuń").unavailable.as_deref(),
                Some("zadanie nie jest w kolejce")
            );
            assert!(by_label("archiwizuj").unavailable.is_some());
            assert!(by_label("otwórz link").unavailable.is_some());
            assert_eq!(app.key_context(), Context::Menu);
        }

        #[test]
        fn the_transition_entry_is_there_when_the_state_has_exits() {
            let task = with_transitions(running("a", 1), "review", &[("approved", "ship", None)], None);
            let mut app = loaded(task);
            ch(&mut app, '.');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let entry = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("przejście ręczne"))
                .unwrap();
            assert_eq!(entry.unavailable, None);
        }

        #[test]
        fn the_menu_moves_picks_by_digit_and_closes_with_esc() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 0);
            ch(&mut app, 'j');
            ch(&mut app, 'j');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 2);
            ch(&mut app, 'k');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 1);
            ch(&mut app, '4');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 3);
            press(&mut app, KeyCode::Esc);
            assert!(app.overlay().is_none());
            assert_eq!(app.key_context(), Context::List);
        }

        #[test]
        fn enter_on_a_greyed_entry_says_why_and_keeps_the_menu_open() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            // The first entry is the move, which a running task cannot do.
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(note(&app), "przesuń: zadanie nie jest w kolejce");
            assert!(app.overlay().is_some(), "the menu stays");
        }

        #[test]
        fn a_shortcut_in_the_menu_runs_the_action_and_closes_it() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            let request = act(ch(&mut app, 'p'));
            assert!(app.overlay().is_none());
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn enter_on_an_available_entry_runs_it() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            ch(&mut app, '2');
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn a_shortcut_to_an_unavailable_action_in_the_menu_gives_the_same_reason() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            assert_eq!(ch(&mut app, 'a'), Request::None);
            assert!(note(&app).starts_with("archiwizacja: zadanie działa"), "{}", note(&app));
            assert!(app.overlay().is_some());
        }

        #[test]
        fn the_move_keys_work_from_the_menu_of_a_queued_task() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, '.');
            let request = ch(&mut app, 'J');
            assert!(matches!(request, Request::Move(_)), "{request:?}");
            assert!(app.overlay().is_none(), "a move to make closes the menu");

            ch(&mut app, '.');
            ch(&mut app, 'm');
            assert!(app.overlay().is_none());
            assert!(app.list().moving().is_some(), "m enters the move mode");
        }

        #[test]
        fn the_menu_is_rebuilt_when_the_task_changes_and_closed_when_it_leaves() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Paused,
                TaskEventKind::Created,
            ));
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(
                chooser.items().iter().any(|item| item.label.contains("wznów")),
                "the pause became a resume"
            );
            app.on_stream(event(
                "e",
                "a",
                3,
                TaskLifecycle::Archived,
                TaskEventKind::Created,
            ));
            assert!(app.overlay().is_none(), "the task left the list");
        }

        #[test]
        fn the_menu_opens_from_the_details_too() {
            let mut app = loaded(running("a", 1));
            press(&mut app, KeyCode::Enter);
            ch(&mut app, '.');
            assert!(app.overlay().is_some());
        }

        #[test]
        fn nothing_is_opened_without_a_task() {
            let mut app = app();
            ch(&mut app, '.');
            assert!(app.overlay().is_none());
            assert_eq!(ch(&mut app, 'p'), Request::None);
        }
    }

    // -- shortcuts ---------------------------------------------------------

    mod shortcuts {
        use super::*;

        #[test]
        fn p_on_a_queued_task_says_why_it_cannot() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            assert_eq!(ch(&mut app, 'p'), Request::None);
            assert_eq!(note(&app), "pauza: zadanie jest w kolejce");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn p_pauses_a_running_task_and_resumes_a_paused_one_without_asking() {
            let mut app = loaded(running("a", 1));
            let pause = act(ch(&mut app, 'p'));
            assert_eq!(pause.send, ActionSend::Owner(OwnerCommand::Pause));
            assert_eq!(pause.generation, 1);
            assert!(!pause.key.is_empty());
            assert!(app.overlay().is_none(), "no window for a plain action");

            let mut app = loaded(lifecycle("paused"));
            let resume = act(ch(&mut app, 'p'));
            assert_eq!(resume.send, ActionSend::Owner(OwnerCommand::Resume));
        }

        #[test]
        fn s_starts_an_interrupted_task_again_and_says_why_for_a_running_one() {
            let mut app = loaded(lifecycle("interrupted"));
            let start = act(ch(&mut app, 's'));
            assert_eq!(start.send, ActionSend::Owner(OwnerCommand::Start));

            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 's'), Request::None);
            assert!(note(&app).starts_with("uruchomienie od nowa:"), "{}", note(&app));
        }

        #[test]
        fn the_shortcuts_work_in_the_details_too() {
            let mut app = loaded(running("a", 1));
            press(&mut app, KeyCode::Enter);
            let pause = act(ch(&mut app, 'p'));
            assert_eq!(pause.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn a_retry_of_a_decision_that_got_no_answer_reuses_its_key() {
            let mut app = loaded(running("a", 1));
            let first = act(ch(&mut app, 'p'));
            let again = act(ch(&mut app, 'p'));
            assert_eq!(first.key, again.key, "the same decision, the same key");
        }

        #[tokio::test]
        async fn a_lost_connection_keeps_the_key_for_the_retry() {
            let mut app = loaded(running("a", 1));
            let first = act(ch(&mut app, 'p'));
            app.perform(Request::Act(Box::new(first.clone()))).await;
            assert!(
                app.notifier().latest().unwrap().severity == Severity::Error,
                "{}",
                note(&app)
            );
            // The connection came back.
            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            let again = act(ch(&mut app, 'p'));
            assert_eq!(first.key, again.key);
        }
    }

    // -- the confirmation window -------------------------------------------

    mod confirmation {
        use super::*;

        #[test]
        fn x_asks_with_the_effect_and_y_sends_the_interrupt() {
            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 'x'), Request::None);
            let window = confirmation(&app);
            assert!(window.heading().contains("Przerwij"), "{}", window.heading());
            let text = window.lines().join(" ");
            assert!(text.contains("„review”"), "the step it cuts short: {text}");
            assert!(text.contains("NAR-12"), "{text}");
            assert_eq!(app.key_context(), Context::Confirm);

            let request = act(ch(&mut app, 'y'));
            assert_eq!(request.send, ActionSend::Operator(OperatorCommand::Interrupt));
            assert_eq!(request.task_id, "a");
            assert_eq!(request.generation, 1);
            assert!(!request.key.is_empty());
            assert!(app.overlay().is_none());
            assert_eq!(app.key_context(), Context::List);
        }

        #[test]
        fn n_and_esc_take_it_back_and_other_keys_do_nothing() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, 'x');
            assert_eq!(ch(&mut app, 'j'), Request::None);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(app.overlay().is_some(), "only y and n answer it");
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());

            ch(&mut app, 'x');
            assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_on_a_finished_task_asks_and_y_archives() {
            let mut app = loaded(finished("a", 1));
            assert_eq!(ch(&mut app, 'a'), Request::None);
            let window = confirmation(&app);
            assert!(window.heading().contains("Archiwizuj"));
            let text = window.lines().join(" ");
            assert!(text.contains("Usuwa workload i storage"), "{text}");
            assert!(text.contains("nie da się cofnąć"), "{text}");
            let request = act(ch(&mut app, 'y'));
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Archive));
        }

        #[test]
        fn every_action_marked_with_a_bang_asks_the_same_way() {
            for (key, task) in [('x', running("a", 1)), ('a', finished("a", 1))] {
                let mut app = loaded(task);
                ch(&mut app, key);
                assert_eq!(app.key_context(), Context::Confirm, "{key}");
                assert!(matches!(
                    app.overlay().unwrap().confirmation().unwrap().confirmed(),
                    Confirmed::Act(_)
                ));
            }
        }

        #[test]
        fn y_does_not_send_without_a_connection() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, 'x');
            app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));
            assert_eq!(ch(&mut app, 'y'), Request::None);
            assert!(note(&app).contains("zablokowana"), "{}", note(&app));
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_window_closes_when_its_task_leaves_the_list() {
            let mut app = loaded(finished("a", 1));
            ch(&mut app, 'a');
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Archived,
                TaskEventKind::Created,
            ));
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_keys_of_the_window_are_in_the_key_map() {
            let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(
                keymap::lookup(Context::Confirm, &press(KeyCode::Char('y'))),
                Some(Action::Accept)
            );
            assert_eq!(
                keymap::lookup(Context::Confirm, &press(KeyCode::Char('n'))),
                Some(Action::Cancel)
            );
            assert_eq!(
                keymap::lookup(Context::Confirm, &press(KeyCode::Esc)),
                Some(Action::Cancel)
            );
            assert!(Context::ALL.contains(&Context::Confirm), "the confirmation is a context of the map");
            keymap::check_consistency(keymap::BINDINGS).expect("the key map stays consistent");
            let hints = keymap::hints(Context::Confirm);
            assert!(hints.iter().any(|(keys, label)| keys == "y" && *label == "potwierdź"));
            assert!(hints.iter().any(|(keys, label)| keys.starts_with('n') && *label == "anuluj"));
        }
    }

    // -- manual transitions --------------------------------------------------

    mod transitions {
        use super::*;

        fn offered(override_targets: Option<&[&str]>) -> TaskSummary {
            with_transitions(
                running("a", 1),
                "review",
                &[
                    ("approved", "ship", Some("Zatwierdza i wypuszcza zmianę.")),
                    ("revise", "implement", None),
                ],
                override_targets,
            )
        }

        #[test]
        fn t_lists_the_exits_with_their_descriptions_and_a_choice_sends_the_transition() {
            let mut app = loaded(offered(None));
            assert_eq!(ch(&mut app, 't'), Request::None);
            let Some(Overlay::Transitions(_)) = app.overlay() else {
                panic!("the list of exits");
            };
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(chooser.items().len(), 2, "no override: only the exits");
            assert!(chooser.items()[0].label.contains("approved"));
            assert!(chooser.items()[0].label.contains("ship"));
            assert!(chooser.items()[0].description.contains("Zatwierdza i wypuszcza"));
            assert!(
                chooser.items()[1].description.contains("nie opisuje"),
                "an exit without a description says so"
            );
            let text = screen(&mut app, 120, 40);
            assert!(text.contains("approved → ship"), "{text}");
            assert!(text.contains("Zatwierdza i wypuszcza"), "{text}");

            ch(&mut app, '2');
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Transition {
                    target: "implement".to_string(),
                    forced: false,
                    reason: None,
                })
            );
            assert!(app.overlay().is_none(), "an exit is sent at once");
        }

        #[test]
        fn esc_leaves_the_list_of_exits() {
            let mut app = loaded(offered(None));
            ch(&mut app, 't');
            press(&mut app, KeyCode::Esc);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_override_is_only_there_when_the_state_allows_it() {
            let mut app = loaded(offered(None));
            ch(&mut app, 't');
            assert!(app
                .overlay()
                .unwrap()
                .chooser()
                .unwrap()
                .items()
                .iter()
                .all(|item| !item.label.contains("override")));

            let mut app = loaded(offered(Some(&["plan", "ship"])));
            ch(&mut app, 't');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(chooser.items().len(), 3);
            assert!(chooser.items()[2].label.contains("override"));
            assert!(chooser.items()[2].label.starts_with('!'));
        }

        #[test]
        fn the_override_needs_a_reason_and_asks_before_it_sends() {
            let mut app = loaded(offered(Some(&["plan", "ship"])));
            ch(&mut app, 't');
            ch(&mut app, '3');
            // Enter goes to the fields; Enter again with no reason complains.
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(app.overlay().is_some(), "no reason, nothing to confirm");
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(
                chooser.fields_of(2)[1].error().is_some(),
                "the reason is what is missing"
            );

            // Down to the reason, type it, send.
            press(&mut app, KeyCode::Down);
            typed(&mut app, "CI nie ruszy bez tego");
            assert_eq!(app.key_context(), Context::Form, "typing: q and ? are text");
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            let window = confirmation(&app);
            assert!(window.heading().contains("Override"));
            let text = window.lines().join(" ");
            assert!(text.contains("„review”") && text.contains("„plan”"), "{text}");
            assert!(text.contains("CI nie ruszy bez tego"), "{text}");
            assert!(text.contains("audytu"), "{text}");

            let request = act(ch(&mut app, 'y'));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Transition {
                    target: "plan".to_string(),
                    forced: true,
                    reason: Some("CI nie ruszy bez tego".to_string()),
                })
            );
        }

        #[test]
        fn n_in_the_override_window_sends_nothing() {
            let mut app = loaded(offered(Some(&["plan"])));
            ch(&mut app, 't');
            ch(&mut app, '3');
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Down);
            typed(&mut app, "powód");
            press(&mut app, KeyCode::Enter);
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn t_where_the_state_has_no_exits_says_why() {
            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 't'), Request::None);
            assert_eq!(
                note(&app),
                "przejście: serwer nie podał wyjść bieżącego stanu"
            );
            assert!(app.overlay().is_none());
        }

        #[test]
        fn server_text_in_the_exits_is_made_safe() {
            let task = with_transitions(
                running("a", 1),
                "review",
                &[("ok", "ship", Some("Dobrze\u{1b}[31m czerwone"))],
                None,
            );
            let mut app = loaded(task);
            ch(&mut app, 't');
            let text = screen(&mut app, 120, 40);
            assert!(!text.contains('\u{1b}'));
            assert!(text.contains("Dobrze"));
        }
    }

    // -- links -----------------------------------------------------------------

    mod links {
        use super::*;

        #[test]
        fn o_without_a_link_value_says_why() {
            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 'o'), Request::None);
            assert_eq!(note(&app), "link: workflow nie deklaruje linków");
            let unresolved = displayed(running("a", 1), "NAR-12", &[("Jira", "—", None)]);
            let mut app = loaded(unresolved);
            assert_eq!(ch(&mut app, 'o'), Request::None);
            assert_eq!(note(&app), "link: linki zadania nie mają jeszcze wartości");
            assert!(app.overlay().is_none());
        }

        #[test]
        fn o_lists_the_links_with_a_value_and_enter_opens_one() {
            let task = displayed(
                running("a", 1),
                "NAR-12",
                &[
                    ("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12")),
                    ("PR", "—", None),
                    ("Docs", "doc", Some("https://docs.example/x")),
                ],
            );
            let mut app = loaded(task);
            ch(&mut app, 'o');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(chooser.items().len(), 2, "the unresolved one is not offered");
            assert!(chooser.items()[0].label.contains("Jira"));
            ch(&mut app, '2');
            let request = press(&mut app, KeyCode::Enter);
            let Request::OpenLink(link) = request else {
                panic!("a link was expected");
            };
            assert_eq!(link.url, "https://docs.example/x");
            assert_eq!(link.label, "Docs");
            assert!(app.overlay().is_none());
        }

        #[tokio::test]
        async fn a_link_that_is_not_http_is_never_handed_to_a_browser() {
            let mut app = loaded(running("a", 1));
            app.perform(Request::OpenLink(
                narsor::tui::task_app::actions::LinkRequest {
                    label: "Zły".to_string(),
                    url: "file:///etc/passwd".to_string(),
                },
            ))
            .await;
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            assert!(note(&app).contains("niebezpieczny"), "{}", note(&app));
        }
    }

    // -- without a connection -------------------------------------------------

    mod without_a_connection {
        use super::*;

        fn disconnected(task: TaskSummary) -> TaskApp {
            let mut app = loaded(task);
            app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));
            assert!(!app.connection().is_live());
            app
        }

        #[test]
        fn every_mutation_is_blocked_with_the_reason() {
            for (key, task) in [
                ('p', running("a", 1)),
                ('x', running("a", 1)),
                ('a', finished("a", 1)),
                ('s', lifecycle("interrupted")),
                (
                    't',
                    with_transitions(running("a", 1), "review", &[("ok", "ship", None)], None),
                ),
            ] {
                let mut app = disconnected(task);
                assert_eq!(ch(&mut app, key), Request::None, "{key}");
                assert!(note(&app).contains("zablokowana"), "{key}: {}", note(&app));
                assert!(app.overlay().is_none(), "{key}: nothing opens");
            }
        }

        #[test]
        fn the_menu_greys_out_what_changes_state_and_leaves_the_links() {
            let task = displayed(
                running("a", 1),
                "NAR-12",
                &[("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12"))],
            );
            let mut app = disconnected(task);
            ch(&mut app, '.');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let pause = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("wstrzymaj"))
                .unwrap();
            assert!(
                pause.unavailable.as_deref().is_some_and(|reason| reason.contains("zablokowana")),
                "{:?}",
                pause.unavailable
            );
            let link = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("otwórz link"))
                .unwrap();
            assert_eq!(link.unavailable, None, "opening a link changes nothing");

            // And it comes back with the connection.
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::Created,
            ));
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(chooser
                .items()
                .iter()
                .find(|item| item.label.contains("wstrzymaj"))
                .unwrap()
                .unavailable
                .is_none());
        }

        #[test]
        fn a_task_that_cannot_be_acted_on_says_so_before_the_connection_does() {
            let mut app = disconnected(queued("a", 1));
            assert_eq!(ch(&mut app, 'p'), Request::None);
            assert_eq!(note(&app), "pauza: zadanie jest w kolejce");
        }
    }

    // -- the verdict of the runner ---------------------------------------------

    mod verdicts {
        use super::*;

        fn queued_command(id: &str, command: OperatorCommand) -> OperatorCommandView {
            OperatorCommandView {
                command_id: id.to_string(),
                command,
                status: OperatorCommandStatus::Queued,
                submitted_at: super::super::task_app_support::at(),
                updated_at: super::super::task_app_support::at(),
                detail: None,
            }
        }

        fn command(app: &TaskApp) -> OperatorCommandView {
            app.list().tasks()[0].operator_commands[0].clone()
        }

        #[test]
        fn queued_and_resolved_events_patch_the_row_and_the_bar_tells_the_verdict() {
            let mut app = loaded(running("a", 1));
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandQueued {
                    command_id: "command-1".to_string(),
                    command: OperatorCommand::Interrupt,
                    requested_by: OwnerRef::new("local"),
                },
            ));
            assert_eq!(command(&app).status, OperatorCommandStatus::Queued);
            assert!(!app.refresh_wanted(), "queueing is patched, not re-read");

            app.on_stream(event(
                "e",
                "a",
                3,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Applied,
                    detail: None,
                },
            ));
            assert_eq!(command(&app).status, OperatorCommandStatus::Applied);
            assert_eq!(note(&app), "NAR-12: przerwanie kroku wykonane");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Success);
            assert_eq!(app.cursor(), Some("e"));
        }

        #[test]
        fn a_command_the_runner_rejects_is_reported_with_its_reason() {
            let mut app = loaded(running("a", 1));
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandQueued {
                    command_id: "command-1".to_string(),
                    command: OperatorCommand::Transition {
                        target: "ship".to_string(),
                        forced: false,
                        reason: None,
                    },
                    requested_by: OwnerRef::new("local"),
                },
            ));
            app.on_stream(event(
                "e",
                "a",
                3,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Rejected,
                    detail: Some("the execution moved on\u{1b}[31m already".to_string()),
                },
            ));
            let text = note(&app);
            assert!(text.contains("przejście do „ship” odrzucone przez runnera"), "{text}");
            assert!(text.contains("the execution moved on"), "{text}");
            assert!(!text.contains('\u{1b}'), "{text:?}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            let command = command(&app);
            assert_eq!(command.status, OperatorCommandStatus::Rejected);
        }

        #[test]
        fn a_rejection_without_a_reason_says_so() {
            let mut app = loaded(running("a", 1));
            app.list_mut().note_operator_command(
                "a",
                queued_command("command-1", OperatorCommand::Interrupt),
            );
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Rejected,
                    detail: None,
                },
            ));
            assert!(note(&app).contains("bez podania przyczyny"), "{}", note(&app));
        }

        #[test]
        fn a_verdict_is_told_once() {
            let mut app = loaded(running("a", 1));
            app.list_mut().note_operator_command(
                "a",
                queued_command("command-1", OperatorCommand::Interrupt),
            );
            let resolved = |cursor: &str, sequence| {
                event(
                    cursor,
                    "a",
                    sequence,
                    TaskLifecycle::Running,
                    TaskEventKind::OperatorCommandResolved {
                        command_id: "command-1".to_string(),
                        status: OperatorCommandStatus::Applied,
                        detail: None,
                    },
                )
            };
            app.on_stream(resolved("d", 2));
            app.on_stream(event(
                "e",
                "a",
                3,
                TaskLifecycle::Running,
                TaskEventKind::Created,
            ));
            let before = app.notifier().latest().unwrap().at;
            app.on_stream(resolved("f", 4));
            assert_eq!(
                app.notifier().latest().unwrap().at,
                before,
                "a settled command does not notify again"
            );
        }

        #[test]
        fn a_verdict_on_a_command_the_row_does_not_know_asks_for_a_read() {
            let mut app = loaded(running("a", 1));
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-9".to_string(),
                    status: OperatorCommandStatus::Applied,
                    detail: None,
                },
            ));
            assert!(app.refresh_wanted());
            assert!(note(&app).contains("polecenie operatora wykonane"), "{}", note(&app));
        }

        #[test]
        fn an_applied_transition_says_the_runner_goes_on() {
            let mut app = loaded(running("a", 1));
            app.list_mut().note_operator_command(
                "a",
                queued_command(
                    "command-1",
                    OperatorCommand::Transition {
                        target: "ship".to_string(),
                        forced: true,
                        reason: Some("bo".to_string()),
                    },
                ),
            );
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Applied,
                    detail: None,
                },
            ));
            let text = note(&app);
            assert!(text.contains("override do „ship” wykonane"), "{text}");
            assert!(text.contains("runner kontynuuje"), "{text}");
        }
    }

    // -- the reasons of early refusals ------------------------------------------

    mod refusals {
        use super::*;

        #[test]
        fn every_early_refusal_of_the_control_plane_has_words() {
            for (code, expected) in [
                ("override_reason_required", "override wymaga powodu"),
                ("override_not_allowed", "nie dopuszcza override"),
                ("illegal_transition", "nie jest dozwolone"),
                ("missing_target", "brak stanu docelowego"),
                ("stale_generation", "zmieniło się w międzyczasie"),
                ("idempotency_key_reused", "klucz został już użyty"),
            ] {
                let text = refusal_reason(&ClientError::Request {
                    code: code.to_string(),
                    message: "detail from the server".to_string(),
                });
                assert!(text.contains(expected), "{code}: {text}");
                assert!(text.contains("detail from the server"), "{code}: {text}");
                let refused = refusal_reason(&ClientError::Refused {
                    code: code.to_string(),
                    message: String::new(),
                });
                assert!(!refused.contains("()"), "{code}: {refused}");
            }
            assert_eq!(
                refusal_reason(&ClientError::Refused {
                    code: "operator_command_refused".to_string(),
                    message: "the task is paused; resume it first".to_string(),
                }),
                "the task is paused; resume it first"
            );
        }
    }

    // -- against a served control plane ------------------------------------------

    mod served {
        use super::*;

        async fn next_event(app: &mut TaskApp, feed: &mut EventFeed) {
            let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                .await
                .expect("an event arrives");
            app.on_stream(item);
        }

        /// The runner of `task` takes the commands it has been sent and
        /// settles each as `applied` (with `detail` when it refuses).
        async fn settle_all(runner: &RunnerClient, applied: bool, detail: Option<&str>) -> usize {
            let polled = runner.poll(None, 0).await.expect("poll");
            for command in &polled.commands {
                runner
                    .ack_command(&command.command_id, applied, detail.map(str::to_string))
                    .await
                    .expect("ack");
            }
            polled.commands.len()
        }

        #[tokio::test]
        async fn x_then_y_ends_with_the_runner_applying_it_and_the_row_updated() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            assert_eq!(ch(&mut app, 'x'), Request::None);
            let request = ch(&mut app, 'y');
            assert!(matches!(request, Request::Act(_)), "{request:?}");
            app.perform(request).await;
            assert_eq!(
                note(&app),
                "NAR-12: przerwanie kroku wysłane do runnera",
                "the answer says: queued"
            );
            assert_eq!(
                app.list().tasks()[0].operator_commands[0].status,
                OperatorCommandStatus::Queued
            );

            assert_eq!(settle_all(&running.runner, true, None).await, 1);
            for _ in 0..8 {
                if app.list().tasks()[0].operator_commands[0].status
                    == OperatorCommandStatus::Applied
                {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            let command = &app.list().tasks()[0].operator_commands[0];
            assert_eq!(command.status, OperatorCommandStatus::Applied);
            assert_eq!(command.command, OperatorCommand::Interrupt);
            assert_eq!(note(&app), "NAR-12: przerwanie kroku wykonane");
            assert_eq!(app.list().tasks()[0].operator_commands.len(), 1, "no duplicate");
        }

        #[tokio::test]
        async fn a_command_the_runner_rejects_ends_on_the_bar_with_its_reason() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            ch(&mut app, 'x');
            let request = ch(&mut app, 'y');
            app.perform(request).await;
            assert_eq!(
                settle_all(&running.runner, false, Some("no step is in flight")).await,
                1
            );
            for _ in 0..8 {
                if app.list().tasks()[0].operator_commands[0].status
                    == OperatorCommandStatus::Rejected
                {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            let text = note(&app);
            assert!(text.contains("przerwanie kroku odrzucone przez runnera"), "{text}");
            assert!(text.contains("no step is in flight"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
        }

        #[tokio::test]
        async fn an_override_without_a_reason_is_refused_early_and_the_bar_says_why() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let task = app.list().tasks()[0].clone();
            let request = ActionRequest {
                task_id: "NAR-12".to_string(),
                title: "NAR-12".to_string(),
                generation: task.generation,
                key: "override-1".to_string(),
                verb: "override".to_string(),
                summary: "override wysłany".to_string(),
                send: ActionSend::Operator(OperatorCommand::Transition {
                    target: "plan".to_string(),
                    forced: true,
                    reason: None,
                }),
            };
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("override odrzucone"), "{text}");
            assert!(text.contains("override wymaga powodu"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
        }

        #[tokio::test]
        async fn a_command_to_a_task_that_moved_on_is_refused_with_the_generation() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            ch(&mut app, 'x');
            let mut request = act(ch(&mut app, 'y'));
            request.generation += 7;
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("przerwanie odrzucone"), "{text}");
            assert!(text.contains("zmieniło się w międzyczasie"), "{text}");
        }

        #[tokio::test]
        async fn archiving_a_finished_task_removes_its_row_and_selects_the_one_below() {
            let harness = RemoteHarness::start().await;
            let mut done = snapshot(1, None);
            done.execution_status = ExecutionStatus::Completed;
            let _running = harness.running("t1", "k8s", done).await;
            harness.create("t2", Some("k8s")).await;
            harness.create("t3", Some("k8s")).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);
            let ids = |app: &TaskApp| -> Vec<String> {
                app.list().tasks().iter().map(|t| t.task_id.to_string()).collect()
            };
            assert_eq!(ids(&app), ["t1", "t2", "t3"]);
            assert_eq!(app.list().selected_id(), Some("t1"));

            assert_eq!(ch(&mut app, 'a'), Request::None);
            assert!(confirmation(&app).heading().contains("Archiwizuj"));
            assert_eq!(ids(&app).len(), 3, "nothing was sent yet");
            let request = ch(&mut app, 'y');
            app.perform(request).await;
            for _ in 0..8 {
                if ids(&app).len() == 2 {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            assert_eq!(ids(&app), ["t2", "t3"], "the row left the list");
            assert_eq!(app.list().selected_id(), Some("t2"), "the selection went below");
            assert!(note(&app).contains("zniknęło z listy"), "{}", note(&app));
        }

        #[tokio::test]
        async fn pause_goes_through_the_owner_command_and_the_stream_shows_it() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);
            let request = ch(&mut app, 'p');
            app.perform(request).await;
            assert_eq!(note(&app), "NAR-12: wstrzymanie zlecone");
            for _ in 0..8 {
                if app.list().tasks()[0].lifecycle != TaskLifecycle::Running {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            assert_eq!(app.list().tasks()[0].lifecycle, TaskLifecycle::PauseRequested);
        }

        #[tokio::test]
        async fn the_exits_of_a_running_task_come_from_its_workflow() {
            let harness = RemoteHarness::start().await;
            let mut first = snapshot(1, None);
            first.execution_cursor.current_state = Some("work".to_string());
            let _running = harness.running("NAR-12", "local", first).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let offer = app.list().tasks()[0]
                .manual_transitions
                .clone()
                .expect("the sealed workflow names the exits of `work`");
            assert_eq!(offer.from_state, "work");
            assert_eq!(offer.exits.len(), 1);
            assert_eq!(offer.exits[0].label, "success");
            assert_eq!(offer.exits[0].target, "done");
            assert!(!offer.override_allowed);
            assert!(offer.override_targets.is_empty());

            // `t` offers it, and the choice goes through the control plane
            // that checks it against the same workflow.
            ch(&mut app, 't');
            let request = press(&mut app, KeyCode::Enter);
            let request = act(request);
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Transition {
                    target: "done".to_string(),
                    forced: false,
                    reason: None,
                })
            );
            app.perform(Request::Act(Box::new(request))).await;
            assert_eq!(note(&app), "NAR-12: przejście do „done” wysłane do runnera");

            // The state the runner reported earlier was not one of the
            // workflow's: the server offers nothing it cannot back.
            let mut other = TaskApp::new(harness.owner.clone(), "local", None);
            harness.running("NAR-13", "backend-a", snapshot(1, None)).await;
            other.refresh().await;
            let task = other
                .list()
                .tasks()
                .iter()
                .find(|task| task.task_id.as_str() == "NAR-13")
                .unwrap();
            assert!(task.manual_transitions.is_none());
            assert_eq!(
                unavailable(task, TaskAction::Transition),
                "serwer nie podał wyjść bieżącego stanu"
            );
        }

        #[tokio::test]
        async fn an_illegal_transition_is_refused_early_with_the_reason() {
            let harness = RemoteHarness::start().await;
            let mut first = snapshot(1, None);
            first.execution_cursor.current_state = Some("work".to_string());
            let _running = harness.running("NAR-12", "local", first).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let request = ActionRequest {
                task_id: "NAR-12".to_string(),
                title: "NAR-12".to_string(),
                generation: app.list().tasks()[0].generation,
                key: "t-1".to_string(),
                verb: "przejście".to_string(),
                summary: "x".to_string(),
                send: ActionSend::Operator(OperatorCommand::Transition {
                    target: "work".to_string(),
                    forced: false,
                    reason: None,
                }),
            };
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("przejście odrzucone"), "{text}");
            assert!(text.contains("nie jest dozwolone"), "{text}");
        }
    }
}

/// U12: the *Dyrygent* tab — the chat, the decisions with their undo, the
/// harness, and what it says when there is no conductor.
mod task_app_conductor {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::{json, Value};

    use narsor::api::{ApiClient, ClientError};
    use narsor::conductor::ConductorView;
    use narsor::controller_protocol::{
        ConductorReadResponse, ConversationUnavailableReason, ExecutionEventsSource, RunnerClient,
        TaskSummary,
    };
    use narsor::domain::ExecutionStatus;
    use narsor::orchestration::{
        AttentionKind, OperatorCommand, OwnerRef, TaskEventKind, TaskLifecycle,
    };
    use narsor::tui::chat::UndoState;
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::actions::{ActionRequest, ActionSend, Overlay};
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::waiting::Submit;
    use narsor::tui::task_app::{DetailTab, Focus, Request, TaskApp};
    use narsor::tui::transcript::TranscriptPresentation;

    use super::remote_support::{snapshot, RemoteHarness};
    use super::task_app_support::{
        chat_entry, conductor_view, event, listing, task, titled, waiting_on, with_conductor,
        with_controller, with_runner, without_conductor,
    };

    fn app() -> TaskApp {
        TaskApp::new(ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn act(request: Request) -> ActionRequest {
        match request {
            Request::Act(request) => *request,
            other => panic!("an action was expected, got {other:?}"),
        }
    }

    fn sent_command(request: Request) -> OperatorCommand {
        match act(request).send {
            ActionSend::Operator(command) => command,
            other => panic!("an operator command was expected, got {other:?}"),
        }
    }

    fn running(id: &str, rank: u32) -> TaskSummary {
        with_conductor(with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Running,
            "review",
        ))
    }

    /// An app over the running task `a`, on the details of the *Dyrygent* tab.
    fn tab_app() -> TaskApp {
        let mut app = app();
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "c"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        app.detail_mut().set_tab(DetailTab::Conductor);
        app
    }

    fn revision(app: &TaskApp) -> u64 {
        app.list().selected().expect("a selection").revision
    }

    fn read(app: &mut TaskApp, view: ConductorView) {
        read_response(
            app,
            Ok(ConductorReadResponse::Available {
                conductor: Box::new(view),
                source: ExecutionEventsSource::Runner,
            }),
        );
    }

    fn read_response(app: &mut TaskApp, result: Result<ConductorReadResponse, ClientError>) {
        let revision = revision(app);
        app.on_detail_read("a", revision, DetailRead::Conductor(result));
    }

    fn conductor(app: &TaskApp) -> &narsor::tui::task_app::tabs::conductor::Conductor {
        app.detail().view("a").expect("details").conductor()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        use narsor::tui::style::Styles;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(frame, app, &Styles::new(false), Instant::now())
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn decision_entry(id: &str, sequence: u64, decision_id: &str) -> Value {
        chat_entry(
            id,
            sequence,
            "conductor",
            "decision",
            "Wybrano wyjście approved",
            Some(json!({
                "kind": "choose_output",
                "decision_id": decision_id,
                "output_label": "approved",
                "confidence": "high",
                "rationale": "Testy przeszły."
            })),
        )
    }

    fn decision_view(decision_id: &str, undone: bool, undoable: bool) -> Value {
        json!({
            "decision_id": decision_id,
            "kind": "choose_output",
            "text": "Wybrano wyjście approved",
            "at": "2026-10-09T12:00:00Z",
            "confidence": "high",
            "rationale": "Testy przeszły.",
            "undone": undone,
            "undoable": undoable
        })
    }

    // -- the chat ----------------------------------------------------------

    mod chat {
        use super::*;

        #[test]
        fn a_message_goes_out_as_a_conductor_chat_exactly_as_typed() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            assert!(conductor(&app).is_typing());
            assert_eq!(app.key_context(), Context::Form, "typing inherits no global key");

            typed(&mut app, "  Czy revise? q");
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
            typed(&mut app, "Dziękuję  ");
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::ConductorChat {
                    text: "  Czy revise? q\nDziękuję  ".to_string()
                }),
                "what the operator typed is sent without a change"
            );
            assert_eq!(request.verb, "wiadomość do dyrygenta");
            assert!(app.detail().has_drafts(), "the text stays until the control plane took it");

            let again = act(press(&mut app, KeyCode::Enter));
            assert_eq!(again.key, request.key, "a retry of the same message reuses its key");
        }

        #[test]
        fn the_operators_entry_and_then_the_conductors_answer_have_their_roles() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry("m1", 1, "operator", "message", "Czy revise?", None)],
                    json!({ "unanswered_chat": ["m1"] }),
                ),
            );
            assert!(conductor(&app).awaits_answer());
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry("m2", 2, "conductor", "message", "Raczej revise — brak planu.", None)],
                    json!({}),
                ),
            );
            let presentations: Vec<_> = conductor(&app)
                .transcript()
                .entries()
                .iter()
                .map(|entry| entry.presentation)
                .collect();
            assert_eq!(
                presentations,
                [TranscriptPresentation::Operator, TranscriptPresentation::Conductor]
            );
            assert!(!conductor(&app).awaits_answer());
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("USER") && text.contains("Czy revise?"), "{text}");
            assert!(text.contains("KONDUKTOR") && text.contains("Raczej revise"), "{text}");
        }

        #[test]
        fn what_was_typed_survives_a_refresh_and_a_visit_to_another_task() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            typed(&mut app, "pół zdania");
            press(&mut app, KeyCode::Esc);
            assert!(!conductor(&app).is_typing(), "Esc leaves the box");
            assert_eq!(conductor(&app).draft(), "pół zdania");
            assert_eq!(app.detail().draft("a"), Some("pół zdania"));

            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            assert_eq!(conductor(&app).draft(), "pół zdania", "a refresh keeps the draft");
            assert!(app.detail().has_drafts());
            assert!(screen(&mut app, 120, 30).contains("szkic: pół zdania"));

            ch(&mut app, 'c');
            typed(&mut app, " i reszta");
            assert_eq!(conductor(&app).draft(), "pół zdania i reszta");
        }

        #[test]
        fn q_and_the_question_mark_are_text_while_typing_and_quitting_asks_about_the_draft() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            typed(&mut app, "q?");
            assert!(!app.should_quit());
            assert!(!app.is_help_open());
            assert_eq!(conductor(&app).draft(), "q?");

            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'q');
            assert!(!app.should_quit(), "an unsent message is asked about first");
            assert!(note(&app).contains("niewysłany szkic"), "{}", note(&app));
        }

        #[test]
        fn a_blank_message_is_not_sent() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            typed(&mut app, "   ");
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(conductor(&app).is_typing());
        }

        #[test]
        fn nothing_from_the_conductor_reaches_the_screen_with_a_terminal_control() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry(
                        "m1",
                        1,
                        "conductor",
                        "message",
                        "ok\u{1b}[31m czerwony\u{202e}tekst\u{200b}",
                        None,
                    )],
                    json!({ "harness": "cla\u{1b}]8;;x\u{7}ude" }),
                ),
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("czerwony"), "{text}");
            assert!(
                !text.chars().any(|c| c.is_control() && c != '\n' || "\u{202e}\u{200b}".contains(c)),
                "{text:?}"
            );
        }
    }

    // -- decisions and their undo ----------------------------------------------

    mod decisions {
        use super::*;

        fn with_decision() -> TaskApp {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![decision_entry("e1", 1, "d1")],
                    json!({ "decisions": [decision_view("d1", false, true)] }),
                ),
            );
            app
        }

        #[test]
        fn undoing_a_decision_asks_with_its_confidence_and_sends_after_y() {
            let mut app = with_decision();
            assert_eq!(ch(&mut app, 'u'), Request::None);
            let window = app.overlay().and_then(Overlay::confirmation).expect("the window");
            assert!(window.heading().contains("Cofnij decyzję"), "{}", window.heading());
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("Pewność dyrygenta: high"), "{text}");
            assert!(text.contains("Uzasadnienie: Testy przeszły."), "{text}");
            assert!(text.contains("skutki uboczne"), "{text}");

            ch(&mut app, 'n');
            assert!(app.overlay().is_none(), "n takes it back");
            ch(&mut app, 'u');
            let command = sent_command(ch(&mut app, 'y'));
            assert_eq!(
                command,
                OperatorCommand::UndoDecision {
                    decision_id: "d1".to_string()
                }
            );
        }

        #[test]
        fn the_undo_comes_back_as_an_entry_and_the_decision_is_marked_undone() {
            let mut app = with_decision();
            assert!(matches!(
                conductor(&app).items()[0].undo,
                Some(UndoState::Undoable { .. })
            ));
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry(
                        "e2",
                        2,
                        "conductor",
                        "decision",
                        "Cofnięto decyzję d1",
                        Some(json!({ "kind": "decision_undone", "decision_id": "d1" })),
                    )],
                    json!({ "decisions": [decision_view("d1", true, false)] }),
                ),
            );
            let items = conductor(&app).items();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].undo, Some(UndoState::Undone));
            let entries = conductor(&app).transcript().entries();
            assert!(entries[0].rendered_lines.iter().any(|line| line.contains("decyzja cofnięta")));
            assert!(entries[1].rendered_lines[0].contains("Cofnięto decyzję"));

            // The selection is on the entry of the undo now: nothing to undo there.
            assert_eq!(ch(&mut app, 'u'), Request::None);
            assert!(app.overlay().is_none());
            assert!(note(&app).contains("zaznacz"), "{}", note(&app));
        }

        #[test]
        fn a_message_is_not_a_decision_and_says_so() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(vec![chat_entry("m1", 1, "conductor", "message", "Cześć", None)], json!({})),
            );
            ch(&mut app, 'u');
            assert!(app.overlay().is_none());
            assert!(note(&app).contains("którą można cofnąć"), "{}", note(&app));
        }

        #[test]
        fn a_decision_the_server_says_cannot_be_undone_is_not_offered() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![decision_entry("e1", 1, "d1")],
                    json!({ "decisions": [decision_view("d1", false, false)] }),
                ),
            );
            assert_eq!(conductor(&app).items()[0].undo, None);
            ch(&mut app, 'u');
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_refused_undo_says_why_on_the_bar() {
            let mut app = with_decision();
            for (cursor, kind) in [
                (
                    "d",
                    TaskEventKind::OperatorCommandQueued {
                        command_id: "command-1".to_string(),
                        command: OperatorCommand::UndoDecision {
                            decision_id: "d1".to_string(),
                        },
                        requested_by: OwnerRef::new("local"),
                    },
                ),
                (
                    "e",
                    TaskEventKind::OperatorCommandResolved {
                        command_id: "command-1".to_string(),
                        status: narsor::orchestration::OperatorCommandStatus::Rejected,
                        detail: Some("a later step already ran".to_string()),
                    },
                ),
            ] {
                let sequence = if cursor == "d" { 2 } else { 3 };
                app.on_stream(event(cursor, "a", sequence, TaskLifecycle::Running, kind));
            }
            let text = note(&app);
            assert!(text.contains("cofnięcie decyzji dyrygenta odrzucone przez runnera"), "{text}");
            assert!(text.contains("a later step already ran"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            let revision = revision(&app);
            assert!(conductor(&app).needs_read(revision, Instant::now()), "the tab is due for a read");
        }
    }

    // -- proposals live in the panel --------------------------------------------

    mod proposals {
        use super::*;

        fn proposal_view() -> ConductorView {
            conductor_view(
                vec![chat_entry("p1", 2, "conductor", "proposal", "Uruchom krok od nowa", None)],
                json!({
                    "proposals": [{
                        "proposal_id": "p1",
                        "text": "Uruchom krok od nowa",
                        "at": "2026-10-09T12:00:00Z",
                        "record": {
                            "episode": "ep-x",
                            "state_id": "test",
                            "proposal": { "kind": "retry", "summary": "Uruchom krok od nowa", "text": "z nowszym toolchainem" },
                            "confidence": "high",
                            "rationale": "Błąd wygląda na przejściowy."
                        },
                        "in_reply_to": "m1",
                        "state": "pending"
                    }]
                }),
            )
        }

        fn waiting_app() -> TaskApp {
            let mut app = app();
            app.load(listing(
                vec![titled(
                    waiting_on(running("a", 1), AttentionKind::ConductorProposal, "p1", Some("test")),
                    "NAR-12",
                )],
                "c",
            ));
            ch(&mut app, ' ');
            press(&mut app, KeyCode::Enter);
            assert!(app.panel_focused());
            app.detail_mut().set_tab(DetailTab::Conductor);
            app
        }

        #[test]
        fn a_proposal_from_the_chat_is_answered_in_the_panel_not_in_the_tab() {
            let mut app = waiting_app();
            read(&mut app, proposal_view());
            let text = screen(&mut app, 120, 40);
            assert!(text.contains("Propozycja dyrygenta"), "the panel: {text}");
            assert!(text.contains("dyrygent proponuje: Uruchom krok od nowa"), "{text}");
            assert!(
                text.contains("(propozycja: czeka na Ciebie w panelu na dole)"),
                "the tab says where it is answered: {text}"
            );
            let answer = match press(&mut app, KeyCode::Enter) {
                Request::Answer(answer) => *answer,
                other => panic!("an answer was expected, got {other:?}"),
            };
            assert_eq!(
                answer.submit,
                Submit::Operator(OperatorCommand::ConfirmProposal {
                    proposal_id: "p1".to_string()
                })
            );
            ch(&mut app, '2');
            let reject = match press(&mut app, KeyCode::Enter) {
                Request::Answer(answer) => *answer,
                other => panic!("an answer was expected, got {other:?}"),
            };
            assert_eq!(
                reject.submit,
                Submit::Operator(OperatorCommand::RejectProposal {
                    proposal_id: "p1".to_string()
                })
            );
        }

        #[test]
        fn one_read_serves_the_tab_and_the_panel() {
            let mut app = waiting_app();
            let revision = revision(&app);
            assert!(conductor(&app).needs_read(revision, Instant::now()));
            read(&mut app, proposal_view());
            assert!(!conductor(&app).needs_read(revision, Instant::now()));
            let panel = app.detail().view("a").unwrap().waiting();
            assert!(!panel.needs_conductor(revision), "the panel did not need a second read");
        }

        #[test]
        fn a_settled_proposal_says_so_in_the_tab() {
            let mut app = tab_app();
            let mut view = proposal_view();
            view.proposals[0].state = narsor::conductor::ProposalState::Executed;
            read(&mut app, view);
            assert!(screen(&mut app, 120, 30).contains("(propozycja: wykonana)"));
        }
    }

    // -- no conductor, nothing to read -------------------------------------------

    mod unavailable {
        use super::*;

        fn app_of(task: TaskSummary) -> TaskApp {
            let mut app = app();
            app.load(listing(vec![titled(task, "NAR-12")], "c"));
            press(&mut app, KeyCode::Enter);
            app.detail_mut().set_tab(DetailTab::Conductor);
            app
        }

        #[test]
        fn a_task_whose_runner_has_no_conductor_shows_why_and_has_no_box() {
            let mut app = app_of(without_conductor(running("a", 1), "brak harnessu dyrygenta"));
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("dyrygent niedostępny: brak harnessu dyrygenta"), "{text}");
            assert!(!text.contains("c napisz"), "no box, no hint for it: {text}");
            ch(&mut app, 'c');
            assert!(!conductor(&app).is_typing());
            assert!(note(&app).contains("brak harnessu dyrygenta"), "{}", note(&app));
            ch(&mut app, 'h');
            assert!(app.overlay().is_none());
            assert!(note(&app).contains("brak harnessu dyrygenta"), "{}", note(&app));
        }

        #[test]
        fn a_view_that_says_there_is_none_wins_and_its_history_stays_readable() {
            let mut app = tab_app();
            let mut view = conductor_view(
                vec![chat_entry("m1", 1, "conductor", "message", "Stara odpowiedź", None)],
                json!({}),
            );
            view.available = false;
            view.unavailable_reason = Some("the runner is not running".to_string());
            read(&mut app, view);
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("dyrygent niedostępny: the runner is not running"), "{text}");
            assert!(text.contains("Stara odpowiedź"), "{text}");
            ch(&mut app, 'c');
            assert!(!conductor(&app).is_typing());
        }

        #[test]
        fn a_remote_task_without_a_runner_says_so_with_the_servers_words() {
            let mut app = tab_app();
            read_response(
                &mut app,
                Ok(ConductorReadResponse::Unavailable {
                    reason: ConversationUnavailableReason::NotStarted,
                    message: "zadanie nie ma żywego runnera".to_string(),
                }),
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("zadanie nie ma żywego runnera"), "{text}");
            ch(&mut app, 'c');
            assert!(!conductor(&app).is_typing());
        }

        #[test]
        fn a_503_with_a_reason_is_a_message_in_the_tab_and_not_a_lost_connection() {
            let mut app = tab_app();
            read_response(
                &mut app,
                Err(ClientError::Degraded {
                    code: "runner_timeout".to_string(),
                    message: "runner nie odpowiedział na czas".to_string(),
                }),
            );
            let text = screen(&mut app, 120, 30);
            assert!(
                text.contains("nie udało się wczytać dyrygenta (runner nie odpowiedział na czas)"),
                "{text}"
            );
            assert!(app.connection().is_live(), "the relay missing a deadline says nothing against the connection");
            assert_eq!(conductor(&app).failure(), Some("runner nie odpowiedział na czas"));
        }

        #[test]
        fn a_failed_refresh_keeps_what_was_read() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(vec![chat_entry("m1", 1, "conductor", "message", "Zostaje", None)], json!({})),
            );
            read_response(
                &mut app,
                Err(ClientError::Degraded {
                    code: "runner_timeout".to_string(),
                    message: "runner nie odpowiedział".to_string(),
                }),
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("Zostaje"), "{text}");
            assert!(text.contains("nie udało się odświeżyć (runner nie odpowiedział)"), "{text}");
            ch(&mut app, 'c');
            assert!(conductor(&app).is_typing(), "the conductor is still there to write to");
        }

        #[test]
        fn an_archived_task_says_its_storage_went_with_the_archiving() {
            let mut app = app();
            app.load(listing(vec![titled(task("a", "local", "archived", 1), "NAR-12")], "c"));
            press(&mut app, KeyCode::Enter);
            app.detail_mut().set_tab(DetailTab::Conductor);
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("storage usunięty przy archiwizacji"), "{text}");
        }

        #[test]
        fn before_the_first_read_the_tab_says_it_is_reading() {
            let mut app = tab_app();
            assert!(screen(&mut app, 120, 30).contains("wczytuję dyrygenta"));
        }

        #[test]
        fn the_tab_keys_say_they_belong_to_the_tab_elsewhere() {
            let mut app = tab_app();
            app.detail_mut().set_tab(DetailTab::Progress);
            for key in ['c', 'u', 'h', 'M'] {
                ch(&mut app, key);
                assert!(note(&app).contains("zakładce Dyrygent"), "{key}: {}", note(&app));
                assert!(app.overlay().is_none());
                assert!(app
                    .detail()
                    .view("a")
                    .is_none_or(|view| !view.conductor().is_typing()));
            }
        }
    }

    // -- the harness ----------------------------------------------------------

    mod harness {
        use super::*;

        fn labels(app: &TaskApp) -> Vec<String> {
            app.overlay()
                .and_then(Overlay::chooser)
                .expect("the list of harnesses")
                .items()
                .iter()
                .map(|item| item.label.clone())
                .collect()
        }

        #[test]
        fn h_lists_the_harnesses_the_current_one_greyed_and_the_choice_is_sent() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'h');
            assert_eq!(labels(&app), ["claude-code", "codex", "opencode"]);
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("Harness dyrygenta"), "{text}");
            let chooser = app.overlay().and_then(Overlay::chooser).unwrap();
            assert!(chooser.items()[0].unavailable.is_some(), "claude-code is the current one");
            assert_eq!(chooser.selected(), 1, "the list opens on another harness");

            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::SetConductorHarness {
                    harness: "codex".to_string()
                })
            );
            assert!(request.summary.contains("codex"));
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_current_harness_cannot_be_chosen_again() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'h');
            ch(&mut app, 'k');
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(note(&app).contains("już działa na tym harnessie"), "{}", note(&app));
            assert!(app.overlay().is_some(), "the list stays to choose again");
            press(&mut app, KeyCode::Esc);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_runner_that_has_no_conductor_refuses_and_the_bar_gives_its_reason() {
            let mut app = tab_app();
            for (cursor, sequence, kind) in [
                (
                    "d",
                    2,
                    TaskEventKind::OperatorCommandQueued {
                        command_id: "command-1".to_string(),
                        command: OperatorCommand::SetConductorHarness {
                            harness: "codex".to_string(),
                        },
                        requested_by: OwnerRef::new("local"),
                    },
                ),
                (
                    "e",
                    3,
                    TaskEventKind::OperatorCommandResolved {
                        command_id: "command-1".to_string(),
                        status: narsor::orchestration::OperatorCommandStatus::Rejected,
                        detail: Some("no conductor runs for this task".to_string()),
                    },
                ),
            ] {
                app.on_stream(event(cursor, "a", sequence, TaskLifecycle::Running, kind));
            }
            let text = note(&app);
            assert!(text.contains("zmiana harnessu dyrygenta odrzucone przez runnera"), "{text}");
            assert!(text.contains("no conductor runs for this task"), "{text}");
        }
    }

    // -- paging ----------------------------------------------------------------

    mod paging {
        use super::*;

        fn page(from: u64, to: u64, more: bool) -> ConductorView {
            conductor_view(
                (from..=to)
                    .map(|sequence| {
                        chat_entry(&format!("m{sequence}"), sequence, "conductor", "message", &format!("wpis {sequence}"), None)
                    })
                    .collect(),
                json!({ "chat_has_more": more }),
            )
        }

        #[test]
        fn a_history_that_goes_on_is_continued_after_the_last_entry_with_m() {
            let mut app = tab_app();
            read(&mut app, page(1, 2, true));
            assert!(conductor(&app).has_more());
            assert_eq!(conductor(&app).last_sequence(), 2);
            let revision = revision(&app);
            assert!(!conductor(&app).needs_read(revision, Instant::now()));
            assert!(screen(&mut app, 120, 30).contains("M dalsze wpisy"));

            ch(&mut app, 'M');
            assert!(conductor(&app).wants_more());
            assert!(conductor(&app).needs_read(revision, Instant::now()), "the same revision is read on");

            // The next page overlaps the last entry: it is not shown twice.
            read(&mut app, page(2, 3, false));
            assert_eq!(conductor(&app).items().len(), 3);
            assert_eq!(conductor(&app).last_sequence(), 3);
            assert!(!conductor(&app).has_more());
            assert!(!conductor(&app).wants_more());
            assert!(!screen(&mut app, 120, 30).contains("M dalsze wpisy"));

            ch(&mut app, 'M');
            assert!(note(&app).contains("to cały czat"), "{}", note(&app));
        }

        #[test]
        fn while_the_conductor_owes_an_answer_the_tab_is_read_again_after_a_moment() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry("m1", 1, "operator", "message", "Hej", None)],
                    json!({ "unanswered_chat": ["m1"] }),
                ),
            );
            let revision = revision(&app);
            let now = Instant::now();
            assert!(!conductor(&app).needs_read(revision, now));
            assert!(conductor(&app).needs_read(revision, now + Duration::from_secs(3)));
        }

        #[test]
        fn a_refresh_makes_the_tab_due_without_forgetting_the_history() {
            let mut app = tab_app();
            read(&mut app, page(1, 2, false));
            let revision = revision(&app);
            assert!(!conductor(&app).needs_read(revision, Instant::now()));
            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            assert!(conductor(&app).needs_read(revision, Instant::now()));
            assert_eq!(conductor(&app).items().len(), 2);
        }
    }

    // -- keys --------------------------------------------------------------------

    #[test]
    fn the_keys_of_the_tab_are_in_the_key_map_and_in_help() {
        assert_eq!(keymap::check_consistency(keymap::BINDINGS), Ok(()));
        for (action, key) in [
            (Action::Compose, "c"),
            (Action::UndoDecision, "u"),
            (Action::ChooseHarness, "h"),
            (Action::LoadMore, "M"),
        ] {
            assert_eq!(keymap::key_of(Context::Details, action).as_deref(), Some(key));
            assert!(
                keymap::help_entries(Context::Details)
                    .iter()
                    .any(|entry| entry.keys == key && entry.description.contains("Dyrygent")),
                "{action:?} is in the help of the details"
            );
        }
    }

    // -- against a served control plane -------------------------------------------

    mod served {
        use super::*;

        fn conducting() -> narsor::orchestration::RunnerSnapshot {
            let mut first = snapshot(1, None);
            first.conductor_available = true;
            first
        }

        /// The tab of task `NAR-12`, nothing read yet.
        async fn open_unread(harness: &RemoteHarness) -> TaskApp {
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            press(&mut app, KeyCode::Enter);
            app.detail_mut().set_tab(DetailTab::Conductor);
            app
        }

        /// The tab of task `NAR-12`, with the conductor read already.
        async fn open(harness: &RemoteHarness) -> TaskApp {
            let mut app = open_unread(harness).await;
            let revision = app.list().selected().expect("a selection").revision;
            app.on_detail_read(
                "NAR-12",
                revision,
                DetailRead::Conductor(Ok(ConductorReadResponse::Available {
                    conductor: Box::new(conductor_view(vec![], json!({}))),
                    source: ExecutionEventsSource::Runner,
                })),
            );
            app
        }

        /// The runner's side of one relayed conductor read: the `after` it
        /// was asked for, and `view` as the answer.
        async fn answer(runner: &RunnerClient, view: ConductorView) -> u64 {
            loop {
                let poll = runner.poll(None, 5).await.expect("poll");
                if let Some(request) = poll.conductor_reads.first() {
                    runner
                        .upload_conductor_read(&request.request_id, &view)
                        .await
                        .expect("upload");
                    return request.after;
                }
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_message_is_taken_by_the_control_plane_and_leaves_the_box() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open(&harness).await;
            ch(&mut app, 'c');
            assert!(conductor_typing(&app));
            typed(&mut app, "Dlaczego revise?");
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;

            assert_eq!(note(&app), "NAR-12: wiadomość wysłana do dyrygenta");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Success);
            assert_eq!(app.detail().view("NAR-12").unwrap().conductor().draft(), "");
            assert!(!app.detail().has_drafts(), "the text left with the message");
            let summary = harness.owner.control_task("NAR-12").await.unwrap();
            assert_eq!(
                summary.operator_commands[0].command,
                OperatorCommand::ConductorChat {
                    text: "Dlaczego revise?".to_string()
                }
            );
        }

        fn conductor_typing(app: &TaskApp) -> bool {
            app.detail().view("NAR-12").unwrap().conductor().is_typing()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_message_that_is_too_long_is_refused_and_stays_in_the_box() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open(&harness).await;
            ch(&mut app, 'c');
            typed(&mut app, &"a".repeat(4001));
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;

            let text = note(&app);
            assert!(text.contains("wiadomość do dyrygenta odrzucone"), "{text}");
            assert!(text.contains("4000"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            let draft = app.detail().view("NAR-12").unwrap().conductor().draft().to_string();
            assert_eq!(draft.chars().count(), 4001, "nothing the operator typed was lost");
            assert!(app.detail().has_drafts());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_unknown_harness_is_refused_early_with_the_supported_ones() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open(&harness).await;
            let generation = app.list().tasks()[0].generation;
            let request = ActionRequest {
                task_id: "NAR-12".to_string(),
                title: "NAR-12".to_string(),
                generation,
                key: "h-1".to_string(),
                verb: "zmiana harnessu dyrygenta".to_string(),
                summary: "x".to_string(),
                send: ActionSend::Operator(OperatorCommand::SetConductorHarness {
                    harness: "nope".to_string(),
                }),
            };
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("zmiana harnessu dyrygenta odrzucone"), "{text}");
            assert!(text.contains("unknown agent harness 'nope'"), "{text}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_chat_is_read_through_the_relay_a_page_at_a_time_after_the_last_entry() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open_unread(&harness).await;

            let first = conductor_view(
                vec![
                    chat_entry("m1", 1, "operator", "message", "Hej", None),
                    chat_entry("m2", 2, "conductor", "message", "Cześć", None),
                ],
                json!({ "chat_has_more": true }),
            );
            let second = conductor_view(
                vec![chat_entry("m3", 3, "conductor", "message", "Jeszcze coś", None)],
                json!({}),
            );
            let (_, afters) = tokio::join!(app.sync_detail(), async {
                let one = answer(&running.runner, first).await;
                let two = answer(&running.runner, second).await;
                (one, two)
            });
            assert_eq!(afters, (0, 2), "the second page continued after the last entry");
            let tab = app.detail().view("NAR-12").unwrap().conductor();
            assert_eq!(tab.items().len(), 3);
            assert!(!tab.has_more());
            assert!(app.connection().is_live());
        }
    }
}

/// U13: the new-task form, the history and the repositories.
mod task_app_create {
    use std::net::SocketAddr;
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use narsor::api::{ApiClient, ClientError};
    use narsor::controller_protocol::{
        LegacyExecutionListResponse, RepositoryListResponse, TaskSummary, ValidateWorkflowResponse,
        WorkflowListResponse,
    };
    use narsor::domain::ParamType;
    use narsor::orchestration::TaskLifecycle;
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::history::HistoryEntry;
    use narsor::tui::task_app::list::PendingState;
    use narsor::tui::task_app::new_task::{
        absolute_path, CreateHow, PathVerdict, DRAFT_KEY, PATH_FIELD, PATH_LABEL, RETRY_AFTER,
    };
    use narsor::tui::task_app::notify::Severity;
    use narsor::persistence::ExecutionStore;
    use narsor::tui::task_app::{DetailTab, Request, TaskApp, ViewKind};

    use super::remote_support::RemoteHarness;
    use super::task_app_support::{
        archived, capabilities, catalogue, catalogue_workflow, legacy, listing, queued, registry,
        repository, titled, workflow_input,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    /// An application with a live connection to nothing: what it is handed is
    /// all it knows.
    fn app_with(tasks: Vec<TaskSummary>) -> TaskApp {
        let mut app = TaskApp::new(offline_client(), "local", None);
        app.set_capabilities(capabilities(true, None));
        app.load(listing(tasks, "c"));
        app
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn workflows() -> WorkflowListResponse {
        catalogue(vec![
            catalogue_workflow(
                "implement-feature",
                "Pełny cykl: plan, implementacja, review, testy i PR.",
                vec![
                    workflow_input("ticket_id", ParamType::String, true, Some("numer zgłoszenia")),
                    workflow_input("attempts", ParamType::Integer, false, Some("ile razy ponawiać")),
                    workflow_input("dry_run", ParamType::Boolean, false, None),
                    workflow_input("target_repo", ParamType::Repository, false, None),
                ],
            ),
            catalogue_workflow(
                "fix-bug",
                "Poprawka błędu.",
                vec![workflow_input("ticket_id", ParamType::String, true, None)],
            ),
        ])
    }

    fn narsor_registry() -> RepositoryListResponse {
        registry(vec![
            repository("docs", "git@github.com:acme/docs.git"),
            repository("narsor", "git@github.com:acme/narsor.git"),
        ])
    }

    /// `n`, with the answers of the server given by hand.
    fn open_form(app: &mut TaskApp) {
        assert_eq!(ch(app, 'n'), Request::OpenNewTask);
        app.open_new_task(workflows(), narsor_registry());
        assert_eq!(app.view_kind(), ViewKind::NewTask);
    }

    fn chooser_item<'a>(app: &'a TaskApp, label: &str) -> &'a narsor::tui::chooser::ChooserItem {
        app.new_task_form()
            .expect("the form is open")
            .chooser()
            .items()
            .iter()
            .find(|item| item.label == label)
            .unwrap_or_else(|| panic!("{label} is not in the form"))
    }

    /// Moves the highlight of the form's list to `label`.
    fn choose(app: &mut TaskApp, label: &str) {
        for _ in 0..12 {
            let at = app.new_task_form().unwrap().chooser();
            if at.selected_item().is_some_and(|item| item.label == label) {
                return;
            }
            ch(app, 'j');
        }
        panic!("{label} was not reached");
    }

    fn fill_ticket(app: &mut TaskApp, ticket: &str) {
        press(app, KeyCode::Tab);
        typed(app, ticket);
    }

    fn field_value(app: &TaskApp, label: &str, field: &str) -> String {
        let chooser = app.new_task_form().unwrap().chooser();
        let index = chooser.items().iter().position(|item| item.label == label).unwrap();
        chooser
            .fields_of(index)
            .iter()
            .find(|candidate| candidate.name == field)
            .unwrap_or_else(|| panic!("{field} is not a field of {label}"))
            .value()
            .to_string()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(
                    frame,
                    app,
                    &narsor::tui::style::Styles::new(false),
                    Instant::now(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rows(app: &TaskApp) -> Vec<(char, String, String)> {
        app.list()
            .rows()
            .into_iter()
            .map(|row| (row.icon.glyph(), row.title, row.label))
            .collect()
    }

    // -- the form -------------------------------------------------------------

    mod the_form {
        use super::*;

        #[test]
        fn n_opens_the_catalogue_with_the_path_last_and_leaves_the_list_alone() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1"), titled(queued("b", 2), "NAR-2")]);
            let before = rows(&app);
            open_form(&mut app);
            let labels: Vec<&str> = app
                .new_task_form()
                .unwrap()
                .chooser()
                .items()
                .iter()
                .map(|item| item.label.as_str())
                .collect();
            assert_eq!(labels, ["implement-feature", "fix-bug", PATH_LABEL]);
            assert_eq!(rows(&app), before, "the list is not touched");
            assert_eq!(app.list().selected_id(), Some("a"));
            assert!(chooser_item(&app, "implement-feature")
                .description
                .contains("Pełny cykl"));
        }

        #[test]
        fn the_fields_are_the_inputs_of_the_highlighted_workflow_with_their_descriptions() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            let item = chooser_item(&app, "implement-feature");
            let names: Vec<&str> = item.fields.iter().map(|field| field.name.as_str()).collect();
            assert_eq!(names, ["ticket_id", "attempts", "dry_run", "target_repo"]);
            assert!(item.fields[0].required);
            assert_eq!(item.fields[0].description.as_deref(), Some("numer zgłoszenia"));
            assert!(!item.fields[1].required);
            assert_eq!(
                item.fields[1].description.as_deref(),
                Some("liczba całkowita · ile razy ponawiać")
            );
        }

        #[test]
        fn a_boolean_is_a_choice_that_starts_on_false_and_a_repository_offers_the_registered_ones() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            let item = chooser_item(&app, "implement-feature");
            assert_eq!(
                item.fields[2].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["true".into(), "false".into()])
            );
            assert_eq!(item.fields[2].value(), "false");
            assert_eq!(
                item.fields[3].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["docs".into(), "narsor".into()])
            );
            assert_eq!(app.new_task_form().unwrap().repositories(), ["docs", "narsor"]);
        }

        #[test]
        fn a_repository_is_a_free_line_while_none_is_registered() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'n');
            app.open_new_task(workflows(), registry(vec![]));
            let item = chooser_item(&app, "implement-feature");
            assert!(matches!(
                item.fields[3].kind,
                narsor::tui::chooser::FieldKind::Text { .. }
            ));
        }

        #[test]
        fn a_missing_required_input_is_an_error_at_the_field_and_nothing_is_sent() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            press(&mut app, KeyCode::Tab);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            let item = chooser_item(&app, "implement-feature");
            assert!(
                item.fields[0].error().is_some_and(|error| error.contains("ticket_id")),
                "{:?}",
                item.fields[0].error()
            );
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert!(app.outgoing().is_empty());
        }

        #[test]
        fn a_wrong_integer_is_an_error_at_the_field_and_nothing_is_sent() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "1-2");
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            let item = chooser_item(&app, "implement-feature");
            assert!(
                item.fields[1]
                    .error()
                    .is_some_and(|error| error.contains("attempts") && error.contains("liczby")),
                "{:?}",
                item.fields[1].error()
            );
            assert!(item.fields[0].error().is_none());
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert!(app.outgoing().is_empty());
            assert!(app.list().pending().is_empty());
        }

        #[test]
        fn esc_gives_the_selected_task_back_and_the_next_n_brings_the_data_back() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1"), titled(queued("b", 2), "NAR-2")]);
            ch(&mut app, 'g');
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            assert!(app.detail().has_drafts(), "typed data is an unsent draft");
            assert!(app.detail().draft(DRAFT_KEY).is_some_and(|text| text.contains("NAR-18")));

            assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("a"), "the task selected before is back");
            assert!(app.remembered_new_task().is_some());

            // `q` asks before throwing the data away.
            ch(&mut app, 'q');
            assert!(!app.should_quit());
            assert!(note(&app).contains("szkic"), "{}", note(&app));

            ch(&mut app, 'n');
            app.open_new_task(workflows(), narsor_registry());
            assert_eq!(field_value(&app, "implement-feature", "ticket_id"), "NAR-18");
            assert!(app.remembered_new_task().is_none(), "it is the open form now");
        }

        #[test]
        fn the_form_opens_with_the_focus_on_the_details_and_esc_gives_the_focus_back() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            open_form(&mut app);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
        }

        #[test]
        fn keys_in_the_form_are_the_choosers_and_q_is_text_in_a_field() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            assert_eq!(app.key_context(), Context::Panel);
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.key_context(), Context::Form);
            ch(&mut app, 'q');
            assert!(!app.should_quit());
            assert_eq!(field_value(&app, "implement-feature", "ticket_id"), "q");
        }

        #[test]
        fn a_closed_catalogue_with_no_answer_says_so_and_opens_nothing() {
            let mut app = app_with(vec![]);
            // Nothing listens where the client points.
            let request = ch(&mut app, 'n');
            assert_eq!(request, Request::OpenNewTask);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(app.perform(request));
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert!(note(&app).contains("nie udało się wczytać katalogu"), "{}", note(&app));
        }
    }

    // -- a path ----------------------------------------------------------------

    mod a_workflow_path {
        use super::*;

        #[test]
        fn a_server_that_does_not_read_files_greys_the_path_out_with_its_reason() {
            let mut app = app_with(vec![]);
            app.set_capabilities(capabilities(false, Some("serwer zdalny nie czyta plików z dysku")));
            open_form(&mut app);
            let item = chooser_item(&app, PATH_LABEL);
            assert_eq!(
                item.unavailable.as_deref(),
                Some("serwer zdalny nie czyta plików z dysku")
            );
            choose(&mut app, PATH_LABEL);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(
                note(&app).contains("serwer zdalny nie czyta plików z dysku"),
                "{}",
                note(&app)
            );
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn without_a_reason_of_its_own_the_path_still_says_why() {
            let mut app = app_with(vec![]);
            app.set_capabilities(capabilities(false, None));
            open_form(&mut app);
            assert!(chooser_item(&app, PATH_LABEL)
                .unavailable
                .as_deref()
                .is_some_and(|reason| reason.contains("nie czyta plików")));
        }

        #[test]
        fn a_local_daemon_offers_the_path() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            assert_eq!(chooser_item(&app, PATH_LABEL).unavailable, None);
        }

        #[test]
        fn a_relative_path_is_sent_as_an_absolute_one() {
            let absolute = absolute_path("workflows/x.yaml");
            assert!(std::path::Path::new(&absolute).is_absolute(), "{absolute}");
            assert!(absolute.ends_with("workflows/x.yaml"));
            assert_eq!(absolute_path("/srv/x.yaml"), "/srv/x.yaml");
            let home = std::env::var("HOME").unwrap_or_default();
            if !home.is_empty() {
                assert_eq!(absolute_path("~/x.yaml"), format!("{home}/x.yaml"));
            }
        }

        #[test]
        fn enter_on_a_path_asks_the_server_to_check_it_first() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Tab);
            typed(&mut app, "/srv/workflows/wf.yaml");
            assert_eq!(
                press(&mut app, KeyCode::Enter),
                Request::ValidatePath("/srv/workflows/wf.yaml".to_string())
            );
            assert!(app.outgoing().is_empty(), "nothing is created before the check");
        }

        #[test]
        fn every_violation_is_in_the_description_and_none_is_sent() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Tab);
            typed(&mut app, "/srv/bad.yaml");
            app.on_path_checked(
                "/srv/bad.yaml",
                Ok(ValidateWorkflowResponse {
                    valid: false,
                    name: None,
                    version: None,
                    inputs: vec![],
                    violations: vec![
                        "state 'plan' has no transition for output 'ok'".to_string(),
                        "state 'ship' is not reachable".to_string(),
                        "input 'ticket' is declared twice".to_string(),
                    ],
                }),
            );
            let description = &chooser_item(&app, PATH_LABEL).description;
            for violation in [
                "state 'plan' has no transition for output 'ok'",
                "state 'ship' is not reachable",
                "input 'ticket' is declared twice",
            ] {
                assert!(description.contains(violation), "{description}");
            }
            assert!(description.contains("naruszeń: 3"), "{description}");
            let error = app
                .new_task_form()
                .unwrap()
                .chooser()
                .fields_of(2)[0]
                .error()
                .map(str::to_string);
            assert!(error.is_some_and(|error| error.contains('3')));
            assert!(matches!(
                app.new_task_form().unwrap().path_check().unwrap().verdict,
                PathVerdict::Invalid(_)
            ));
            assert!(app.outgoing().is_empty());
        }

        #[test]
        fn a_definition_that_loads_brings_its_inputs_and_enter_then_creates_the_task() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Tab);
            typed(&mut app, "/srv/ok.yaml");
            app.on_path_checked(
                "/srv/ok.yaml",
                Ok(ValidateWorkflowResponse {
                    valid: true,
                    name: Some("my-flow".to_string()),
                    version: Some(4),
                    inputs: vec![workflow_input("ticket", ParamType::String, true, None)],
                    violations: vec![],
                }),
            );
            let item = chooser_item(&app, PATH_LABEL);
            let names: Vec<&str> = item.fields.iter().map(|field| field.name.as_str()).collect();
            assert_eq!(names, [PATH_FIELD, "ticket"]);
            assert!(item.description.contains("my-flow v4"));
            assert_eq!(field_value(&app, PATH_LABEL, PATH_FIELD), "/srv/ok.yaml", "what was typed stays");

            press(&mut app, KeyCode::Down);
            typed(&mut app, "NAR-3");
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("the task should be sent");
            };
            let outgoing = &app.outgoing()[0];
            assert_eq!(outgoing.task_id, id);
            assert_eq!(outgoing.title, "NAR-3");
            assert_eq!(
                outgoing.send,
                CreateHow::Path {
                    workflow: "/srv/ok.yaml".to_string(),
                    inputs: json!({ "ticket": "NAR-3" }).as_object().unwrap().clone(),
                }
            );
        }

        #[test]
        fn a_path_that_could_not_be_checked_says_why_in_the_description() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Tab);
            typed(&mut app, "/nope.yaml");
            app.on_path_checked(
                "/nope.yaml",
                Err(ClientError::Request {
                    code: "workflow_not_found".to_string(),
                    message: "there is no workflow file at that path".to_string(),
                }),
            );
            assert!(chooser_item(&app, PATH_LABEL)
                .description
                .contains("there is no workflow file at that path"));
        }
    }

    // -- sending ---------------------------------------------------------------

    mod sending {
        use super::*;

        fn filled() -> TaskApp {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1"), titled(queued("b", 2), "NAR-2")]);
            ch(&mut app, 'g');
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "3");
            app
        }

        #[test]
        fn enter_makes_the_id_first_and_the_row_goes_last_and_is_selected() {
            let mut app = filled();
            let before = rows(&app);
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            assert!(id.starts_with("task-"), "{id}");
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            let now = rows(&app);
            assert_eq!(now[..2], before[..], "the rows above are where they were");
            assert_eq!(now.len(), 3);
            assert_eq!(now[2], ('↻', "NAR-18".to_string(), "wysyłanie…".to_string()));
            assert!(app.detail().has_drafts(), "until the server has it, it is unsent");
        }

        #[test]
        fn the_request_carries_the_id_the_version_and_the_typed_inputs() {
            let mut app = filled();
            ch(&mut app, 'x'); // not a field: ignored once the form closed
            let _ = app;
            let mut app = filled();
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("a workflow of the catalogue is created through the control plane");
            };
            assert_eq!(request.task_id.as_deref(), Some(id.as_str()));
            assert_eq!(request.launch.workflow_id, "implement-feature");
            assert_eq!(request.launch.workflow_version, 1);
            assert_eq!(
                request.launch.inputs,
                json!({
                    "ticket_id": "NAR-18",
                    "attempts": 3,
                    "dry_run": false,
                    "target_repo": "docs",
                })
            );
        }

        #[test]
        fn a_boolean_chosen_with_the_arrow_is_sent_as_a_boolean() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-1");
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Right);
            assert_eq!(field_value(&app, "implement-feature", "dry_run"), "true");
            press(&mut app, KeyCode::Enter);
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("catalogue");
            };
            assert_eq!(request.launch.inputs["dry_run"], json!(true));
        }

        #[test]
        fn without_a_connection_nothing_is_sent_and_the_form_keeps_what_was_typed() {
            let mut app = TaskApp::new(offline_client(), "local", None);
            app.set_capabilities(capabilities(true, None));
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(app.view_kind(), ViewKind::NewTask, "the form stays");
            assert_eq!(field_value(&app, "implement-feature", "ticket_id"), "NAR-18");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
            assert!(app.outgoing().is_empty());
            assert!(app.list().pending().is_empty());
        }

        #[tokio::test]
        async fn a_lost_answer_is_sent_again_with_the_same_id_and_then_stays_unsent() {
            let mut app = filled();
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            // The first attempt got nothing: the row says it is being sent
            // again, and the request is waiting to go out again.
            assert_eq!(rows(&app)[2].0, '↻');
            assert_eq!(app.list().pending()[0].state, PendingState::Sending);
            assert!(app.next_outgoing_due().is_some());
            assert!(note(&app).contains("ponawiam z tym samym id"), "{}", note(&app));
            let first = app.outgoing()[0].clone();

            // Not yet due.
            assert!(!app.retry_outgoing_if_due(Instant::now()).await);
            assert_eq!(app.outgoing()[0].attempts(), 1);

            // Due: the same request goes out and gets nothing again.
            assert!(app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2).await);
            assert_eq!(app.outgoing()[0].send, first.send, "the same request");
            assert_eq!(app.outgoing()[0].task_id, id, "under the same id");
            assert_eq!(rows(&app)[2], ('✗', "NAR-18".to_string(), "nie wysłano".to_string()));
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);
            assert!(app.next_outgoing_due().is_none(), "no more by itself");
            assert!(app.detail().has_drafts(), "the data is kept");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
        }

        #[test]
        fn r_on_an_unsent_row_sends_it_again_when_the_connection_allows_and_says_why_not_otherwise() {
            let mut app = filled();
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                app.perform(Request::SendTask(id.clone())).await;
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2).await;
            });
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);

            // The connection is down: the reason is on the bar, nothing is sent.
            assert_eq!(ch(&mut app, 'r'), Request::None);
            assert!(!note(&app).is_empty());
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);

            // It came back.
            app.load(listing(vec![titled(queued("a", 1), "NAR-1")], "d"));
            assert_eq!(app.list().selected_id(), Some(id.as_str()), "the row keeps the selection");
            assert_eq!(ch(&mut app, 'r'), Request::SendTask(id.clone()));
            assert_eq!(app.list().pending()[0].state, PendingState::Sending);
            assert_eq!(app.outgoing()[0].task_id, id);
        }

        #[test]
        fn r_on_a_task_the_server_has_has_nothing_to_send() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            assert_eq!(ch(&mut app, 'r'), Request::None);
            assert!(note(&app).contains("nie ma czego ponawiać"), "{}", note(&app));
        }

        #[test]
        fn the_unsent_row_has_details_that_say_where_it_stands() {
            let mut app = filled();
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                app.perform(Request::SendTask(id.clone())).await;
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2).await;
            });
            let screen = screen(&mut app, 120, 30);
            for expected in [
                "✗ nie wysłano",
                &id,
                "Dane zostały zachowane",
                "implement-feature",
                "ticket_id = NAR-18",
                "attempts = 3",
            ] {
                assert!(screen.contains(expected), "{expected}:\n{screen}");
            }
        }

        #[test]
        fn what_the_server_lists_is_no_longer_waiting_to_be_confirmed() {
            let mut app = filled();
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            // The answer was lost, but the task is there when the list is read.
            app.load(listing(
                vec![
                    titled(queued("a", 1), "NAR-1"),
                    titled(queued("b", 2), "NAR-2"),
                    titled(queued(&id, 3), "NAR-18"),
                ],
                "d",
            ));
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty(), "nothing is left to send again");
            assert!(!app.detail().has_drafts(), "and nothing is unsent");
            assert_eq!(rows(&app).len(), 3);
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert_eq!(rows(&app)[2].0, '◷');
        }

        #[test]
        fn the_pending_row_can_be_moved_over_and_the_selection_stays_on_it() {
            let mut app = filled();
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            ch(&mut app, 'k');
            assert_eq!(app.list().selected_id(), Some("b"));
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            ch(&mut app, 'G');
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
        }
    }

    // -- the history -----------------------------------------------------------

    mod history {
        use super::*;

        fn opened() -> TaskApp {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            assert_eq!(ch(&mut app, 'H'), Request::OpenHistory);
            app.open_history(
                listing(
                    vec![archived("old", "NAR-9", 5), archived("older", "NAR-8", 1)],
                    "c",
                ),
                Ok(LegacyExecutionListResponse {
                    executions: vec![
                        legacy("exec-failed", "fix-bug", "failed", 3, true, None),
                        legacy("exec-done", "fix-bug", "completed", 2, false, Some("zakończone — nie ma czego wznawiać")),
                    ],
                }),
            );
            assert_eq!(app.view_kind(), ViewKind::History);
            app
        }

        fn titles(app: &TaskApp) -> Vec<String> {
            app.history().unwrap().rows().into_iter().map(|row| row.title).collect()
        }

        #[test]
        fn it_lists_archived_tasks_and_old_executions_newest_first() {
            let app = opened();
            assert_eq!(
                titles(&app),
                ["NAR-9", "fix-bug exec-failed", "fix-bug exec-done", "NAR-8"]
            );
            assert_eq!(app.history().unwrap().selected(), 0);
        }

        #[test]
        fn an_archived_task_has_the_tabs_read_only_and_says_its_storage_is_gone() {
            let mut app = opened();
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.key_context(), Context::HistoryDetails);
            for (tab, expected) in [
                (DetailTab::Progress, "storage usunięty przy archiwizacji"),
                (DetailTab::Conversation, "storage usunięty przy archiwizacji"),
                (DetailTab::Events, "storage usunięty przy archiwizacji"),
                (DetailTab::Conductor, "storage usunięty przy archiwizacji"),
            ] {
                while app.history().unwrap().tab() != tab {
                    press(&mut app, KeyCode::Right);
                }
                let screen = screen(&mut app, 130, 30);
                assert!(screen.contains(expected), "{tab:?}:\n{screen}");
            }
            // The details are not the ones of the task list.
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("NAR-9"), "{screen}");
            assert!(screen.contains("Historia"), "{screen}");
        }

        #[test]
        fn esc_in_the_details_goes_to_the_history_list_and_esc_there_to_the_tasks() {
            let mut app = opened();
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::History);
            assert_eq!(app.key_context(), Context::History);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("a"));
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
        }

        #[test]
        fn recover_is_offered_only_for_an_execution_that_can_be_recovered() {
            let mut app = opened();
            // The archived task: nothing to recover, and why.
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert!(note(&app).contains("storage usunięty przy archiwizacji"), "{}", note(&app));

            // The failed execution can be.
            ch(&mut app, 'j');
            assert_eq!(ch(&mut app, 'w'), Request::Recover("exec-failed".to_string()));

            // The completed one cannot; the server's reason is on the bar.
            ch(&mut app, 'j');
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert!(note(&app).contains("zakończone — nie ma czego wznawiać"), "{}", note(&app));
        }

        #[test]
        fn recover_is_blocked_without_a_connection() {
            let mut app = opened();
            ch(&mut app, 'j');
            // The connection broke after the history was read.
            app.on_stream(Some(Err(ClientError::Unavailable("gone".to_string()))));
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn the_only_action_of_the_history_is_recover_the_task_keys_do_nothing() {
            let mut app = opened();
            for key in ['p', 'x', 't', 's', 'a', 'm', 'o', 'n', 'z', '.', 'R', 'H'] {
                assert_eq!(ch(&mut app, key), Request::None, "{key}");
            }
            assert_eq!(app.view_kind(), ViewKind::History);
            assert!(app.overlay().is_none());
            assert_eq!(app.list().tasks().len(), 1, "the task list is untouched");
        }

        #[test]
        fn an_old_execution_has_a_header_and_tabs_of_its_own() {
            let mut app = opened();
            ch(&mut app, 'j');
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("fix-bug exec-failed"), "{screen}");
            assert!(screen.contains("failed"), "{screen}");
            assert!(screen.contains("Przebieg"), "{screen}");
        }

        #[test]
        fn what_the_server_could_not_list_is_said_and_the_archived_tasks_are_still_there() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'H');
            app.open_history(
                listing(vec![archived("old", "NAR-9", 5)], "c"),
                Err(ClientError::Unavailable("timeout".to_string())),
            );
            assert_eq!(titles(&app), ["NAR-9"]);
            assert!(app.history().unwrap().note().is_some());
            assert!(note(&app).contains("dawne executions niedostępne"), "{}", note(&app));
        }

        #[test]
        fn an_empty_history_is_an_empty_list_not_an_error() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'H');
            app.open_history(listing(vec![], "c"), Ok(LegacyExecutionListResponse { executions: vec![] }));
            assert!(titles(&app).is_empty());
            let screen = screen(&mut app, 120, 30);
            assert!(screen.contains("historia jest pusta"), "{screen}");
        }

        #[test]
        fn keys_and_help_of_the_history_come_from_the_map() {
            let app = opened();
            assert_eq!(app.key_context(), Context::History);
            let keys = keymap::hints(Context::History);
            assert!(keys.iter().any(|(_, label)| *label == "wznów"));
            assert_eq!(
                keymap::lookup(Context::History, &KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
                Some(Action::Recover)
            );
            let entry = app.history().unwrap().selected_entry().unwrap();
            assert!(matches!(entry, HistoryEntry::Archived(_)));
        }

        /// An execution from before the task scheduler, with a retried step
        /// and a conversation per attempt, written under `root`.
        fn seed_old_execution(root: &std::path::Path, id: &str) {
            use narsor::domain::{
                Context as WorkflowContext, EventData, ExecutionStatus, RepositoryInfo,
                WorkflowExecution, Workspace,
            };
            use narsor::persistence::conversation::ConversationSource;
            let store = ExecutionStore::new(root);
            store
                .save_execution(&WorkflowExecution {
                    id: id.to_string(),
                    workflow_id: "implement-feature".to_string(),
                    workflow_version: 1,
                    status: ExecutionStatus::Completed,
                    current_state: Some("plan".to_string()),
                    started_at: chrono::Utc::now(),
                    completed_at: None,
                    inputs: json!({}),
                    context: WorkflowContext::new(),
                    workspace: Workspace::git_checkout(
                        "/tmp/workspace".to_string(),
                        RepositoryInfo {
                            id: "repo-1".to_string(),
                            name: "narsor".to_string(),
                            provider: "gitlab".to_string(),
                            remote: "git@example.com:team/narsor.git".to_string(),
                        },
                        Some("main".to_string()),
                    ),
                    current_action_execution: None,
                    metadata: json!({}),
                })
                .expect("save execution");
            let log = store.event_log(id);
            let id_string = id.to_string();
            for data in [
                EventData::WorkflowStarted {
                    workflow_id: "implement-feature".to_string(),
                    workflow_version: 1,
                    inputs: json!({}),
                },
                EventData::StateEntered { state_id: "plan".to_string() },
                EventData::ActionStarted {
                    action_execution_id: "attempt-1".to_string(),
                    state_id: "plan".to_string(),
                    action_type: "agent".to_string(),
                },
                EventData::ActionCompleted {
                    action_execution_id: "attempt-1".to_string(),
                    output_label: "success".to_string(),
                },
            ] {
                log.append_next(&id_string, data).expect("append");
            }
            let conversations = store.conversation_store(id);
            let mut writer = conversations.writer("attempt-1").expect("writer");
            writer.append_narsor("prompt: zaplanuj zmianę").expect("prompt");
            writer
                .append(ConversationSource::Stdout, br#"{"type":"assistant"}"#)
                .expect("stdout");
            writer.finish().expect("finish");
            conversations.close_attempt("attempt-1").expect("close");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_old_execution_is_read_through_the_history_routes_in_every_tab_and_recover_is_refused_with_the_reason() {
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-old");
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;

            let request = ch(&mut app, 'H');
            app.perform(request).await;
            assert_eq!(titles(&app), ["implement-feature exec-old"]);
            press(&mut app, KeyCode::Enter);

            app.sync_detail().await;
            let progress = screen(&mut app, 130, 30);
            assert!(progress.contains("plan #1"), "the timeline of the old execution:\n{progress}");
            assert!(progress.contains("completed"), "{progress}");

            press(&mut app, KeyCode::Right);
            app.sync_detail().await;
            let conversation = screen(&mut app, 130, 30);
            assert!(conversation.contains("Workflow sends input to agent"), "{conversation}");
            assert!(conversation.contains("assistant message"), "{conversation}");

            press(&mut app, KeyCode::Right);
            app.sync_detail().await;
            let events = screen(&mut app, 130, 30);
            assert!(events.contains("WorkflowStarted") || events.contains("workflow"), "{events}");
            assert!(events.contains("plan"), "{events}");

            press(&mut app, KeyCode::Right);
            app.sync_detail().await;
            let conductor = screen(&mut app, 130, 30);
            assert!(conductor.contains("tylko do odczytu"), "no chat box, only history:\n{conductor}");

            // Nothing of the old execution is a key of the task list: `w` is
            // refused up front, and the server's refusal is shown if it is asked.
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert!(!note(&app).is_empty());
            app.perform(Request::Recover("exec-old".to_string())).await;
            let said = note(&app);
            assert!(said.contains("wznowienie exec-old odrzucone"), "{said}");
            assert!(said.contains("completed"), "{said}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_archived_tasks_of_a_server_are_the_history() {
            let harness = RemoteHarness::start().await;
            harness.create("NAR-9", Some("k8s")).await;
            harness.create("NAR-10", Some("k8s")).await;
            // Archiving a task that never started completes at once.
            harness
                .owner
                .control_command(
                    "NAR-9",
                    &narsor::controller_protocol::OwnerCommandRequest {
                        command: narsor::orchestration::OwnerCommand::Archive,
                        idempotency_key: "archive-1".to_string(),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archive");

            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            assert_eq!(app.list().tasks().len(), 1, "the archived task left the list");

            let request = ch(&mut app, 'H');
            app.perform(request).await;
            assert_eq!(app.view_kind(), ViewKind::History);
            assert_eq!(titles(&app), ["NAR-9"]);
            let entry = app.history().unwrap().selected_entry().unwrap();
            assert_eq!(entry.summary().lifecycle, TaskLifecycle::Archived);

            // The details are read-only and read nothing: the storage is gone.
            press(&mut app, KeyCode::Enter);
            app.sync_detail().await;
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("storage usunięty przy archiwizacji"), "{screen}");

            // `Esc` twice: the task list is where it was.
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("NAR-10"));
        }
    }

    // -- over the wire ---------------------------------------------------------

    mod over_the_wire {
        use super::*;

        use tokio::io::AsyncReadExt as _;
        use tokio::net::{TcpListener, TcpStream};

        use super::super::environment_support::{workflow_image, workflow_yaml};

        /// The workflow the tests create tasks from: a ticket, a number, a
        /// flag and a repository, and a title made of the ticket.
        fn implement_feature(version: u32) -> String {
            let header = format!("name: implement-feature\nversion: {version}\n");
            let declared = "description: \"Pełny cykl: plan, implementacja, review.\"\n\
                inputs:\n  - name: ticket_id\n    type: string\n    required: true\n    \
                description: numer zgłoszenia\n  - name: attempts\n    type: integer\n  \
                - name: dry_run\n    type: boolean\n  - name: target_repo\n    \
                type: repository\ndisplay:\n  title: \"{{ inputs.ticket_id }}\"\n";
            let body = workflow_yaml("implement-feature", Some(&workflow_image()));
            let rest = body.split_once("version: 1\n").expect("a version").1;
            format!("{header}{declared}{rest}")
        }

        fn write_workflow(root: &std::path::Path) {
            std::fs::write(
                root.join("workflows/implement-feature.yaml"),
                implement_feature(1),
            )
            .expect("write workflow");
        }

        /// The served control plane, with a live controller on its backend: a
        /// queued task shows its place (`#n`), not `offline`. Keep the second
        /// part alive for as long as the test runs.
        async fn harness() -> (RemoteHarness, (narsor::controller_protocol::ControllerClient, u64)) {
            let harness = RemoteHarness::start_with(write_workflow).await;
            let controller = harness.controller("k8s").await;
            (harness, controller)
        }

        /// A task the server already has.
        async fn existing(harness: &RemoteHarness, id: &str) {
            let mut request = super::super::remote_support::create_request(id, Some("k8s"));
            request.launch.inputs = json!({ "ticket_id": id });
            harness.owner.create_control_task(&request).await.expect("create");
        }

        async fn open(harness: &RemoteHarness) -> TaskApp {
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            app
        }

        /// An application whose client reaches the server through a proxy
        /// that loses the answer of the first `losses` connections.
        async fn app_behind_a_lossy_proxy(harness: &RemoteHarness, losses: usize) -> TaskApp {
            let proxy = lossy_proxy(harness.handle.addr(), losses).await;
            let client = ApiClient::new(proxy, harness.handle.token().to_string());
            let mut app = TaskApp::new(client, "test", None);
            // Read through the direct client: the proxy's first connection
            // must be the creation.
            app.set_capabilities(harness.owner.control_capabilities().await.unwrap());
            app.load(harness.owner.control_tasks().await.unwrap());
            app
        }

        /// Forwards connections to `upstream`; for the first `losses` of them
        /// the request goes through and the answer never comes back: the
        /// server did what it was asked and the client does not know.
        async fn lossy_proxy(upstream: SocketAddr, losses: usize) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
            let address = listener.local_addr().expect("an address");
            tokio::spawn(async move {
                let mut seen = 0usize;
                while let Ok((client, _)) = listener.accept().await {
                    let lose = seen < losses;
                    seen += 1;
                    tokio::spawn(async move {
                        let Ok(server) = TcpStream::connect(upstream).await else {
                            return;
                        };
                        if !lose {
                            let (mut client, mut server) = (client, server);
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            return;
                        }
                        let (mut from_client, _to_client) = client.into_split();
                        let (mut from_server, mut to_server) = server.into_split();
                        let request = tokio::spawn(async move {
                            let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
                        });
                        // The answer started: the server has the task. Hang up.
                        let mut first = [0u8; 1024];
                        let _ = from_server.read(&mut first).await;
                        request.abort();
                    });
                }
            });
            address
        }

        async fn open_the_form(app: &mut TaskApp, harness: &RemoteHarness) {
            let workflows = harness.owner.control_workflows().await.unwrap();
            let repositories = harness.owner.control_repositories().await.unwrap();
            ch(app, 'n');
            app.open_new_task(workflows, repositories);
            choose(app, "implement-feature");
            fill_ticket(app, "NAR-18");
        }

        async fn tasks_on_the_server(harness: &RemoteHarness) -> Vec<String> {
            harness
                .owner
                .control_tasks()
                .await
                .unwrap()
                .tasks
                .iter()
                .map(|task| task.task_id.to_string())
                .collect()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn n_then_a_workflow_then_a_ticket_then_enter_puts_a_queued_row_last_and_selected() {
            let (harness, _controller) = harness().await;
            existing(&harness, "NAR-1").await;
            existing(&harness, "NAR-2").await;
            let mut app = open(&harness).await;
            let before = rows(&app);
            assert_eq!(before.len(), 2);

            let request = ch(&mut app, 'n');
            assert_eq!(request, Request::OpenNewTask);
            app.perform(request).await;
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            choose(&mut app, "implement-feature");
            assert!(
                chooser_item(&app, "implement-feature")
                    .description
                    .contains("Pełny cykl"),
                "{}",
                chooser_item(&app, "implement-feature").description
            );
            fill_ticket(&mut app, "NAR-18");
            let request = press(&mut app, KeyCode::Enter);
            assert!(matches!(request, Request::SendTask(_)), "{request:?}");
            app.perform(request).await;

            let now = rows(&app);
            assert_eq!(now.len(), 3);
            assert_eq!(now[..2], before[..], "the list below is not touched");
            assert_eq!(now[2], ('◷', "NAR-18".to_string(), "#3".to_string()));
            let id = app.list().tasks()[2].task_id.to_string();
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty());
            assert!(app.remembered_new_task().is_none(), "what was sent is forgotten");
            assert!(!app.detail().has_drafts());
            assert!(note(&app).contains("zadanie utworzone"), "{}", note(&app));
            assert_eq!(tasks_on_the_server(&harness).await.len(), 3);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_lost_answer_is_sent_again_with_the_same_id_and_makes_one_task() {
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 1).await;
            open_the_form(&mut app, &harness).await;
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;

            // The server has the task, the client does not know.
            assert_eq!(tasks_on_the_server(&harness).await, std::slice::from_ref(&id));
            assert_eq!(rows(&app), [('↻', "NAR-18".to_string(), "wysyłanie…".to_string())]);

            assert!(app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2).await);
            assert_eq!(rows(&app), [('◷', "NAR-18".to_string(), "#1".to_string())]);
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert_eq!(
                tasks_on_the_server(&harness).await,
                std::slice::from_ref(&id),
                "the same id made no second task"
            );
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn when_the_retry_is_lost_too_the_row_is_unsent_and_a_read_that_lists_the_task_settles_it() {
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 2).await;
            open_the_form(&mut app, &harness).await;
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            assert!(app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2).await);
            assert_eq!(rows(&app), [('✗', "NAR-18".to_string(), "nie wysłano".to_string())]);
            assert_eq!(tasks_on_the_server(&harness).await, std::slice::from_ref(&id));

            // The connection is back (a read succeeded), the operator sends again.
            app.load(harness.owner.control_tasks().await.unwrap());
            assert_eq!(
                rows(&app),
                [('◷', "NAR-18".to_string(), "#1".to_string())],
                "a read that lists the task is as good as an answer"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn r_on_the_unsent_row_makes_the_same_task_when_the_list_does_not_have_it_yet() {
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 2).await;
            open_the_form(&mut app, &harness).await;
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2).await;
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);

            // A read that does not list it (an older listing) brings the
            // connection back without settling the row.
            app.load(listing(vec![], "stale"));
            assert_eq!(app.list().pending().len(), 1);
            let request = ch(&mut app, 'r');
            assert_eq!(request, Request::SendTask(id.clone()));
            app.perform(request).await;
            assert_eq!(rows(&app), [('◷', "NAR-18".to_string(), "#1".to_string())]);
            assert_eq!(tasks_on_the_server(&harness).await, [id], "one task, one id");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_refusal_is_not_a_lost_answer_the_form_comes_back_with_what_was_typed() {
            let (harness, _controller) = harness().await;
            let mut app = open(&harness).await;
            open_the_form(&mut app, &harness).await;
            // The catalogue moved on after the form was opened.
            std::fs::write(
                harness.root.path().join("workflows/implement-feature.yaml"),
                implement_feature(2),
            )
            .unwrap();
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;

            assert_eq!(app.view_kind(), ViewKind::NewTask, "the form is back");
            assert_eq!(field_value(&app, "implement-feature", "ticket_id"), "NAR-18");
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty());
            assert!(note(&app).contains("serwer odrzucił"), "{}", note(&app));
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            assert!(tasks_on_the_server(&harness).await.is_empty());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_path_with_violations_lists_all_of_them_and_a_good_one_creates_a_task() {
            let (harness, _controller) = harness().await;
            let broken = "name: broken\nversion: 1\nstates:\n  work:\n    action:\n      type: script\n      \
                command: \"true\"\n    outputs: [success]\n    transitions:\n      success: nowhere\n  \
                island:\n    description: nobody comes here\nterminal_states: [done]\n";
            let expected: Vec<String> = narsor::workflow::validate_source(broken)
                .expect("the YAML loads")
                .iter()
                .map(ToString::to_string)
                .collect();
            assert!(expected.len() >= 2, "{expected:?}");
            let bad_path = harness.root.path().join("broken.yaml");
            std::fs::write(&bad_path, broken).unwrap();
            let good_path = harness.root.path().join("mine.yaml");
            std::fs::write(
                &good_path,
                implement_feature(1).replace("implement-feature", "mine"),
            )
            .unwrap();

            let mut app = open(&harness).await;
            let request = ch(&mut app, 'n');
            app.perform(request).await;
            assert_eq!(chooser_item(&app, PATH_LABEL).unavailable, None, "a local daemon reads files");
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Tab);
            typed(&mut app, bad_path.to_str().unwrap());
            let request = press(&mut app, KeyCode::Enter);
            assert_eq!(request, Request::ValidatePath(bad_path.to_str().unwrap().to_string()));
            app.perform(request).await;
            let description = &chooser_item(&app, PATH_LABEL).description;
            for violation in &expected {
                assert!(description.contains(violation.as_str()), "{violation}\n{description}");
            }
            assert!(tasks_on_the_server(&harness).await.is_empty(), "nothing was created");

            // A good file: first checked, then created from.
            for _ in 0..bad_path.to_str().unwrap().len() {
                press(&mut app, KeyCode::Backspace);
            }
            typed(&mut app, good_path.to_str().unwrap());
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;
            assert!(matches!(
                app.new_task_form().unwrap().path_check().unwrap().verdict,
                PathVerdict::Valid { version: 1, .. }
            ));
            press(&mut app, KeyCode::Down);
            typed(&mut app, "NAR-77");
            let request = press(&mut app, KeyCode::Enter);
            let Request::SendTask(id) = &request else {
                panic!("{request:?}");
            };
            let id = id.clone();
            app.perform(request).await;
            assert_eq!(tasks_on_the_server(&harness).await, [id]);
            assert_eq!(rows(&app)[0].0, '◷');
            assert_eq!(rows(&app)[0].1, "NAR-77");
        }
    }

    // -- the repositories ------------------------------------------------------

    mod repositories {
        use super::*;

        use super::super::environment_support::{workflow_image, workflow_yaml};

        fn with_a_repository_input(root: &std::path::Path) {
            let body = workflow_yaml("implement-feature", Some(&workflow_image()));
            let rest = body.split_once("version: 1\n").unwrap().1;
            std::fs::write(
                root.join("workflows/implement-feature.yaml"),
                format!(
                    "name: implement-feature\nversion: 1\ninputs:\n  - name: target_repo\n    \
                     type: repository\n{rest}"
                ),
            )
            .unwrap();
        }

        fn names(app: &TaskApp) -> Vec<String> {
            app.repositories()
                .unwrap()
                .repositories()
                .iter()
                .map(|repository| repository.id.clone())
                .collect()
        }

        #[test]
        fn the_list_shows_what_is_registered_with_a_description_and_a_remote_only_form_for_a_remote_server() {
            let mut app = app_with(vec![]);
            app.set_capabilities(capabilities(false, Some("remote")));
            assert_eq!(ch(&mut app, 'R'), Request::OpenRepositories);
            app.open_repositories(narsor_registry());
            assert_eq!(app.view_kind(), ViewKind::Repositories);
            assert_eq!(names(&app), ["docs", "narsor"]);
            assert_eq!(app.key_context(), Context::Repositories);
            let screen = screen(&mut app, 130, 30);
            for expected in ["Repozytoria (2)", "docs", "narsor", "git@github.com:acme/docs.git", "a dodaj", "d usuń"] {
                assert!(screen.contains(expected), "{expected}:\n{screen}");
            }

            ch(&mut app, 'a');
            let form = app.repositories().unwrap().form().expect("the form");
            let remote = &form.items()[0].fields[1];
            assert_eq!(remote.name, "remote");
            assert!(
                remote.description.as_deref().is_some_and(|text| text.contains("tylko zdalny URL")),
                "{:?}",
                remote.description
            );
        }

        #[test]
        fn d_asks_y_n_and_n_removes_nothing() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'j');
            ch(&mut app, 'd');
            let window = app.overlay().and_then(|overlay| overlay.confirmation()).expect("the window");
            assert!(window.heading().contains("Usuń repozytorium"));
            assert!(window.lines().iter().any(|line| line.contains("narsor")));
            assert_eq!(app.key_context(), Context::Confirm);
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());
            // The window survives a re-read of the task list.
            ch(&mut app, 'd');
            app.load(listing(vec![], "d"));
            assert!(app.overlay().is_some(), "it is not about a task of the list");
            assert_eq!(ch(&mut app, 'y'), Request::RemoveRepository("narsor".to_string()));
        }

        #[test]
        fn removing_is_blocked_without_a_connection_and_says_why() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'd');
            app.on_stream(Some(Err(ClientError::Unavailable("gone".to_string()))));
            assert_eq!(ch(&mut app, 'y'), Request::None);
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn esc_goes_back_to_the_tasks_and_q_asks_while_a_form_holds_text() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "docs-2");
            assert_eq!(app.key_context(), Context::Form);
            assert!(app.detail().has_drafts());
            press(&mut app, KeyCode::Esc);
            assert!(app.repositories().unwrap().form().is_none());
            assert!(!app.detail().has_drafts(), "the form was closed");
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("a"));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_repository_that_is_added_is_listed_and_offered_by_the_repository_field() {
            let harness = RemoteHarness::start_with(with_a_repository_input).await;
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;

            let request = ch(&mut app, 'R');
            app.perform(request).await;
            assert_eq!(app.view_kind(), ViewKind::Repositories);
            assert!(names(&app).is_empty());

            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter); // to the first field
            typed(&mut app, "narsor");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "git@github.com:acme/narsor.git");
            let request = press(&mut app, KeyCode::Enter);
            assert!(matches!(request, Request::AddRepository(_)), "{request:?}");
            app.perform(request).await;

            assert_eq!(names(&app), ["narsor"]);
            assert!(app.repositories().unwrap().form().is_none());
            assert!(note(&app).contains("dodano repozytorium narsor"), "{}", note(&app));
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("git@github.com:acme/narsor.git"), "{screen}");

            // It is in the form of a new task, too.
            press(&mut app, KeyCode::Esc);
            let request = ch(&mut app, 'n');
            app.perform(request).await;
            choose(&mut app, "implement-feature");
            let item = chooser_item(&app, "implement-feature");
            assert_eq!(
                item.fields[0].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["narsor".to_string()])
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn what_the_server_refuses_is_shown_next_to_the_field_and_the_form_stays() {
            let harness = RemoteHarness::start().await;
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'R');
            app.perform(request).await;

            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "evil");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "ext::sh -c evil");
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;

            let form = app.repositories().unwrap().form().expect("the form stays");
            let remote = &form.items()[0].fields[1];
            assert!(remote.error().is_some(), "the refusal is at the remote field");
            assert!(form.items()[0].fields[0].error().is_none());
            assert_eq!(remote.value(), "ext::sh -c evil", "what was typed stays");
            assert!(names(&app).is_empty());
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("✗"), "{screen}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn d_y_removes_the_repository_and_a_removed_one_is_gone_from_the_list() {
            let harness = RemoteHarness::start().await;
            for (id, remote) in [("docs", "git@github.com:acme/docs.git"), ("narsor", "git@github.com:acme/narsor.git")] {
                harness
                    .owner
                    .control_add_repository(&narsor::controller_protocol::CreateRepositoryRequest {
                        id: id.to_string(),
                        remote: remote.to_string(),
                        name: None,
                        provider: None,
                        default_branch: None,
                        description: None,
                    })
                    .await
                    .unwrap();
            }
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'R');
            app.perform(request).await;
            assert_eq!(names(&app), ["docs", "narsor"]);

            ch(&mut app, 'd');
            let request = ch(&mut app, 'y');
            assert_eq!(request, Request::RemoveRepository("docs".to_string()));
            app.perform(request).await;
            assert_eq!(names(&app), ["narsor"]);
            assert!(note(&app).contains("usunięto repozytorium docs"), "{}", note(&app));
            assert_eq!(harness.owner.control_repositories().await.unwrap().repositories.len(), 1);

            // Gone behind our back: said, and the list is read again.
            harness.owner.control_remove_repository("narsor").await.unwrap();
            ch(&mut app, 'd');
            let request = ch(&mut app, 'y');
            app.perform(request).await;
            assert!(names(&app).is_empty());
            assert!(note(&app).contains("już nie istnieje"), "{}", note(&app));
        }
    }
}
