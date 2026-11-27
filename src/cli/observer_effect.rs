//! The effect commands of an observer's script: `narsor-cli task create
//! --data JSON` (R3, PO1) and `narsor-cli task decide TASK --episode EP`
//! (R12, R13).
//!
//! The worker runs a script with `NARSOR_OBSERVER_RUN` (the run's id),
//! `NARSOR_SERVER` and `NARSOR_TOKEN` (the run's token) set. With
//! `NARSOR_OBSERVER_RUN` set, the command goes to the effect routes of that
//! run (`/observer/v1/runs/{run}/…`) and nowhere else:
//!
//! ```text
//!                          NARSOR_OBSERVER_RUN set          not set
//! task create [WORKFLOW] [--repository W=ID] [--branch W=B] --data
//!                          effect route                     the owner's create
//! task decide              effect route                     usage error
//! ```
//!
//! `task decide` takes `--choice CHOICE [--reason TEXT] [--var NAME=VALUE …]
//! [--var-file NAME=PATH …]`, or `--data JSON` with `{choice, reason,
//! variables}` (the answer of a script step), never both.
//!
//! The server does not deduplicate tasks: every call creates one. The
//! script keeps track in its state of what it already reported.
//!
//! What the script sees:
//!
//! | answer | prints | exit code |
//! | --- | --- | --- |
//! | created, delivered | the server's line | 0 |
//! | task limit reached, refused data, refused answer (stale episode, another answer first, invalid choice or variables), token not valid | the reason, on stderr | 1 |
//! | usage error | the reason, on stderr | 2 |

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::commands::{CliError, Result};
use super::queue_text::effect_output;
use super::remote::{failure, parse_workspaces, split_pair, TaskCommand};
use crate::api::{ApiClient, ClientError, ServerTarget, TokenSource};
use crate::controller_protocol::{
    ObserverCreateTaskRequest, ObserverDecideRequest, ObserverEffectResponse,
};

/// The environment variable naming the observer run a script belongs to.
pub const OBSERVER_RUN_ENV: &str = "NARSOR_OBSERVER_RUN";

/// An effect a script asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectCall {
    /// `task create --data JSON`.
    CreateTask(ObserverCreateTaskRequest),
    /// `task decide TASK --episode EP …`.
    Decide(ObserverDecideRequest),
}

/// What `command` asks for given the observer run of the environment
/// (`NARSOR_OBSERVER_RUN`, `None` when unset or empty): an effect of that
/// run, nothing of the kind (`Ok(None)`, the command goes its usual way),
/// or a usage error (see the module documentation).
pub fn effect_call(
    command: &TaskCommand,
    observer_run: Option<&str>,
) -> Result<Option<EffectCall>> {
    let observer_run = observer_run.filter(|run| !run.trim().is_empty());
    match (command, observer_run) {
        (TaskCommand::Create { data, .. }, None) if data.is_some() => {
            Err(CliError::usage(format!(
                "--data is for the script of an observer run only: {OBSERVER_RUN_ENV} is not set"
            )))
        }
        (TaskCommand::Decide { .. }, None) => Err(CliError::usage(format!(
            "narsor-cli task decide is for the script of an observer run only: \
             {OBSERVER_RUN_ENV} is not set"
        ))),
        (
            TaskCommand::Decide {
                task_id,
                episode,
                choice,
                reason,
                vars,
                var_files,
                data,
            },
            Some(_),
        ) => Ok(Some(EffectCall::Decide(decide_request(
            task_id, episode, choice, reason, vars, var_files, data,
        )?))),
        (
            TaskCommand::Create {
                workflow,
                file,
                workflow_version,
                inputs,
                repositories,
                branches,
                commits,
                backend,
                profile,
                harness,
                task_id,
                data,
            },
            Some(_),
        ) => {
            let chosen: Vec<&str> = [
                (file.is_some(), "--file"),
                (workflow_version.is_some(), "--workflow-version"),
                (!inputs.is_empty(), "--input"),
                (!commits.is_empty(), "--commit"),
                (backend.is_some(), "--backend"),
                (profile.is_some(), "--profile"),
                (harness.is_some(), "--harness"),
                (task_id.is_some(), "--task-id"),
            ]
            .into_iter()
            .filter_map(|(given, name)| given.then_some(name))
            .collect();
            if !chosen.is_empty() {
                return Err(CliError::usage(format!(
                    "In an observer's script, task create takes only WORKFLOW, --repository, \
                     --branch and --data: the queue decides the backend and profile, and \
                     its mapping the inputs (refused: {})",
                    chosen.join(", ")
                )));
            }
            Ok(Some(EffectCall::CreateTask(ObserverCreateTaskRequest {
                data: parse_data(data.as_deref())?,
                workflow_id: workflow.clone(),
                workspaces: parse_workspaces(repositories, branches, &[])?,
            })))
        }
        _ => Ok(None),
    }
}

