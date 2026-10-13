//! The text `narsor task history|repo|workflows|validate|capabilities` print:
//! what the control plane says about old executions, the repository
//! registry and the workflow catalogue.
//!
//! Pure functions of what the control plane returned, like
//! [`super::task_text`]. Everything in them may come from a server, a
//! runner or a workflow file, so every line goes through [`ToneLine`]
//! (terminal-unsafe characters never get into it).

use serde_json::{Map, Value};

use crate::controller_protocol::{
    CapabilitiesView, LegacyExecutionView, RepositoryView, ValidateWorkflowResponse,
    WorkflowInputView, WorkflowSource, WorkflowView,
};
use crate::domain::without_unsafe_chars;
use narsor_protocol::controller::WorkflowInputType;

use super::commands::{CliError, Result};
use super::task_text::{Tone, ToneLine};

/// The heading of the `narsor task history list` table.
pub fn history_header() -> String {
    format!("{:<38} {:<10} {:<22} WORKFLOW", "ID", "STATUS", "STATE")
}

/// One row of the table of executions from before the scheduler: id,
/// status, the state it stopped at and its workflow with version.
pub fn history_row(view: &LegacyExecutionView) -> String {
    without_unsafe_chars(&format!(
        "{:<38} {:<10} {:<22} {} v{}",
        view.id,
        view.status.as_str(),
        view.current_state.as_deref().unwrap_or("-"),
        view.workflow_id,
        view.workflow_version
    ))
}

/// Every field of one old execution, with why it cannot be recovered.
pub fn history_detail_lines(view: &LegacyExecutionView) -> Vec<ToneLine> {
    let mut lines = vec![
        ToneLine::new(format!("Execution  {}", view.id), Tone::Normal),
        ToneLine::new(
            format!("Workflow   {} v{}", view.workflow_id, view.workflow_version),
            Tone::Normal,
        ),
        ToneLine::new(format!("Status     {}", view.status.as_str()), Tone::Normal),
        ToneLine::new(
            format!(
                "State      {}",
                view.current_state.as_deref().unwrap_or("-")
            ),
            Tone::Normal,
        ),
        ToneLine::new(
            format!("Started    {}", view.started_at.to_rfc3339()),
            Tone::Normal,
        ),
        ToneLine::new(
            format!(
                "Completed  {}",
                view.completed_at
                    .map(|at| at.to_rfc3339())
                    .unwrap_or_else(|| "-".to_string())
            ),
            Tone::Normal,
        ),
    ];
    if let Some(problem) = &view.problem {
        lines.push(ToneLine::new(
            format!(
                "Problem    {}: {}",
                problem.kind.name(),
                problem.reason.replace('\n', " ")
            ),
            Tone::Error,
        ));
    }
    lines.push(match (&view.not_recoverable_reason, view.recoverable) {
        (_, true) => ToneLine::new(
            "Recovery   possible (narsor task history recover <id>)",
            Tone::Good,
        ),
        (Some(reason), false) => {
            ToneLine::new(format!("Recovery   not possible: {reason}"), Tone::Muted)
        }
        (None, false) => ToneLine::new("Recovery   not possible", Tone::Muted),
    });
    lines
}

fn source_name(source: WorkflowSource) -> &'static str {
    match source {
        WorkflowSource::BuiltIn => "built-in",
        WorkflowSource::Catalog => "catalogue",
        WorkflowSource::User => "user",
    }
}

fn input_line(input: &WorkflowInputView) -> ToneLine {
    let mut text = format!(
        "    input {}: {:?} {}",
        input.name,
        input.input_type,
        if input.required {
            "required"
        } else {
            "optional"
        }
    );
    if let Some(default) = &input.default {
        text.push_str(&format!(", default {default}"));
    }
    if let Some(description) = &input.description {
        text.push_str(&format!(" — {description}"));
    }
    ToneLine::new(text, Tone::Normal)
}

/// One workflow of the catalogue: its reference, version and source, what
/// it does, its inputs, and every violation when it cannot be used.
pub fn workflow_lines(view: &WorkflowView) -> Vec<ToneLine> {
    let version = view
        .version
        .map(|version| format!(" v{version}"))
        .unwrap_or_default();
    let mut head = format!("{}{version}  [{}]", view.name, source_name(view.source));
    if view.overrides {
        head.push_str("  (hides another copy of the same name)");
    }
    if !view.valid {
        head.push_str("  INVALID");
    }
    let tone = if view.valid {
        Tone::Normal
    } else {
        Tone::Error
    };
    let mut lines = vec![ToneLine::new(head, tone)];
    if let Some(description) = &view.description {
        lines.push(ToneLine::new(format!("    {description}"), Tone::Muted));
    }
    lines.extend(view.inputs.iter().map(input_line));
    lines.extend(
        view.violations
            .iter()
            .map(|violation| ToneLine::new(format!("    violation: {violation}"), Tone::Error)),
    );
    lines
}

