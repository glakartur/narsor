//! Implementation of every subcommand.
//!
//! # Where a `run` gets its workspace
//!
//! `run` owns the command line — parsing `--input name=value` pairs against the
//! declared parameters, and printing what happened — and nothing else: creating
//! the execution, provisioning its workspace and recording where its definition
//! lives is [`crate::tasks::start_task`], which the interface calls too. That
//! module documents the ordering those steps have to keep.
//!
//! # Where a definition comes from
//!
//! `validate`, `run` and `--file` all take a *workflow reference*, not a path:
//! an existing file wins, then `{root}/workflows/<name>.yaml`, then the user
//! directory, then the built-in of that name ([`crate::workflows`]). That is
//! what makes `orchestrator run compound-engineering` work from an installed
//! binary, which has no checkout to read from.
//!
//! # How `input` and `resume` find the definition
//!
//! The engine needs the [`WorkflowDefinition`] to continue an execution, but
//! `state.json` stores only the workflow *id*. So `run` records both the
//! absolute path of the file it read (when there was one) and the workflow's
//! name, and the later commands re-resolve from that —
//! [`crate::tasks::resolve_definition`]. A built-in has no path at all, so
//! without the name an execution started from one could never be resumed.
//! `--file` overrides both, for the case where the definition has moved.

use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::domain::{
    Event, ExecutionStatus, InputParameter, ParamType, WorkflowDefinition, WorkflowExecution,
};
use crate::engine::{Engine, HumanInputDelivery, PendingHumanInput};
use crate::persistence::ExecutionStore;
use crate::registry::{infer_provider, RepositoryEntry, RepositoryRegistry};
use crate::tasks::{self, TaskError};
use crate::workflows::{builtin, builtin_names, ResolvedWorkflow, WorkflowResolver};

use super::{EXIT_FAILURE, EXIT_USAGE};

/// How often `events --follow` looks for new entries in the log.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

/// A message for the operator plus the exit code it carries.
#[derive(Debug)]
pub struct CliError {
    /// What is printed to stderr.
    pub message: String,
    /// The process exit code.
    pub exit_code: i32,
}

impl CliError {
    /// A validation error, a failed execution, or an unreadable store.
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_FAILURE,
        }
    }

    /// A wrong invocation: nothing ran.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_USAGE,
        }
    }
}

/// Result of a subcommand.
pub type Result<T> = std::result::Result<T, CliError>;

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

/// `orchestrator validate <workflow>` — static validation, all violations at
/// once.
///
/// The argument is a workflow reference: a file, a name in the search path, or
/// a built-in.
pub fn validate(root: &Path, reference: &str) -> Result<()> {
    let resolved = resolve_workflow(root, reference)?;
    let definition = tasks::load_resolved(&resolved).map_err(|error| match error {
        TaskError::InvalidDefinition { violations } => {
            let mut message = format!(
                "Definition '{}' ({}) is invalid — {} {} found:",
                resolved.name,
                resolved.origin,
                violations.len(),
                violation_noun(violations.len())
            );
            for violation in &violations {
                message.push_str(&format!("\n  - {violation}"));
            }
            CliError::failure(message)
        }
        other => CliError::failure(other.to_string()),
    })?;

    println!(
        "The definition is valid: {} (version {}), states: {}, initial state: {}.",
        definition.name,
        definition.version,
        definition.states.len(),
        definition.initial_state
    );
    Ok(())
}

