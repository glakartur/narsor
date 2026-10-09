//! The new-task form (U13, origin §3): `n` opens it over the details; the task
//! list is not touched.
//!
//! ```text
//! │ Zadania            │ Nowe zadanie → trafi na koniec kolejki (#3)         │
//! │ ● NAR-14  edit     │┌ Akcje ─────────────────┐ ┌ Opis ──────────────────┐│
//! │?● NAR-12  approval ││ ▸ implement-feature    │ │ Pełny cykl: plan,      ││
//! │ ◷ NAR-16  #1       ││   fix-bug              │ │ implementacja, review. ││
//! │                    ││   ścieżka…             │ │                        ││
//! │                    │└────────────────────────┘ └────────────────────────┘│
//! │                    │ ticket_id (wymagane) · numer zgłoszenia             │
//! │                    │ > NAR-18▏                                           │
//! ```
//!
//! The form is a [`Chooser`]: the workflows of the catalogue are its entries
//! and the inputs a workflow declares are the fields of its entry
//! ([`crate::tui::input_form::workflow_input_fields`]), so moving to another
//! workflow and back finds what was typed where it was left. The last entry,
//! `ścieżka…`, is a workflow file on the server's disk: greyed out, with the
//! server's reason, when its capabilities say it does not read files; otherwise
//! `Enter` validates the path (every violation, as `narsor validate` prints
//! them, goes in the description) and the inputs of the definition appear.
//!
//! # Sending
//!
//! The id of the task is made when `Enter` is pressed, before the first
//! attempt ([`Outgoing`]), and every attempt sends the same request under it,
//! so the control plane — which answers a repeat of an identical request with
//! the task it created — can never make two. The row of the task is
//! [`PendingRow`](super::list::PendingRow) at the end of the list, selected:
//!
//! ```text
//! attempt 1 ── answer ─────────────────────────────▶ ◷ title #n
//!      │ lost (the server may or may not have it)
//!      ▼
//! ↻ wysyłanie…  ── attempt 2 (same id) ── answer ──▶ ◷ title #n
//!                        │ lost again
//!                        ▼
//!                  ✗ nie wysłano  ── `r` ── attempt (same id) ──▶ …
//! ```
//!
//! A refusal (a definition that does not load, an input the server does not
//! accept) is not a lost answer: the row goes away and the form comes back
//! with what was typed. Until the task is on the server what was typed counts
//! as an unsent draft ([`DetailState::set_draft`]), so `q` asks first.
//!
//! The loop's part: [`TaskApp::retry_outgoing_if_due`] sends again what is due,
//! [`TaskApp::next_outgoing_due`] says when to wake for it.
//!
//! # A workflow *path*
//!
//! Only the trusted local daemon reads files of its disk (`capabilities`),
//! and the local façade `POST /tasks` resolves and seals a file reference, so a
//! path is created through it ([`CreateHow::Path`]), always as an absolute
//! path ([`absolute_path`]); a catalogue workflow goes through
//! `POST /control/v1/tasks` ([`CreateHow::Catalogue`]).

use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::{Map, Value};

use crate::api::ClientError;
use crate::controller_protocol::{
    CreateTaskRequest, LaunchRequest, RepositoryListResponse, TaskSummary,
    ValidateWorkflowResponse, WorkflowInputView, WorkflowListResponse, WorkflowSource,
    WorkflowView,
};
use crate::domain::ParamType;

use super::super::scroll::one_line;
use super::super::chooser::{self, Chooser, ChooserAction, ChooserField, ChooserItem};
use super::super::input_form::{workflow_input_fields, workflow_input_values};
use super::super::keymap::{self, Action, Context};
use super::super::scroll::sanitize;
use super::super::style::{Role, Styles};
use super::actions::chooser_context;
use super::list::{fit, PendingRow, PendingState};
use super::notify::Severity;
use super::{is_transport, failure_reason, Focus, Request, TaskApp, View};

/// The key the unsent data of the form is kept under in
/// [`DetailState::set_draft`](super::DetailState::set_draft); not a task id.
pub const DRAFT_KEY: &str = "·nowe-zadanie";

/// How long after a lost answer the same request is sent again.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);

/// The label of the entry that takes a workflow file.
pub const PATH_LABEL: &str = "ścieżka…";

/// The field of that entry the path is typed into.
pub const PATH_FIELD: &str = "ścieżka";

