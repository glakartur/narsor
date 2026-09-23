//! Implementation of every subcommand.
//!
//! # Where a `run` gets its workspace
//!
//! An execution needs a directory to work in before its first action starts.
//! Three cases, in this order:
//!
//! * the definition declares `globals.workspace: { type: git, ... }` — the
//!   `repository` field may still hold a `{{ inputs.* }}` template, so it is
//!   expanded against the inputs first and the result is handed to
//!   [`WorkspaceProvisioner`] as a git remote (anything `git clone` accepts,
//!   including a local path);
//! * the definition declares `{ type: local, path: ... }` — the path is
//!   expanded the same way and its contents are copied into the workspace;
//! * the definition declares no workspace at all — the CLI creates an empty
//!   directory `{root}/tasks/{execution_id}/code/workspace` and uses that. A
//!   workflow whose scripts live outside the repository (the usual shape of a
//!   smoke test) needs nothing more, and an empty directory is a better default
//!   than refusing to run.
//!
//! Provisioning is deliberately split around [`crate::engine::Engine::start`]:
//! everything checkable without touching the disk
//! ([`WorkspaceProvisioner::validate`]) runs *before* the execution is created,
//! so a bad remote leaves nothing behind; the actual clone runs *after*, because
//! the task directory is named for the execution id (`DOMAIN_MODEL.md` §2.8) and
//! that id only exists once the execution does. The execution record is then
//! saved again with the real workspace. If the clone fails, the freshly created
//! execution is marked `Failed` rather than left `Running` forever.
//!
//! # How `input` and `resume` find the definition
//!
//! The engine needs the [`WorkflowDefinition`] to continue an execution, but
//! `state.json` stores only the workflow *id*. So `run` records the absolute
//! path of the file it was given in `metadata.cli.definition_path`, and the
//! later commands read it back. `--file` overrides it, for the case where the
//! definition has moved.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::domain::{
    Context, Event, ExecutionStatus, InputParameter, ParamType, WorkflowDefinition,
    WorkflowExecution, Workspace, WorkspaceInfo, WorkspaceSpec,
};
use crate::engine::{Engine, HumanInputDelivery, PendingHumanInput};
use crate::persistence::ExecutionStore;
use crate::workflow::{load_from_file, validate_path, TemplateScope};
use crate::workspace::{RepositorySpec, WorkspaceProvisioner, WorkspaceRequest};

use super::{EXIT_FAILURE, EXIT_USAGE};

/// Key of the CLI's own scratch area inside `WorkflowExecution::metadata`.
const METADATA_CLI_KEY: &str = "cli";

/// Key of the recorded definition path inside that scratch area.
const METADATA_DEFINITION_PATH_KEY: &str = "definition_path";

/// Directory name of the fallback workspace, used when the definition declares
/// none.
const FALLBACK_WORKSPACE_NAME: &str = "workspace";

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

/// `orchestrator validate <plik>` — static validation, all violations at once.
pub fn validate(file: &Path) -> Result<()> {
    let violations = validate_path(file)
        .map_err(|error| CliError::failure(format!("Nie udało się wczytać definicji: {error}")))?;

    if !violations.is_empty() {
        let mut message = format!(
            "Definicja '{}' jest niepoprawna — znaleziono {} {}:",
            file.display(),
            violations.len(),
            violation_noun(violations.len())
        );
        for violation in &violations {
            message.push_str(&format!("\n  - {violation}"));
        }
        return Err(CliError::failure(message));
    }

    let definition = load_from_file(file)
        .map_err(|error| CliError::failure(format!("Nie udało się wczytać definicji: {error}")))?;

    println!(
        "Definicja jest poprawna: {} (wersja {}), stanów: {}, stan początkowy: {}.",
        definition.name,
        definition.version,
        definition.states.len(),
        definition.initial_state
    );
    Ok(())
}

