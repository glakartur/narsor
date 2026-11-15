//! An attempt that produced no result.

use super::*;

/// An attempt that produced no result.
pub(crate) struct AttemptFailure {
    /// The message that ends up in `ActionFailed`.
    pub(crate) message: String,
    /// The kind of error the action named, for `ActionFailed` (KTD13).
    error_type: Option<String>,
    /// Whether trying again could plausibly give a different answer.
    pub(crate) retriable: bool,
    /// Whether the attempt cannot have had a side effect (KTD14): nothing
    /// ran, or what ran left no trace of having acted.
    pub(crate) side_effect_free: bool,
}

impl AttemptFailure {
    /// A failure worth another attempt: the action could not run, or ran out of
    /// time.
    pub(crate) fn retriable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_type: None,
            retriable: true,
            side_effect_free: false,
        }
    }

    /// A failure that repeating cannot fix: a broken template, an unknown
    /// action type, a label the state does not declare. Every one of them
    /// stops the attempt before its action runs.
    pub(crate) fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_type: None,
            retriable: false,
            side_effect_free: true,
        }
    }

    /// The failure an action reported itself: worth another attempt, and of
    /// the kind it named.
    pub(crate) fn of_action(error: ActionError) -> Self {
        Self {
            error_type: error.error_type,
            ..Self::retriable(error.message)
        }
    }

    /// The same failure, known to have had no side effect when `clean`.
    pub(crate) fn side_effect_free_if(mut self, clean: bool) -> Self {
        self.side_effect_free = clean;
        self
    }
}

/// What `ActionFailed` says about the action that failed.
pub(crate) struct FailedAction {
    pub(crate) message: String,
    pub(crate) error_type: Option<String>,
}

impl From<AttemptFailure> for FailedAction {
    fn from(failure: AttemptFailure) -> Self {
        Self {
            message: failure.message,
            error_type: failure.error_type,
        }
    }
}
