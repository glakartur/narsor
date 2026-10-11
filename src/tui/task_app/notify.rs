//! The notification on the right of the bottom bar (origin §3).
//!
//! One line, the latest thing that happened — "NAR-12: przesunięto za NAR-17",
//! "przesuń: zadanie nie jest w kolejce" — coloured by what it means (green a
//! success, yellow a warning, red an error) and gone a few seconds later. The
//! time is handed in by the caller, so a test steps it without sleeping.
//!
//! There is one slot, so news of one kind arriving in a burst — several
//! tasks resumed automatically after a restart — is counted in one
//! notification ([`Notifier::push_counted`]) rather than each replacing the
//! last.

use std::time::{Duration, Instant};

use ratatui::text::Span;

use super::super::scroll::one_line;
use super::super::style::{Role, Styles};

/// How long a notification stays on the bar.
pub const LIFETIME: Duration = Duration::from_secs(6);

/// What a notification means; picks its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// It worked: green.
    Success,
    /// Not done, and why — the operator can act: yellow.
    Warning,
    /// It failed: red.
    Error,
}

impl Severity {
    fn role(self) -> Role {
        match self {
            Self::Success => Role::Live,
            Self::Warning => Role::Attention,
            Self::Error => Role::Problem,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Success => "✓",
            Self::Warning => "!",
            Self::Error => "✗",
        }
    }
}

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// What it says, on one line.
    pub text: String,
    /// What it means.
    pub severity: Severity,
    /// When it was raised.
    pub at: Instant,
    /// The kind of news it counts, and how many so far
    /// ([`Notifier::push_counted`]).
    pub counted: Option<(&'static str, usize)>,
}

/// The latest notification.
#[derive(Debug, Clone, Default)]
pub struct Notifier {
    current: Option<Notification>,
}

impl Notifier {
    /// Raises a notification, replacing the one on the bar. Text from the
    /// server is made safe and put on one line.
    pub fn push(&mut self, severity: Severity, text: impl AsRef<str>, now: Instant) {
        self.current = Some(Notification {
            text: one_line(text.as_ref()),
            severity,
            at: now,
            counted: None,
        });
    }

    /// Raises a notification of the kind `kind`: `one` alone, or — while
    /// one of the same kind is still on the bar — `many` of how many there
    /// were in a row.
    pub fn push_counted(
        &mut self,
        severity: Severity,
        kind: &'static str,
        one: impl AsRef<str>,
        many: impl FnOnce(usize) -> String,
        now: Instant,
    ) {
        let count = self
            .current(now)
            .and_then(|note| note.counted)
            .filter(|(counted, _)| *counted == kind)
            .map_or(1, |(_, count)| count + 1);
        let text = if count == 1 {
            one.as_ref().to_string()
        } else {
            many(count)
        };
        self.current = Some(Notification {
            text: one_line(&text),
            severity,
            at: now,
            counted: Some((kind, count)),
        });
    }

    /// The notification, while it is still on the bar.
    pub fn current(&self, now: Instant) -> Option<&Notification> {
        self.current
            .as_ref()
            .filter(|note| now.saturating_duration_since(note.at) < LIFETIME)
    }

    /// The latest notification whether or not it has faded.
    pub fn latest(&self) -> Option<&Notification> {
        self.current.as_ref()
    }

    /// The notification as a span: an icon, then the text, in its role.
    pub fn span(&self, styles: &Styles, now: Instant) -> Option<Span<'static>> {
        let note = self.current(now)?;
        Some(Span::styled(
            format!("{} {}", note.severity.icon(), note.text),
            styles.of(note.severity.role()),
        ))
    }
}