/// Polish noun agreement for the violation count.
fn violation_noun(count: usize) -> &'static str {
    let last_two = count % 100;
    let last = count % 10;
    if count == 1 {
        "naruszenie"
    } else if (2..=4).contains(&last) && !(12..=14).contains(&last_two) {
        "naruszenia"
    } else {
        "naruszeń"
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// `orchestrator run <plik> [--input k=v]…` — create an execution and run it.
pub async fn run(root: &Path, file: &Path, inputs: &[String]) -> Result<()> {
    let violations = validate_path(file)
        .map_err(|error| CliError::failure(format!("Nie udało się wczytać definicji: {error}")))?;
    if !violations.is_empty() {
        let mut message = String::from("Definicja jest niepoprawna — wykonanie nie zostało utworzone:");
        for violation in &violations {
            message.push_str(&format!("\n  - {violation}"));
        }
        return Err(CliError::failure(message));
    }

    let definition = load_from_file(file)
        .map_err(|error| CliError::failure(format!("Nie udało się wczytać definicji: {error}")))?;

    // Inputs first: a missing required parameter must be reported before
    // anything is written to disk.
    let inputs = collect_inputs(&definition, inputs)?;

    let provisioner = WorkspaceProvisioner::new(root);
    let request = workspace_request(&definition, &inputs)?;
    if let Some(request) = &request {
        provisioner.validate(request).map_err(|error| {
            CliError::failure(format!("Workspace nie może zostać utworzony: {error}"))
        })?;
    }

    let engine = Engine::new(ExecutionStore::new(root));
    let mut execution = engine
        .start(
            &definition,
            Value::Object(inputs),
            // Replaced below, once the execution id names the task directory.
            Workspace::local_folder(root.display().to_string()),
        )
        .map_err(|error| CliError::failure(format!("Nie udało się utworzyć wykonania: {error}")))?;

    println!("Utworzono wykonanie: {}", execution.id);

    match provision_workspace(&provisioner, &execution.id, request.as_ref()) {
        Ok(workspace) => {
            execution.context.workspace = Some(WorkspaceInfo {
                id: workspace.id.clone(),
                path: workspace.path.clone(),
                workspace_type: format!("{:?}", workspace.workspace_type).to_lowercase(),
            });
            execution.workspace = workspace;
            record_definition_path(&mut execution, file);
            engine.store().save_execution(&execution).map_err(|error| {
                CliError::failure(format!("Nie udało się zapisać wykonania: {error}"))
            })?;
        }
        Err(message) => {
            abandon_execution(engine.store(), &mut execution, &message);
            return Err(CliError::failure(message));
        }
    }

    let execution = engine
        .run(&definition, &execution.id)
        .await
        .map_err(|error| CliError::failure(format!("Błąd silnika: {error}")))?;

    print_execution_summary(&execution);
    if let Some(pending) = engine
        .pending_input(&execution.id)
        .map_err(|error| CliError::failure(format!("Błąd silnika: {error}")))?
    {
        print_pending_input(&pending);
    }

    if execution.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Wykonanie {} zakończyło się niepowodzeniem.",
            execution.id
        )));
    }
    Ok(())
}

/// Parses `--input` pairs against the declared parameters, fills in defaults and
/// reports everything that is missing.
fn collect_inputs(
    definition: &WorkflowDefinition,
    pairs: &[String],
) -> Result<Map<String, Value>> {
    let mut values = Map::new();

    for pair in pairs {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Nieprawidłowe wejście '{pair}' — oczekiwano postaci nazwa=wartość."
            ))
        })?;

        let parameter = definition
            .inputs
            .iter()
            .find(|parameter| parameter.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "Workflow '{}' nie deklaruje wejścia '{name}'. Zadeklarowane wejścia: {}.",
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
            "Brakuje wymaganych wejść: {}. Podaj je opcją --input nazwa=wartość.",
            missing.join(", ")
        )));
    }

    Ok(values)
}

/// The declared input names, for an error message.
fn declared_input_names(definition: &WorkflowDefinition) -> String {
    if definition.inputs.is_empty() {
        return "(brak)".to_string();
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
                "Wejście '{}' jest typu integer, a '{raw}' nie jest liczbą całkowitą.",
                parameter.name
            ))
        }),
        ParamType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(CliError::usage(format!(
                "Wejście '{}' jest typu boolean — dozwolone wartości to 'true' i 'false', podano '{raw}'.",
                parameter.name
            ))),
        },
    }
}

/// Builds the workspace request from the definition, expanding templates in it.
///
/// `None` means the definition declares no workspace and the fallback directory
/// is used.
fn workspace_request(
    definition: &WorkflowDefinition,
    inputs: &Map<String, Value>,
) -> Result<Option<WorkspaceRequest>> {
    let Some(spec) = &definition.workspace else {
        return Ok(None);
    };

    let context = Context::new();
    let scope = TemplateScope::new(inputs, &context);
    let render = |template: &str, field: &str| -> Result<String> {
        scope
            .render(template, &format!("globals.workspace.{field}"))
            .map_err(|error| {
                CliError::failure(format!("Nie udało się rozwinąć szablonu workspace: {error}"))
            })
    };

    let spec = match spec {
        WorkspaceSpec::Git {
            repository,
            git_ref,
        } => {
            let remote = render(repository, "repository")?;
            let git_ref = match git_ref {
                Some(git_ref) => Some(render(git_ref, "ref")?),
                None => None,
            };
            RepositorySpec::git(repository_name(&remote), remote, git_ref)
        }
        WorkspaceSpec::Local { path } => {
            let path = render(path, "path")?;
            RepositorySpec::local(repository_name(&path), PathBuf::from(path))
        }
    };

    Ok(Some(WorkspaceRequest::new(spec)))
}

