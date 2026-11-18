//! The application's part of the queues view (U12): `O` opens it; the
//! keys, the reads and the writes. The state and the drawing are
//! [`crate::tui::queues`].
//!
//! * Every read is a keyed background job ([`JobKey::Open`],
//!   [`JobKey::Reload`], [`JobKey::Queue`], [`JobKey::Script`]); the panel
//!   shows the loading marker while the read of a queue runs, and an
//!   answer that comes after the view was left changes nothing.
//! * A write is awaited, like every write of the interface, and passes
//!   [`TaskApp::mutation_allowed`] first. A form the server refuses stays
//!   open with what was typed and the reason next to its field.
//! * Delete asks in the `y` / `n` window that names the observer (R33);
//!   approval shows the script text first (R32); `Esc` goes back from
//!   every level without sending anything.
//! * Script editing asks the loop to open `$EDITOR` with the terminal
//!   suspended ([`TaskApp::take_editor_request`],
//!   [`TaskApp::on_script_edited`]); with no editor set it opens a paste
//!   field (KTD-10).

use std::io;

use crossterm::event::{KeyCode, KeyEvent};

use crate::api::{ApiClient, ClientError};
use crate::controller_protocol::{
    ApproveObserverRequest, ObserverQueueListResponse, ObserverQueueView, ObserverScriptTextView,
    SetObserverScriptRequest,
};

use super::super::chooser::ChooserAction;
use super::super::keymap::{self, Action, Context};
use super::super::queues::{ApproveScript, EditorRequest, ObserverForm, QueueWrite, Queues};
use super::super::scroll::one_line;
use super::actions::{chooser_context, Confirmation, Confirmed, Overlay};
use super::background::{JobKey, OpeningKind, Outcome};
use super::notify::Severity;
use super::{failure_reason, Focus, Request, TaskApp, View};

/// The key the unsent data of a form is kept under in
/// [`DetailState::set_draft`](super::DetailState::set_draft); not a task id.
pub const DRAFT_KEY: &str = "·observer";

/// The lines a page key scrolls.
const PAGE: isize = 10;

/// Why a script version is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptPurpose {
    /// To show it before the owner approves it.
    Approve,
    /// To edit the next version from it.
    Edit,
}

impl TaskApp {
    /// The queues view, while it is open.
    pub fn queues(&self) -> Option<&Queues> {
        match &self.view {
            View::Queues(queues) => Some(queues),
            _ => None,
        }
    }

    fn queues_mut(&mut self) -> Option<&mut Queues> {
        match &mut self.view {
            View::Queues(queues) => Some(queues),
            _ => None,
        }
    }

    /// Sets the editor the script edit opens; `None` makes it a paste field.
    /// The loop sets it from `$VISUAL` / `$EDITOR`.
    pub fn set_editor(&mut self, editor: Option<String>) {
        self.editor = editor;
    }