/// The verdict on a workflow file: the definition and its inputs when it
/// loads, every violation at once when it does not.
pub fn validation_lines(response: &ValidateWorkflowResponse) -> Vec<ToneLine> {
    let mut lines = Vec::new();
    if response.valid {
        let name = response.name.as_deref().unwrap_or("-");
        let version = response
            .version
            .map(|version| format!(" v{version}"))
            .unwrap_or_default();
        lines.push(ToneLine::new(format!("Valid: {name}{version}"), Tone::Good));
    } else {
        lines.push(ToneLine::new(
            format!("Invalid — {} violation(s):", response.violations.len()),
            Tone::Error,
        ));
    }
    lines.extend(
        response
            .violations
            .iter()
            .map(|violation| ToneLine::new(format!("  - {violation}"), Tone::Error)),
    );
    lines.extend(response.inputs.iter().map(input_line));
    lines
}

/// What the server offers, with the reason for what it does not.
pub fn capabilities_lines(view: &CapabilitiesView) -> Vec<ToneLine> {
    let yes_no = |on: bool| if on { "yes" } else { "no" };
    let mut paths = format!("Workflow paths:         {}", yes_no(view.workflow_paths));
    if let Some(reason) = &view.workflow_paths_reason {
        paths.push_str(&format!(" ({reason})"));
    }
    vec![
        ToneLine::new(
            format!("Connection:             {}", view.connection_name),
            Tone::Normal,
        ),
        ToneLine::new(paths, Tone::Normal),
        ToneLine::new(
            format!("Old executions:         {}", yes_no(view.legacy_executions)),
            Tone::Normal,
        ),
        ToneLine::new(
            format!(
                "Local repository paths: {}",
                yes_no(view.local_repository_paths)
            ),
            Tone::Normal,
        ),
    ]
}

/// The heading of the `narsor task repo list` table.
pub fn repository_header() -> String {
    format!(
        "{:<24} {:<10} {:<16} REMOTE",
        "ID", "PROVIDER", "DEFAULT BRANCH"
    )
}

/// One registered repository, in the columns of `narsor repo list`.
pub fn repository_row(view: &RepositoryView) -> String {
    without_unsafe_chars(&format!(
        "{:<24} {:<10} {:<16} {}",
        view.id,
        view.provider,
        view.default_branch.as_deref().unwrap_or("-"),
        view.remote
    ))
}

/// The `--input name=value` pairs of a task created from a workflow file,
/// typed by the inputs the definition declares: a `string` or `repository`
/// is the text as given, an `integer` and a `boolean` are checked, defaults
/// fill what was left out, and what is missing or unknown is a usage error
/// naming every input at once.
pub fn typed_inputs(
    declared: &[WorkflowInputView],
    pairs: &[String],
) -> Result<Map<String, Value>> {
    let mut values = Map::new();
    for pair in pairs {
        let (name, raw) = pair
            .split_once('=')
            .filter(|(name, _)| !name.trim().is_empty())
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Invalid input '{pair}' — expected the form name=value."
                ))
            })?;
        let name = name.trim();
        let input = declared
            .iter()
            .find(|input| input.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "The workflow declares no input named '{name}'. Declared inputs: {}.",
                    declared_names(declared)
                ))
            })?;
        values.insert(name.to_string(), typed_value(input, raw)?);
    }
    let mut missing = Vec::new();
    for input in declared {
        if values.contains_key(&input.name) {
            continue;
        }
        match (&input.default, input.required) {
            (Some(default), _) => {
                values.insert(input.name.clone(), default.clone());
            }
            (None, true) => missing.push(input.name.as_str()),
            (None, false) => {}
        }
    }
    if !missing.is_empty() {
        return Err(CliError::usage(format!(
            "Missing required inputs: {}. Pass them with --input name=value.",
            missing.join(", ")
        )));
    }
    Ok(values)
}

fn declared_names(declared: &[WorkflowInputView]) -> String {
    if declared.is_empty() {
        return "(none)".to_string();
    }
    declared
        .iter()
        .map(|input| input.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn typed_value(input: &WorkflowInputView, raw: &str) -> Result<Value> {
    match input.input_type {
        WorkflowInputType::String | WorkflowInputType::Text | WorkflowInputType::Repository => Ok(Value::String(raw.to_string())),
        WorkflowInputType::Integer => raw.parse::<i64>().map(Value::from).map_err(|_| {
            CliError::usage(format!(
                "Input '{}' is of type integer, and '{raw}' is not a whole number.",
                input.name
            ))
        }),
        WorkflowInputType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(CliError::usage(format!(
                "Input '{}' is of type boolean — the allowed values are 'true' and 'false', got '{raw}'.",
                input.name
            ))),
        },
    }
}
