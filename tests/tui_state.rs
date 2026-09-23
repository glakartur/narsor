//! Tests for the TUI's state layer (U10).
//!
//! The plan's execution note is explicit: test `app.rs`, not the rendering.
//! Frame snapshots are brittle and prove little, so what is asserted here is
//! what the interface *decides* — which executions it lists, what a key press
//! does to it, which transitions it offers, what it refuses to send and what it
//! writes when the operator forces a transition. The one thing taken from the
//! rendering half is [`views::layout`], because it is pure geometry and a
//! resize is nothing but running it against a different rectangle.
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
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use orchestrator::domain::{
    Event, EventData, ExecutionStatus, HumanInputType, WorkflowDefinition, WorkflowExecution,
    Workspace,
};
use orchestrator::engine::Engine;
use orchestrator::persistence::ExecutionStore;
use orchestrator::tui::app::{
    App, FormRow, Modal, Panel, Request, StreamStatus, TransitionMode, OVERRIDE_PROVIDER,
    OVERRIDE_REASON,
};
use orchestrator::tui::views;
use orchestrator::workflow::load_from_file;

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
        let path = std::env::temp_dir().join(format!("orchestrator-tui-{}", uuid::Uuid::new_v4()));
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

/// Creates an execution the way `orchestrator run` does, definition path
/// included, without running it.
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

/// An engine over a fresh store in `root`.
fn engine_for(root: &Path) -> Arc<Engine> {
    Arc::new(Engine::new(ExecutionStore::new(root)))
}

/// Starts an execution and runs it until it stops on the `review` question.
async fn paused_at_review(engine: &Engine, root: &Path) -> String {
    let execution = start(engine, root);
    let execution = engine
        .run(&definition(), &execution.id)
        .await
        .expect("run up to the human state");
    assert_eq!(execution.status, ExecutionStatus::Paused);
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

/// The events an execution has on disk.
fn events_of(engine: &Engine, execution_id: &str) -> Vec<Event> {
    engine
        .store()
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
    let engine = engine_for(root.path());

    let paused = paused_at_review(&engine, root.path()).await;

    // A second execution, parked in a terminal status by hand, so the list has
    // two different statuses to show.
    let mut finished = start(&engine, root.path());
    finished.status = ExecutionStatus::Completed;
    finished.current_state = Some("done".to_string());
    engine.store().save_execution(&finished).expect("save");

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    assert_eq!(app.executions().len(), 2);

    let paused_row = app
        .executions()
        .iter()
        .find(|row| row.id == paused)
        .expect("the paused execution is listed");
    assert_eq!(paused_row.status, ExecutionStatus::Paused);
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
    let engine = engine_for(root.path());
    let execution_id = paused_at_review(&engine, root.path()).await;

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

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
    app.on_stream(Ok(event));

    assert_eq!(app.events().len(), backfilled + 1);
    assert_eq!(app.event_cursor(), app.events().len() - 1);
    assert!(app.follows_stream());
    assert_eq!(app.stream_status(), &StreamStatus::Live);
}

/// A paused execution opens a form built from the definition's fields.
#[tokio::test(flavor = "multi_thread")]
async fn selecting_a_paused_execution_exposes_the_input_form() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    paused_at_review(&engine, root.path()).await;

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    assert!(app.pending().is_some(), "the question is visible");
    assert_eq!(app.on_key(key(KeyCode::Char('i'))), Request::None);

    let Modal::Input(form) = app.modal() else {
        panic!("the input form is open");
    };
    assert_eq!(form.instruction, "Approve the prepared change?");

    let names: Vec<&str> = form
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    assert_eq!(names, vec!["approval_status", "comment"]);
    assert_eq!(form.fields[0].field_type, HumanInputType::Select);
    assert_eq!(form.fields[0].options, vec!["approved", "rejected"]);
    assert_eq!(form.fields[1].field_type, HumanInputType::Text);

    // The select starts on its first option, and a single select lets the label
    // be inferred, so the label row offers that as its first choice.
    assert_eq!(form.values[0], "approved");
    assert_eq!(form.label_choices.first(), Some(&None));
    assert!(form.rows().contains(&FormRow::Label));
}

