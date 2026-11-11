//! `validate` and `run`: checking a workflow definition and starting a task from it.

use std::path::Path;

use serde_json::{Map, Value};

use crate::api::{ApiClient, ClientError};
use crate::config::{Flags, Resolved};
use crate::domain::{without_unsafe_chars, ExecutionStatus};
use narsor_protocol::controller::{WorkflowInputType, WorkflowInputView, WorkflowSource};

use super::daemon_access::{daemon_client, load_execution_summary, STATUS_POLL_INTERVAL};
use super::open_matters::print_open_matters;
use super::{CliError, Result};

/// `narsor validate <workflow>` — static validation, all violations at
/// once.
///
/// The argument is a workflow reference: a file, a name in the search path, or
/// a built-in.
pub async fn validate(config: &Resolved, flags: &Flags, reference: &str) -> Result<()> {
    let client = daemon_client(config.state_dir(), flags).await?;
    if Path::new(reference).is_file() {
        let path = absolute_workflow_path(reference)?;
        let verdict = client
            .control_validate_workflow(&path)
            .await
            .map_err(|error| {
                CliError::failure(format!("Could not validate '{reference}': {error}"))
            })?;
        return print_validation(
            verdict.name.as_deref().unwrap_or(reference),
            &path,
            verdict.valid,
            verdict.version,
            verdict.state_count,
            verdict.initial_state.as_deref(),
            &verdict.violations,
        );
    }

    let listing = client
        .control_workflows()
        .await
        .map_err(|error| CliError::failure(format!("Could not list workflows: {error}")))?;
    let Some(workflow) = listing
        .workflows
        .iter()
        .find(|workflow| workflow.name == reference)
    else {
        let builtins = listing
            .workflows
            .iter()
            .filter(|workflow| workflow.source == WorkflowSource::BuiltIn)
            .map(|workflow| workflow.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::usage(format!(
            "Workflow '{reference}' is not in the control plane catalogue. Looked for '{reference}' in the server workflow catalogue; available built-ins: {builtins}."
        )));
    };
    print_validation(
        &workflow.name,
        workflow_source_name(workflow.source),
        workflow.valid,
        workflow.version,
        workflow.state_count,
        workflow.initial_state.as_deref(),
        &workflow.violations,
    )
}

/// Singular or plural, for the violation count.
fn violation_noun(count: usize) -> &'static str {
    if count == 1 {
        "violation"
    } else {
        "violations"
    }
}

fn print_validation(
    name: &str,
    source: &str,
    valid: bool,
    version: Option<u32>,
    state_count: usize,
    initial_state: Option<&str>,
    violations: &[String],
) -> Result<()> {
    if !valid {
        let mut message = format!(
            "Definition '{name}' ({source}) is invalid — {} {} found:",
            violations.len(),
            violation_noun(violations.len())
        );
        for violation in violations {
            message.push_str(&format!("\n  - {}", without_unsafe_chars(violation)));
        }
        return Err(CliError::failure(message));
    }

    println!(
        "The definition is valid: {name} (version {}), states: {state_count}, initial state: {}.",
        version.unwrap_or_default(),
        initial_state.unwrap_or("-")
    );
    Ok(())
}

