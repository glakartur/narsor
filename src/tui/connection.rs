//! The state of the connection to the backend (R18, origin §4).
//!
//! The interface always opens, even when the server is unreachable, and keeps
//! showing what it last knew. What the operator must be able to tell at a
//! glance is whether that is live data or a memory:
//!
//! * `● live` — events are arriving;
//! * `◌ ponawiam (12s)` — the stream dropped and the next attempt is on its
//!   way; the countdown is real;
//! * `✖ brak połączenia` — no attempt is scheduled (the stream ended for good
//!   or was never up).
//!
//! While not live the data is grey and carries its age (`stan sprzed 40s`),
//! and anything that would change state is refused with a reason
//! ([`Connection::mutation_gate`]). Drafts and forms are not this module's
//! business: they stay.
//!
//! Like [`super::refresh::RefreshSchedule`] it is plain data over
//! [`Instant`]s handed in by the caller, so a test steps time without
//! sleeping.

use std::time::{Duration, Instant};

use ratatui::text::Span;

use super::style::{Role, Styles};
use crate::cli::task_text::short_age;

/// Where the connection stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Opened, nothing heard yet.
    Connecting,
    /// Events are arriving.
    Live,
    /// The stream dropped; the next attempt is due at `next_attempt`.
    Retrying {
        /// When the next attempt runs.
        next_attempt: Instant,
    },
    /// No attempt is scheduled.
    Disconnected,
}

/// The connection indicator: an icon, a text and the role they are drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusText {
    /// The icon: `●`, `◌` or `✖`.
    pub icon: &'static str,
    /// The words: `live`, `ponawiam (12s)`, `brak połączenia`.
    pub text: String,
    /// The role it is drawn in.
    pub role: Role,
}

impl StatusText {
    /// The indicator as one styled span (`● live`).
    pub fn span(&self, styles: &Styles) -> Span<'static> {
        Span::styled(format!("{} {}", self.icon, self.text), styles.of(self.role))
    }
}

/// Why a mutation is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationBlocked {
    /// What the operator is told, in the notification.
    pub reason: String,
}

/// The connection to one backend.
#[derive(Debug, Clone)]
pub struct Connection {
    name: String,
    state: LinkState,
    /// When data last arrived, live or from a read.
    last_data: Option<Instant>,
}

impl Connection {
    /// A connection to the backend called `name` (the name from the
    /// configuration), just opened.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: LinkState::Connecting,
            last_data: None,
        }
    }

    /// The backend's name, for the header.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Replaces the name once the server has told its own (`capabilities`);
    /// the state and the age of the data are kept.
    pub fn rename(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Where the connection stands.
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// Whether data is live.
    pub fn is_live(&self) -> bool {
        self.state == LinkState::Live
    }

    /// Something arrived — an event or a read. The connection is live.
    pub fn on_data(&mut self, now: Instant) {
        self.state = LinkState::Live;
        self.last_data = Some(now);
    }

    /// The stream or a read failed. With `retry_in` the next attempt is
    /// scheduled; without it nothing is, and the connection is down.
    pub fn on_failure(&mut self, now: Instant, retry_in: Option<Duration>) {
        self.state = match retry_in {
            Some(delay) => LinkState::Retrying {
                next_attempt: now + delay,
            },
            None => LinkState::Disconnected,
        };
    }

    /// How old the data is. `None` when nothing ever arrived.
    pub fn data_age(&self, now: Instant) -> Option<Duration> {
        self.last_data
            .map(|last| now.saturating_duration_since(last))
    }

    /// The age as the operator reads it: `stan sprzed 40s`. `None` while
    /// live, or when there is no data to be old.
    pub fn age_label(&self, now: Instant) -> Option<String> {
        if self.is_live() {
            return None;
        }
        self.data_age(now).map(|age| {
            format!(
                "stan sprzed {}",
                short_age(i64::try_from(age.as_secs()).unwrap_or(i64::MAX))
            )
        })
    }

    /// Whether the data on screen should be drawn grey: it is not live and
    /// there is some.
    pub fn is_stale(&self) -> bool {
        !self.is_live() && self.last_data.is_some()
    }

    /// The indicator for the header.
    pub fn status(&self, now: Instant) -> StatusText {
        match self.state {
            LinkState::Live => StatusText {
                icon: "●",
                text: "live".to_string(),
                role: Role::Live,
            },
            LinkState::Connecting => StatusText {
                icon: "◌",
                text: format!("łączę z {}", self.name),
                role: Role::Attention,
            },
            LinkState::Retrying { next_attempt } => StatusText {
                icon: "◌",
                text: format!("ponawiam ({}s)", seconds_left(next_attempt, now)),
                role: Role::Attention,
            },
            LinkState::Disconnected => StatusText {
                icon: "✖",
                text: "brak połączenia".to_string(),
                role: Role::Problem,
            },
        }
    }

    /// Whether an action that changes state may run now. Refused, with the
    /// reason, while the connection is not live.
    pub fn mutation_gate(&self, now: Instant) -> Result<(), MutationBlocked> {
        if self.is_live() {
            return Ok(());
        }
        let state = match self.state {
            LinkState::Connecting => "łączę z backendem",
            LinkState::Retrying { .. } => "ponawiam połączenie",
            LinkState::Disconnected | LinkState::Live => "brak połączenia",
        };
        let reason = match self.age_label(now) {
            Some(age) => format!("{state} ({age}): akcja zmieniająca stan jest zablokowana"),
            None => format!("{state}: akcja zmieniająca stan jest zablokowana"),
        };
        Err(MutationBlocked { reason })
    }
}

/// Whole seconds until `at`, rounded up so the countdown never shows `0s`
/// while the attempt is still ahead.
fn seconds_left(at: Instant, now: Instant) -> u64 {
    let left = at.saturating_duration_since(now);
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}
