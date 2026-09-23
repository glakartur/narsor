//! `HumanAction` executor and the suspend/resume mechanism (`WORKFLOW_SCHEMA.md` §3.3.C).
//!
//! A human action is the one action that cannot be *run*. Entering such a state
//! records what the user is being asked for, moves the execution to
//! [`ExecutionStatus::Paused`] and gives control back to the caller; the answer
//! arrives later — possibly in a different process, after a restart — through
//! [`resume`].
//!
//! # The two halves
//!
//! * [`HumanExecutor`] is the [`ActionExecutor`] half. It is pure: it reads the
//!   configuration and reports [`ActionOutcome::Suspended`], doing no I/O and
//!   knowing nothing about the state it belongs to.
//! * [`suspend`] and [`resume`] are the durable half. They are what the engine
//!   loop (U7) calls around the executor, and they own the `Paused` status, the
//!   pending request on disk and the two events of the human interaction.
//!
//! # Where the pending request lives
//!
//! In its own `pending_input.json` beside `state.json`, not in a field of
//! [`WorkflowExecution`]. Two reasons decided it:
//!
//! * the TUI lists executions by scanning the store and needs to answer "is
//!   this one waiting for me, and what does it want?" — one small file read per
//!   execution, without deserializing the whole execution record and without
//!   loading the workflow definition to look the fields up;
//! * [`WorkflowExecution`] is serialized by every other unit, and a new field
//!   there would ripple through all of them.
//!
//! The usual objection to a second file is drift, and it is answered by making
//! the file *not* authoritative: `status == Paused` in `state.json` is the
//! truth, and [`resume`] checks the status before it ever looks at the pending
//! file. A leftover `pending_input.json` next to a non-paused execution is
//! therefore inert, which is exactly what protects against a second delivery
//! after a crash.
//!
//! # Crash safety
//!
//! [`suspend`] writes the pending file *before* it flips the status to
//! `Paused`. A crash in between leaves a pending file that nothing reads —
//! harmless. The other order would leave a paused execution with no record of
//! what it is waiting for, which nothing could repair.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use super::{ActionExecutor, ActionOutcome, ActionRequest, SuspendedAction};
use crate::domain::{
    ActionConfig, ActionError, ActionExecutionId, ActionResult, EventData, ExecutionId,
    ExecutionStatus, HumanActionConfig, HumanInputField, HumanInputType, OutputLabel, StateId,
    WorkflowExecution,
};
use crate::persistence::{ExecutionStore, PersistenceError};

/// Executes `HumanAction` states.
///
/// Stateless — one instance serves the whole engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct HumanExecutor;

impl HumanExecutor {
    /// Creates an executor.
    pub fn new() -> Self {
        Self
    }
}

impl ActionExecutor for HumanExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            let config = human_config(request.action)?;

            Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingHumanInput(HumanInputRequest {
                    action_execution_id: request.action_execution_id.to_string(),
                    instruction: config.instruction.clone(),
                    fields: config.inputs.clone(),
                }),
            ))
        })
    }
}

/// Narrows the action configuration to a human configuration.
fn human_config(action: &ActionConfig) -> Result<&HumanActionConfig, ActionError> {
    match action {
        ActionConfig::Human(config) => Ok(config),
        other => Err(ActionError {
            message: format!(
                "HumanExecutor received a {} action",
                action_type_name(other)
            ),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}

/// The schema name of an action type, for error messages.
fn action_type_name(action: &ActionConfig) -> &'static str {
    match action {
        ActionConfig::Agent(_) => "agent",
        ActionConfig::Script(_) => "script",
        ActionConfig::Human(_) => "human",
        ActionConfig::Wait(_) => "wait",
    }
}

/// What [`HumanExecutor`] observed: the question, and nothing else.
///
/// Deliberately free of everything the executor cannot know — which execution
/// and state this belongs to, and which labels the state declares. That context
/// is added by the engine in [`PendingHumanInput::new`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanInputRequest {
    /// Id of the [`crate::domain::ActionExecution`] that suspended.
    pub action_execution_id: ActionExecutionId,

    /// Message shown to the user (`message` in YAML).
    pub instruction: String,

    /// Form fields the user is asked to fill in. May be empty.
    #[serde(default)]
    pub fields: Vec<HumanInputField>,
}

/// The durable record of a question awaiting an answer — `pending_input.json`.
///
/// Self-contained on purpose: everything the TUI needs to render the form and
/// everything [`resume`] needs to validate an answer is here, so neither has to
/// load the workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHumanInput {
    /// The execution that is waiting.
    pub execution_id: ExecutionId,

    /// The state whose action suspended.
    pub state_id: StateId,

    /// The question, as the executor reported it.
    #[serde(flatten)]
    pub request: HumanInputRequest,

    /// The output labels the state declares.
    ///
    /// This is the menu a user picks from when the action declares no fields
    /// (§3.3.C, `human_review_tests` in §4), and it is what the TUI offers
    /// alongside the form otherwise. Empty means the caller recorded no menu,
    /// and then [`resume`] leaves judging the label entirely to U7.
    #[serde(default)]
    pub outputs: Vec<OutputLabel>,

    /// When the request was raised.
    pub requested_at: DateTime<Utc>,
}