/// `narsor run <workflow> [--input k=v]…` — create an execution and run
/// it.
///
/// No longer constructs `Engine`/`ExecutionStore` itself (R4/D52): it
/// discovers or transparently launches a daemon
/// ([`crate::api::ensure_daemon_running`], AE2) and drives the rest of this
/// command entirely as that daemon's REST client — `POST /tasks`, then
/// blocking until the execution stops needing a driver
/// ([`wait_until_stopped`]).
///
/// Workflow discovery and validation come from the control plane. A file
/// reference is validated by the local server, and a catalogue reference is
/// looked up through its workflow endpoint; this client never parses a
/// workflow definition before creating the task.
///
/// [`wait_until_stopped`]: super::daemon_access::wait_until_stopped
pub async fn run(
    config: &Resolved,
    flags: &Flags,
    reference: &str,
    inputs: &[String],
) -> Result<()> {
    let client = daemon_client(config.state_dir(), flags).await?;
    let (workflow, name, source, schema) = workflow_for_run(&client, reference).await?;
    eprintln!("Workflow '{name}' — {source}.");

    // Inputs first: a missing required parameter must be reported before
    // anything is written to disk.
    let inputs = collect_inputs(&name, &schema, inputs)?;

    let submission_id = format!("task-{}", uuid::Uuid::new_v4().simple());

    let started = submit_task(&client, &submission_id, &workflow, inputs)
        .await
        .map_err(|error| CliError::failure(format!("Could not start the workflow: {error}")))?;
    println!("Queued task: {}", started.task.task_id);
    let task_id = started.task.task_id.to_string();
    let final_task = wait_until_task_terminal(&client, &task_id).await?;
    if let Some(runner) = &final_task.runner {
        if let Some(execution_id) = runner.execution_cursor.execution_id.as_deref() {
            // A task may have been queued and provisioned before this point,
            // but once its runner projects a real execution the established
            // attached-run output (including a human-input form) remains
            // useful and is read through the backend-aware execution route.
            let summary = load_execution_summary(&client, execution_id).await?;
            print_open_matters(&summary);
        }
        println!("Status: {}", runner.execution_status.as_str());
        if matches!(
            runner.execution_status,
            ExecutionStatus::Failed | ExecutionStatus::Cancelled
        ) {
            return Err(CliError::failure(format!("Task {task_id} failed.")));
        }
    }
    Ok(())
}

/// Parses `--input` pairs against the declared parameters, fills in defaults and
/// reports everything that is missing.
fn collect_inputs(
    workflow_name: &str,
    schema: &[WorkflowInputView],
    pairs: &[String],
) -> Result<Map<String, Value>> {
    let mut values = Map::new();

    for pair in pairs {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid input '{pair}' — expected the form name=value."
            ))
        })?;

        let parameter = schema
            .iter()
            .find(|parameter| parameter.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Workflow '{}' declares no input named '{name}'. Declared inputs: {}.",
                    workflow_name,
                    declared_input_names(schema)
                ))
            })?;

        values.insert(name.to_string(), parse_input_value(parameter, raw)?);
    }

    let mut missing = Vec::new();
    for parameter in schema {
        if values.contains_key(&parameter.name) {
            continue;
        }
        if let Some(default) = &parameter.default {
            values.insert(parameter.name.clone(), default.clone());
        } else if parameter.required {
            missing.push(parameter.name.clone());
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

/// The declared input names, for an error message.
fn declared_input_names(schema: &[WorkflowInputView]) -> String {
    if schema.is_empty() {
        return "(none)".to_string();
    }
    schema
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses one `--input` value according to the parameter's declared type.
///
/// This is why `--input is_experimental=true` reaches the context as a boolean
/// and not as the string `"true"`: the type comes from the definition, never
/// from guessing at the text.
fn parse_input_value(parameter: &WorkflowInputView, raw: &str) -> Result<Value> {
    match parameter.input_type {
        WorkflowInputType::String | WorkflowInputType::Text => {
            Ok(Value::String(raw.to_string()))
        }
        WorkflowInputType::Integer => raw.parse::<i64>().map(Value::from).map_err(|_| {
            CliError::usage(format!(
                "Input '{}' is of type integer, and '{raw}' is not a whole number.",
                parameter.name
            ))
        }),
        WorkflowInputType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(CliError::usage(format!(
                "Input '{}' is of type boolean — the allowed values are 'true' and 'false', got '{raw}'.",
                parameter.name
            ))),
        },
    }
}

/// Resolves the input schema through the server, returning the original
/// reference to submit so the server applies the same catalogue resolution.
async fn workflow_for_run(
    client: &ApiClient,
    reference: &str,
) -> Result<(String, String, String, Vec<WorkflowInputView>)> {
    if Path::new(reference).is_file() {
        let path = absolute_workflow_path(reference)?;
        let verdict = client
            .control_validate_workflow(&path)
            .await
            .map_err(|error| {
                CliError::failure(format!("The file could not be validated: {error}"))
            })?;
        if !verdict.valid {
            return Err(invalid_workflow_error(
                verdict.name.as_deref().unwrap_or(reference),
                &verdict.violations,
            ));
        }
        let name = verdict
            .name
            .unwrap_or_else(|| Path::new(reference).display().to_string());
        return Ok((path, name, "file".to_string(), verdict.inputs));
    }

    let listing = client
        .control_workflows()
        .await
        .map_err(|error| CliError::failure(format!("Could not list workflows: {error}")))?;
    let Some(workflow) = listing
        .workflows
        .iter()
        .find(|workflow| workflow.name == reference)
    else {
        let builtins = listing
            .workflows
            .iter()
            .filter(|workflow| workflow.source == WorkflowSource::BuiltIn)
            .map(|workflow| workflow.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::usage(format!(
            "Workflow '{reference}' is not in the control plane catalogue. Looked for '{reference}' in the server workflow catalogue; available built-ins: {builtins}."
        )));
    };
    if !workflow.valid {
        return Err(invalid_workflow_error(&workflow.name, &workflow.violations));
    }
    Ok((
        reference.to_string(),
        workflow.name.clone(),
        workflow_source_name(workflow.source).to_string(),
        workflow.inputs.clone(),
    ))
}

fn absolute_workflow_path(reference: &str) -> Result<String> {
    std::path::absolute(reference)
        .map(|path| path.display().to_string())
        .map_err(|error| CliError::usage(format!("Cannot make '{reference}' absolute: {error}")))
}

fn invalid_workflow_error(name: &str, violations: &[String]) -> CliError {
    let mut message = format!("The definition '{name}' is invalid — no task was created:");
    for violation in violations {
        message.push_str(&format!("\n  - {}", without_unsafe_chars(violation)));
    }
    CliError::failure(message)
}

fn workflow_source_name(source: WorkflowSource) -> &'static str {
    match source {
        WorkflowSource::BuiltIn => "built-in",
        WorkflowSource::Catalog => "root/catalog",
        WorkflowSource::User => "user",
    }
}

/// How many times an uncertain submission is repeated before giving up.
const SUBMIT_ATTEMPTS: u32 = 3;

/// Submits a task, repeating the identical request when the outcome is
/// uncertain.
///
/// A transport failure says nothing about whether the daemon recorded the
/// task. The id and body are kept byte-equivalent across attempts, so a
/// repeat returns the already-queued task instead of creating another.
async fn submit_task(
    client: &ApiClient,
    task_id: &str,
    workflow: &str,
    inputs: Map<String, Value>,
) -> std::result::Result<crate::api::StartTaskResponse, ClientError> {
    let mut attempt = 1;
    loop {
        match client.start_task(task_id, workflow, inputs.clone()).await {
            Err(ClientError::Unavailable(_)) if attempt < SUBMIT_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(STATUS_POLL_INTERVAL).await;
            }
            outcome => return outcome,
        }
    }
}

