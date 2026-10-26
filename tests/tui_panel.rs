//! The panel of the conversation: the forms the server offers, `Esc` to step
//! through them, the drafts that stay, the free message to the agent.
//!
//! Tasks are built by hand with the offer of forms the server would put on
//! them (`with_panel`), and the application takes them with `TaskApp::load`.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::{json, Value};

use narsor::api::{ApiClient, ClientError};
use narsor::tui::chooser::Submission;
use narsor::tui::style::Styles;
use narsor::tui::task_app::actions::{ActionSend, Overlay};
use narsor::tui::task_app::detail::DetailRead;
use narsor::tui::task_app::notify::Severity;
use narsor::tui::task_app::panel::{Submit, WaitingPanel};
use narsor::tui::task_app::{layout, Request, TaskApp};
use narsor_engine::controller_protocol::{
    PanelChoice, PanelForm, PanelFormKind, TaskActionId, TaskSummary,
};
use narsor_engine::domain::ExecutionStatus;
use narsor_engine::orchestration::{AttentionKind, OwnerCommand};
use narsor_protocol::api::{
    PermissionDecisionKindDto, ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_DECISION_FIELD,
    ENTRY_LIMIT_RESET_COUNTER,
};
use narsor_protocol::panel::PanelContextLine;

mod task_app_support;

use task_app_support::{
    listing, task, timeline, titled, waiting_on, with_controller, with_panel, with_pending,
    with_runner,
};

fn app() -> TaskApp {
    TaskApp::new(
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
        "local",
        None,
    )
}

fn key(app: &mut TaskApp, code: KeyCode) -> Request {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn typed(app: &mut TaskApp, text: &str) {
    for character in text.chars() {
        key(app, KeyCode::Char(character));
    }
}

fn running() -> TaskSummary {
    with_runner(
        with_controller(task("a", "local", "running", 1)),
        ExecutionStatus::InputNeeded,
        "review_scope",
    )
}

fn question() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Która wersja API?",
        "fields": [{ "name": "answer", "type": "text", "required": true }],
        "continuation": "resume_agent",
        "episode": "ep-q",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn asking() -> TaskSummary {
    with_panel(with_pending(running(), question()))
}

/// An app whose only task is `task`, with the panel open on it.
fn opened(task: TaskSummary) -> TaskApp {
    let mut app = app();
    app.load(listing(vec![titled(task, "NAR-12")], "c"));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);
    app
}

fn shown(app: &TaskApp) -> Option<PanelFormKind> {
    app.detail().view("a")?.waiting().active_form()
}

fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| layout::render(frame, app, &Styles::new(true), Instant::now()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_agents_question_opens_the_answer_and_esc_goes_to_the_chat_and_back() {
    let mut app = opened(asking());
    assert!(app.panel_focused());
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert!(app.panel_focused(), "Esc does not leave the panel");

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
}

#[test]
fn the_header_says_which_form_of_how_many_and_the_key() {
    let mut app = opened(asking());
    let first = screen(&mut app, 120, 40);
    assert!(first.contains("1/2 · Esc przełącza"), "{first}");
    key(&mut app, KeyCode::Esc);
    let second = screen(&mut app, 120, 40);
    assert!(second.contains("2/2 · Esc przełącza"), "{second}");
    assert!(second.contains("Rozmowa z agentem"), "{second}");
}

fn permission_wait() -> TaskSummary {
    let mut form = question();
    form["continuation"] = json!("resolve_permission");
    form["permission"] = json!({ "denials": [{
        "tool_name": "Bash",
        "tool_use_id": "t1",
        "input": { "command": "cargo test" },
        "cwd": "/tmp/task",
        "reason_type": "asyncAgent",
        "reason": "This command requires approval."
    }] });
    form["agent_harness"] = json!("claude-code");
    form["agent_session_id"] = json!("s1");
    with_panel(with_pending(running(), form))
}

fn output_wait() -> TaskSummary {
    let mut form = question();
    form["continuation"] = json!("resolve_output");
    form["outputs"] = json!([{ "label": "go" }, { "label": "stop" }]);
    with_panel(with_pending(running(), form))
}

#[test]
fn the_header_counts_the_permission_form_and_the_chat_after_it() {
    let mut app = opened(permission_wait());
    assert_eq!(shown(&app), Some(PanelFormKind::Permission));
    let first = screen(&mut app, 120, 40);
    assert!(first.contains("1/2 · Esc przełącza"), "{first}");
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    let second = screen(&mut app, 120, 40);
    assert!(second.contains("2/2 · Esc przełącza"), "{second}");
}

#[test]
fn the_header_counts_the_action_form_after_the_chat() {
    let mut app = opened(output_wait());
    assert_eq!(shown(&app), Some(PanelFormKind::Action));
    let first = screen(&mut app, 120, 40);
    assert!(first.contains("2/2 · Esc przełącza"), "{first}");
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    let second = screen(&mut app, 120, 40);
    assert!(second.contains("1/2 · Esc przełącza"), "{second}");
}

#[test]
fn esc_in_a_text_field_switches_and_the_draft_stays() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    typed(&mut app, "pisze");
    assert!(app.detail().view("a").unwrap().waiting().is_typing());

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
    assert_eq!(
        app.detail().view("a").unwrap().waiting().chat_draft(),
        "pisze"
    );

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert_eq!(
        app.detail().view("a").unwrap().waiting().chat_draft(),
        "pisze"
    );
}