/// What the entry says when the server does not read workflow files and gave
/// no reason of its own.
const PATHS_REFUSED: &str = "ten serwer nie czyta plików workflow ze swojego dysku";

// ---------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------

/// What an entry of the form creates.
#[derive(Debug, Clone)]
enum Entry {
    /// A workflow of the catalogue.
    Workflow(Box<WorkflowView>),
    /// A workflow file, typed.
    Path,
}

/// What the server said of a workflow file.
#[derive(Debug, Clone, PartialEq)]
pub enum PathVerdict {
    /// It loads: its name, its version and the inputs it declares.
    Valid {
        /// The definition's name.
        name: String,
        /// Its version.
        version: u32,
        /// Its declared inputs.
        inputs: Vec<WorkflowInputView>,
    },
    /// It does not: every violation.
    Invalid(Vec<String>),
    /// It could not be checked, for this reason.
    Failed(String),
}

/// The last path checked, and the verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct PathCheck {
    /// The absolute path that was sent.
    pub path: String,
    /// What came back.
    pub verdict: PathVerdict,
}

/// The new-task form while it is open — or remembered for the next `n`.
#[derive(Debug, Clone)]
pub struct NewTaskForm {
    chooser: Chooser,
    entries: Vec<Entry>,
    repositories: Vec<String>,
    /// `Err(reason)` when the server does not read workflow files.
    path_access: Result<(), String>,
    check: Option<PathCheck>,
    /// The id of the task last made from this form, so that its success can
    /// clear what was remembered.
    submitted: Option<String>,
}

impl NewTaskForm {
    /// A form over the catalogue. `path_access` is `Err(reason)` when the
    /// `ścieżka…` entry must be greyed out.
    pub fn new(
        workflows: &WorkflowListResponse,
        repositories: &[String],
        path_access: Result<(), String>,
    ) -> Self {
        let mut form = Self {
            chooser: Chooser::new(Vec::new()),
            entries: Vec::new(),
            repositories: repositories.to_vec(),
            path_access,
            check: None,
            submitted: None,
        };
        form.entries = workflows
            .workflows
            .iter()
            .map(|workflow| Entry::Workflow(Box::new(workflow.clone())))
            .chain(std::iter::once(Entry::Path))
            .collect();
        let first = form.first_available();
        form.chooser = Chooser::new(form.items()).selecting(first);
        form
    }

    /// The same form over a newer catalogue: what was typed stays where the
    /// entry and the field still exist.
    pub fn refreshed(
        mut self,
        workflows: &WorkflowListResponse,
        repositories: &[String],
        path_access: Result<(), String>,
    ) -> Self {
        self.entries = workflows
            .workflows
            .iter()
            .map(|workflow| Entry::Workflow(Box::new(workflow.clone())))
            .chain(std::iter::once(Entry::Path))
            .collect();
        self.repositories = repositories.to_vec();
        self.path_access = path_access;
        let items = self.items();
        self.chooser = self.chooser.rebuilt(items);
        self
    }

    /// The list, the description and the fields.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    /// What was typed and not sent, line by line.
    pub fn draft_text(&self) -> String {
        self.chooser.draft_text()
    }

    /// The path checked last, and what the server said.
    pub fn path_check(&self) -> Option<&PathCheck> {
        self.check.as_ref()
    }

    /// The repositories the `repository` fields offer.
    pub fn repositories(&self) -> &[String] {
        &self.repositories
    }

    fn first_available(&self) -> usize {
        self.entries
            .iter()
            .position(|entry| match entry {
                Entry::Workflow(workflow) => workflow.valid,
                Entry::Path => self.path_access.is_ok(),
            })
            .unwrap_or(0)
    }

    /// The entries of the chooser: the workflows, then the path.
    fn items(&self) -> Vec<ChooserItem> {
        self.entries
            .iter()
            .map(|entry| match entry {
                Entry::Workflow(workflow) => workflow_item(workflow, &self.repositories),
                Entry::Path => {
                    path_item(&self.path_access, self.check.as_ref(), &self.repositories)
                }
            })
            .collect()
    }

    /// Puts the items together again, as when a path was checked.
    fn rebuild(&mut self) {
        let items = self.items();
        let chooser = std::mem::replace(&mut self.chooser, Chooser::new(Vec::new()));
        self.chooser = chooser.rebuilt(items);
    }

