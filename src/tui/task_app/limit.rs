//! The limit of tasks processed at once, changed in the running daemon.
//!
//! `L`, from the list or the details, reads the limit and opens a list: one
//! more or one less. The daemon holds the number and computes every figure
//! the list shows; the interface adds and subtracts nothing but the step of
//! one it offers. The change lasts until the daemon restarts, when the
//! `max_concurrent_tasks` setting applies again.
//!
//! A lower limit does not stop work at once. The daemon asks running tasks
//! to stop at a safe point, and only as many stop as the limit is exceeded
//! by. The list says how many still wait.

use crossterm::event::KeyEvent;
use narsor_protocol::controller::{ConcurrencyLimitView, MAX_CONCURRENT_TASKS_LIMIT};

use crate::api::ClientError;

use super::super::chooser::{Chooser, ChooserAction, ChooserItem};
use super::actions::Overlay;
use super::background::{JobKey, Outcome};
use super::notify::Severity;
use super::{failure_reason, Request, TaskApp};

/// Most concurrent tasks the daemon accepts; the daemon checks it again.
const MOST: u32 = MAX_CONCURRENT_TASKS_LIMIT;

/// Why the entry cannot be chosen.
const AT_MOST: &str = "limit jest już największy";
const AT_LEAST: &str = "limit jest już najmniejszy";
const NO_LIMIT: &str = "limit jest wyłączony";

/// One entry of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// One more than now.
    Raise,
    /// One less than now.
    Lower,
}

/// The list `L` opens, over the limit of tasks processed at once.
#[derive(Debug, Clone)]
pub struct LimitList {
    view: ConcurrencyLimitView,
    chooser: Chooser,
}

impl LimitList {
    /// The list for `view`.
    pub fn new(view: ConcurrencyLimitView) -> Self {
        let state = status(&view);
        let raise = ChooserItem::new(
            format!(
                "  podnieś o 1 (do {})",
                view.limit.map_or(0, |limit| limit.saturating_add(1))
            ),
            format!(
                "{state} Wyższy limit uruchamia czekające zadania od razu; działających nie \
                 rusza."
            ),
        );
        let raise = match view.limit {
            None => raise.unavailable(NO_LIMIT),
            Some(limit) if limit >= MOST => raise.unavailable(AT_MOST),
            Some(_) => raise,
        };
        let lower = ChooserItem::new(
            format!(
                "  obniż o 1 (do {})",
                view.limit.map_or(0, |limit| limit.saturating_sub(1))
            ),
            format!(
                "{state} Niższy limit wstrzymuje nowe starty. Działające zadania proszą o \
                 zatrzymanie w bezpiecznym punkcie: staje tyle zadań, ile wynosi nadwyżka, a \
                 pierwsze gotowe są pierwsze. Zatrzymane zadanie wraca do kolejki na swoje \
                 miejsce."
            ),
        );
        let lower = match view.limit {
            None => lower.unavailable(NO_LIMIT),
            Some(limit) if limit <= 1 => lower.unavailable(AT_LEAST),
            Some(_) => lower,
        };
        Self {
            view,
            chooser: Chooser::new(vec![raise, lower]),
        }
    }

    /// The list as shown.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    pub(super) fn chooser_mut(&mut self) -> &mut Chooser {
        &mut self.chooser
    }

    fn limit_for(&self, choice: Choice) -> Option<u32> {
        let limit = self.view.limit?;
        Some(match choice {
            Choice::Raise => limit.saturating_add(1).min(MOST),
            Choice::Lower => limit.saturating_sub(1).max(1),
        })
    }
}

/// What the daemon says about the limit, in one sentence.
fn status(view: &ConcurrencyLimitView) -> String {
    let limit = view
        .limit
        .map_or_else(|| "bez limitu".to_string(), |limit| limit.to_string());
    format!(
        "Limit {limit} (do restartu demona). Sloty zajęte: {}. Nadwyżka: {}. Czekają na \
         zatrzymanie: {}.",
        view.slots_held, view.overshoot, view.waiting_to_stop
    )
}

impl TaskApp {
    /// `L`: reads the limit and opens the list. The marker shows while the
    /// daemon answers.
    pub(super) fn start_limit_read(&mut self) {
        let client = self.client.clone();
        self.background.start(JobKey::Limit, async move {
            let read = client.control_concurrency_limit().await;
            Outcome::new(move |app: &mut TaskApp| app.on_limit_read(read))
        });
    }

    fn on_limit_read(&mut self, read: Result<ConcurrencyLimitView, ClientError>) {
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
                let choice = [Choice::Raise, Choice::Lower]
                    .get(submission.index)
                    .copied();
                let limit = choice.and_then(|choice| list.limit_for(choice));
                self.overlay = None;
                match limit {
                    Some(limit) if self.mutation_allowed() => Request::SetLimit { limit },
                    _ => Request::None,
                }
            }
        }
    }

    /// Sets the limit. The bar shows the daemon's answer; a refusal keeps the
    /// old value.
    pub(super) async fn change_limit(&mut self, limit: u32) {
        match self.client.control_set_concurrency_limit(limit).await {
            Ok(view) => {
                self.notify(Severity::Success, status(&view));
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

    fn view(limit: Option<u32>) -> ConcurrencyLimitView {
        ConcurrencyLimitView {
            limit,
            slots_held: 3,
            overshoot: 1,
            waiting_to_stop: 2,
        }
    }

    #[test]
    fn the_steps_stay_inside_the_range() {
        let list = LimitList::new(view(Some(1)));
        assert_eq!(list.limit_for(Choice::Lower), Some(1));
        assert_eq!(list.limit_for(Choice::Raise), Some(2));
        let list = LimitList::new(view(Some(MOST)));
        assert_eq!(list.limit_for(Choice::Raise), Some(MOST));
        assert_eq!(LimitList::new(view(None)).limit_for(Choice::Raise), None);
    }

    #[test]
    fn the_status_repeats_the_figures_of_the_daemon() {
        let text = status(&view(Some(2)));
        assert!(text.contains("Limit 2 (do restartu"), "{text}");
        assert!(text.contains("Sloty zajęte: 3"), "{text}");
        assert!(text.contains("Nadwyżka: 1"), "{text}");
        assert!(text.contains("Czekają na zatrzymanie: 2"), "{text}");
    }
}