impl PendingHumanInput {
    /// Builds the durable record from what the executor reported plus the state
    /// context only the engine has.
    pub fn new(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
        outputs: Vec<OutputLabel>,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            outputs,
            requested_at: Utc::now(),
        }
    }

    /// The message shown to the user.
    pub fn instruction(&self) -> &str {
        &self.request.instruction
    }

    /// The fields the user is asked to fill in.
    pub fn fields(&self) -> &[HumanInputField] {
        &self.request.fields
    }

    /// The action execution that suspended.
    pub fn action_execution_id(&self) -> &str {
        &self.request.action_execution_id
    }
}

/// An answer on its way in.
///
/// Both shapes of §3.3.C are expressed here; [`resolve_label`] documents how a
/// single label comes out of either:
///
/// * a form with fields — [`HumanInputDelivery::with_values`], optionally
///   pinned to a label with [`HumanInputDelivery::choosing`];
/// * no fields at all — [`HumanInputDelivery::label`], which picks straight
///   from the state's `outputs`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HumanInputDelivery {
    /// Values for the declared fields, by field name.
    #[serde(default)]
    pub values: Map<String, Value>,

    /// The output label the user picked, when they picked one explicitly.
    #[serde(default)]
    pub chosen_label: Option<OutputLabel>,
}

impl HumanInputDelivery {
    /// A delivery carrying filled-in field values.
    pub fn with_values(values: Map<String, Value>) -> Self {
        Self {
            values,
            chosen_label: None,
        }
    }

    /// A delivery that only picks a label, for an action without fields.
    pub fn label(label: impl Into<OutputLabel>) -> Self {
        Self {
            values: Map::new(),
            chosen_label: Some(label.into()),
        }
    }

    /// The same delivery with an explicit label attached.
    pub fn choosing(mut self, label: impl Into<OutputLabel>) -> Self {
        self.chosen_label = Some(label.into());
        self
    }
}

/// What [`resume`] hands back to the engine loop.
#[derive(Debug, Clone)]
pub struct ResumedHumanAction {
    /// The execution, now back to [`ExecutionStatus::Running`] and saved.
    pub execution: WorkflowExecution,

    /// The request that was satisfied, as it was read from disk.
    pub pending: PendingHumanInput,

    /// The result of the human action.
    ///
    /// `variables` carries the field values; per D10 the engine — not this
    /// module — merges them into `context.variables`.
    pub result: ActionResult,
}