/// A valid form resumes the execution.
#[tokio::test(flavor = "multi_thread")]
async fn submitting_a_valid_form_resumes_the_execution() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let execution_id = paused_at_review(&engine, root.path()).await;

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();
    app.on_key(key(KeyCode::Char('i')));

    // approval_status already reads `approved`; move to `comment` and fill it.
    app.on_key(key(KeyCode::Tab));
    type_text(&mut app, "looks good");

    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::SubmitInput);
    app.submit_input().await;

    let execution = engine
        .store()
        .load_execution(&execution_id)
        .expect("reload the execution");
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));
    assert!(!matches!(app.modal(), Modal::Input(_)), "the form closed");
    assert!(!app.notice().expect("a notice").is_error);
}

/// The offered transitions are exactly the state's `outputs`.
#[tokio::test(flavor = "multi_thread")]
async fn permitted_transitions_are_exactly_the_declared_outputs() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    paused_at_review(&engine, root.path()).await;

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    assert_eq!(app.permitted_transitions(), vec!["approved", "rejected"]);

    app.on_key(key(KeyCode::Char('t')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert_eq!(menu.mode, TransitionMode::Outputs);
    assert_eq!(menu.entries(), ["approved", "rejected"]);
}

// ---------------------------------------------------------------------------
// edge cases
// ---------------------------------------------------------------------------

/// An execution with no events is an empty stream, not a failure.
#[tokio::test(flavor = "multi_thread")]
async fn an_execution_without_events_shows_an_empty_stream() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let store = engine.store();

    // Written straight to the store, so not a single event exists for it.
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

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    assert_eq!(app.executions().len(), 1);
    assert!(app.events().is_empty());
    assert_eq!(app.event_cursor(), 0);
    assert!(app.notice().is_none(), "an empty log is not an error");
}

/// A state that does not declare `allow_manual_override` offers no override.
#[tokio::test(flavor = "multi_thread")]
async fn a_state_without_allow_manual_override_offers_no_override_mode() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let execution_id = paused_at_review(&engine, root.path()).await;

    // `prepare` is the fixture's state that deliberately does not allow it.
    let mut execution = engine
        .store()
        .load_execution(&execution_id)
        .expect("load the execution");
    execution.current_state = Some("prepare".to_string());
    engine.store().save_execution(&execution).expect("save");

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    assert!(!app.override_allowed());
    assert!(app.override_targets().is_empty());

    app.on_key(key(KeyCode::Char('t')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is open");
    };
    assert!(!menu.override_allowed);
    assert_eq!(menu.mode, TransitionMode::Outputs);

    // `o` must not switch modes for a state that forbids it.
    app.on_key(key(KeyCode::Char('o')));
    let Modal::Transition(menu) = app.modal() else {
        panic!("the transition menu is still open");
    };
    assert_eq!(menu.mode, TransitionMode::Outputs);
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

/// An empty required field is reported and nothing is sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_form_missing_a_required_field_reports_an_error_and_sends_nothing() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let execution_id = paused_at_review(&engine, root.path()).await;
    let before = events_of(&engine, &execution_id).len();

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();
    app.on_key(key(KeyCode::Char('i')));

    // `comment` is required and nothing is typed into it.
    assert_eq!(app.on_key(key(KeyCode::Enter)), Request::SubmitInput);
    app.submit_input().await;

    let Modal::Input(form) = app.modal() else {
        panic!("the form stays open so the operator can fix it");
    };
    let error = form.error.as_deref().expect("the form reports the failure");
    assert!(error.contains("comment"), "error names the field: {error}");
    assert!(app.notice().expect("a notice").is_error);

    let execution = engine
        .store()
        .load_execution(&execution_id)
        .expect("reload the execution");
    assert_eq!(execution.status, ExecutionStatus::Paused);
    assert!(
        engine.store().has_pending_input(&execution_id),
        "the question is still answerable"
    );
    assert_eq!(
        events_of(&engine, &execution_id).len(),
        before,
        "nothing was written"
    );
}

