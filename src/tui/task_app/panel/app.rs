//! The application's part of the panel: the keys, the sends and the reads
//! that reach the panel of the selected task.

use crossterm::event::{KeyCode, KeyEvent};

use crate::api::ClientError;
use crate::controller_protocol::{
    ConductorReadResponse, OwnerCommandRequest, SubmitCommentRequest, SubmitInputRequest,
    SubmitOperatorCommandRequest, TaskSummary,
};

use crate::tui::chooser::Submission;
use crate::tui::keymap::{self, Action, Context};
use crate::tui::scroll::one_line;
use crate::tui::task_app::background::{DetailJob, JobKey, Outcome};
use crate::tui::task_app::notify::Severity;
use crate::tui::task_app::tabs::conductor;
use crate::tui::task_app::{failure_reason, Request, TaskApp};

use super::request::{AnswerRequest, PanelEffect, Submit};
use super::state::{one_line_per_row, panel_action, WaitingPanel};

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// Whether the panel has the keyboard: the focus is in the details, on
    /// the panel, and there is a panel.
    pub fn panel_focused(&self) -> bool {
        matches!(self.view, crate::tui::task_app::View::Tasks)
            && self.focus == crate::tui::task_app::Focus::Details
            && self.panel_focus
            && self.selected_panel().is_some_and(WaitingPanel::is_present)
    }

    fn selected_panel(&self) -> Option<&WaitingPanel> {
        let id = self.list.selected_id()?;
        self.detail.view(id).map(|view| &view.waiting)
    }

    /// Whether the panel's form asks the operator to confirm the send.
    pub(in crate::tui::task_app) fn panel_confirming(&self) -> bool {
        self.panel_focused()
            && self
                .selected_panel()
                .is_some_and(WaitingPanel::is_confirming)
    }

    /// Whether a key is typed into a field of the panel.
    pub(in crate::tui::task_app) fn panel_typing(&self) -> bool {
        self.panel_focused() && self.selected_panel().is_some_and(WaitingPanel::is_typing)
    }

    /// `Enter` on the list: the details of the selected task, and — when it
    /// waits — its panel unfolded with the focus on the actions.
    pub(in crate::tui::task_app) fn open_selected(&mut self) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        self.focus = crate::tui::task_app::Focus::Details;
        self.panel_focus = false;
        self.back_to_axis();
        self.sync_waiting();
        if let Some(view) = self.detail.views.get_mut(&id) {
            if view.waiting.is_present() {
                view.waiting.expand();
                self.panel_focus = true;
            }
        }
    }

    /// `Tab` in the details: to the panel, unfolding it.
    pub(in crate::tui::task_app) fn focus_panel(&mut self) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        match self.detail.views.get_mut(&id) {
            Some(view) if view.waiting.is_present() => {
                view.waiting.expand();
                self.panel_focus = true;
            }
            _ => self.notify(Severity::Warning, "to zadanie na nic nie czeka"),
        }
    }

    /// Brings every task's panel in line with what its row says it waits on,
    /// and the drafts with what the panels hold. Call it after a list was
    /// read, an event was applied, a key edited a draft.
    pub fn sync_waiting(&mut self) {
        let selected = self.list.selected_id().map(str::to_string);
        let mut lost = false;
        let mut released = false;
        let mut switched = false;
        let mut drafts = Vec::new();
        for task in self.list.tasks() {
            let id = task.task_id.as_str();
            let present = self
                .detail
                .views
                .get(id)
                .is_some_and(|view| view.waiting.is_present());
            let offers_forms = task
                .panel
                .as_ref()
                .is_some_and(|panel| !panel.forms.is_empty());
            if task.attention.is_empty() && !offers_forms && !present {
                continue;
            }
            let view = self.detail.view_mut(id);
            let positions = view.progress().positions().to_vec();
            let effect = view.waiting.sync(task, &positions);
            if selected.as_deref() == Some(id) {
                lost = effect.lost_current;
                released = effect.released;
                switched = effect.switched;
            }
            drafts.push((id.to_string(), view.draft_text()));
        }
        for (id, text) in drafts {
            self.detail.set_draft(&id, text);
        }
        if switched {
            self.notify(
                Severity::Warning,
                "agent czeka na coś nowego — formularz zmieniony, szkic zostaje",
            );
        }
        if lost && self.panel_focus {
            self.panel_focus = false;
            let text = if released {
                "oczekiwanie zniknęło — szkic został w panelu do skopiowania"
            } else {
                "oczekiwanie zniknęło (odpowiedź z innego klienta?)"
            };
            self.notify(Severity::Warning, text);
        }
    }

    /// A key while the panel has the keyboard.
    pub(in crate::tui::task_app) fn on_panel_key(&mut self, key: KeyEvent) -> Request {
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && !self.panel_confirming() {
            // Tab changes the outer panel focus. Chooser fields use arrows and
            // Enter, so leaving here never submits or discards a draft.
            self.panel_focus = false;
            return Request::None;
        }
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return Request::None;
        };
        if !self.panel_typing() {
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
        let effect = self
            .detail
            .views
            .get_mut(&id)
            .map_or(PanelEffect::None, |view| view.waiting.on_key(key));
        let request = match effect {
            PanelEffect::None => Request::None,
            PanelEffect::Collapse => {
                if let Some(view) = self.detail.views.get_mut(&id) {
                    view.waiting.collapsed = !view.waiting.is_empty();
                }
                self.panel_focus = false;
                Request::None
            }
            PanelEffect::Notice(text) => {
                self.notify(Severity::Warning, text);
                Request::None
            }
            PanelEffect::Submit(submission) => self.submit_answer(&id, &submission),
            PanelEffect::Confirm(submission) => self.ask_to_confirm(&id, &submission),
            PanelEffect::Comment(text) => self.submit_comment_text(&id, text),
            PanelEffect::Action(action) => match panel_action(action) {
                Some(action) => self.invoke(action),
                None => Request::None,
            },
        };
        self.sync_draft(&id);
        request
    }

    /// A paste while the panel has the keyboard: into the field being typed
    /// in, and nowhere else.
    pub(in crate::tui::task_app) fn on_panel_paste(&mut self, pasted: &str) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        let used = self
            .detail
            .views
            .get_mut(&id)
            .is_some_and(|view| view.waiting.paste(pasted));
        if used {
            self.sync_draft(&id);
        }
    }

    /// An action that cannot be undone: the request is prepared as for any
    /// other, but waits in the confirmation window for `y`.
    fn ask_to_confirm(&mut self, id: &str, submission: &Submission) -> Request {
        let Request::Answer(request) = self.submit_answer(id, submission) else {
            return Request::None;
        };
        if let Some(view) = self.detail.views.get_mut(id) {
            view.waiting.unprepare(&request.waiting_id);
        }
        let (label, mut lines) = self
            .selected_panel()
            .and_then(|panel| {
                // The submission counts from after action 0, when there is one.
                panel.chooser().and_then(|chooser| {
                    chooser
                        .items()
                        .get(submission.index + panel.chooser_offset())
                })
            })
            .map_or_else(
                || (submission.label.clone(), Vec::new()),
                |item| (item.label.clone(), one_line_per_row(&item.description)),
            );
        lines.push("! Tego nie da się cofnąć.".to_string());
        self.confirm_answer(request, format!("{} !", one_line(&label)), lines);
        Request::None
    }

    /// Turns a chosen action into a request, unless the connection or the
    /// action's own rules say no.
    fn submit_answer(&mut self, id: &str, submission: &Submission) -> Request {
        self.submit_with(id, |waiting, task| waiting.prepare(task, submission))
    }

    /// Turns the text of action 0 into a request, unless the connection says no.
    fn submit_comment_text(&mut self, id: &str, text: String) -> Request {
        self.submit_with(id, |waiting, task| waiting.prepare_comment(task, text))
    }

    fn submit_with(
        &mut self,
        id: &str,
        prepare: impl FnOnce(&mut WaitingPanel, &TaskSummary) -> Result<AnswerRequest, String>,
    ) -> Request {
        let refusal = self.mutation_refusal().err();
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == id)
        else {
            return Request::None;
        };
        let Some(view) = self.detail.views.get_mut(id) else {
            return Request::None;
        };
        if let Some(reason) = refusal {
            view.waiting.fail(reason);
            return Request::None;
        }
        match prepare(&mut view.waiting, task) {
            Ok(request) => Request::Answer(Box::new(request)),
            Err(message) => {
                view.waiting.fail(message);
                Request::None
            }
        }
    }

    /// Sends a chosen action. Success keeps the focus in the details on the timeline and says
    /// what happened; a refusal keeps the panel with the typed text, shows why
    /// and reads the tasks again.
    pub(in crate::tui::task_app) async fn answer(&mut self, request: AnswerRequest) {
        let AnswerRequest {
            task_id,
            waiting_id,
            title,
            generation,
            key,
            summary,
            submit,
        } = request;
        let had_slot = self.holds_slot(&task_id);
        let result = match submit {
            Submit::Input {
                pending_input_id,
                delivery,
            } => self
                .client
                .control_input(
                    &task_id,
                    &SubmitInputRequest {
                        pending_input_id,
                        idempotency_key: key,
                        delivery: *delivery,
                    },
                )
                .await
                .map(drop),
            Submit::Operator(command) => self
                .client
                .control_operator_command(
                    &task_id,
                    &SubmitOperatorCommandRequest {
                        command,
                        idempotency_key: key,
                        expected_generation: Some(generation),
                    },
                )
                .await
                .map(drop),
            Submit::Owner(command) => self
                .client
                .control_command(
                    &task_id,
                    &OwnerCommandRequest {
                        command,
                        idempotency_key: key,
                        expected_generation: Some(generation),
                    },
                )
                .await
                .map(drop),
            Submit::Comment(text) => self
                .client
                .control_comment(
                    &task_id,
                    &SubmitCommentRequest {
                        text,
                        idempotency_key: key,
                        expected_generation: Some(generation),
                    },
                )
                .await
                .map(drop),
        };
        match result {
            Ok(()) => {
                self.settle_answer(&task_id, &waiting_id, Ok(()));
                self.focus = crate::tui::task_app::Focus::Details;
                self.panel_focus = false;
                self.back_to_axis();
                // An answer to a task without a slot resumes it; the resume
                // waits when the backend is full.
                self.after_work_check(
                    &task_id,
                    had_slot,
                    Severity::Success,
                    format!("{title}: {summary}"),
                );
            }
            Err(error) => {
                let retryable = crate::tui::task_app::is_transport(&error);
                let reason = failure_reason(&error);
                self.note_transport(&error);
                self.settle_answer(&task_id, &waiting_id, Err((reason.clone(), retryable)));
                let text = format!("{title}: nie wysłano — {reason}");
                if retryable {
                    self.notify(Severity::Error, text);
                } else {
                    // The read changes the list; the reason is the last word
                    // on the bar once it is in.
                    self.notify_after_refresh(Severity::Error, text);
                }
            }
        }
    }

    fn settle_answer(
        &mut self,
        task_id: &str,
        waiting_id: &str,
        outcome: Result<(), (String, bool)>,
    ) {
        if let Some(view) = self.detail.views.get_mut(task_id) {
            view.waiting.settle(waiting_id, outcome);
        }
        self.sync_draft(task_id);
    }

    /// Starts the read of the conductor's state when the answer panel is made
    /// of it, or the *Dyrygent* tab is shown, and the task has not got it for
    /// this revision. One read serves both. The tab's chat is read from the
    /// last entry held, a page at a time, up to [`conductor::MAX_PAGES`] pages.
    pub(in crate::tui::task_app) fn start_conductor_read(&mut self, id: &str, revision: u64) {
        let shown = self.detail.tab() == crate::tui::task_app::DetailTab::Conductor;
        let Some(view) = self.detail.views.get(id) else {
            return;
        };
        let panel = view.waiting.needs_conductor(revision);
        let tab = shown
            && view
                .conductor
                .needs_read(revision, std::time::Instant::now());
        if !panel && !tab {
            return;
        }
        let first = view.conductor.last_sequence();
        let client = self.client.clone();
        let task_id = id.to_string();
        self.background.start(
            JobKey::Detail(task_id.clone(), DetailJob::Conductor),
            async move {
                let mut reads = Vec::new();
                let mut after = first;
                for _ in 0..conductor::MAX_PAGES {
                    let query =
                        crate::controller_protocol::ConductorReadQuery { after, limit: None };
                    let result = crate::tui::task_app::detail::bounded_read(
                        crate::tui::task_app::detail::RELAYED_READ_LIMIT,
                        client.control_conductor(&task_id, &query),
                    )
                    .await;
                    // The next page, when this one was read, the tab shows
                    // the chat and there is more of it.
                    let next = match &result {
                        Ok(ConductorReadResponse::Available { conductor, .. })
                            if shown && conductor.chat_has_more =>
                        {
                            conductor
                                .chat
                                .last()
                                .map(|entry| entry.sequence)
                                .filter(|last| *last > after)
                        }
                        _ => None,
                    };
                    reads.push(crate::tui::task_app::detail::DetailRead::Conductor(result));
                    match next {
                        Some(next) => after = next,
                        None => break,
                    }
                }
                Outcome::new(move |app: &mut TaskApp| {
                    for read in reads {
                        app.on_detail_read(&task_id, revision, read);
                    }
                })
            },
        );
    }

    /// Takes a read of the conductor's state in: the panel and the tab both.
    pub(in crate::tui::task_app) fn apply_conductor_read(
        &mut self,
        task_id: &str,
        revision: u64,
        result: Result<ConductorReadResponse, ClientError>,
    ) {
        let result = self.settle(result);
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
        else {
            return;
        };
        let view = self.detail.view_mut(task_id);
        view.conductor
            .apply(revision, std::time::Instant::now(), result.clone());
        let positions = view.progress().positions().to_vec();
        view.waiting
            .apply_conductor(task, &positions, revision, result);
    }

    /// The steps were read: what a waiting says about them is built again.
    pub(in crate::tui::task_app) fn reread_waiting(&mut self, task_id: &str) {
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
        else {
            return;
        };
        let view = self.detail.view_mut(task_id);
        let positions = view.progress().positions().to_vec();
        view.waiting.reread(task, &positions);
    }
}