    /// The index of the path entry.
    fn path_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Entry::Path))
    }
}

/// How a workflow of the catalogue reads in the list and the description.
fn workflow_item(workflow: &WorkflowView, repositories: &[String]) -> ChooserItem {
    let source = match workflow.source {
        WorkflowSource::BuiltIn => "wbudowany",
        WorkflowSource::Catalog => "z katalogu",
        WorkflowSource::User => "użytkownika",
    };
    let mut lines = Vec::new();
    if let Some(description) = workflow
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        lines.push(description.trim().to_string());
    }
    let mut facts = vec![source.to_string()];
    if let Some(version) = workflow.version {
        facts.push(format!("wersja {version}"));
    }
    if workflow.overrides {
        facts.push("przesłania inną kopię o tej nazwie".to_string());
    }
    lines.push(facts.join(" · "));
    if !workflow.inputs.is_empty() {
        lines.push(format!("Wejścia: {}", inputs_summary(&workflow.inputs)));
    }
    if !workflow.violations.is_empty() {
        lines.push(violations_text(&workflow.violations));
    }
    let item = ChooserItem::new(one_line(&workflow.name), lines.join("\n"));
    if !workflow.valid {
        let first = workflow
            .violations
            .first()
            .map(|violation| one_line(violation))
            .unwrap_or_else(|| "definicja nie jest poprawna".to_string());
        return item.unavailable(format!("definicja jest niepoprawna: {first}"));
    }
    if workflow.version.is_none() {
        return item.unavailable("definicja się nie wczytuje");
    }
    workflow_input_fields(&workflow.inputs, repositories)
        .into_iter()
        .fold(item, ChooserItem::with_field)
}