#[test]
fn a_message_in_the_chat_is_sent_as_a_comment_after_the_confirmation() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Esc);
    typed(&mut app, "sprawdź logi");
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert!(app.form_confirming(), "Enter asks before it sends");
    let Request::Answer(request) = key(&mut app, KeyCode::Enter) else {
        panic!("the confirmed message is sent");
    };
    assert_eq!(request.submit, Submit::Comment("sprawdź logi".to_string()));
}

/// The timeline read that follows every change of the row's revision.
fn timeline_read(app: &mut TaskApp) {
    app.on_detail_read(
        "a",
        1,
        DetailRead::Timeline(Ok(timeline("a", vec![], None))),
    );
}

#[test]
fn the_confirmation_of_an_answer_survives_a_read_of_the_timeline() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert!(app.form_confirming(), "Enter asks before it sends");

    timeline_read(&mut app);

    assert!(app.form_confirming(), "the read closed the confirmation");
    let Request::Answer(request) = key(&mut app, KeyCode::Enter) else {
        panic!("the first Enter in the window sends the answer");
    };
    assert!(
        matches!(request.submit, Submit::Input { .. }),
        "{:?}",
        request.submit
    );
}

#[test]
fn the_same_listing_keeps_the_focus_and_the_confirmation() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    key(&mut app, KeyCode::Enter);
    assert!(app.form_confirming());
    app.load(listing(vec![titled(asking(), "NAR-12")], "c"));
    assert!(app.form_confirming());
    assert!(app.detail().view("a").unwrap().waiting().is_confirming());
}

#[test]
fn a_refused_confirmation_says_why_in_the_panel_and_keeps_the_text() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    key(&mut app, KeyCode::Enter);
    app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));

    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);

    let drawn = screen(&mut app, 120, 40);
    assert!(
        drawn.contains("✗") && drawn.contains("zablokowana"),
        "{drawn}"
    );
    assert!(drawn.contains("v2"), "{drawn}");
}

#[test]
fn a_second_send_waits_for_the_first_to_settle() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    key(&mut app, KeyCode::Enter);
    let Request::Answer(_) = key(&mut app, KeyCode::Enter) else {
        panic!("the confirmed answer is sent");
    };
    // The answer is not settled yet: nothing is sent twice.
    key(&mut app, KeyCode::Enter);
    let again = key(&mut app, KeyCode::Enter);
    assert_eq!(again, Request::None, "the second send must wait");
}

