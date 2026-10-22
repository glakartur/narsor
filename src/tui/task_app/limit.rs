//! The limit of concurrent tasks of a backend, changed in the running daemon.
//!
//! `L`, from the list or the details, reads the limit of the backend of the
//! selected task and opens a list: one more, one less, or back to the value
//! of the backend file. The daemon holds the number and computes every
//! figure the list shows; the interface adds and subtracts nothing but the
//! step of one it offers. Everything is the same the CLI says with
//! `narsor backend limit`. The ad hoc value lasts until the daemon restarts.
//!
//! A lower limit does not stop work at once. The daemon asks running tasks
//! to stop at a safe point, and only as many stop as the limit is exceeded
//! by. The list says how many still wait.

use crossterm::event::KeyEvent;
use narsor_protocol::controller::{
    BackendLimitSource, BackendLimitView, MAX_CONCURRENT_TASKS_LIMIT,
};

use crate::api::ClientError;

use super::super::chooser::{Chooser, ChooserAction, ChooserItem};
use super::super::scroll::one_line;
use super::actions::Overlay;
use super::background::{JobKey, Outcome};
use super::notify::Severity;
use super::{failure_reason, Request, TaskApp};

/// Most concurrent tasks the daemon accepts; the daemon checks it again.
const MOST: u32 = MAX_CONCURRENT_TASKS_LIMIT;

/// Why the entry cannot be chosen.
const AT_MOST: &str = "limit jest już największy";
const AT_LEAST: &str = "limit jest już najmniejszy";
const NO_OVERRIDE: &str = "nie ma nadpisania do usunięcia";

/// One entry of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// One more than now.
    Raise,
    /// One less than now.
    Lower,
    /// The ad hoc value goes; the file's applies.
    File,
}

/// The list `L` opens, over the limit of one backend.
#[derive(Debug, Clone)]
pub struct LimitList {
    backend: String,
    view: BackendLimitView,
    chooser: Chooser,
}

impl LimitList {
    /// The list for `view`.
    pub fn new(view: BackendLimitView) -> Self {
        let backend = one_line(&view.backend_id);
        let state = status(&view);
        let items = vec![
            {
                let item = ChooserItem::new(
                    format!("  podnieś o 1 (do {})", view.effective.saturating_add(1)),
                    format!(
                        "{state} Wyższy limit uruchamia czekające zadania od razu; działających \
                         nie rusza."
                    ),
                );
                if view.effective >= MOST {
                    item.unavailable(AT_MOST)
                } else {
                    item
                }
            },
            {
                let item = ChooserItem::new(
                    format!("  obniż o 1 (do {})", view.effective.saturating_sub(1)),
                    format!(
                        "{state} Niższy limit wstrzymuje nowe starty. Działające zadania proszą \
                         o zatrzymanie w bezpiecznym punkcie: staje tyle zadań, ile wynosi \
                         nadwyżka, a pierwsze gotowe są pierwsze. Zatrzymane zadanie wraca do \
                         kolejki na swoje miejsce."
                    ),
                );
                if view.effective <= 1 {
                    item.unavailable(AT_LEAST)
                } else {
                    item
                }
            },
            {
                let item = ChooserItem::new(
                    format!("  wartość z pliku ({})", view.file_value),
                    format!(
                        "{state} Usuwa nadpisanie: obowiązuje wartość max_concurrent_tasks z \
                         pliku backendu."
                    ),
                );
                if view.override_value.is_none() {
                    item.unavailable(NO_OVERRIDE)
                } else {
                    item
                }
            },
        ];
        Self {
            backend,
            view,
            chooser: Chooser::new(items),
        }
    }

    /// The backend the list is about.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// The list as shown.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    pub(super) fn chooser_mut(&mut self) -> &mut Chooser {
        &mut self.chooser
    }

    fn limit_for(&self, choice: Choice) -> Option<u32> {
        match choice {
            Choice::Raise => Some(self.view.effective.saturating_add(1).min(MOST)),
            Choice::Lower => Some(self.view.effective.saturating_sub(1).max(1)),
            Choice::File => None,
        }
    }
}