/// `ticket_id*, repository, dry_run`: the inputs, required ones starred.
fn inputs_summary(inputs: &[WorkflowInputView]) -> String {
    inputs
        .iter()
        .map(|input| {
            let star = if input.required && input.default.is_none() {
                "*"
            } else {
                ""
            };
            format!("{}{star}", one_line(&input.name))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every violation, numbered: the verdict of `narsor validate`.
fn violations_text(violations: &[String]) -> String {
    let mut text = format!(
        "✗ definicja jest niepoprawna — naruszeń: {}",
        violations.len()
    );
    for (index, violation) in violations.iter().enumerate() {
        text.push_str(&format!("\n{}. {}", index + 1, sanitize(violation)));
    }
    text
}

/// The entry that takes a workflow file.
fn path_item(
    access: &Result<(), String>,
    check: Option<&PathCheck>,
    repositories: &[String],
) -> ChooserItem {
    let mut item = ChooserItem::new(PATH_LABEL, path_description(check)).with_field(
        ChooserField::text(PATH_FIELD)
            .required()
            .described("bezwzględna ścieżka pliku .yaml na dysku serwera"),
    );
    if let Err(reason) = access {
        let reason = match reason.trim() {
            "" => PATHS_REFUSED.to_string(),
            reason => one_line(reason),
        };
        return item.unavailable(reason);
    }
    if let Some(PathCheck {
        verdict: PathVerdict::Valid { inputs, .. },
        ..
    }) = check
    {
        for field in workflow_input_fields(inputs, repositories) {
            item = item.with_field(field);
        }
    }
    item
}

fn path_description(check: Option<&PathCheck>) -> String {
    match check.map(|check| &check.verdict) {
        None => "Workflow z pliku na dysku serwera. Wpisz bezwzględną ścieżkę i naciśnij Enter: \
                 plik zostanie sprawdzony tak jak `narsor validate`, a gdy jest poprawny — \
                 pojawią się jego wejścia."
            .to_string(),
        Some(PathVerdict::Valid {
            name,
            version,
            inputs,
        }) => {
            let mut text = format!("✓ {} v{version} — definicja jest poprawna.", one_line(name));
            if !inputs.is_empty() {
                text.push_str(&format!("\nWejścia: {}", inputs_summary(inputs)));
            }
            text.push_str("\nWypełnij wejścia i naciśnij Enter, żeby utworzyć zadanie.");
            text
        }
        Some(PathVerdict::Invalid(violations)) => violations_text(violations),
        Some(PathVerdict::Failed(reason)) => {
            format!("✗ nie udało się sprawdzić pliku: {}", sanitize(reason))
        }
    }
}

/// `path` as an absolute path: `~/` is the home directory, a relative one is
/// taken from the directory the interface runs in. The server reads files from
/// its own disk and has its own idea of "here"; what is sent is never relative.
pub fn absolute_path(path: &str) -> String {
    let path = path.trim();
    let expanded = match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(path),
    };
    std::path::absolute(&expanded)
        .map(|absolute| absolute.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

/// How a task is created.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateHow {
    /// A workflow of the catalogue: `POST /control/v1/tasks`.
    Catalogue(Box<CreateTaskRequest>),
    /// A workflow file: the local façade, `POST /tasks`, which resolves and
    /// seals the file.
    Path {
        /// The absolute path of the definition.
        workflow: String,
        /// The inputs.
        inputs: Map<String, Value>,
    },
}

/// A task being created: the id it will have, made before the first attempt,
/// and what every attempt sends.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    /// The id of the task, the same in every attempt.
    pub task_id: String,
    /// What the row shows.
    pub title: String,
    /// The workflow it runs, for the details of the row.
    pub workflow: String,
    /// The inputs it is created with, for the details of the row.
    pub inputs: Map<String, Value>,
    /// What is sent.
    pub send: CreateHow,
    attempts: u8,
    retry_at: Option<Instant>,
}

impl Outgoing {
    /// How many times it was sent.
    pub fn attempts(&self) -> u8 {
        self.attempts
    }
}

/// The title a task being sent has until the server gives it one: its first
/// text input as declared — the ticket, usually — else the workflow's name.
fn title_for(workflow: &str, inputs: &[WorkflowInputView], values: &Map<String, Value>) -> String {
    inputs
        .iter()
        .filter(|input| matches!(input.input_type, ParamType::String | ParamType::Repository))
        .find_map(|input| values.get(&input.name).and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .map_or_else(|| workflow.to_string(), |text| text.trim().to_string())
}

fn new_task_id() -> String {
    format!("task-{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// The open new-task form.
    pub fn new_task_form(&self) -> Option<&NewTaskForm> {
        match &self.view {
            View::NewTask(form) => Some(form),
            _ => None,
        }
    }

    /// What was typed in the form and left with `Esc`, kept for the next `n`.
    pub fn remembered_new_task(&self) -> Option<&NewTaskForm> {
        self.new_task_memory.as_ref()
    }

    /// The tasks being created that the server has not confirmed.
    pub fn outgoing(&self) -> &[Outgoing] {
        &self.outbox
    }

    /// What a pending row will send, for its details.
    pub(super) fn send_data(&self, task_id: &str) -> Option<&Outgoing> {
        self.outbox
            .iter()
            .find(|outgoing| outgoing.task_id == task_id)
    }

    /// Why a workflow path cannot be offered, or `Ok` when it can.
    fn path_access(&self) -> Result<(), String> {
        match &self.capabilities {
            Some(capabilities) if capabilities.workflow_paths => Ok(()),
            Some(capabilities) => Err(capabilities
                .workflow_paths_reason
                .clone()
                .unwrap_or_else(|| PATHS_REFUSED.to_string())),
            None => Err("serwer nie podał jeszcze swoich możliwości".to_string()),
        }
    }

    /// `n`, after the catalogue and the repositories were read: opens the form
    /// over the details. What was typed the last time is still in it.
    pub fn open_new_task(
        &mut self,
        workflows: WorkflowListResponse,
        repositories: RepositoryListResponse,
    ) {
        let ids: Vec<String> = repositories
            .repositories
            .iter()
            .map(|repository| repository.id.clone())
            .collect();
        let access = self.path_access();
        let form = match self.new_task_memory.take() {
            Some(remembered) => remembered.refreshed(&workflows, &ids, access),
            None => NewTaskForm::new(&workflows, &ids, access),
        };
        self.show_new_task(form);
    }

    fn show_new_task(&mut self, form: NewTaskForm) {
        self.enter_view(View::NewTask(Box::new(form)), Focus::Details);
    }

    /// `n`: reads the catalogue, then opens the form. Without an answer the
    /// form of the last time opens as it was, or the bar says why not.
    pub(super) async fn read_and_open_new_task(&mut self) {
        if self.capabilities.is_none() {
            self.read_capabilities().await;
        }
        let workflows = self.client.control_workflows().await;
        let repositories = self.client.control_repositories().await;
        match workflows {
            Ok(workflows) => {
                let repositories = repositories.unwrap_or_else(|error| {
                    self.notify(
                        Severity::Warning,
                        format!(
                            "nie udało się wczytać repozytoriów ({}) — pole repository będzie tekstem",
                            failure_reason(&error)
                        ),
                    );
                    RepositoryListResponse {
                        repositories: Vec::new(),
                        unreadable: Vec::new(),
                    }
                });
                self.open_new_task(workflows, repositories);
            }
            Err(error) => {
                self.note_transport(&error);
                match self.new_task_memory.take() {
                    Some(remembered) => {
                        self.show_new_task(remembered);
                        self.notify(
                            Severity::Warning,
                            format!(
                                "katalog workflow sprzed chwili: nie udało się go wczytać ({})",
                                failure_reason(&error)
                            ),
                        );
                    }
                    None => self.notify(
                        Severity::Error,
                        format!(
                            "nowe zadanie: nie udało się wczytać katalogu workflow ({})",
                            failure_reason(&error)
                        ),
                    ),
                }
            }
        }
    }

    /// `Esc`: closes the form, remembering what was typed, and gives the
    /// details back to the task that was selected.
    fn close_new_task(&mut self) {
        let View::NewTask(form) = self.leave_view() else {
            return;
        };
        self.new_task_memory = Some(*form);
        self.sync_new_task_draft();
    }

    /// Keeps the unsent data of the form true to what the form holds, so that
    /// leaving asks first.
    fn sync_new_task_draft(&mut self) {
        let text = match &self.view {
            View::NewTask(form) => form.draft_text(),
            _ => self
                .new_task_memory
                .as_ref()
                .map(NewTaskForm::draft_text)
                .unwrap_or_default(),
        };
        self.detail.set_draft(DRAFT_KEY, text);
    }

    /// A key while the form is open: the chooser's own, then what it chose.
    pub(super) fn on_new_task_key(&mut self, key: KeyEvent, confirming_quit: bool) -> Request {
        let View::NewTask(form) = &mut self.view else {
            return Request::None;
        };
        if !form.chooser.is_typing() {
            match keymap::lookup(Context::Panel, &key) {
                Some(Action::Help) => {
                    self.help_open = true;
                    return Request::None;
                }
                Some(Action::Quit) => {
                    self.on_quit(confirming_quit);
                    return Request::None;
                }
                _ => {}
            }
        }
        let request = match form.chooser.on_key(key) {
            ChooserAction::None => Request::None,
            ChooserAction::Cancel => {
                self.close_new_task();
                Request::None
            }
            ChooserAction::Blocked(reason) => {
                self.notify(Severity::Warning, one_line(&reason));
                Request::None
            }
            ChooserAction::Submit(submission) => self.submit_new_task(&submission),
        };
        self.sync_new_task_draft();
        request
    }

    /// The context whose keys apply while the form is open.
    pub(super) fn new_task_context(&self) -> Option<Context> {
        match &self.view {
            View::NewTask(form) => Some(chooser_context(&form.chooser)),
            _ => None,
        }
    }

    /// `Enter` on an entry that passed the chooser's own checks. A path that
    /// was not checked yet is checked first; anything else is sent.
    fn submit_new_task(&mut self, submission: &chooser::Submission) -> Request {
        let View::NewTask(form) = &mut self.view else {
            return Request::None;
        };
        let Some(entry) = form.entries.get(submission.index).cloned() else {
            return Request::None;
        };
        let (workflow, declared, how_for) = match entry {
            Entry::Workflow(workflow) => {
                let Some(version) = workflow.version else {
                    return Request::None;
                };
                let name = workflow.name.clone();
                let declared = workflow.inputs.clone();
                (name.clone(), declared, Sending::Catalogue { name, version })
            }
            Entry::Path => {
                let typed = submission
                    .values
                    .iter()
                    .find(|(name, _)| name == PATH_FIELD)
                    .map(|(_, value)| absolute_path(value))
                    .unwrap_or_default();
                match &form.check {
                    Some(PathCheck {
                        path,
                        verdict: PathVerdict::Valid { name, inputs, .. },
                    }) if *path == typed => {
                        (name.clone(), inputs.clone(), Sending::Path { path: typed })
                    }
                    _ => return Request::ValidatePath(typed),
                }
            }
        };
        let inputs = match workflow_input_values(&declared, &submission.values) {
            Ok(inputs) => inputs,
            Err(problem) => {
                form.chooser
                    .set_field_error(submission.index, &problem.field, problem.message);
                return Request::None;
            }
        };
        if !self.mutation_allowed() {
            return Request::None;
        }
        let title = title_for(&workflow, &declared, &inputs);
        let task_id = new_task_id();
        let send = match how_for {
            Sending::Catalogue { name, version } => {
                CreateHow::Catalogue(Box::new(CreateTaskRequest {
                    task_id: Some(task_id.clone()),
                    backend_id: None,
                    launch: LaunchRequest {
                        workflow_id: name,
                        workflow_version: version,
                        inputs: Value::Object(inputs.clone()),
                        workspace: None,
                        profile: None,
                        harness: None,
                        resources: None,
                    },
                }))
            }
            Sending::Path { path } => CreateHow::Path {
                workflow: path,
                inputs: inputs.clone(),
            },
        };
        self.begin_sending(Outgoing {
            task_id,
            title,
            workflow,
            inputs,
            send,
            attempts: 0,
            retry_at: None,
        })
    }

    /// The form leaves for the task list with its row at the end, selected;
    /// what was typed is kept until the server has the task.
    fn begin_sending(&mut self, outgoing: Outgoing) -> Request {
        let id = outgoing.task_id.clone();
        if let View::NewTask(mut form) = self.leave_view() {
            form.submitted = Some(id.clone());
            self.new_task_memory = Some(*form);
        }
        self.list.push_pending(PendingRow {
            task_id: id.clone(),
            title: outgoing.title.clone(),
            state: PendingState::Sending,
            reason: None,
        });
        self.detail
            .set_draft(&id, format!("wysyłanie: {}", outgoing.title));
        self.outbox.push(outgoing);
        self.sync_new_task_draft();
        Request::SendTask(id)
    }

    /// One attempt to create the task `id`, with the id and the request it was
    /// made with.
    pub(super) async fn send_task(&mut self, id: &str) {
        let Some(outgoing) = self
            .outbox
            .iter()
            .find(|outgoing| outgoing.task_id == id)
            .cloned()
        else {
            return;
        };
        let result = match &outgoing.send {
            CreateHow::Catalogue(request) => self.client.create_control_task(request).await,
            CreateHow::Path { workflow, inputs } => self
                .client
                .start_task(&outgoing.task_id, workflow, inputs.clone())
                .await
                .map(|started| started.task),
        };
        match result {
            Ok(task) => self.task_sent(&outgoing, task),
            Err(error) if is_transport(&error) => self.task_lost(id, &error),
            Err(error) => self.task_refused(&outgoing, &error),
        }
    }

    /// The server has the task: its row is the server's, at the end of the
    /// list, selected, and what was kept for the form is not needed.
    fn task_sent(&mut self, outgoing: &Outgoing, task: TaskSummary) {
        self.outbox
            .retain(|known| known.task_id != outgoing.task_id);
        self.detail.set_draft(&outgoing.task_id, "");
        self.list.append(task);
        if self
            .new_task_memory
            .as_ref()
            .is_some_and(|form| form.submitted.as_deref() == Some(outgoing.task_id.as_str()))
        {
            self.new_task_memory = None;
            self.sync_new_task_draft();
        }
        let place = self
            .list
            .queue_position(&outgoing.task_id)
            .map_or_else(String::new, |place| format!(", #{place} w kolejce"));
        self.notify(
            Severity::Success,
            format!("{}: zadanie utworzone{place}", one_line(&outgoing.title)),
        );
    }

    /// Forgets what a read of the list proved the server has: a task whose
    /// answer was lost is on the list, so there is nothing to send again and
    /// nothing unsent to protect.
    pub(super) fn settle_outbox(&mut self) {
        let settled: Vec<String> = self
            .outbox
            .iter()
            .filter(|outgoing| {
                self.list
                    .tasks()
                    .iter()
                    .any(|task| task.task_id.as_str() == outgoing.task_id)
            })
            .map(|outgoing| outgoing.task_id.clone())
            .collect();
        for id in settled {
            self.outbox.retain(|outgoing| outgoing.task_id != id);
            self.detail.set_draft(&id, "");
            if self
                .new_task_memory
                .as_ref()
                .is_some_and(|form| form.submitted.as_deref() == Some(id.as_str()))
            {
                self.new_task_memory = None;
            }
        }
        self.sync_new_task_draft();
    }

    /// The answer did not come: the server may have the task or not. The first
    /// time the same request is sent again soon; the second time it stays
    /// unsent, with its data, for the operator to send again.
    fn task_lost(&mut self, id: &str, error: &ClientError) {
        self.note_transport(error);
        let now = Instant::now();
        let reason = failure_reason(error);
        let Some(outgoing) = self
            .outbox
            .iter_mut()
            .find(|outgoing| outgoing.task_id == id)
        else {
            return;
        };
        outgoing.attempts = outgoing.attempts.saturating_add(1);
        let title = one_line(&outgoing.title);
        if outgoing.attempts < 2 {
            outgoing.retry_at = Some(now + RETRY_AFTER);
            self.list.set_pending(id, PendingState::Sending, None);
            self.notify(
                Severity::Warning,
                format!("{title}: brak odpowiedzi ({reason}) — ponawiam z tym samym id"),
            );
        } else {
            outgoing.retry_at = None;
            self.list
                .set_pending(id, PendingState::NotSent, Some(reason.clone()));
            let key = keymap::key_of(Context::List, Action::RetrySend).unwrap_or_default();
            self.notify(
                Severity::Error,
                format!("{title}: nie wysłano ({reason}) — {key} ponawia z tym samym id"),
            );
        }
    }

    /// The server said no — not a lost answer: the row goes, the form comes
    /// back as it was.
    fn task_refused(&mut self, outgoing: &Outgoing, error: &ClientError) {
        self.outbox
            .retain(|known| known.task_id != outgoing.task_id);
        self.list.remove_pending(&outgoing.task_id);
        self.detail.set_draft(&outgoing.task_id, "");
        if let Some(form) = self.new_task_memory.take() {
            self.show_new_task(form);
            self.sync_new_task_draft();
        }
        self.notify(
            Severity::Error,
            format!(
                "{}: serwer odrzucił zadanie — {}",
                one_line(&outgoing.title),
                super::actions::refusal_reason(error)
            ),
        );
    }

    /// Sends again what a lost answer left waiting, once its moment came.
    /// Returns whether anything was sent.
    pub async fn retry_outgoing_if_due(&mut self, now: Instant) -> bool {
        let due: Vec<String> = self
            .outbox
            .iter()
            .filter(|outgoing| outgoing.retry_at.is_some_and(|at| at <= now))
            .map(|outgoing| outgoing.task_id.clone())
            .collect();
        for id in &due {
            if let Some(outgoing) = self
                .outbox
                .iter_mut()
                .find(|outgoing| outgoing.task_id == *id)
            {
                outgoing.retry_at = None;
            }
            self.send_task(id).await;
        }
        !due.is_empty()
    }

    /// When the next automatic attempt is due.
    pub fn next_outgoing_due(&self) -> Option<Instant> {
        self.outbox
            .iter()
            .filter_map(|outgoing| outgoing.retry_at)
            .min()
    }

    /// `r`: sends the selected task again, with its id, when it did not get
    /// through.
    pub(super) fn retry_selected(&mut self) -> Request {
        let Some(row) = self.list.selected_pending().cloned() else {
            self.notify(
                Severity::Warning,
                "ponów: zaznaczone zadanie jest na serwerze — nie ma czego ponawiać",
            );
            return Request::None;
        };
        if row.state == PendingState::Sending {
            self.notify(
                Severity::Warning,
                format!("{}: wysyłanie trwa", one_line(&row.title)),
            );
            return Request::None;
        }
        if !self.mutation_allowed() {
            return Request::None;
        }
        if let Some(outgoing) = self
            .outbox
            .iter_mut()
            .find(|outgoing| outgoing.task_id == row.task_id)
        {
            // One attempt by hand: if it is lost, the row stays unsent.
            outgoing.attempts = 1;
            outgoing.retry_at = None;
        }
        self.list
            .set_pending(&row.task_id, PendingState::Sending, None);
        Request::SendTask(row.task_id)
    }

    /// Asks the server for the verdict on a workflow file and shows it.
    pub(super) async fn validate_path(&mut self, path: &str) {
        let result = self.client.control_validate_workflow(path).await;
        self.on_path_checked(path, result);
    }

    /// Takes the verdict on `path` in: the full list of violations goes to the
    /// description of the entry, a definition that loads brings its inputs.
    pub fn on_path_checked(
        &mut self,
        path: &str,
        result: Result<ValidateWorkflowResponse, ClientError>,
    ) {
        if let Err(error) = &result {
            self.note_transport(error);
        }
        let View::NewTask(form) = &mut self.view else {
            return;
        };
        let verdict = match result {
            Ok(ValidateWorkflowResponse {
                valid: true,
                name,
                version,
                inputs,
                ..
            }) => match (name, version) {
                (Some(name), Some(version)) => PathVerdict::Valid {
                    name,
                    version,
                    inputs,
                },
                _ => PathVerdict::Failed("serwer nie podał nazwy ani wersji definicji".to_string()),
            },
            Ok(response) => PathVerdict::Invalid(response.violations),
            Err(error) => PathVerdict::Failed(failure_reason(&error)),
        };
        let complaint = match &verdict {
            PathVerdict::Valid { .. } => None,
            PathVerdict::Invalid(violations) => Some(format!(
                "definicja jest niepoprawna — naruszeń: {} (pełna lista w opisie)",
                violations.len()
            )),
            PathVerdict::Failed(reason) => Some(one_line(reason)),
        };
        form.check = Some(PathCheck {
            path: path.to_string(),
            verdict,
        });
        form.rebuild();
        if let (Some(complaint), Some(index)) = (complaint, form.path_index()) {
            form.chooser.set_field_error(index, PATH_FIELD, complaint);
        }
        self.sync_new_task_draft();
    }
}

/// How a task is created, before its id exists.
enum Sending {
    Catalogue { name: String, version: u32 },
    Path { path: String },
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Draws the form into `area`: where the task will land, then the chooser.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    form: &mut NewTaskForm,
    place: usize,
    styles: &Styles,
) {
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    let line = Line::from(vec![
        Span::styled("Nowe zadanie", styles.of(Role::Focus)),
        Span::styled(
            format!(" → trafi na koniec kolejki (#{place})"),
            styles.of(Role::Done),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), header);
    chooser::render(frame, body, &mut form.chooser, styles);
}

/// The details of a task that is being sent or was not: where it stands, what
/// is kept and how to send it again.
pub fn render_pending(
    frame: &mut Frame,
    area: Rect,
    row: &PendingRow,
    outgoing: Option<&Outgoing>,
    styles: &Styles,
) {
    let text = row.text();
    let role = text.icon.role();
    let mut lines = vec![
        Line::styled(
            fit(&one_line(&row.title), usize::from(area.width)),
            styles.strong(Role::Calm),
        ),
        Line::from(vec![
            Span::styled(
                format!("{} {}", text.icon.glyph(), text.label),
                styles.of(role),
            ),
            Span::styled(format!(" · {}", row.task_id), styles.of(Role::Done)),
        ]),
        Line::raw(""),
    ];
    match row.state {
        PendingState::Sending => lines.push(Line::raw(
            "Serwer jeszcze nie potwierdził zadania. Wysyłam je ponownie z tym samym id, więc \
             nie powstanie drugie.",
        )),
        PendingState::NotSent => {
            let reason = row
                .reason
                .as_deref()
                .map(one_line)
                .unwrap_or_else(|| "brak odpowiedzi".to_string());
            lines.push(Line::styled(
                format!("Nie wysłano: {reason}"),
                styles.of(Role::Problem),
            ));
            let key = keymap::key_of(Context::List, Action::RetrySend).unwrap_or_default();
            lines.push(Line::raw(format!(
                "Dane zostały zachowane. {key} wysyła zadanie jeszcze raz z tym samym id — jeśli \
                 serwer je już ma, nie powstanie drugie."
            )));
        }
    }
    if let Some(outgoing) = outgoing {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("workflow  ", styles.of(Role::Done)),
            Span::raw(one_line(&outgoing.workflow)),
        ]));
        lines.push(Line::styled("wejścia", styles.of(Role::Done)));
        if outgoing.inputs.is_empty() {
            lines.push(Line::styled("  brak", styles.of(Role::Done)));
        }
        for (name, value) in &outgoing.inputs {
            let value = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            lines.push(Line::raw(format!(
                "  {} = {}",
                one_line(name),
                one_line(&value)
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}