    /// The read `O` started is back: opens the view over it.
    pub(super) fn on_queues_read(
        &mut self,
        listing: Result<ObserverQueueListResponse, ClientError>,
    ) {
        match listing {
            Ok(listing) => self.open_queues(listing),
            Err(error) => {
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "queues: the list could not be read ({})",
                        failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Opens the view over `listing`, or brings the open one up to date.
    pub fn open_queues(&mut self, listing: ObserverQueueListResponse) {
        if let Some(queues) = self.queues_mut() {
            queues.set_listing(listing);
            return;
        }
        self.enter_view(View::Queues(Box::new(Queues::new(listing))), Focus::Details);
    }

    /// Shows the read of one queue, as the loop does when the read `Enter`
    /// started is back.
    pub fn show_queue(&mut self, view: ObserverQueueView) {
        if let Some(queues) = self.queues_mut() {
            queues.show_queue(view);
        }
    }

    /// Opens the approval screen over `script` of the selected observer, as
    /// the loop does when the read `v` started is back.
    pub fn open_approve(
        &mut self,
        observer: &str,
        approved: Option<u32>,
        script: ObserverScriptTextView,
    ) {
        if let Some(queues) = self.queues_mut() {
            queues.open_approve(ApproveScript::new(observer, approved, script));
        }
    }

    /// The context whose keys apply while the view is open.
    pub(super) fn queues_context(&self) -> Option<Context> {
        let queues = self.queues()?;
        Some(if queues.approve().is_some() {
            Context::QueueApprove
        } else if let Some(form) = queues.form() {
            chooser_context(form.chooser())
        } else if queues.detail().is_some() || queues.opening().is_some() {
            Context::QueueDetail
        } else {
            Context::Queues
        })
    }

    /// Whether the view's form asks to confirm its send.
    pub(super) fn queues_confirming(&self) -> bool {
        self.queues()
            .and_then(Queues::form)
            .is_some_and(|form| form.chooser().is_confirming())
    }

    /// A key while the view has the keys.
    pub(super) fn on_queues_key(&mut self, key: KeyEvent) -> Request {
        let Some(queues) = self.queues() else {
            return Request::None;
        };
        let modal = queues.form().is_some() || queues.approve().is_some();
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && !modal {
            self.focus = Focus::List;
            return Request::None;
        }
        let request = if queues.approve().is_some() {
            self.on_approve_key(key)
        } else if queues.form().is_some() {
            self.on_queue_form_key(key)
        } else if queues.detail().is_some() || queues.opening().is_some() {
            self.on_queue_detail_key(key)
        } else {
            self.on_queue_list_key(key)
        };
        self.sync_observer_draft();
        request
    }

    /// A paste while the view has the keys: into the field of the form being
    /// filled, if there is one.
    pub(super) fn on_queues_paste(&mut self, pasted: &str) {
        let pasted_in = self
            .queues_mut()
            .and_then(Queues::form_mut)
            .is_some_and(|form| form.chooser_mut().paste(pasted));
        if pasted_in {
            self.sync_observer_draft();
        }
    }

    fn sync_observer_draft(&mut self) {
        let text = self.queues().map(Queues::draft_text).unwrap_or_default();
        self.detail.set_draft(DRAFT_KEY, text);
    }

    fn on_queue_list_key(&mut self, key: KeyEvent) -> Request {
        let Some(action) = keymap::lookup(Context::Queues, &key) else {
            return Request::None;
        };
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(),
            Action::Back => self.close_view(),
            Action::Open => {
                if let Some(name) = self
                    .queues()
                    .and_then(Queues::selected_queue)
                    .map(|queue| queue.queue.clone())
                {
                    self.open_queue(name);
                }
            }
            Action::AddObserver => {
                if let Some(queues) = self.queues_mut() {
                    queues.open_form(ObserverForm::create(None, None));
                }
            }
            Action::Down | Action::Up | Action::Pick | Action::PageDown | Action::PageUp => {
                if let Some(queues) = self.queues_mut() {
                    // The chooser reads the same keys of the panel's context.
                    queues.chooser_mut().on_key(key);
                }
            }
            _ => {}
        }
        Request::None
    }

    fn on_queue_detail_key(&mut self, key: KeyEvent) -> Request {
        let Some(action) = keymap::lookup(Context::QueueDetail, &key) else {
            return Request::None;
        };
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(),
            Action::Back => {
                self.background
                    .abort_where(|key| matches!(key, JobKey::Queue(_) | JobKey::Script(_)));
                if let Some(queues) = self.queues_mut() {
                    queues.close_queue();
                }
            }
            Action::AddObserver => {
                let shown = self
                    .queues()
                    .and_then(Queues::detail)
                    .map(|detail| (detail.view().queue.clone(), detail.view().mode.clone()));
                if let (Some((queue, mode)), Some(queues)) = (shown, self.queues_mut()) {
                    queues.open_form(ObserverForm::create(Some(&queue), mode.as_deref()));
                }
            }
            Action::EditObserver => {
                if let Some(observer) = self.selected_observer_or_warn("edit") {
                    if let Some(queues) = self.queues_mut() {
                        queues.open_form(ObserverForm::edit(&observer));
                    }
                }
            }
            Action::EditScript => self.start_script_read(ScriptPurpose::Edit),
            Action::ApproveScript => self.start_script_read(ScriptPurpose::Approve),
            Action::PauseObserver => {
                if let Some(observer) = self.selected_observer_or_warn("pause") {
                    return self.queue_request(QueueWrite::Pause(observer.name));
                }
            }
            Action::ResumeObserver => {
                if let Some(observer) = self.selected_observer_or_warn("resume") {
                    return self.queue_request(QueueWrite::Resume(observer.name));
                }
            }
            Action::DeleteObserver => self.ask_delete_observer(),
            Action::Down | Action::Up | Action::Pick => {
                if let Some(detail) = self.queues_mut().and_then(Queues::detail_mut) {
                    detail.observers_mut().on_key(key);
                }
            }
            Action::PageDown | Action::PageUp => {
                let delta = if action == Action::PageDown {
                    PAGE
                } else {
                    -PAGE
                };
                if let Some(detail) = self.queues_mut().and_then(Queues::detail_mut) {
                    detail.scroll_entries(delta);
                }
            }
            _ => {}
        }
        Request::None
    }

    fn on_approve_key(&mut self, key: KeyEvent) -> Request {
        let Some(action) = keymap::lookup(Context::QueueApprove, &key) else {
            return Request::None;
        };
        let scroll = |app: &mut TaskApp, delta: isize| {
            if let Some(approve) = app.queues_mut().and_then(Queues::approve_mut) {
                approve.scroll(delta);
            }
        };
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(),
            Action::Accept => {
                let Some(approve) = self.queues().and_then(Queues::approve).cloned() else {
                    return Request::None;
                };
                if let Some(queues) = self.queues_mut() {
                    queues.close_approve();
                }
                return self.queue_request(QueueWrite::Approve {
                    observer: approve.observer.clone(),
                    version: approve.version(),
                });
            }
            Action::Cancel => {
                if let Some(queues) = self.queues_mut() {
                    queues.close_approve();
                }
            }
            Action::Down => scroll(self, 1),
            Action::Up => scroll(self, -1),
            Action::PageDown => scroll(self, PAGE),
            Action::PageUp => scroll(self, -PAGE),
            _ => {}
        }
        Request::None
    }