/// What the daemon says about the limit, in one sentence.
fn status(view: &BackendLimitView) -> String {
    let source = match view.source {
        BackendLimitSource::File => "z pliku",
        BackendLimitSource::AdHoc => "nadpisany, do restartu demona",
        BackendLimitSource::Unrecognized => "źródło nieznane",
    };
    format!(
        "Limit {} ({source}). Sloty zajęte: {}. Nadwyżka: {}. Czekają na zatrzymanie: {}.",
        view.effective, view.slots_held, view.overshoot, view.waiting_to_stop
    )
}

impl TaskApp {
    /// `L`: reads the limit of the selected task's backend and opens the
    /// list. The marker shows while the daemon answers.
    pub(super) fn start_limit_read(&mut self) {
        let Some(backend) = self.list.selected().map(|task| task.backend_id.to_string()) else {
            self.notify(Severity::Warning, "limit: wybierz zadanie z backendu");
            return;
        };
        let client = self.client.clone();
        self.background
            .start(JobKey::Limit(backend.clone()), async move {
                let read = client.control_backend_limit(&backend).await;
                Outcome::new(move |app: &mut TaskApp| app.on_limit_read(read))
            });
    }

    fn on_limit_read(&mut self, read: Result<BackendLimitView, ClientError>) {
        match read {
            Ok(view) => self.overlay = Some(Overlay::Limit(LimitList::new(view))),
            Err(error) => self.notify(
                Severity::Error,
                format!(
                    "limit zadań: nie udało się odczytać — {}",
                    failure_reason(&error)
                ),
            ),
        }
    }

    /// A key while the list is open.
    pub(super) fn on_limit_key(&mut self, key: KeyEvent) -> Request {
        let Some(Overlay::Limit(list)) = &mut self.overlay else {
            return Request::None;
        };
        match list.chooser.on_key(key) {
            ChooserAction::None => Request::None,
            ChooserAction::Cancel | ChooserAction::Blocked(_) => {
                self.overlay = None;
                Request::None
            }
            ChooserAction::Submit(submission) => {
                let choice = [Choice::Raise, Choice::Lower, Choice::File]
                    .get(submission.index)
                    .copied();
                let request =
                    choice.map(|choice| (list.backend().to_string(), list.limit_for(choice)));
                self.overlay = None;
                match request {
                    Some((backend, limit)) if self.mutation_allowed() => {
                        Request::SetLimit { backend, limit }
                    }
                    _ => Request::None,
                }
            }
        }
    }

    /// Sets the ad hoc limit of `backend`, or removes it (`None`). The bar
    /// shows the daemon's answer; a refusal keeps the old value.
    pub(super) async fn change_limit(&mut self, backend: String, limit: Option<u32>) {
        let result = match limit {
            Some(limit) => self.client.control_set_backend_limit(&backend, limit).await,
            None => self.client.control_clear_backend_limit(&backend).await,
        };
        match result {
            Ok(view) => {
                self.notify(Severity::Success, format!("{backend}: {}", status(&view)));
                self.refresh_after_write();
            }
            Err(error) => self.notify(
                Severity::Error,
                format!("limit zadań nie zmieniony — {}", failure_reason(&error)),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(effective: u32, override_value: Option<u32>) -> BackendLimitView {
        BackendLimitView {
            backend_id: "k8s".to_string(),
            effective,
            source: if override_value.is_some() {
                BackendLimitSource::AdHoc
            } else {
                BackendLimitSource::File
            },
            file_value: 3,
            override_value,
            slots_held: 3,
            overshoot: 1,
            waiting_to_stop: 2,
        }
    }

    #[test]
    fn the_steps_stay_inside_the_range_and_the_file_value_is_no_step() {
        let list = LimitList::new(view(1, None));
        assert_eq!(list.limit_for(Choice::Lower), Some(1));
        assert_eq!(list.limit_for(Choice::Raise), Some(2));
        assert_eq!(list.limit_for(Choice::File), None);
        let list = LimitList::new(view(MOST, Some(MOST)));
        assert_eq!(list.limit_for(Choice::Raise), Some(MOST));
    }

    #[test]
    fn the_status_repeats_the_figures_of_the_daemon() {
        let text = status(&view(2, Some(2)));
        assert!(text.contains("Limit 2 (nadpisany"), "{text}");
        assert!(text.contains("Sloty zajęte: 3"), "{text}");
        assert!(text.contains("Nadwyżka: 1"), "{text}");
        assert!(text.contains("Czekają na zatrzymanie: 2"), "{text}");
    }
}