/// The directory name a remote or path is checked out into.
fn repository_name(remote: &str) -> String {
    let trimmed = remote.trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() || name == "." || name == ".." {
        FALLBACK_WORKSPACE_NAME.to_string()
    } else {
        name.to_string()
    }
}

/// Materialises the workspace of an execution, or the fallback directory.
fn provision_workspace(
    provisioner: &WorkspaceProvisioner,
    execution_id: &str,
    request: Option<&WorkspaceRequest>,
) -> std::result::Result<Workspace, String> {
    match request {
        Some(request) => provisioner
            .provision(execution_id, request)
            .map(|provisioned| provisioned.workspace)
            .map_err(|error| format!("Nie udało się utworzyć workspace: {error}")),
        None => {
            let path = provisioner
                .code_dir(execution_id)
                .join(FALLBACK_WORKSPACE_NAME);
            fs::create_dir_all(&path)
                .map_err(|error| format!("Nie udało się utworzyć katalogu workspace: {error}"))?;
            Ok(Workspace::local_folder(path.display().to_string()))
        }
    }
}

/// Records the definition path so `input` and `resume` can find it again.
fn record_definition_path(execution: &mut WorkflowExecution, file: &Path) {
    let path = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if !execution.metadata.is_object() {
        execution.metadata = json!({});
    }
    if let Some(metadata) = execution.metadata.as_object_mut() {
        metadata.insert(
            METADATA_CLI_KEY.to_string(),
            json!({ METADATA_DEFINITION_PATH_KEY: path.display().to_string() }),
        );
    }
}

/// Marks an execution that never got a workspace as failed.
///
/// Leaving it `Running` would make it look resumable when nothing can ever
/// step it: it has no directory to run in.
fn abandon_execution(store: &ExecutionStore, execution: &mut WorkflowExecution, message: &str) {
    execution.status = ExecutionStatus::Failed;
    execution.completed_at = Some(chrono::Utc::now());
    let _ = store.event_log(&execution.id).append_next(
        &execution.id,
        crate::domain::EventData::Error {
            message: message.to_string(),
            context: json!({ "stage": "workspace_provisioning" }),
        },
    );
    let _ = store.save_execution(execution);
}

// ---------------------------------------------------------------------------
// list / show / events
// ---------------------------------------------------------------------------

/// `orchestrator list` — every execution in the store, newest first.
pub fn list(root: &Path) -> Result<()> {
    let store = ExecutionStore::new(root);
    let ids = store
        .list_execution_ids()
        .map_err(|error| CliError::failure(format!("Nie udało się odczytać wykonań: {error}")))?;

    if ids.is_empty() {
        println!("Brak wykonań.");
        return Ok(());
    }

    let mut executions = Vec::new();
    for id in ids {
        match store.load_execution(&id) {
            Ok(execution) => executions.push(execution),
            Err(error) => eprintln!("Pomijam wykonanie '{id}': {error}"),
        }
    }
    executions.sort_by(|left, right| right.started_at.cmp(&left.started_at));

    println!(
        "{:<38} {:<10} {:<22} {}",
        "ID", "STATUS", "STAN", "WORKFLOW"
    );
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

    println!("Wykonanie:     {}", execution.id);
    println!(
        "Workflow:      {} (wersja {})",
        execution.workflow_id, execution.workflow_version
    );
    println!("Status:        {}", status_name(&execution.status));
    println!(
        "Bieżący stan:  {}",
        execution.current_state.as_deref().unwrap_or("-")
    );
    println!("Rozpoczęto:    {}", execution.started_at.to_rfc3339());
    if let Some(completed_at) = execution.completed_at {
        println!("Zakończono:    {}", completed_at.to_rfc3339());
    }
    println!("Workspace:     {}", execution.workspace.path);

    let context = store
        .load_context(execution_id)
        .unwrap_or_else(|_| execution.context.clone());

    println!("Zmienne kontekstu:");
    if context.variables.is_empty() {
        println!("  (brak)");
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
            .map_err(|error| CliError::failure(format!("Nie udało się odczytać zdarzeń: {error}")))?;

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
    definition_file: Option<&Path>,
) -> Result<()> {
    let store = ExecutionStore::new(root);
    let execution = load_execution(&store, execution_id)?;

    let Some(pending) = pending_input(&store, execution_id)? else {
        return Err(CliError::failure(format!(
            "Wykonanie '{execution_id}' nie czeka na wejście człowieka (status: {}).",
            status_name(&execution.status)
        )));
    };

    let mut values = Map::new();
    for pair in fields {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Nieprawidłowe pole '{pair}' — oczekiwano postaci nazwa=wartość."
            ))
        })?;
        values.insert(name.to_string(), Value::String(raw.to_string()));
    }

    let mut delivery = HumanInputDelivery::with_values(values);
    if let Some(label) = label {
        delivery = delivery.choosing(label);
    }

    let definition = definition_for(&execution, definition_file)?;
    let engine = Engine::new(store);
    let execution = engine
        .deliver_human_input(&definition, &pending.execution_id, &delivery)
        .await
        .map_err(|error| CliError::failure(format!("Nie udało się dostarczyć wejścia: {error}")))?;

    println!("Dostarczono wejście do wykonania {}.", execution.id);
    print_execution_summary(&execution);
    if let Some(pending) = engine
        .pending_input(&execution.id)
        .map_err(|error| CliError::failure(format!("Błąd silnika: {error}")))?
    {
        print_pending_input(&pending);
    }

    if execution.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Wykonanie {} zakończyło się niepowodzeniem.",
            execution.id
        )));
    }
    Ok(())
}