    fn on_queue_form_key(&mut self, key: KeyEvent) -> Request {
        let Some(typing) = self
            .queues()
            .and_then(Queues::form)
            .map(|form| form.chooser().is_typing())
        else {
            return Request::None;
        };
        if !typing {
            match keymap::lookup(Context::Panel, &key) {
                Some(Action::Help) => {
                    self.help_open = true;
                    return Request::None;
                }
                Some(Action::Quit) => {
                    self.on_quit();
                    return Request::None;
                }
                _ => {}
            }
        }
        let Some(form) = self.queues_mut().and_then(Queues::form_mut) else {
            return Request::None;
        };
        match form.chooser_mut().on_key(key) {
            ChooserAction::None => Request::None,
            ChooserAction::Cancel => {
                // What was typed is not kept: `Esc` leaves the form.
                if let Some(queues) = self.queues_mut() {
                    queues.close_form();
                }
                Request::None
            }
            ChooserAction::Blocked(reason) => {
                self.notify(Severity::Warning, one_line(&reason));
                Request::None
            }
            ChooserAction::Submit(submission) => {
                let Some(form) = self.queues_mut().and_then(Queues::form_mut) else {
                    return Request::None;
                };
                match form.write_of(&submission) {
                    Ok(write) => self.queue_request(write),
                    Err((field, reason)) => {
                        form.show_error(field, &reason);
                        Request::None
                    }
                }
            }
        }
    }

    /// The request of `write`, once the connection allows it.
    fn queue_request(&mut self, write: QueueWrite) -> Request {
        if !self.mutation_allowed() {
            return Request::None;
        }
        Request::QueueWrite(Box::new(write))
    }

