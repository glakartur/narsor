//! The application's part of the queues view (U12, U9): `O` opens it; the
//! keys and the reads. The state and the drawing are
//! [`crate::tui::queues`].
//!
//! The view is read only: queues and automations are catalogue files, and
//! no key here writes anything.
//!
//! * Every read is a keyed background job ([`JobKey::Open`],
//!   [`JobKey::Reload`], [`JobKey::Queue`]); the panel shows the loading
//!   marker while the read of a queue runs, and an answer that comes after
//!   the view was left changes nothing.
//! * The open view is read again with the task list
//!   ([`TaskApp::start_refresh`]); `Esc` goes back from every level.

use crossterm::event::{KeyCode, KeyEvent};

use crate::api::ClientError;
use crate::controller_protocol::{ObserverQueueListResponse, ObserverQueueView};

use super::super::chooser::ListMouse;
use super::super::keymap::{self, Action, Context};
use super::super::queues::Queues;
use super::super::scroll::one_line;
use super::background::{JobKey, OpeningKind, Outcome};
use super::notify::Severity;
use super::{failure_reason, Focus, Request, TaskApp, View};

/// The lines a page key scrolls.
const PAGE: isize = 10;

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

    /// A notch or a click on the list of the queues moves or highlights the
    /// queue under the pointer; nothing is opened. An open queue does not
    /// show the list, so the list takes nothing then.
    pub(super) fn on_queues_mouse(&mut self, mouse: ListMouse, column: u16, row: u16) {
        let Some(queues) = self.queues_mut() else {
            return;
        };
        if queues.detail().is_none() && queues.opening().is_none() {
            queues.chooser_mut().on_mouse(mouse, column, row);
        }
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

    /// The context whose keys apply while the view is open.
    pub(super) fn queues_context(&self) -> Option<Context> {
        let queues = self.queues()?;
        Some(if queues.detail().is_some() || queues.opening().is_some() {
            Context::QueueDetail
        } else {
            Context::Queues
        })
    }

    /// A key while the view has the keys.
    pub(super) fn on_queues_key(&mut self, key: KeyEvent) -> Request {
        let Some(queues) = self.queues() else {
            return Request::None;
        };
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.focus = Focus::List;
            return Request::None;
        }
        if queues.detail().is_some() || queues.opening().is_some() {
            self.on_queue_detail_key(key)
        } else {
            self.on_queue_list_key(key)
        }
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
                    .abort_where(|key| matches!(key, JobKey::Queue(_)));
                if let Some(queues) = self.queues_mut() {
                    queues.close_queue();
                }
            }
            Action::Down | Action::Up | Action::Pick => {
                if let Some(detail) = self.queues_mut().and_then(Queues::detail_mut) {
                    detail.automations_mut().on_key(key);
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
                // Its file and its entries are gone.
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
}
