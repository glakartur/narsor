//! The installation's default agent harness (U5, R12–R15).
//!
//! The header says which harness is in force — `harness claude-code
//! (konfiguracja)` while the configured value holds, `harness opencode` once
//! an override replaced it — and `h`, from the list, the details, the history
//! or the repositories, opens the list to change it. It is one setting of the
//! daemon (`/settings/agent-harness`), not of a task: the same thing
//! `narsor harness default get|set|reset` reads and changes.
//!
//! # What is read
//!
//! The snapshot ([`HarnessSelectionSnapshot`]) is read alongside every read of
//! the task list, when the list opens and after a change. A read that fails
//! keeps the last value known — a daemon without a selector, or one that is
//! briefly away, does not make the header lie or the connection look broken;
//! before the first read the header says `harness ?`.
//!
//! # The list
//!
//! First "domyślny z konfiguracji (<configured>)", which removes the
//! override, then the harnesses the daemon supports, in its order. The one in
//! force is marked `●` and greyed out: choosing it changes nothing. The list
//! closes as soon as a choice is made; until the daemon answers, the header
//! shows `harness <new>…`.

use crossterm::event::KeyEvent;

use crate::api::{ClientError, HarnessSelectionSnapshot};

use super::super::chooser::{Chooser, ChooserAction, ChooserItem};
use super::super::scroll::one_line;
use super::actions::Overlay;
use super::notify::Severity;
use super::{failure_reason, Request, TaskApp};

/// Why the entry in force cannot be chosen.
const CURRENT: &str = "to jest obecny wybór";

/// Shown when the daemon names no harness at all.
const NONE_SUPPORTED: &str = "demon nie zgłasza obsługiwanych harnessów";

/// One entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    /// Back to the configured value: the override goes.
    Configured,
    /// This harness, as the override.
    Named(String),
    /// Nothing to choose: the daemon names no harness.
    Nothing,
}

/// The list `h` opens: the configured value, then the supported harnesses.
#[derive(Debug, Clone)]
pub struct HarnessList {
    choices: Vec<Choice>,
    chooser: Chooser,
}

impl HarnessList {
    /// The list for `snapshot`, its cursor on the entry in force.
    pub fn new(snapshot: &HarnessSelectionSnapshot) -> Self {
        let configured = one_line(&snapshot.configured);
        let mut choices = vec![Choice::Configured];
        choices.extend(snapshot.supported.iter().cloned().map(Choice::Named));
        if snapshot.supported.is_empty() {
            choices.push(Choice::Nothing);
        }
        let current = |choice: &Choice| match choice {
            Choice::Configured => snapshot.override_harness.is_none(),
            Choice::Named(name) => snapshot.override_harness.as_deref() == Some(name.as_str()),
            Choice::Nothing => false,
        };
        let items = choices
            .iter()
            .map(|choice| {
                let marker = if current(choice) { "● " } else { "  " };
                let item = match choice {
                    Choice::Configured => ChooserItem::new(
                        format!("{marker}domyślny z konfiguracji ({configured})"),
                        format!(
                            "Usuwa nadpisanie: akcje agentów, które nie wskazują harnessu w \
                             workflow, i dyrygenci idą na harnessie z konfiguracji demona \
                             ({configured})."
                        ),
                    ),
                    Choice::Named(name) => {
                        let name = one_line(name);
                        ChooserItem::new(
                            format!("{marker}{name}"),
                            format!(
                                "Ustawia „{name}” jako domyślny harness instalacji: dla akcji \
                                 agentów, które nie wskazują harnessu w workflow, i dla \
                                 dyrygentów. Zostaje do zmiany albo powrotu do konfiguracji."
                            ),
                        )
                    }
                    Choice::Nothing => ChooserItem::new(
                        format!("  {NONE_SUPPORTED}"),
                        "Demon nie podał listy harnessów, które obsługuje; zostaje tylko \
                         wartość z konfiguracji.",
                    )
                    .unavailable(NONE_SUPPORTED),
                };
                if current(choice) {
                    item.unavailable(CURRENT)
                } else {
                    item
                }
            })
            .collect();
        let at = choices.iter().position(current).unwrap_or(0);
        Self {
            choices,
            chooser: Chooser::new(items).selecting(at),
        }
    }

    /// The list as shown.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    pub(super) fn chooser_mut(&mut self) -> &mut Chooser {
        &mut self.chooser
    }
}

