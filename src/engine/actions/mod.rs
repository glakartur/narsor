//! Action executors.
//!
//! Every action type of `WORKFLOW_SCHEMA.md` §3.3 — `script`, `agent`, `human`
//! and `wait` — is executed through the single [`ActionExecutor`] trait, so the
//! engine loop stays the same no matter what the state actually does.
//!
//! # Contract
//!
//! An executor receives an [`ActionRequest`]: the action configuration **with
//! templates already expanded** by [`crate::workflow::TemplateScope::render_action`],
//! the workspace the action runs in, the time limit, and the id of the
//! [`crate::domain::ActionExecution`] that owns the attempt. It returns either
//! an [`ActionOutcome`] or an [`ActionError`].
//!
//! Two rules hold for every implementation:
//!
//! * **No rendering.** The configuration arrives expanded; an executor that
//!   rendered again would expand a `{{ ... }}` that a variable's *value*
//!   happened to contain, which §3.3 forbids (substitution is single-pass).
//! * **No label validation.** An executor reports the raw output label it
//!   observed, even one the state does not declare in its `outputs`. The
//!   engine (U7) compares it against the state and produces the error message,
//!   because only the engine knows the state.
//!
//! # Async without `async-trait`
//!
//! `async fn` in traits is not usable for `dyn` dispatch in this edition, and
//! the engine needs `Box<dyn ActionExecutor>` to pick an executor at runtime.
//! So the async is hand-rolled the way `#[async_trait]` would desugar it: the
//! method returns a boxed, pinned, `Send` future borrowing `self` and the
//! request. Implementors write an `async move` block and box it; see
//! [`ScriptExecutor::execute`] for the shape to copy.

pub mod script;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::domain::{ActionConfig, ActionError, ActionResult, OutputLabel, Workspace};

pub use script::ScriptExecutor;

/// Environment variable carrying the [`crate::domain::ActionExecution`] id into
/// the child process.
///
/// Per D24 the id is an idempotency key: a script (or agent) that is retried
/// after a crash sees the same value for the same attempt and can recognise
/// work it has already done.
pub const ACTION_ID_ENV_VAR: &str = "ORCHESTRATOR_ACTION_ID";

/// The output label a timed-out action asks for, when the state allows it.
pub const TIMEOUT_LABEL: &str = "timeout";

/// Everything an executor needs for one attempt at one action.
#[derive(Debug, Clone, Copy)]
pub struct ActionRequest<'a> {
    /// The action configuration, templates already expanded (U4).
    pub action: &'a ActionConfig,

    /// The workspace of the execution. `workspace.path` is the primary
    /// repository directory and the default working directory of the action.
    pub workspace: &'a Workspace,

    /// Time limit for the whole attempt. `None` means no limit.
    pub timeout: Option<Duration>,

    /// Id of the owning [`crate::domain::ActionExecution`], passed to child
    /// processes as [`ACTION_ID_ENV_VAR`].
    pub action_execution_id: &'a str,
}

impl<'a> ActionRequest<'a> {
    /// Builds a request without a time limit.
    pub fn new(
        action: &'a ActionConfig,
        workspace: &'a Workspace,
        action_execution_id: &'a str,
    ) -> Self {
        Self {
            action,
            workspace,
            timeout: None,
            action_execution_id,
        }
    }

    /// Returns the same request with a time limit applied.
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

/// How an attempt ended, short of failing outright.
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    /// The action ran to completion and produced a label.
    Completed(ActionResult),

    /// The time limit ran out and the action was stopped.
    ///
    /// This is deliberately *not* resolved to a label inside the executor: an
    /// executor does not know the state's `outputs`, and §3.3.B makes the
    /// `timeout` label conditional on the state declaring it. The engine calls
    /// [`TimedOutAction::resolve`] with the declared labels in hand, which
    /// keeps all `outputs` knowledge in one place (U7).
    TimedOut(TimedOutAction),
}

impl ActionOutcome {
    /// The result, if the action completed.
    pub fn result(&self) -> Option<&ActionResult> {
        match self {
            Self::Completed(result) => Some(result),
            Self::TimedOut(_) => None,
        }
    }
}

/// An attempt that was stopped because it ran out of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOutAction {
    /// The limit that was exceeded.
    pub timeout: Duration,

    /// What was stopped, for the error message (e.g. `script './build.sh'`).
    pub subject: String,
}

impl TimedOutAction {
    /// Turns the timeout into a `timeout` result when the state declares that
    /// label, and into an action error when it does not.
    ///
    /// A state that maps exit code 124 to some label is unaffected: that is an
    /// ordinary exit-code mapping of a process that finished on its own, and
    /// never reaches here.
    pub fn resolve(self, declared_outputs: &[OutputLabel]) -> Result<ActionResult, ActionError> {
        let seconds = self.timeout.as_secs_f64();

        if declared_outputs.iter().any(|label| label == TIMEOUT_LABEL) {
            return Ok(ActionResult {
                output_label: TIMEOUT_LABEL.to_string(),
                data: serde_json::json!({ "timeout_secs": seconds }),
                variables: serde_json::Map::new(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        Err(ActionError {
            message: format!(
                "{} exceeded its time limit of {seconds}s and was killed, \
                 but the state does not declare a '{TIMEOUT_LABEL}' output",
                self.subject
            ),
            error_type: Some("timeout".to_string()),
            details: Some(serde_json::json!({
                "timeout_secs": seconds,
                "declared_outputs": declared_outputs,
            })),
        })
    }
}

/// Runs one action of one state.
///
/// Implemented by [`ScriptExecutor`] (U5) and, next, by the human, agent and
/// wait executors. See the module documentation for the contract and for the
/// hand-rolled async shape every implementation follows.
pub trait ActionExecutor: Send + Sync {
    /// Executes the action described by `request`.
    ///
    /// `Err` means the action could not produce a label at all (the command did
    /// not start, the process died from a signal, the configuration was of the
    /// wrong type). A label the state does not declare is *not* an error here —
    /// it is returned as-is and judged by the engine.
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>>;
}