/// `orchestrator resume <id>` — continue a paused execution.
pub async fn resume(root: &Path, execution_id: &str, definition_file: Option<&Path>) -> Result<()> {
    let store = ExecutionStore::new(root);
    let execution = load_execution(&store, execution_id)?;
    let definition = definition_for(&execution, definition_file)?;

    let engine = Engine::new(store);
    let execution = engine
        .resume(&definition, execution_id)
        .await
        .map_err(|error| CliError::failure(format!("Nie udało się wznowić wykonania: {error}")))?;

    println!("Wznowiono wykonanie {}.", execution.id);
    print_execution_summary(&execution);
    if execution.status == ExecutionStatus::Failed {
        return Err(CliError::failure(format!(
            "Wykonanie {} zakończyło się niepowodzeniem.",
            execution.id
        )));
    }
    Ok(())
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
/// The store is the default root: the [`Command::Tui`](crate::cli::Command::Tui)
/// subcommand takes no arguments of its own and this function takes none
/// either, so `--root` does not reach it. That is the one place where the TUI
/// is less capable than the rest of the CLI.
pub fn tui() -> Result<()> {
    let store = ExecutionStore::with_default_root();
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(move || handle.block_on(crate::tui::run(store))).map_err(|error| {
        CliError::failure(format!("The terminal interface stopped with an error: {error}"))
    })
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Loads an execution, turning "not found" into a readable message.
fn load_execution(store: &ExecutionStore, execution_id: &str) -> Result<WorkflowExecution> {
    store.load_execution(execution_id).map_err(|error| {
        CliError::failure(format!(
            "Nie udało się wczytać wykonania '{execution_id}': {error}"
        ))
    })
}

/// The pending question of an execution, if it has one.
fn pending_input(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<Option<PendingHumanInput>> {
    store
        .load_pending_input::<PendingHumanInput>(execution_id)
        .map_err(|error| {
            CliError::failure(format!(
                "Nie udało się odczytać oczekującego wejścia: {error}"
            ))
        })
}

/// Resolves the definition an execution continues with.
fn definition_for(
    execution: &WorkflowExecution,
    override_path: Option<&Path>,
) -> Result<WorkflowDefinition> {
    let path = match override_path {
        Some(path) => path.to_path_buf(),
        None => stored_definition_path(execution).ok_or_else(|| {
            CliError::usage(format!(
                "Wykonanie '{}' nie ma zapisanej ścieżki definicji — wskaż plik opcją --file.",
                execution.id
            ))
        })?,
    };

    load_from_file(&path).map_err(|error| {
        CliError::failure(format!(
            "Nie udało się wczytać definicji '{}': {error}",
            path.display()
        ))
    })
}

/// The definition path `run` recorded in the execution metadata.
fn stored_definition_path(execution: &WorkflowExecution) -> Option<PathBuf> {
    execution
        .metadata
        .get(METADATA_CLI_KEY)?
        .get(METADATA_DEFINITION_PATH_KEY)?
        .as_str()
        .map(PathBuf::from)
}

/// Status, current state and — when it stopped — the outcome.
fn print_execution_summary(execution: &WorkflowExecution) {
    println!("Status: {}", status_name(&execution.status));
    println!(
        "Bieżący stan: {}",
        execution.current_state.as_deref().unwrap_or("-")
    );
}

/// The question an execution is waiting on, as a form the operator can fill in.
fn print_pending_input(pending: &PendingHumanInput) {
    println!("Oczekiwane wejście człowieka: {}", pending.instruction());
    for field in pending.fields() {
        let requirement = if field.required {
            "wymagane"
        } else {
            "opcjonalne"
        };
        let options = if field.options.is_empty() {
            String::new()
        } else {
            format!(", dozwolone wartości: {}", field.options.join(", "))
        };
        println!(
            "  --field {}=…  ({:?}, {requirement}{options})",
            field.name, field.field_type
        );
    }
    if !pending.outputs.is_empty() {
        println!("  Etykiety wyjścia: {}", pending.outputs.join(", "));
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