/// What the header says about the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessHeader<'a> {
    /// Nothing read yet: `harness ?`.
    Unknown,
    /// A change was sent and has no answer yet: `harness <new>…`.
    Pending(&'a str),
    /// The harness in force, and whether it is the configured one.
    Known {
        /// Its name.
        name: &'a str,
        /// No override: it comes from the configuration.
        configured: bool,
    },
}

impl TaskApp {
    /// The daemon's default harness, as last read.
    pub fn harness_selection(&self) -> Option<&HarnessSelectionSnapshot> {
        self.harness_selection.as_ref()
    }

    /// Takes a snapshot of the default harness in, as a read does.
    pub fn set_harness_selection(&mut self, snapshot: HarnessSelectionSnapshot) {
        self.harness_selection = Some(snapshot);
    }

    /// What the header says about the harness.
    pub fn harness_header(&self) -> HarnessHeader<'_> {
        if let Some(pending) = &self.harness_pending {
            return HarnessHeader::Pending(pending);
        }
        match &self.harness_selection {
            None => HarnessHeader::Unknown,
            Some(snapshot) => HarnessHeader::Known {
                name: &snapshot.effective,
                configured: snapshot.override_harness.is_none(),
            },
        }
    }

    /// Reads the snapshot. A failure keeps what was known and is only
    /// returned: the header is the only place it shows, and the connection is
    /// judged by the task list, not by this.
    pub(super) async fn read_harness_selection(&mut self) -> Result<(), ClientError> {
        self.harness_selection = Some(self.client.harness_selection().await?);
        Ok(())
    }

    /// `h`: reads the snapshot and opens the list over what is known.
    pub(super) async fn read_and_open_harness(&mut self) {
        let read = self.read_harness_selection().await;
        if self.open_harness() {
            return;
        }
        if let Err(error) = read {
            self.notify(
                Severity::Error,
                format!(
                    "domyślny harness: nie udało się odczytać — {}",
                    failure_reason(&error)
                ),
            );
        }
    }

    /// Opens the list over the snapshot last read. Returns whether there was
    /// one to open it over.
    pub fn open_harness(&mut self) -> bool {
        let Some(snapshot) = &self.harness_selection else {
            return false;
        };
        self.overlay = Some(Overlay::Harness(HarnessList::new(snapshot)));
        true
    }

    /// A key while the list is open.
    pub(super) fn on_harness_key(&mut self, key: KeyEvent) -> Request {
        let Some(Overlay::Harness(list)) = &mut self.overlay else {
            return Request::None;
        };
        match list.chooser.on_key(key) {
            ChooserAction::None => Request::None,
            // The entry in force, or none at all: choosing it changes nothing.
            ChooserAction::Cancel | ChooserAction::Blocked(_) => {
                self.overlay = None;
                Request::None
            }
            ChooserAction::Submit(submission) => {
                let choice = list.choices.get(submission.index).cloned();
                self.overlay = None;
                self.choose_harness(choice)
            }
        }
    }

    /// Sends a choice, once the connection allows it; the header shows the
    /// new value as pending until the answer.
    fn choose_harness(&mut self, choice: Option<Choice>) -> Request {
        let Some(snapshot) = &self.harness_selection else {
            return Request::None;
        };
        let (shown, sent) = match choice {
            Some(Choice::Configured) => (snapshot.configured.clone(), None),
            Some(Choice::Named(name)) => (name.clone(), Some(name)),
            Some(Choice::Nothing) | None => return Request::None,
        };
        if !self.mutation_allowed() {
            return Request::None;
        }
        self.harness_pending = Some(shown);
        Request::SetHarness(sent)
    }

    /// Sets the override to `harness`, or removes it (`None`). The header and
    /// the bar show the daemon's answer; a refusal keeps the old value.
    pub(super) async fn change_harness(&mut self, harness: Option<String>) {
        let result = match &harness {
            Some(name) => self.client.set_harness_selection(name).await,
            None => self.client.reset_harness_selection().await,
        };
        self.harness_pending = None;
        match result {
            Ok(snapshot) => {
                let tail = if snapshot.override_harness.is_none() {
                    " (konfiguracja)"
                } else {
                    ""
                };
                self.notify(
                    Severity::Success,
                    format!("domyślny harness: {}{tail}", snapshot.effective),
                );
                self.harness_selection = Some(snapshot);
            }
            Err(error) => self.notify(
                Severity::Error,
                format!(
                    "domyślny harness nie zmieniony — {}",
                    failure_reason(&error)
                ),
            ),
        }
    }
}
