//! `input`: answering an execution that waits for a person.

use serde_json::{Map, Value};

use crate::api::{ClientError, ExecutionStatusDto, ExecutionSummary, HumanInputDeliveryDto};
use crate::config::{Flags, Resolved};

use super::daemon_access::{
    client_error, daemon_client, load_execution_summary, wait_until_stopped,
};
use super::open_matters::print_open_matters;
use super::{CliError, Result};

/// What `narsor input` was told on its command line.
#[derive(Debug, Clone, Default)]
pub struct InputAnswer {
    /// `--field name=value` pairs.
    pub fields: Vec<String>,
    /// `--label`.
    pub label: Option<String>,
    /// `--artifact name=path` pairs.
    pub artifacts: Vec<String>,
    /// `--variable name=value` pairs.
    pub variables: Vec<String>,
    /// `--comment`.
    pub comment: Option<String>,
    /// `--permission` choice for a Claude Code permission wait.
    pub permission: Option<String>,
    /// `--denial-index` within the pending permission request; every denial
    /// when `None`.
    pub denial_index: Option<usize>,
    /// `--pattern` for `--permission allow-pattern`.
    pub pattern: Option<String>,
}

/// The permission decision `--permission` names, with its `--denial-index`,
/// `--pattern` and `--comment` (the agent reads it); `None` without
/// `--permission`.
pub(in crate::cli) fn permission_decision(
    permission: Option<&str>,
    denial_index: Option<usize>,
    pattern: Option<&str>,
    comment: Option<&str>,
) -> Result<Option<narsor_protocol::api::PermissionDecisionDto>> {
    use narsor_protocol::api::{
        PermissionDecisionKindDto as Kind, PermissionGrantScopeDto as Scope,
    };
    let Some(permission) = permission else {
        return match pattern {
            Some(_) => Err(CliError::usage(
                "--pattern needs --permission allow-pattern.",
            )),
            None => Ok(None),
        };
    };
    let (decision, scope) = match permission {
        "deny" => (Kind::Deny, None),
        "allow-exact" => (Kind::Allow, Some(Scope::ExactOperation)),
        "allow-group" => (Kind::Allow, Some(Scope::CommandGroup)),
        _ => (Kind::Allow, Some(Scope::Pattern)),
    };
    if (scope == Some(Scope::Pattern)) != pattern.is_some() {
        return Err(CliError::usage(
            "--permission allow-pattern needs --pattern, and --pattern needs allow-pattern.",
        ));
    }
    Ok(Some(narsor_protocol::api::PermissionDecisionDto {
        denial_index,
        decision,
        scope,
        pattern: pattern.map(str::to_string),
        message: comment.map(str::to_string),
    }))
}