/// Losing the channel shows an indicator instead of a frozen view.
#[tokio::test(flavor = "multi_thread")]
async fn losing_the_event_channel_shows_a_disconnection_indicator() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    paused_at_review(&engine, root.path()).await;

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();
    assert_eq!(app.stream_status(), &StreamStatus::Live);

    app.on_stream(Err(RecvError::Lagged(12)));
    assert_eq!(app.stream_status(), &StreamStatus::Lagged { skipped: 12 });
    assert!(!app.stream_status().is_live());
    assert!(app.stream_status().indicator().contains("12"));
    assert!(app.notice().expect("a notice").is_error);

    app.on_stream(Err(RecvError::Closed));
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
    let engine = engine_for(root.path());
    let execution_id = paused_at_review(&engine, root.path()).await;
    let before = events_of(&engine, &execution_id).len();

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();
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
        events_of(&engine, &execution_id).len(),
        before,
        "nothing is written before the confirmation"
    );

    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Request::ApplyTransition
    );
    app.apply_transition().await;

    let events = events_of(&engine, &execution_id);
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

    let execution = engine
        .store()
        .load_execution(&execution_id)
        .expect("reload the execution");
    assert_eq!(execution.current_state.as_deref(), Some("prepare"));
    assert_eq!(execution.status, ExecutionStatus::Paused);
    assert!(
        !engine.store().has_pending_input(&execution_id),
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
    let before = orchestrator::tui::restore_count();
    orchestrator::tui::restore_terminal();
    assert_eq!(
        orchestrator::tui::restore_count(),
        before + 1,
        "the ordinary exit path restores the terminal"
    );

    orchestrator::tui::install_panic_hook();
    let before_panic = orchestrator::tui::restore_count();

    let result = panic::catch_unwind(|| panic!("a crash inside the interface"));
    assert!(result.is_err(), "the panic really happened");

    assert!(
        orchestrator::tui::restore_count() > before_panic,
        "the panic hook restored the terminal before unwinding"
    );
}

/// The whole journey, driven through the state layer only.
///
/// From an execution the way `orchestrator run` creates one, every step after
/// that is a key press or the operation a key press asks for: run the script
/// state, open the generated form, fill it in, submit, and land in a terminal
/// state. Starting a *new* workflow is not part of the TUI — that stays with
/// `orchestrator run` — so this begins where the interface begins.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_drives_the_workflow_to_a_terminal_state_from_the_interface() {
    let root = TempRoot::new();
    let engine = engine_for(root.path());
    let execution = start(&engine, root.path());

    let mut app = App::new(Arc::clone(&engine));
    app.refresh();

    assert_eq!(app.focus(), Panel::Executions);
    assert_eq!(app.selected().expect("a selection").id, execution.id);
    assert_eq!(
        app.selected().expect("a selection").status,
        ExecutionStatus::Running
    );

    // `s` — run the script state; the workflow stops on the question.
    assert_eq!(app.on_key(key(KeyCode::Char('s'))), Request::RunSelected);
    app.run_selected().await;

    assert_eq!(
        app.selected().expect("a selection").status,
        ExecutionStatus::Paused
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

    // `i` — the generated form, filled in and submitted.
    app.on_key(key(KeyCode::Char('i')));
    app.on_key(key(KeyCode::Tab));
    type_text(&mut app, "approved after review");
    app.on_key(key(KeyCode::Enter));
    app.submit_input().await;

    let execution = engine
        .store()
        .load_execution(&execution.id)
        .expect("reload the execution");
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));

    let events = events_of(&engine, &execution.id);
    assert!(events
        .iter()
        .any(|event| matches!(&event.data, EventData::HumanInputReceived { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(&event.data, EventData::WorkflowCompleted { .. })));

    // And the interface says so without being reloaded from outside.
    assert_eq!(
        app.selected().expect("a selection").status,
        ExecutionStatus::Completed
    );

    // `q` ends the session.
    app.on_key(key(KeyCode::Char('q')));
    assert!(app.should_quit());
}
