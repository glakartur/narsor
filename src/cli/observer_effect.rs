//! The effect commands of an observer's script: `narsor-cli task create`
//! (R3, PO1) and `narsor-cli task decide TASK --episode EP` (R12, R13).
//!
//! The worker runs a script with `NARSOR_OBSERVER_RUN` (the run's id),
//! `NARSOR_SERVER` and `NARSOR_TOKEN` (the run's token) set. With
//! `NARSOR_OBSERVER_RUN` set, the command goes to the effect routes of that
//! run (`/observer/v1/runs/{run}/…`) and nowhere else:
//!
//! ```text
//!                          NARSOR_OBSERVER_RUN set          not set
//! task create WORKFLOW …   effect route                     the owner's create
//! task create --file …     usage error                      the owner's create
//! task decide              effect route                     usage error
//! ```
//!
//! `task create` takes the same options in both columns (and `--stdin`
//! with them as one JSON object, the options over it) and sends the same request: in a script the
//! server creates it for the installation owner and records the run.
//!
//! `task decide` takes `--choice CHOICE [--reason TEXT] [--var NAME=VALUE …]
//! [--var-file NAME=PATH …]` and `--stdin` with `{choice, reason,
//! variables}` (the answer of a script step); the options override it.
//!
//! The server does not deduplicate tasks: every call creates one. The
//! script keeps track in its state of what it already reported.
//!
//! What the script sees:
//!
//! | answer | prints | exit code |
//! | --- | --- | --- |
//! | created, delivered | the server's line | 0 |
//! | refused task create, refused answer (stale episode, another answer first, invalid choice or variables), token not valid | the reason, on stderr | 1 |
//! | usage error | the reason, on stderr | 2 |

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::commands::{CliError, Result};
use super::queue_text::effect_output;
use super::remote::{create_request, failure, split_pair, TaskCommand};
use crate::api::{ApiClient, ClientError, ServerTarget, TokenSource};
use crate::controller_protocol::{
    CreateTaskRequest, ObserverDecideRequest, ObserverEffectResponse,
};

/// The environment variable naming the observer run a script belongs to.
pub const OBSERVER_RUN_ENV: &str = "NARSOR_OBSERVER_RUN";

/// An effect a script asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectCall {
    /// `task create …`, the request the owner's create sends.
    CreateTask(CreateTaskRequest),
    /// `task decide TASK --episode EP …`.
    Decide(ObserverDecideRequest),
}

/// What `command` asks for given the observer run of the environment
/// (`NARSOR_OBSERVER_RUN`, `None` when unset or empty): an effect of that
/// run, nothing of the kind (`Ok(None)`, the command goes its usual way),
/// or a usage error (see the module documentation). `stdin` is what
/// `--stdin` read.
pub fn effect_call(
    command: &TaskCommand,
    observer_run: Option<&str>,
    stdin: Option<&str>,
) -> Result<Option<EffectCall>> {
    let observer_run = observer_run.filter(|run| !run.trim().is_empty());
    match (command, observer_run) {
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
                ..
            },
            Some(_),
        ) => Ok(Some(EffectCall::Decide(decide_request(
            task_id, episode, choice, reason, vars, var_files, stdin,
        )?))),
        (TaskCommand::Create { file: Some(_), .. }, Some(_)) => Err(CliError::usage(
            "In an observer's script, task create runs a catalogue workflow: --file is a \
             definition on a local daemon's disk",
        )),
        (TaskCommand::Create { .. }, Some(_)) => {
            Ok(create_request(command, stdin)?.map(|(request, _)| EffectCall::CreateTask(request)))
        }
        _ => Ok(None),
    }
}

/// The answer of a script step (`{choice, reason, variables}`), as `task
/// decide --stdin` reads it. No artifacts in this version.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideData {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    variables: Map<String, Value>,
}

/// The answer `task decide` sends: the object of `--stdin` (what `stdin`
/// holds), with `--choice`, `--reason`, `--var` and `--var-file` over it.
fn decide_request(
    task_id: &str,
    episode: &str,
    choice: &Option<String>,
    reason: &Option<String>,
    vars: &[String],
    var_files: &[String],
    stdin: Option<&str>,
) -> Result<ObserverDecideRequest> {
    let base = match stdin {
        Some(raw) => serde_json::from_str::<DecideData>(raw).map_err(|error| {
            CliError::usage(format!(
                "Invalid --stdin — expected JSON {{\"choice\", \"reason\", \"variables\"}}: {error}"
            ))
        })?,
        None => DecideData::default(),
    };
    let Some(choice) = choice.clone().or(base.choice) else {
        return Err(CliError::usage(
            "task decide needs --choice CHOICE, or \"choice\" in the --stdin object",
        ));
    };
    let mut variables = base.variables;
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
    Ok(ObserverDecideRequest {
        task_id: task_id.to_string(),
        episode: episode.to_string(),
        choice,
        reason: reason.clone().or(base.reason),
        variables,
    })
}

/// The effect routes of one run, as the script's commands use them: the
/// API client in `narsor-cli`, the service itself in tests.
pub trait EffectApi: Send + Sync {
    /// `POST /observer/v1/runs/{run}/tasks`.
    fn create_task<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a CreateTaskRequest,
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
        request: &'a CreateTaskRequest,
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