/// Singular or plural, for the violation count.
fn violation_noun(count: usize) -> &'static str {
    if count == 1 {
        "violation"
    } else {
        "violations"
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// `orchestrator run <workflow> [--input k=v]…` — create an execution and run
/// it.
pub async fn run(root: &Path, reference: &str, inputs: &[String]) -> Result<()> {
    let resolved = resolve_workflow(root, reference)?;
    eprintln!("Workflow '{}' — {}.", resolved.name, resolved.origin);
    let definition = tasks::load_resolved(&resolved).map_err(|error| match error {
        TaskError::InvalidDefinition { violations } => {
            let mut message = String::from("The definition is invalid — no execution was created:");
            for violation in &violations {
                message.push_str(&format!("\n  - {violation}"));
            }
            CliError::failure(message)
        }
        other => CliError::failure(other.to_string()),
    })?;

    // Inputs first: a missing required parameter must be reported before
    // anything is written to disk.
    let inputs = collect_inputs(&definition, inputs)?;

    let engine = Engine::new(ExecutionStore::new(root));
    // The new id is announced the moment it exists, so that a failing clone
    // still tells the operator which execution to look at.
    let execution =
        tasks::start_task_with(root, &engine, &definition, &resolved, inputs, |execution| {
            println!("Created execution: {}", execution.id);
        })
        .map_err(|error| CliError::failure(error.to_string()))?;

    let execution = engine
        .run(&definition, &execution.id)
        .await
        .map_err(|error| CliError::failure(format!("Engine error: {error}")))?;

    print_execution_summary(&execution);
    if let Some(pending) = engine
        .pending_input(&execution.id)
        .map_err(|error| CliError::failure(format!("Engine error: {error}")))?
    {
        print_pending_input(&pending);
    }

    if execution.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            execution.id
        )));
    }
    Ok(())
}