/// `narsor input <id> [--field k=v]… [--label l] [--data k=v]… [--variable k=v]…
/// [--comment text]` — answer a question, or resolve a problem.
///
/// Goes through `ApiClient` (R4/D52): `POST /executions/:id/input`, which
/// hands the answer to the execution's task, then [`wait_until_stopped`].
///
/// `--file` has no REST equivalent: the task carries its sealed workflow,
/// and a REST call cannot point the server at a local override path.
///
/// The answer is bound to the request this command read (KTD9): if another
/// client answered or closed it in between, the daemon refuses this one
/// rather than letting it land on whatever the execution waits on next.
pub async fn input(
    config: &Resolved,
    flags: &Flags,
    execution_id: &str,
    answer: InputAnswer,
    definition_file: Option<&str>,
) -> Result<()> {
    if let Some(file) = definition_file {
        return Err(CliError::usage(format!(
            "'--file {file}' is not supported for 'input': the daemon always resolves the \
             workflow from the execution's own record, with no way for a REST call to point it \
             at a local override path."
        )));
    }

    let mut values = Map::new();
    for pair in &answer.fields {
        let (name, raw) = pair.split_once('=').ok_or_else(|| {
            CliError::usage(format!(
                "Invalid field '{pair}' — expected the form name=value."
            ))
        })?;
        values.insert(name.to_string(), Value::String(raw.to_string()));
    }

    let mut delivery = HumanInputDeliveryDto {
        values,
        artifacts: artifact_paths(&answer.artifacts)?,
        variables: step_values("variable", &answer.variables)?,
        chosen_label: answer.label,
        comment: answer.comment,
        ..HumanInputDeliveryDto::default()
    };

    let client = daemon_client(config.state_dir(), flags).await?;

    // Checked here, not left to the server: an execution that is not
    // awaiting human input is a caller mistake, not an engine failure, and
    // deserves the same readable "not waiting (status: X)" message this
    // command always gave — `EngineError`'s mapping to `ApiError` (U3) does
    // not single this case out from every other "engine can't proceed"
    // reason, so relying on the REST error alone would only report a bare
    // `500`.
    let current = load_execution_summary(&client, execution_id).await?;
    let Some(pending) = &current.pending_input else {
        return Err(CliError::failure(format!(
            "Execution '{execution_id}' is not waiting for human input (status: {}).",
            current.status.as_str()
        )));
    };
    delivery.episode = Some(pending.episode_key().to_string());

    let permission_wait =
        pending.continuation == narsor_protocol::api::InputContinuationView::ResolvePermission;
    if permission_wait {
        let permission_decision = permission_decision(
            answer.permission.as_deref(),
            answer.denial_index,
            answer.pattern.as_deref(),
            delivery.comment.as_deref(),
        )?
        .ok_or_else(|| {
            CliError::usage(
                "This execution needs a permission decision. Use --permission deny, allow-exact, allow-group, or allow-pattern.",
            )
        })?;
        client
            .resolve_permission(
                execution_id,
                narsor_protocol::api::PermissionDecisionRequestDto {
                    episode: pending.episode_key().to_string(),
                    permission_decision,
                },
            )
            .await
            .map_err(|error| client_error(execution_id, error))?;
        wait_until_stopped(&client, execution_id, current.status).await?;
        return Ok(());
    }
    if answer.permission.is_some() {
        return Err(CliError::usage(
            "--permission can only answer a permission request.",
        ));
    }

    client
        .deliver_input(execution_id, delivery)
        .await
        .map_err(|error| match error {
            ClientError::Conflict => CliError::failure(format!(
                "Execution '{execution_id}' is no longer waiting for the input this answer was \
                 for: it was answered or closed meanwhile, or another process is driving it \
                 right now. Check it with 'narsor show {execution_id}'."
            )),
            other => client_error(execution_id, other),
        })?;

    let summary = wait_until_stopped(&client, execution_id, current.status).await?;

    println!("Input delivered to execution {}.", summary.id);
    print_summary(&summary);
    print_open_matters(&summary);
    if summary.status == ExecutionStatusDto::Failed {
        return Err(CliError::failure(format!(
            "Execution {} failed.",
            summary.id
        )));
    }
    Ok(())
}

/// Parses `name=value` pairs of `--variable` into a map. A value that reads
/// as JSON is taken as that, so `units_completed=3` is a number; anything
/// else is the text as written.
fn step_values(flag: &str, pairs: &[String]) -> Result<Map<String, Value>> {
    let mut values = Map::new();
    for (name, raw) in step_pairs(flag, pairs)? {
        let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        values.insert(name, value);
    }
    Ok(values)
}

/// Parses `name=path` pairs of `--artifact` into the step's artifacts.
fn artifact_paths(pairs: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    Ok(step_pairs("artifact", pairs)?
        .into_iter()
        .map(|(name, path)| (name, path.to_string()))
        .collect())
}

/// Splits `name=value` pairs of `--{flag}`; a usage error for one without a
/// name.
fn step_pairs<'a>(flag: &str, pairs: &'a [String]) -> Result<Vec<(String, &'a str)>> {
    pairs
        .iter()
        .map(|pair| {
            pair.split_once('=')
                .filter(|(name, _)| !name.trim().is_empty())
                .map(|(name, raw)| (name.trim().to_string(), raw))
                .ok_or_else(|| {
                    CliError::usage(format!(
                        "Invalid --{flag} '{pair}' — expected the form name=value."
                    ))
                })
        })
        .collect()
}

/// The REST counterpart of the old, in-process
/// `print_execution_summary(&WorkflowExecution)`: same two lines, from
/// an [`ExecutionSummary`] the daemon answered with instead of a
/// [`WorkflowExecution`] read straight off the store.
fn print_summary(summary: &ExecutionSummary) {
    print_status_line(&summary.status, summary.current_state.as_deref());
}

/// The two lines both of the above print, factored out so `run`/`input`
/// print the same regardless of which of the two shapes above they hold.
fn print_status_line(status: &ExecutionStatusDto, current_state: Option<&str>) {
    println!("Status: {}", status.as_str());
    println!("Current state: {}", current_state.unwrap_or("-"));
}