/// Everything that can go wrong around a human action.
#[derive(Debug, Error)]
pub enum HumanActionError {
    /// The execution is not paused, so there is nothing to answer.
    ///
    /// This is the guard against a double delivery: once an answer has moved
    /// the execution back to `Running`, a second delivery of the same answer —
    /// after a restart, or from a second client — is refused here.
    #[error(
        "execution '{execution_id}' is {status} and cannot be resumed: \
         delivering human input requires it to be paused"
    )]
    NotPaused {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// Its actual status, as the schema spells it.
        status: String,
    },

    /// The execution is paused but no pending request is stored for it.
    #[error("execution '{execution_id}' is paused but has no pending human input request")]
    NoPendingRequest {
        /// The execution that was addressed.
        execution_id: ExecutionId,
    },

    /// A field declared `required: true` was not delivered.
    #[error("required field '{field}' is missing")]
    MissingRequiredField {
        /// Name of the missing field.
        field: String,
    },

    /// A value was delivered for a field the action does not declare.
    #[error(
        "field '{field}' is not declared by this human action{}",
        name_list(declared)
    )]
    UndeclaredField {
        /// The unexpected field name.
        field: String,
        /// The names the action does declare.
        declared: Vec<String>,
    },

    /// A `select` field was given a value outside its `options`.
    #[error(
        "'{value}' is not a permitted value for field '{field}'; permitted values are: {}",
        options.join(", ")
    )]
    ValueNotPermitted {
        /// The field that was filled in.
        field: String,
        /// The value that was delivered.
        value: String,
        /// The values the field allows.
        options: Vec<String>,
    },

    /// A field was given a value of the wrong shape (a `select` needs a string).
    #[error("field '{field}' expects a {expected} value")]
    WrongValueType {
        /// The field that was filled in.
        field: String,
        /// What the field expects.
        expected: &'static str,
    },

    /// No label could be derived from the delivery.
    #[error(
        "the delivered input does not select an output label; \
         pick one of the state's outputs explicitly{}",
        name_list(outputs)
    )]
    NoLabelChosen {
        /// The labels the state declares, when they were recorded.
        outputs: Vec<OutputLabel>,
    },

    /// An explicitly picked label is not one the state declares.
    #[error("'{label}' is not an output this state declares{}", name_list(outputs))]
    LabelNotDeclared {
        /// The label that was picked.
        label: OutputLabel,
        /// The labels the state declares.
        outputs: Vec<OutputLabel>,
    },

    /// Reading or writing the store failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// Renders `; declared: a, b` for an error message, or nothing when empty.
fn name_list(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!("; declared: {}", names.join(", "))
    }
}

/// Suspends the execution on a human action: records the request, pauses, logs.
///
/// Mutates `execution` in place and persists it, so the caller's copy and the
/// disk agree afterwards. The order of writes is load-bearing — see the module
/// documentation.
pub fn suspend(
    store: &ExecutionStore,
    execution: &mut WorkflowExecution,
    pending: &PendingHumanInput,
) -> Result<(), HumanActionError> {
    store.save_pending_input(&execution.id, pending)?;

    execution.status = ExecutionStatus::Paused;
    execution.current_state = Some(pending.state_id.clone());
    execution.current_action_execution = Some(pending.action_execution_id().to_string());
    store.save_execution(execution)?;

    store.event_log(&execution.id).append_next(
        &execution.id,
        EventData::HumanInputRequested {
            instruction: pending.instruction().to_string(),
        },
    )?;

    Ok(())
}

/// Reads the pending request of an execution, if it has one that can still be
/// answered.
///
/// Returns `None` both when nothing is pending and when the execution is no
/// longer paused, so a caller scanning the store never offers a form that
/// [`resume`] would refuse.
pub fn pending_input(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<Option<PendingHumanInput>, HumanActionError> {
    let execution = store.load_execution(execution_id)?;
    if execution.status != ExecutionStatus::Paused {
        return Ok(None);
    }
    Ok(store.load_pending_input(execution_id)?)
}

/// Delivers an answer, resuming the execution.
///
/// Refuses any execution that is not [`ExecutionStatus::Paused`]. On success
/// the execution is back to `Running` on disk, `HumanInputReceived` is in the
/// log, the pending file is gone, and the caller gets the [`ActionResult`] the
/// engine loop transitions on. A rejected delivery changes nothing: the
/// execution stays paused and the request stays answerable.
pub fn resume(
    store: &ExecutionStore,
    execution_id: &str,
    delivery: &HumanInputDelivery,
) -> Result<ResumedHumanAction, HumanActionError> {
    let mut execution = store.load_execution(execution_id)?;

    if execution.status != ExecutionStatus::Paused {
        return Err(HumanActionError::NotPaused {
            execution_id: execution_id.to_string(),
            status: status_name(&execution.status).to_string(),
        });
    }

    let pending: PendingHumanInput = store.load_pending_input(execution_id)?.ok_or_else(|| {
        HumanActionError::NoPendingRequest {
            execution_id: execution_id.to_string(),
        }
    })?;

    // Validation first, and before any write: a rejected delivery must leave
    // the execution exactly as paused as it was.
    let values = validate_values(pending.fields(), &delivery.values)?;
    let label = resolve_label(&pending, delivery, &values)?;

    store.event_log(execution_id).append_next(
        &execution.id,
        EventData::HumanInputReceived {
            input: Value::Object(values.clone()),
        },
    )?;

    execution.status = ExecutionStatus::Running;
    store.save_execution(&execution)?;

    // Last, so that a crash before this point leaves the request answerable
    // rather than losing it.
    store.clear_pending_input(execution_id)?;

    let result = ActionResult {
        output_label: label,
        data: Value::Object(values.clone()),
        variables: values,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
    };

    Ok(ResumedHumanAction {
        execution,
        pending,
        result,
    })
}

/// Checks the delivered values against the declared fields.
///
/// Returns the values destined for `ActionResult.variables`, which is exactly
/// the declared fields that were filled in: an undeclared name is an error
/// rather than a silent extra context variable.
fn validate_values(
    fields: &[HumanInputField],
    delivered: &Map<String, Value>,
) -> Result<Map<String, Value>, HumanActionError> {
    let declared: Vec<String> = fields.iter().map(|field| field.name.clone()).collect();

    for name in delivered.keys() {
        if !declared.iter().any(|field| field == name) {
            return Err(HumanActionError::UndeclaredField {
                field: name.clone(),
                declared,
            });
        }
    }

    let mut values = Map::new();

    for field in fields {
        let Some(value) = delivered.get(&field.name).filter(|value| !is_blank(value)) else {
            if field.required {
                return Err(HumanActionError::MissingRequiredField {
                    field: field.name.clone(),
                });
            }
            continue;
        };

        if field.field_type == HumanInputType::Select {
            let Some(text) = value.as_str() else {
                return Err(HumanActionError::WrongValueType {
                    field: field.name.clone(),
                    expected: "string",
                });
            };

            if !field.options.iter().any(|option| option == text) {
                return Err(HumanActionError::ValueNotPermitted {
                    field: field.name.clone(),
                    value: text.to_string(),
                    options: field.options.clone(),
                });
            }
        }

        values.insert(field.name.clone(), value.clone());
    }

    Ok(values)
}

/// A value that counts as "not filled in": null, or an all-whitespace string.
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        _ => false,
    }
}