/// Parses `--input` pairs against the declared parameters, fills in defaults and
/// reports everything that is missing.
fn collect_inputs(definition: &WorkflowDefinition, pairs: &[String]) -> Result<Map<String, Value>> {
    let mut values = Map::new();

    for pair in pairs {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid input '{pair}' — expected the form name=value."
            ))
        })?;

        let parameter = definition
            .inputs
            .iter()
            .find(|parameter| parameter.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Workflow '{}' declares no input named '{name}'. Declared inputs: {}.",
                    definition.name,
                    declared_input_names(definition)
                ))
            })?;

        values.insert(name.to_string(), parse_input_value(parameter, raw)?);
    }

    let mut missing = Vec::new();
    for parameter in &definition.inputs {
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
fn declared_input_names(definition: &WorkflowDefinition) -> String {
    if definition.inputs.is_empty() {
        return "(none)".to_string();
    }
    definition
        .inputs
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
fn parse_input_value(parameter: &InputParameter, raw: &str) -> Result<Value> {
    match parameter.param_type {
        ParamType::String | ParamType::Repository => Ok(Value::String(raw.to_string())),
        ParamType::Integer => raw.parse::<i64>().map(Value::from).map_err(|_| {
            CliError::usage(format!(
                "Input '{}' is of type integer, and '{raw}' is not a whole number.",
                parameter.name
            ))
        }),
        ParamType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(CliError::usage(format!(
                "Input '{}' is of type boolean — the allowed values are 'true' and 'false', got '{raw}'.",
                parameter.name
            ))),
        },
    }
}

// ---------------------------------------------------------------------------
// list / show / events
// ---------------------------------------------------------------------------

/// `orchestrator list` — every execution in the store, newest first.
pub fn list(root: &Path) -> Result<()> {
    let store = ExecutionStore::new(root);
    let ids = store
        .list_execution_ids()
        .map_err(|error| CliError::failure(format!("Could not read the executions: {error}")))?;

    if ids.is_empty() {
        println!("No executions.");
        return Ok(());
    }

    let mut executions = Vec::new();
    for id in ids {
        match store.load_execution(&id) {
            Ok(execution) => executions.push(execution),
            Err(error) => eprintln!("Skipping execution '{id}': {error}"),
        }
    }
    // Newest first: the execution an operator just started is the one they
    // are looking for.
    executions.sort_by_key(|execution| std::cmp::Reverse(execution.started_at));

    println!("{:<38} {:<10} {:<22} WORKFLOW", "ID", "STATUS", "STATE");
    for execution in &executions {
        println!(
            "{:<38} {:<10} {:<22} {}",
            execution.id,
            status_name(&execution.status),
            execution.current_state.as_deref().unwrap_or("-"),
            execution.workflow_id
        );
    }
    Ok(())
}

/// `orchestrator show <id>` — the current state, the status and the context.
pub fn show(root: &Path, execution_id: &str) -> Result<()> {
    let store = ExecutionStore::new(root);
    let execution = load_execution(&store, execution_id)?;

    println!("Execution:     {}", execution.id);
    println!(
        "Workflow:      {} (version {})",
        execution.workflow_id, execution.workflow_version
    );
    println!("Status:        {}", status_name(&execution.status));
    println!(
        "Current state: {}",
        execution.current_state.as_deref().unwrap_or("-")
    );
    println!("Started:       {}", execution.started_at.to_rfc3339());
    if let Some(completed_at) = execution.completed_at {
        println!("Finished:      {}", completed_at.to_rfc3339());
    }
    println!("Workspace:     {}", execution.workspace.path);

    let context = store
        .load_context(execution_id)
        .unwrap_or_else(|_| execution.context.clone());

    println!("Context variables:");
    if context.variables.is_empty() {
        println!("  (none)");
    } else {
        let mut names: Vec<&String> = context.variables.keys().collect();
        names.sort();
        for name in names {
            let value = &context.variables[name];
            println!("  {name} = {value}");
        }
    }

    if let Some(pending) = pending_input(&store, execution_id)? {
        print_pending_input(&pending);
    }
    Ok(())
}

/// `orchestrator events <id> [--follow]` — the event log, in sequence order.
pub async fn events(root: &Path, execution_id: &str, follow: bool) -> Result<()> {
    let store = ExecutionStore::new(root);
    // Fails early and readably when the id is unknown.
    let mut execution = load_execution(&store, execution_id)?;

    let mut printed = 0usize;
    loop {
        let events = store
            .event_log(execution_id)
            .read_all()
            .map_err(|error| CliError::failure(format!("Could not read the events: {error}")))?;

        for event in events.iter().skip(printed) {
            print_event(event);
        }
        printed = events.len();

        if !follow || is_final(&execution.status) {
            break;
        }
        tokio::time::sleep(FOLLOW_INTERVAL).await;
        execution = load_execution(&store, execution_id)?;
    }
    Ok(())
}

/// One event line: sequence, timestamp, type, description.
fn print_event(event: &Event) {
    println!(
        "{:>5}  {}  {:<22} {}",
        event.sequence,
        event.timestamp.to_rfc3339(),
        event_type(event),
        event.description()
    );
}

/// The `type` tag serde writes for an event's data.
fn event_type(event: &Event) -> String {
    serde_json::to_value(&event.data)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// input / resume
// ---------------------------------------------------------------------------

/// `orchestrator input <id> [--field k=v]… [--label l]` — answer a question.
pub async fn input(
    root: &Path,
    execution_id: &str,
    fields: &[String],
    label: Option<String>,
    definition_file: Option<&str>,
) -> Result<()> {
    let store = ExecutionStore::new(root);
    let execution = load_execution(&store, execution_id)?;

    let Some(pending) = pending_input(&store, execution_id)? else {
        return Err(CliError::failure(format!(
            "Execution '{execution_id}' is not waiting for human input (status: {}).",
            status_name(&execution.status)
        )));
    };

    let mut values = Map::new();
    for pair in fields {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid field '{pair}' — expected the form name=value."
            ))
        })?;
        values.insert(name.to_string(), Value::String(raw.to_string()));
    }

    let mut delivery = HumanInputDelivery::with_values(values);
    if let Some(label) = label {
        delivery = delivery.choosing(label);
    }

    let definition = definition_for(root, &execution, definition_file)?;
    let engine = Engine::new(store);
    let execution = engine
        .deliver_human_input(&definition, &pending.execution_id, &delivery)
        .await
        .map_err(|error| CliError::failure(format!("Could not deliver the input: {error}")))?;

    println!("Input delivered to execution {}.", execution.id);
    print_execution_summary(&execution);
    if let Some(pending) = engine
        .pending_input(&execution.id)
        .map_err(|error| CliError::failure(format!("Engine error: {error}")))?
    {
        print_pending_input(&pending);
    }

    if execution.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            execution.id
        )));
    }
    Ok(())
}