/// Waits on the task identity returned by local creation.  A task can be
/// queued or provisioning before a runner has an execution at all, so the
/// legacy execution stream is not a valid acknowledgement boundary here.
async fn wait_until_task_terminal(
    client: &ApiClient,
    task_id: &str,
) -> Result<crate::controller_protocol::TaskSummary> {
    let mut announced_execution = false;
    loop {
        let task = client.control_task(task_id).await.map_err(|error| {
            CliError::failure(format!("Could not load task '{task_id}': {error}"))
        })?;
        if let Some(runner) = &task.runner {
            if !announced_execution {
                if let Some(execution_id) = runner.execution_cursor.execution_id.as_deref() {
                    // Retain the long-standing machine-readable hand-off
                    // line, but only once the task projection names a real
                    // execution. Creation itself remains task-first.
                    println!("Created execution: {execution_id}");
                    announced_execution = true;
                }
            }
            if let Some(problem) = &runner.problem {
                return Err(CliError::failure(format!(
                    "Task {task_id} failed: {}",
                    problem.message
                )));
            }
            // `run` has always returned when a real execution reaches an
            // operator-visible stopping point (not only when it is terminal).
            // Before task-first admission that fell out of the in-process
            // engine pass; preserve it once the runner projection exists.
            // A pre-execution provisioning failure is handled above: its
            // honest `Unknown` snapshot must be a nonzero outcome, not an
            // apparently successful stop.
            if !matches!(runner.execution_status, ExecutionStatus::Running) {
                return Ok(task);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