/// `--data`, which must be JSON; absent data is `null`.
fn parse_data(raw: Option<&str>) -> Result<Value> {
    match raw {
        None => Ok(Value::Null),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|error| CliError::usage(format!("Invalid --data — expected JSON: {error}"))),
    }
}

/// The answer of a script step (`{choice, reason, variables}`), as `task
/// decide --data` takes it. No artifacts in this version.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideData {
    choice: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    variables: Map<String, Value>,
}

/// The answer `task decide` sends: from `--data`, or from `--choice` and
/// its options, never both.
fn decide_request(
    task_id: &str,
    episode: &str,
    choice: &Option<String>,
    reason: &Option<String>,
    vars: &[String],
    var_files: &[String],
    data: &Option<String>,
) -> Result<ObserverDecideRequest> {
    let request = |choice: String, reason: Option<String>, variables| ObserverDecideRequest {
        task_id: task_id.to_string(),
        episode: episode.to_string(),
        choice,
        reason,
        variables,
    };
    if let Some(raw) = data {
        let given: Vec<&str> = [
            (choice.is_some(), "--choice"),
            (reason.is_some(), "--reason"),
            (!vars.is_empty(), "--var"),
            (!var_files.is_empty(), "--var-file"),
        ]
        .into_iter()
        .filter_map(|(given, name)| given.then_some(name))
        .collect();
        if !given.is_empty() {
            return Err(CliError::usage(format!(
                "task decide takes either --data or --choice with its options, not both \
                 (refused: --data with {})",
                given.join(", ")
            )));
        }
        let data: DecideData = serde_json::from_str(raw).map_err(|error| {
            CliError::usage(format!(
                "Invalid --data — expected JSON {{\"choice\", \"reason\", \"variables\"}}: {error}"
            ))
        })?;
        return Ok(request(data.choice, data.reason, data.variables));
    }
    let Some(choice) = choice else {
        return Err(CliError::usage(
            "task decide needs --choice CHOICE, or --data JSON with the whole answer",
        ));
    };
    let mut variables = Map::new();
    for pair in vars {
        let (name, value) = split_pair("var", pair)?;
        variables.insert(name.to_string(), Value::String(value.to_string()));
    }
    for pair in var_files {
        let (name, path) = split_pair("var-file", pair)?;
        let text = std::fs::read_to_string(path).map_err(|error| {
            CliError::usage(format!("Cannot read --var-file {name}={path}: {error}"))
        })?;
        variables.insert(name.to_string(), Value::String(text));
    }
    Ok(request(choice.clone(), reason.clone(), variables))
}

/// The effect routes of one run, as the script's commands use them: the
/// API client in `narsor-cli`, the service itself in tests.
pub trait EffectApi: Send + Sync {
    /// `POST /observer/v1/runs/{run}/tasks`.
    fn create_task<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverCreateTaskRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverEffectResponse, ClientError>>;

    /// `POST /observer/v1/runs/{run}/decide`.
    fn decide<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverDecideRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverEffectResponse, ClientError>>;
}

impl EffectApi for ApiClient {
    fn create_task<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverCreateTaskRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverEffectResponse, ClientError>> {
        Box::pin(self.observer_create_task(run_id, request))
    }

    fn decide<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverDecideRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverEffectResponse, ClientError>> {
        Box::pin(self.observer_decide(run_id, request))
    }
}

/// Performs `call` for the run `run_id` and returns the line to print.
///
/// Every answer of the route is a success; a refusal is a failure with its
/// reason.
pub async fn perform<A: EffectApi + ?Sized>(
    api: &A,
    run_id: &str,
    call: &EffectCall,
) -> Result<String> {
    let response = match call {
        EffectCall::CreateTask(request) => api
            .create_task(run_id, request)
            .await
            .map_err(|error| failure("The task was not created", error))?,
        EffectCall::Decide(request) => api
            .decide(run_id, request)
            .await
            .map_err(|error| failure("The answer was refused", error))?,
    };
    Ok(effect_output(&response))
}

/// The client of an observer's script: the run's token on `NARSOR_SERVER`,
/// never the local daemon of a state directory.
pub fn effect_client(target: &ServerTarget) -> Result<ApiClient> {
    let Some(server) = target.server.as_deref() else {
        return Err(CliError::usage(format!(
            "An observer's script reaches the control plane through NARSOR_SERVER, which is not \
             set ({OBSERVER_RUN_ENV} is)"
        )));
    };
    let token = match &target.token {
        Some(token @ TokenSource::Static(_)) => token.clone(),
        _ => {
            return Err(CliError::usage(format!(
                "An observer's script presents the run's token from NARSOR_TOKEN, which is not \
                 set ({OBSERVER_RUN_ENV} is)"
            )))
        }
    };
    ApiClient::remote(server, token)
        .map_err(|error| failure("Cannot reach the control plane", error))
}