/// `orchestrator resume <id>` — continue a paused execution.
pub async fn resume(root: &Path, execution_id: &str, definition_file: Option<&str>) -> Result<()> {
    let store = ExecutionStore::new(root);
    let execution = load_execution(&store, execution_id)?;
    let definition = definition_for(root, &execution, definition_file)?;

    let engine = Engine::new(store);
    let execution = engine
        .resume(&definition, execution_id)
        .await
        .map_err(|error| CliError::failure(format!("Could not resume the execution: {error}")))?;

    println!("Resumed execution {}.", execution.id);
    print_execution_summary(&execution);
    if execution.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            execution.id
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// repo
// ---------------------------------------------------------------------------

/// `orchestrator repo add <id> --remote <url|path> …` — register a repository.
///
/// The remote is checked with the workspace provisioner's own rules, so a
/// remote the registry accepts is one a workflow will be able to clone: a value
/// starting with `-` or using the `ext::` transport is refused here rather than
/// halfway through a run.
pub fn repo_add(
    root: &Path,
    id: &str,
    remote: &str,
    name: Option<String>,
    provider: Option<String>,
    default_branch: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let mut entry = RepositoryEntry::new(id, remote)
        .with_name(name.unwrap_or_else(|| id.to_string()))
        .with_provider(provider.unwrap_or_else(|| infer_provider(remote).to_string()));
    entry.default_branch = default_branch;
    entry.description = description;

    let registry = RepositoryRegistry::new(root);
    registry.add(entry).map_err(|error| {
        CliError::failure(format!("Could not register the repository: {error}"))
    })?;

    println!("Registered repository '{id}' -> {remote}");
    Ok(())
}

/// `orchestrator repo list` — every registered repository, ordered by id.
pub fn repo_list(root: &Path) -> Result<()> {
    let entries = RepositoryRegistry::new(root)
        .list()
        .map_err(|error| CliError::failure(format!("Could not read the registry: {error}")))?;

    if entries.is_empty() {
        println!("No repositories registered.");
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:<16} REMOTE",
        "ID", "PROVIDER", "DEFAULT BRANCH"
    );
    for entry in &entries {
        println!(
            "{:<24} {:<10} {:<16} {}",
            entry.id,
            entry.provider,
            entry.default_branch.as_deref().unwrap_or("-"),
            entry.remote
        );
    }
    Ok(())
}

/// `orchestrator repo show <id>` — every field of one registered repository.
pub fn repo_show(root: &Path, id: &str) -> Result<()> {
    let registry = RepositoryRegistry::new(root);
    let entry = registry
        .get(id)
        .map_err(|error| CliError::failure(format!("Could not read the registry: {error}")))?
        .ok_or_else(|| {
            CliError::failure(format!(
                "Repository '{id}' is not registered in '{}'.",
                registry.path().display()
            ))
        })?;

    println!("Repository:     {}", entry.id);
    println!("Name:           {}", entry.name);
    println!("Provider:       {}", entry.provider);
    println!("Remote:         {}", entry.remote);
    println!(
        "Default branch: {}",
        entry.default_branch.as_deref().unwrap_or("-")
    );
    println!(
        "Description:    {}",
        entry.description.as_deref().unwrap_or("-")
    );
    Ok(())
}

/// `orchestrator repo remove <id>` — drop a repository from the registry.
pub fn repo_remove(root: &Path, id: &str) -> Result<()> {
    RepositoryRegistry::new(root)
        .remove(id)
        .map_err(|error| CliError::failure(format!("Could not remove the repository: {error}")))?;

    println!("Removed repository '{id}'.");
    Ok(())
}