    /// The highlighted observer of the open queue; otherwise the bar says
    /// there is none to `verb`.
    fn selected_observer_or_warn(
        &mut self,
        verb: &str,
    ) -> Option<crate::controller_protocol::ObserverView> {
        let observer = self
            .queues()
            .and_then(Queues::detail)
            .and_then(|detail| detail.selected_observer())
            .cloned();
        if observer.is_none() {
            self.notify(
                Severity::Warning,
                format!("{verb}: no observer is selected"),
            );
        }
        observer
    }

    /// `d`: asks before deleting the highlighted observer, naming it (R33).
    fn ask_delete_observer(&mut self) {
        let Some(observer) = self.selected_observer_or_warn("delete") else {
            return;
        };
        let name = one_line(&observer.name);
        self.overlay = Some(Overlay::Confirm(Confirmation::new(
            observer.name.clone(),
            format!("Delete observer {name} !"),
            vec![
                format!("Deletes observer {name} with its scripts and run errors."),
                format!(
                    "When no other observer serves queue {}, its entries go too.",
                    one_line(&observer.queue)
                ),
                "The tasks it created stay.".to_string(),
                "! This cannot be undone.".to_string(),
            ],
            Confirmed::DeleteObserver(observer.name),
        )));
    }

    /// `Enter` on a queue: shows it with the loading marker and starts its
    /// read.
    fn open_queue(&mut self, name: String) {
        if let Some(queues) = self.queues_mut() {
            queues.set_opening(Some(name.clone()));
        }
        self.start_queue_read(name);
    }

    fn start_queue_read(&mut self, name: String) {
        let client = self.client.clone();
        self.background
            .start(JobKey::Queue(name.clone()), async move {
                let read = client.observer_queue(&name).await;
                Outcome::new(move |app: &mut TaskApp| app.on_queue_read(&name, read))
            });
    }