/// Works out the single output label an answer selects (§3.3.C).
///
/// The schema says only that "output labels correspond to the values chosen by
/// the user", which covers two shapes that look different in the YAML of §4:
///
/// * **With fields.** `approval_status` is a `select` over
///   `[approved, changes_requested, rejected]` and the state's outputs are the
///   same words, so the label *is* the chosen value. It is taken from the one
///   `select` field, and any other field (`comments`) is only data. That value
///   has already been checked against the field's own `options`, so it is not
///   checked against `outputs` here — matching a label to a state is U7's job.
/// * **Without fields.** `human_review_tests` declares no `inputs` at all and
///   the user simply picks `ignore` or `fix_it` from the state's `outputs`.
///   There is nothing to infer from, so the label must be delivered explicitly
///   through [`HumanInputDelivery::label`], and it *is* checked against the
///   recorded menu — with no fields, that menu is the whole form.
///
/// `human_clarification` in §4 is the case that forces both to exist at once:
/// it has one *text* field and two outputs (`clarified`, `abort`), so the user
/// fills a field in **and** picks a label. An explicit label therefore always
/// wins over an inferred one.
fn resolve_label(
    pending: &PendingHumanInput,
    delivery: &HumanInputDelivery,
    values: &Map<String, Value>,
) -> Result<OutputLabel, HumanActionError> {
    if let Some(label) = &delivery.chosen_label {
        if !pending.outputs.is_empty() && !pending.outputs.iter().any(|output| output == label) {
            return Err(HumanActionError::LabelNotDeclared {
                label: label.clone(),
                outputs: pending.outputs.clone(),
            });
        }
        return Ok(label.clone());
    }

    let selects: Vec<&HumanInputField> = pending
        .fields()
        .iter()
        .filter(|field| field.field_type == HumanInputType::Select)
        .collect();

    if let [field] = selects[..] {
        if let Some(Value::String(value)) = values.get(&field.name) {
            return Ok(value.clone());
        }
    }

    Err(HumanActionError::NoLabelChosen {
        outputs: pending.outputs.clone(),
    })
}

/// The schema spelling of a status, for error messages.
fn status_name(status: &ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Running => "running",
        ExecutionStatus::Paused => "paused",
        ExecutionStatus::WaitingForEvent => "waiting_for_event",
        ExecutionStatus::Completed => "completed",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Cancelled => "cancelled",
        ExecutionStatus::Unknown => "unknown",
    }
}