// ---------------------------------------------------------------------------
// workflow
// ---------------------------------------------------------------------------

/// `orchestrator workflow list` — every workflow reachable right now.
///
/// One line per *name*: where the copy that wins comes from, what it overrides
/// when it overrides something, and its `description`. A user copy shadowing a
/// built-in is one row marked as an override, never two rows that leave the
/// operator guessing which one would run.
pub fn workflow_list(root: &Path) -> Result<()> {
    let resolver = WorkflowResolver::new(root);
    let entries = resolver.list();

    if entries.is_empty() {
        println!("No workflows are reachable.");
        return Ok(());
    }

    println!("{:<28} {:<10} DESCRIPTION", "NAME", "SOURCE");
    for entry in &entries {
        let description = entry.description.as_deref().unwrap_or("-");
        println!(
            "{:<28} {:<10} {}",
            entry.name,
            entry.origin.label(),
            truncate(description, 80)
        );
        if entry.is_override() {
            println!("{:<28} {:<10} overrides: {}", "", "", entry.shadow_labels());
        }
        if let Some(path) = entry.origin.path() {
            println!("{:<28} {:<10} path: {}", "", "", path.display());
        }
    }
    Ok(())
}

/// `orchestrator workflow show <name>` — the YAML that would run.
///
/// The definition goes to stdout and nothing else does, so
/// `orchestrator workflow show compound-engineering > mine.yaml` is a copy;
/// where it came from is said on stderr.
pub fn workflow_show(root: &Path, reference: &str) -> Result<()> {
    let resolved = resolve_workflow(root, reference)?;
    eprintln!("Workflow '{}' — {}.", resolved.name, resolved.origin);
    print!("{}", resolved.source);
    if !resolved.source.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `orchestrator workflow eject <name> [--to <dir>] [--force]` — write a
/// built-in out as a file.
///
/// The default target is the user directory, which the resolver searches ahead
/// of the built-ins: the ejected copy takes precedence the moment it is
/// written, which is the whole point of ejecting one.
pub fn workflow_eject(root: &Path, name: &str, to: Option<&Path>, force: bool) -> Result<()> {
    let builtin = builtin(name).ok_or_else(|| {
        CliError::usage(format!(
            "There is no built-in workflow named '{}'. Available built-in workflows: {}.",
            crate::workflows::workflow_name(name),
            builtin_names().join(", ")
        ))
    })?;

    let resolver = WorkflowResolver::new(root);
    let directory = match to {
        Some(directory) => directory.to_path_buf(),
        None => resolver.user_dir().map(Path::to_path_buf).ok_or_else(|| {
            CliError::usage(
                "There is no user configuration directory (neither XDG_CONFIG_HOME nor HOME is \
                 set) — name a directory with --to."
                    .to_string(),
            )
        })?,
    };

    std::fs::create_dir_all(&directory).map_err(|error| {
        CliError::failure(format!(
            "Could not create '{}': {error}",
            directory.display()
        ))
    })?;

    let target = directory.join(format!("{}.yaml", builtin.name));
    if target.exists() && !force {
        return Err(CliError::failure(format!(
            "'{}' already exists — pass --force to overwrite it.",
            target.display()
        )));
    }

    std::fs::write(&target, builtin.source).map_err(|error| {
        CliError::failure(format!("Could not write '{}': {error}", target.display()))
    })?;

    println!("Wrote {}", target.display());
    match resolver.resolve_name(builtin.name) {
        Ok(resolved) if resolved.path() == Some(target.as_path()) => println!(
            "'{}' now resolves to this copy instead of the built-in.",
            builtin.name
        ),
        Ok(resolved) => println!(
            "Note: '{}' still resolves to {} — that copy has precedence over this one.",
            builtin.name, resolved.origin
        ),
        Err(_) => println!(
            "Note: '{}' is not on the search path from this data directory; \
             run it by path, or point --root at the directory holding it.",
            builtin.name
        ),
    }
    Ok(())
}

/// Shortens a description to one line of a listing.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

// ---------------------------------------------------------------------------
// tui (U10)
// ---------------------------------------------------------------------------

/// `orchestrator tui` — the full-screen interface, added in U10.
///
/// The loop is async but this entry point is not, because the subcommand
/// dispatch calls it from inside the runtime `main` already started. So the
/// current worker thread is handed over with `block_in_place` and the loop is
/// driven on it: spawning a second runtime inside the first would panic, and
/// the interface has to own a thread anyway — it blocks on keys.
///
/// The store is opened on the global `--root`, exactly like every other
/// subcommand: the interface shows the executions of the data directory the
/// operator asked for, not always those of the default one.
pub fn tui(root: &Path) -> Result<()> {
    let store = ExecutionStore::new(root);
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(move || handle.block_on(crate::tui::run(store))).map_err(|error| {
        CliError::failure(format!(
            "The terminal interface stopped with an error: {error}"
        ))
    })
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Loads an execution, turning "not found" into a readable message.
fn load_execution(store: &ExecutionStore, execution_id: &str) -> Result<WorkflowExecution> {
    store.load_execution(execution_id).map_err(|error| {
        CliError::failure(format!(
            "Could not load execution '{execution_id}': {error}"
        ))
    })
}

/// The pending question of an execution, if it has one.
fn pending_input(store: &ExecutionStore, execution_id: &str) -> Result<Option<PendingHumanInput>> {
    store
        .load_pending_input::<PendingHumanInput>(execution_id)
        .map_err(|error| CliError::failure(format!("Could not read the pending input: {error}")))
}

/// Resolves the definition an execution continues with.
///
/// `--file` wins; otherwise the execution's own record decides — the path it
/// was started from while that file is still there, and the workflow name
/// otherwise, which is the only thing a built-in leaves behind.
fn definition_for(
    root: &Path,
    execution: &WorkflowExecution,
    override_reference: Option<&str>,
) -> Result<WorkflowDefinition> {
    let resolved = match override_reference {
        Some(reference) => resolve_workflow(root, reference)?,
        None => tasks::resolve_definition(root, execution)
            .map_err(|error| CliError::failure(error.to_string()))?,
    };

    resolved.load().map_err(|error| {
        CliError::failure(format!(
            "Could not load definition '{}' ({}): {error}",
            resolved.name, resolved.origin
        ))
    })
}

/// Resolves a workflow reference, turning a miss into the message that names
/// every place that was tried.
fn resolve_workflow(root: &Path, reference: &str) -> Result<ResolvedWorkflow> {
    WorkflowResolver::new(root)
        .resolve(reference)
        .map_err(|error| CliError::usage(error.to_string()))
}

/// Status, current state and — when it stopped — the outcome.
fn print_execution_summary(execution: &WorkflowExecution) {
    println!("Status: {}", status_name(&execution.status));
    println!(
        "Current state: {}",
        execution.current_state.as_deref().unwrap_or("-")
    );
}

/// The question an execution is waiting on, as a form the operator can fill in.
fn print_pending_input(pending: &PendingHumanInput) {
    println!("Human input expected: {}", pending.instruction());
    for field in pending.fields() {
        let requirement = if field.required {
            "required"
        } else {
            "optional"
        };
        let options = if field.options.is_empty() {
            String::new()
        } else {
            format!(", allowed values: {}", field.options.join(", "))
        };
        println!(
            "  --field {}=…  ({:?}, {requirement}{options})",
            field.name, field.field_type
        );
    }
    if !pending.outputs.is_empty() {
        println!("  Output labels: {}", pending.outputs.join(", "));
    }
}

/// Whether an execution can still move.
fn is_final(status: &ExecutionStatus) -> bool {
    matches!(
        status,
        ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
    )
}

/// The schema spelling of a status.
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