    /// The read of a queue is back: shown when the view still waits for it
    /// or shows it.
    fn on_queue_read(&mut self, name: &str, read: Result<ObserverQueueView, ClientError>) {
        let Some(queues) = self.queues() else {
            return;
        };
        let opening = queues.opening() == Some(name);
        let shown = queues
            .detail()
            .is_some_and(|detail| detail.view().queue == name);
        if !opening && !shown {
            return;
        }
        let Some(queues) = self.queues_mut() else {
            return;
        };
        match read {
            Ok(view) => queues.show_queue(view),
            Err(ClientError::NotFound) => {
                // Its last observer and its entries are gone.
                queues.close_queue();
                self.notify(
                    Severity::Warning,
                    format!("queue {} is gone", one_line(name)),
                );
                self.reload_queues();
            }
            Err(error) => {
                if opening {
                    queues.set_opening(None);
                }
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "queue {}: could not be read ({})",
                        one_line(name),
                        failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Starts the reads that bring the open view up to date: the list, and
    /// the queue shown.
    pub(super) fn reload_queues(&mut self) {
        let Some(queues) = self.queues() else {
            return;
        };
        let shown = queues.detail().map(|detail| detail.view().queue.clone());
        let client = self.client.clone();
        self.background
            .start(JobKey::Reload(OpeningKind::Queues), async move {
                let listing = client.observer_queues().await;
                Outcome::new(move |app: &mut TaskApp| {
                    if let (Ok(listing), Some(queues)) = (listing, app.queues_mut()) {
                        queues.set_listing(listing);
                    }
                })
            });
        if let Some(name) = shown {
            self.start_queue_read(name);
        }
    }

    /// `s` and `v`: reads the newest script version of the highlighted
    /// observer, to edit or to approve it.
    fn start_script_read(&mut self, purpose: ScriptPurpose) {
        let verb = match purpose {
            ScriptPurpose::Approve => "approve",
            ScriptPurpose::Edit => "script",
        };
        let Some(observer) = self.selected_observer_or_warn(verb) else {
            return;
        };
        let name = observer.name.clone();
        let Some(version) = observer.latest_version else {
            match purpose {
                ScriptPurpose::Approve => self.notify(
                    Severity::Warning,
                    format!(
                        "approve: {} has no script yet; s writes one",
                        one_line(&name)
                    ),
                ),
                ScriptPurpose::Edit => self.edit_script_text(name, None, String::new()),
            }
            return;
        };
        if purpose == ScriptPurpose::Approve && observer.approved_version == Some(version) {
            self.notify(
                Severity::Warning,
                format!(
                    "approve: version {version} of {} is approved already",
                    one_line(&name)
                ),
            );
            return;
        }
        let approved = observer.approved_version;
        let client = self.client.clone();
        self.background
            .start(JobKey::Script(name.clone()), async move {
                let read = client.observer_script(&name, version).await;
                Outcome::new(move |app: &mut TaskApp| {
                    app.on_script_read(name, approved, purpose, read)
                })
            });
    }

    fn on_script_read(
        &mut self,
        name: String,
        approved: Option<u32>,
        purpose: ScriptPurpose,
        read: Result<ObserverScriptTextView, ClientError>,
    ) {
        if self.queues().is_none() {
            return;
        }
        match read {
            Ok(script) => match purpose {
                ScriptPurpose::Approve => self.open_approve(&name, approved, script),
                ScriptPurpose::Edit => {
                    self.edit_script_text(name, Some(script.version), script.script)
                }
            },
            Err(error) => {
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "{}: the script could not be read ({})",
                        one_line(&name),
                        failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Edits the next version from `text`: in the editor when one is set,
    /// in the paste field otherwise (KTD-10).
    fn edit_script_text(&mut self, observer: String, base_version: Option<u32>, text: String) {
        let editor = self.editor.is_some();
        let Some(queues) = self.queues_mut() else {
            return;
        };
        if editor {
            queues.request_editor(EditorRequest {
                observer,
                base_version,
                text,
            });
        } else {
            let next = base_version.map_or(1, |version| version + 1);
            queues.open_form(ObserverForm::script(&observer, next, &text));
        }
    }

    /// The editor the loop must open now, with what to open it on: the loop
    /// suspends the terminal, runs it and hands what it saved to
    /// [`Self::on_script_edited`].
    pub fn take_editor_request(&mut self) -> Option<(String, EditorRequest)> {
        let editor = self.editor.clone()?;
        let request = self.queues_mut()?.take_editor()?;
        Some((editor, request))
    }

    /// What the editor saved for `request`: the next version to save, or why
    /// nothing is saved. A failed editor falls back to the paste field.
    pub fn on_script_edited(
        &mut self,
        request: EditorRequest,
        edited: io::Result<Option<String>>,
    ) -> Request {
        let name = one_line(&request.observer);
        match edited {
            Err(error) => {
                self.notify(
                    Severity::Error,
                    format!("the editor did not start ({error}); paste the script instead"),
                );
                let next = request.base_version.map_or(1, |version| version + 1);
                if let Some(queues) = self.queues_mut() {
                    queues.open_form(ObserverForm::script(&request.observer, next, &request.text));
                }
                Request::None
            }
            Ok(None) => {
                self.notify(
                    Severity::Warning,
                    format!("{name}: the editor exited with an error; nothing saved"),
                );
                Request::None
            }
            Ok(Some(text)) if text == request.text || text.trim().is_empty() => {
                self.notify(
                    Severity::Warning,
                    format!("{name}: the script is unchanged or empty; nothing saved"),
                );
                Request::None
            }
            Ok(Some(script)) => self.queue_request(QueueWrite::SaveScript {
                observer: request.observer,
                script,
            }),
        }
    }

    /// Sends `write` and takes its answer in.
    pub(super) async fn queue_write(&mut self, write: QueueWrite) {
        let result = perform(&self.client, &write).await;
        self.on_queue_written(&write, result);
    }

    /// The answer of `write`: on success the bar says what was done and the
    /// view is read again; a refused form stays open with the reason next to
    /// its field and what was typed (R31).
    pub fn on_queue_written(
        &mut self,
        write: &QueueWrite,
        result: Result<(Severity, String), ClientError>,
    ) {
        match result {
            Ok((severity, text)) => {
                if let Some(queues) = self.queues_mut() {
                    if write.is_form() {
                        queues.close_form();
                    }
                }
                self.sync_observer_draft();
                self.notify(severity, text);
                self.reload_queues();
            }
            Err(error) => {
                self.note_transport(&error);
                let refusal = match &error {
                    ClientError::Request { code, message }
                    | ClientError::Refused { code, message } => {
                        Some((code.clone(), message.clone()))
                    }
                    _ => None,
                };
                let shown = match (
                    write.is_form(),
                    self.queues_mut().and_then(Queues::form_mut),
                    &refusal,
                ) {
                    (true, Some(form), Some((code, message))) => form.show_refusal(code, message),
                    _ => false,
                };
                if !shown {
                    self.notify(
                        Severity::Error,
                        format!(
                            "{} {}: {}",
                            write.verb(),
                            one_line(write.observer()),
                            failure_reason(&error)
                        ),
                    );
                }
                if matches!(error, ClientError::NotFound) {
                    self.reload_queues();
                }
            }
        }
    }
}

/// Carries `write` out: the owner routes it needs, and what the bar says
/// when it worked.
async fn perform(
    client: &ApiClient,
    write: &QueueWrite,
) -> Result<(Severity, String), ClientError> {
    let name = one_line(write.observer());
    match write {
        QueueWrite::Create { request, script } => {
            let created = client.create_observer(request).await?;
            let Some(script) = script else {
                return Ok((
                    Severity::Success,
                    format!("created observer {name} (draft): s saves its script"),
                ));
            };
            let saved = client
                .set_observer_script(
                    &created.name,
                    &SetObserverScriptRequest {
                        script: script.clone(),
                    },
                )
                .await;
            Ok(match saved {
                Ok(saved) => (
                    Severity::Success,
                    format!(
                        "created observer {name} with script version {}: not approved, v approves it",
                        saved.version
                    ),
                ),
                Err(error) => (
                    Severity::Warning,
                    format!(
                        "created observer {name}; its script was not saved ({}): s saves it",
                        failure_reason(&error)
                    ),
                ),
            })
        }
        QueueWrite::Update { observer, request } => {
            let updated = client.update_observer(observer, request).await?;
            Ok((
                Severity::Success,
                format!(
                    "observer {name}: queue {}, mode {}",
                    one_line(&updated.queue),
                    one_line(&updated.mode)
                ),
            ))
        }
        QueueWrite::SaveScript { observer, script } => {
            let saved = client
                .set_observer_script(
                    observer,
                    &SetObserverScriptRequest {
                        script: script.clone(),
                    },
                )
                .await?;
            Ok((
                Severity::Success,
                format!(
                    "saved version {} of {name}: not approved, v approves it",
                    saved.version
                ),
            ))
        }
        QueueWrite::Approve { observer, version } => {
            client
                .approve_observer(observer, &ApproveObserverRequest { version: *version })
                .await?;
            Ok((
                Severity::Success,
                format!("approved version {version} of {name}: the schedule runs it"),
            ))
        }
        QueueWrite::Pause(observer) => {
            let paused = client.pause_observer(observer).await?;
            Ok((
                Severity::Success,
                format!("{name}: {}", one_line(&paused.state)),
            ))
        }
        QueueWrite::Resume(observer) => {
            let resumed = client.resume_observer(observer).await?;
            Ok((
                Severity::Success,
                format!("{name}: {}", one_line(&resumed.state)),
            ))
        }
        QueueWrite::Delete(observer) => {
            client.delete_observer(observer).await?;
            Ok((
                Severity::Success,
                format!("deleted observer {name} with its scripts and run errors"),
            ))
        }
    }
}