#[test]
fn a_new_default_takes_the_view_and_the_chat_draft_stays() {
    // The operator writes in the chat of a paused task; the agent then asks.
    let paused = with_panel(with_runner(
        with_controller(task("a", "local", "stopped", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = opened(paused);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    typed(&mut app, "szkic");

    app.load(listing(vec![titled(asking(), "NAR-12")], "d"));
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert_eq!(
        app.detail().view("a").unwrap().waiting().chat_draft(),
        "szkic"
    );
}

#[test]
fn a_working_agent_has_no_panel() {
    let working = with_panel(with_runner(
        with_controller(task("a", "local", "running", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = app();
    app.load(listing(vec![titled(working, "NAR-12")], "c"));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);
    assert!(!app.panel_focused());
    assert!(!app
        .detail()
        .view("a")
        .is_some_and(|view| view.waiting().is_present()));
}

#[test]
fn with_one_form_esc_changes_nothing_and_the_panel_stays() {
    let paused = with_panel(with_runner(
        with_controller(task("a", "local", "stopped", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = opened(paused);
    assert_eq!(app.detail().view("a").unwrap().waiting().form_count(), 1);
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert!(app.panel_focused());
    assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
}

#[test]
fn tab_leaves_the_panel_and_it_stays_when_there_is_a_form() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Tab);
    assert!(!app.panel_focused());
    assert!(app.detail().view("a").unwrap().waiting().is_present());
    assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
}

#[test]
fn a_form_of_an_unknown_kind_sends_nothing_for_the_wait_behind_it() {
    let mut task = asking();
    let offer = task.panel.as_mut().expect("panel");
    offer.forms.insert(
        0,
        PanelForm {
            kind: PanelFormKind::Unrecognized,
            waiting_id: None,
            choices: Vec::new(),
            definition: None,
        },
    );
    offer.default = Some(0);
    let mut app = opened(task);
    assert_eq!(shown(&app), Some(PanelFormKind::Unrecognized));

    typed(&mut app, "x");
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);

    // The wait is still there, behind the next form.
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
}

#[test]
fn a_form_of_an_unknown_kind_that_names_a_wait_does_not_take_the_wait_entry() {
    let mut task = asking();
    let offer = task.panel.as_mut().expect("panel");
    let waiting_id = offer.forms[0]
        .waiting_id
        .clone()
        .expect("the answer's wait");
    offer.forms.insert(
        0,
        PanelForm {
            kind: PanelFormKind::Unrecognized,
            waiting_id: Some(waiting_id),
            choices: Vec::new(),
            definition: None,
        },
    );
    offer.default = Some(0);
    let mut app = opened(task);
    assert_eq!(shown(&app), Some(PanelFormKind::Unrecognized));

    typed(&mut app, "x");
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);

    // The answer form behind it still answers the wait.
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
}

#[test]
fn the_panel_shows_the_definition_of_the_server_and_not_a_copy_of_its_own() {
    let mut task = asking();
    let offer = task.panel.as_mut().expect("panel");
    let answer = offer
        .forms
        .iter_mut()
        .find(|form| form.kind == PanelFormKind::Answer)
        .expect("the answer form");
    let definition = answer.definition.as_mut().expect("the server's definition");
    definition.entries[0].label = "Etykieta z serwera".to_string();
    definition.entries[0].summary = "podsumowanie z serwera".to_string();
    definition
        .context
        .push(PanelContextLine::plain("kontekst z serwera"));

    let mut app = opened(task);
    let drawn = screen(&mut app, 120, 40);
    assert!(drawn.contains("kontekst z serwera"), "{drawn}");

    // The composer takes the answer once the operator enters its field.
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    let request = enter_until_sent(&mut app);
    assert_eq!(request.summary, "podsumowanie z serwera");
}

#[test]
fn a_form_of_an_unknown_kind_next_to_the_manual_action_form_does_not_show_its_chooser() {
    let mut task = action_task(TaskActionId::Resume, "stopped");
    let offer = task.panel.as_mut().expect("panel");
    offer.forms.insert(
        0,
        PanelForm {
            kind: PanelFormKind::Unrecognized,
            waiting_id: None,
            choices: Vec::new(),
            definition: None,
        },
    );
    offer.default = Some(0);
    let mut app = opened(task);
    assert_eq!(shown(&app), Some(PanelFormKind::Unrecognized));

    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);

    // The manual form is still there, behind the unknown one and the chat.
    for _ in 0..2 {
        key(&mut app, KeyCode::Esc);
    }
    assert_eq!(shown(&app), Some(PanelFormKind::Action));
}

#[test]
fn a_wait_the_server_gave_no_definition_for_shows_no_content() {
    // A server from before the panel field sends no forms, so nothing says
    // what the wait asks: the panel does not guess.
    let mut old = with_pending(running(), question());
    old.panel = None;
    let mut app = opened(old);
    assert_eq!(shown(&app), None);
    let drawn = screen(&mut app, 120, 40);
    assert!(
        drawn.contains("Treść oczekiwania jeszcze nie dotarła"),
        "{drawn}"
    );
}

#[test]
fn the_chat_box_grows_with_the_text_up_to_half_the_details() {
    let paused = with_panel(with_runner(
        with_controller(task("a", "local", "stopped", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = opened(paused);
    let one_line = screen(&mut app, 120, 40);
    typed(&mut app, "a");
    for _ in 0..3 {
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        typed(&mut app, "b");
    }
    let grown = screen(&mut app, 120, 40);
    let height = |screen: &str| {
        screen
            .lines()
            .position(|row| row.contains("Rozmowa z agentem"))
            .map(|top| screen.lines().count() - top)
            .expect("the chat heading")
    };
    assert!(height(&grown) > height(&one_line), "{one_line}\n{grown}");
    for _ in 0..30 {
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        typed(&mut app, "c");
    }
    let limited = screen(&mut app, 120, 40);
    assert!(
        height(&limited) <= 40 / 2 + 8,
        "the box stops at half the details:\n{limited}"
    );
}

/// Presses `Enter` until a form hands over its request: a choice, then the
/// confirmation of it.
fn enter_until_sent(app: &mut TaskApp) -> narsor::tui::task_app::panel::AnswerRequest {
    for _ in 0..4 {
        if let Request::Answer(request) = key(app, KeyCode::Enter) {
            return *request;
        }
    }
    panic!("the form sent nothing: {}", screen(app, 120, 40));
}

fn entry_limit_wait() -> TaskSummary {
    let mut form = question();
    form["continuation"] = json!("entry_limit");
    form["fields"] = json!([{
        "name": "entry_limit_decision",
        "type": "select",
        "options": ["continue", "reset_counter"],
        "required": false
    }]);
    form["outputs"] = json!([{ "label": "clean" }, { "label": "findings" }]);
    form["entry_limit"] = json!({ "entries": 5, "max_entries": 5 });
    with_panel(with_pending(running(), form))
}

#[test]
fn the_permission_form_sends_the_decision_on_its_wait() {
    let mut app = opened(permission_wait());
    assert_eq!(shown(&app), Some(PanelFormKind::Permission));
    let request = enter_until_sent(&mut app);
    assert_eq!(request.waiting_id, "ep-q");
    let Submit::Input {
        pending_input_id,
        delivery,
    } = request.submit
    else {
        panic!("the permission answers the pending input");
    };
    assert_eq!(pending_input_id, "ep-q");
    let decision = delivery.permission_decision.expect("a decision");
    assert_eq!(decision.denial_index, None, "{decision:?}");
    assert_eq!(decision.decision, PermissionDecisionKindDto::Deny);
    assert_eq!(decision.scope, None);
    assert_eq!(decision.message, None);
}

#[test]
fn the_entry_limit_form_sends_the_reset_decision_on_its_wait() {
    let mut app = opened(entry_limit_wait());
    key(&mut app, KeyCode::Down);
    let request = enter_until_sent(&mut app);
    assert_eq!(request.waiting_id, "ep-q");
    assert_eq!(request.summary, "wyzerowano licznik i kontynuowano");
    let Submit::Input { delivery, .. } = request.submit else {
        panic!("the entry limit answers the pending input");
    };
    assert_eq!(
        delivery.values.get(ENTRY_LIMIT_DECISION_FIELD),
        Some(&json!(ENTRY_LIMIT_RESET_COUNTER)),
        "{delivery:?}"
    );
}

#[test]
fn the_entry_limit_form_sends_the_decision_on_its_wait() {
    let mut app = opened(entry_limit_wait());
    assert_eq!(shown(&app), Some(PanelFormKind::EntryLimit));
    let request = enter_until_sent(&mut app);
    assert_eq!(request.waiting_id, "ep-q");
    let Submit::Input {
        pending_input_id,
        delivery,
    } = request.submit
    else {
        panic!("the entry limit answers the pending input");
    };
    assert_eq!(pending_input_id, "ep-q");
    assert_eq!(
        delivery.values.get(ENTRY_LIMIT_DECISION_FIELD),
        Some(&json!(ENTRY_LIMIT_CONTINUE)),
        "{delivery:?}"
    );
}

/// A task with a manual action form that offers `action` and nothing else.
fn action_task(action: TaskActionId, runner_state: &str) -> TaskSummary {
    let mut task = with_panel(with_runner(
        with_controller(task("a", "local", runner_state, 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let offer = task.panel.as_mut().expect("panel");
    offer.forms.push(PanelForm {
        kind: PanelFormKind::Action,
        waiting_id: None,
        choices: vec![PanelChoice {
            label: "Akcja".to_string(),
            description: None,
            action: Some(action),
        }],
        definition: None,
    });
    offer.default = Some(1);
    task
}

/// Presses `Enter` on the manual action form until it sends the action.
fn act(action: TaskActionId, runner_state: &str) -> narsor::tui::task_app::actions::ActionRequest {
    let mut app = opened(action_task(action, runner_state));
    assert_eq!(shown(&app), Some(PanelFormKind::Action));
    for _ in 0..2 {
        if let Request::Act(request) = key(&mut app, KeyCode::Enter) {
            assert_eq!(request.task_id, "a");
            return *request;
        }
    }
    panic!("the action sent nothing: {}", screen(&mut app, 120, 40));
}

#[test]
fn the_manual_action_form_sends_the_task_action() {
    let request = act(TaskActionId::Resume, "stopped");
    assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Resume));
}

// -- `prepare`: the refusals ------------------------------------------------

/// A panel that has taken `task`, as the application would.
fn synced(task: &TaskSummary) -> WaitingPanel {
    let mut panel = WaitingPanel::default();
    panel.sync(task, &[]);
    panel
}

fn choice(index: usize) -> Submission {
    Submission {
        index,
        label: String::new(),
        values: Vec::new(),
    }
}

#[test]
fn nothing_is_sent_from_a_form_that_has_no_entry() {
    let task = asking();
    let mut panel = synced(&task);
    panel.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(panel.active_form(), Some(PanelFormKind::Chat));
    assert_eq!(
        panel.prepare(&task, &choice(0)).unwrap_err(),
        "Nie ma czego wysłać."
    );
}

#[test]
fn an_action_the_owner_panel_does_not_have_is_refused() {
    let task = waiting_on(
        task("a", "local", "interrupted", 1),
        AttentionKind::ProvisioningFailed,
        "provisioning-1",
        None,
    );
    let mut panel = synced(&task);
    assert_eq!(
        panel.prepare(&task, &choice(99)).unwrap_err(),
        "Ta akcja nie istnieje."
    );
    let request = panel.prepare(&task, &choice(0)).expect("the first action");
    assert_eq!(request.submit, Submit::Owner(OwnerCommand::Start));
}

#[test]
fn an_action_the_operator_panel_does_not_have_is_refused() {
    let task = waiting_on(
        with_runner(
            with_controller(task("a", "local", "stopped", 1)),
            ExecutionStatus::Running,
            "test",
        ),
        AttentionKind::FailedStep,
        "fk1",
        Some("test"),
    );
    let mut panel = synced(&task);
    assert_eq!(
        panel.prepare(&task, &choice(99)).unwrap_err(),
        "Ta akcja nie istnieje."
    );
}

#[test]
fn a_wait_of_a_kind_the_client_does_not_know_has_no_content_to_send() {
    let task = waiting_on(
        with_runner(
            with_controller(task("a", "local", "stopped", 1)),
            ExecutionStatus::Running,
            "test",
        ),
        AttentionKind::Unrecognized,
        "w1",
        Some("test"),
    );
    let mut panel = synced(&task);
    assert_eq!(
        panel.prepare(&task, &choice(0)).unwrap_err(),
        "Treść oczekiwania jeszcze nie dotarła."
    );
}

#[test]
fn an_empty_message_is_not_sent() {
    let task = asking();
    let mut panel = synced(&task);
    assert_eq!(
        panel.prepare_comment(&task, String::new()).unwrap_err(),
        "Napisz, co agent ma zobaczyć."
    );
}

// -- the send lock, the notice of a changed form, the window of a rebuild ----

/// A task whose runner failed to provision: the owner's panel, with `Start`
/// first and the irreversible `Archive` after it.
fn provisioning_failed() -> TaskSummary {
    waiting_on(
        task("a", "local", "interrupted", 1),
        AttentionKind::ProvisioningFailed,
        "provisioning-1",
        None,
    )
}

#[test]
fn a_second_prepare_of_the_same_decision_waits_for_the_first() {
    let task = provisioning_failed();
    let mut panel = synced(&task);
    panel.prepare(&task, &choice(0)).expect("the first send");
    assert_eq!(panel.prepare(&task, &choice(0)).unwrap_err(), "Zapis trwa.");
}

#[test]
fn a_second_comment_of_the_same_text_waits_for_the_first() {
    let task = provisioning_failed();
    let mut panel = synced(&task);
    panel
        .prepare_comment(&task, "uwaga".to_string())
        .expect("the first send");
    assert_eq!(
        panel
            .prepare_comment(&task, "uwaga".to_string())
            .unwrap_err(),
        "Zapis trwa."
    );
}

/// A control plane that takes every input as queued, lists `tasks` and refuses
/// every other request with "not found".
async fn serving(tasks: Vec<TaskSummary>) -> ApiClient {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let tasks = serde_json::to_string(&listing(tasks, "after")).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tasks = tasks.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, wanted) = loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while request.len() < head_end + wanted {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                let head = String::from_utf8_lossy(&request[..head_end]).to_string();
                let first = head.lines().next().unwrap_or_default().to_string();
                let (status, body) = if first.contains("/input") {
                    (
                        "200 OK",
                        json!({
                            "input": {
                                "input_id": "in-1",
                                "pending_input_id": "ep-q",
                                "status": "queued",
                                "submitted_at": "2026-10-09T12:00:00Z",
                                "updated_at": "2026-10-09T12:00:00Z",
                                "detail": null
                            },
                            "replayed": false
                        })
                        .to_string(),
                    )
                } else if first.contains("/control/v1/tasks?") {
                    ("200 OK", tasks)
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    ApiClient::new(address, "token")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settled_send_frees_the_decision() {
    let mut app = TaskApp::new(serving(Vec::new()).await, "local", None);
    app.load(listing(vec![titled(entry_limit_wait(), "NAR-12")], "c"));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);

    let first = enter_until_sent(&mut app);
    app.perform(Request::Answer(Box::new(first))).await;

    // The answer went through: the panel no longer waits for the save.
    for _ in 0..2 {
        key(&mut app, KeyCode::Enter);
    }
    let drawn = screen(&mut app, 120, 40);
    assert!(!drawn.contains("Zapis trwa"), "{drawn}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_connection_frees_the_decision_for_a_retry() {
    // Nothing listens on the port of `app()`: the send is lost in transit.
    let mut app = opened(provisioning_failed());
    let first = enter_until_sent(&mut app);
    app.perform(Request::Answer(Box::new(first.clone()))).await;

    // The connection is back with the next read of the list.
    app.load(listing(vec![titled(provisioning_failed(), "NAR-12")], "d"));
    let retry = enter_until_sent(&mut app);
    assert_eq!(first.submit, retry.submit);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_frees_the_decision_and_the_next_send_has_a_new_key() {
    let listed = vec![titled(provisioning_failed(), "NAR-12")];
    let mut app = TaskApp::new(serving(listed.clone()).await, "local", None);
    app.load(listing(listed, "c"));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);

    let first = enter_until_sent(&mut app);
    app.perform(Request::Answer(Box::new(first.clone()))).await;

    let next = enter_until_sent(&mut app);
    assert_ne!(first.key, next.key);
}

#[test]
fn the_panel_says_the_save_is_running_while_the_first_send_is_not_settled() {
    let mut app = opened(provisioning_failed());
    enter_until_sent(&mut app);

    // The send is not settled yet: the same decision is not sent twice.
    for _ in 0..2 {
        assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    }
    let drawn = screen(&mut app, 120, 40);
    assert!(drawn.contains("Zapis trwa"), "{drawn}");
}

#[test]
fn an_irreversible_action_that_the_operator_declines_can_be_prepared_again() {
    let mut app = opened(provisioning_failed());
    key(&mut app, KeyCode::Down);
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert!(
        app.overlay().and_then(Overlay::confirmation).is_some(),
        "the irreversible action asks first"
    );

    key(&mut app, KeyCode::Char('n'));
    assert!(app.overlay().and_then(Overlay::confirmation).is_none());

    // The decision was never sent: choosing it again asks again.
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert!(
        app.overlay().and_then(Overlay::confirmation).is_some(),
        "the window opens again"
    );
    let drawn = screen(&mut app, 120, 40);
    assert!(!drawn.contains("Zapis trwa"), "{drawn}");
}

/// What the bar of notices says last.
fn note(app: &TaskApp) -> String {
    app.notifier()
        .latest()
        .map(|note| note.text.clone())
        .unwrap_or_default()
}

#[test]
fn a_new_default_form_warns_the_operator_who_was_typing() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    assert!(app.detail().view("a").unwrap().waiting().is_typing());

    app.load(listing(vec![titled(permission_wait(), "NAR-12")], "d"));

    assert_eq!(shown(&app), Some(PanelFormKind::Permission));
    assert!(note(&app).contains("formularz zmieniony"), "{}", note(&app));
    assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
}

#[test]
fn a_new_default_form_warns_the_operator_who_was_confirming() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    key(&mut app, KeyCode::Enter);
    assert!(app.form_confirming());

    app.load(listing(vec![titled(permission_wait(), "NAR-12")], "d"));

    assert!(note(&app).contains("formularz zmieniony"), "{}", note(&app));
}

/// [`permission_wait`] under another waiting identity.
fn other_permission_wait() -> TaskSummary {
    let mut form = question();
    form["episode"] = json!("ep-p2");
    form["continuation"] = json!("resolve_permission");
    form["permission"] = json!({ "denials": [{
        "tool_name": "Bash",
        "tool_use_id": "t1",
        "input": { "command": "cargo test" },
        "cwd": "/tmp/task",
        "reason_type": "asyncAgent",
        "reason": "This command requires approval."
    }] });
    form["agent_harness"] = json!("claude-code");
    form["agent_session_id"] = json!("s1");
    with_panel(with_pending(running(), form))
}

/// The panel checks who was writing in the form that is replaced, not in the
/// one that comes: the new form has another waiting identity.
#[test]
fn the_switch_is_judged_by_the_form_that_was_replaced() {
    let mut panel = synced(&asking());
    for code in [KeyCode::Enter, KeyCode::Char('v'), KeyCode::Char('2')] {
        panel.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert!(panel.is_typing());

    let effect = panel.sync(&other_permission_wait(), &[]);

    assert_eq!(panel.active_form(), Some(PanelFormKind::Permission));
    assert!(effect.switched);
}

#[test]
fn the_switch_is_not_reported_when_nobody_was_writing() {
    let mut panel = synced(&asking());

    let effect = panel.sync(&other_permission_wait(), &[]);

    assert_eq!(panel.active_form(), Some(PanelFormKind::Permission));
    assert!(!effect.switched);
}

#[test]
fn a_new_default_form_does_not_warn_the_operator_who_was_not_writing() {
    let mut app = opened(asking());

    app.load(listing(vec![titled(permission_wait(), "NAR-12")], "d"));

    assert_eq!(shown(&app), Some(PanelFormKind::Permission));
    assert!(
        !note(&app).contains("formularz zmieniony"),
        "{}",
        note(&app)
    );
}

#[test]
fn the_draft_of_a_form_stays_when_esc_switches_to_the_chat() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert!(
        !note(&app).contains("formularz zmieniony"),
        "{}",
        note(&app)
    );
    assert!(
        app.detail()
            .draft("a")
            .is_some_and(|draft| draft.contains("v2")),
        "{:?}",
        app.detail().draft("a")
    );

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
    assert!(
        app.detail()
            .draft("a")
            .is_some_and(|draft| draft.contains("v2")),
        "{:?}",
        app.detail().draft("a")
    );
}

/// A task that waits on the conductor's proposal `p1`.
fn proposal_wait() -> TaskSummary {
    waiting_on(running(), AttentionKind::ConductorProposal, "p1", None)
}

/// A conductor that holds the proposal `p1`.
fn conductor_with_proposal() -> narsor_protocol::controller::ConductorView {
    task_app_support::conductor_view(
        Vec::new(),
        json!({
            "proposals": [{
                "proposal_id": "p1",
                "text": "Uruchom krok od nowa",
                "at": "2026-10-09T12:00:00Z",
                "record": {
                    "episode": "fk1",
                    "state_id": "test",
                    "proposal": {
                        "kind": "retry",
                        "summary": "Uruchom krok od nowa",
                        "text": "z nowszym toolchainem"
                    },
                    "confidence": "high",
                    "rationale": "Błąd wygląda na przejściowy."
                },
                "state": "pending"
            }]
        }),
    )
}

#[test]
fn a_read_that_changes_the_chosen_entry_closes_the_window_and_the_panel_says_so() {
    let mut app = opened(proposal_wait());
    // The first entry after action 0, while the proposal is still being read.
    key(&mut app, KeyCode::Char('2'));
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert!(app.form_confirming(), "Enter asks before it sends");

    // The proposal comes in and the entry says something else.
    app.on_detail_read(
        "a",
        1,
        DetailRead::Conductor(Ok(
            narsor::controller_protocol::ConductorReadResponse::Available {
                conductor: Box::new(conductor_with_proposal()),
                source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
            },
        )),
    );

    assert!(!app.form_confirming(), "the window of another entry closed");
    let drawn = screen(&mut app, 120, 40);
    assert!(drawn.contains("Lista opcji"), "{drawn}");
}

#[test]
fn once_the_connection_is_back_the_first_enter_in_the_window_sends() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);
    typed(&mut app, "v2");
    key(&mut app, KeyCode::Enter);
    app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));
    assert!(app.form_confirming(), "the drop alone keeps the window");

    app.load(listing(vec![titled(asking(), "NAR-12")], "d"));
    assert!(app.form_confirming(), "the read keeps the window");

    let Request::Answer(request) = key(&mut app, KeyCode::Enter) else {
        panic!("the first Enter after the connection came back sends");
    };
    assert!(
        matches!(request.submit, Submit::Input { .. }),
        "{:?}",
        request.submit
    );
}

#[test]
fn an_empty_required_field_shows_its_error_at_the_field() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Enter);

    // Nothing is typed: the form does not send and says why at the field.
    for _ in 0..2 {
        assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    }
    assert!(!app.form_confirming());
    let drawn = screen(&mut app, 120, 40);
    assert!(drawn.contains("✗"), "{drawn}");
}
