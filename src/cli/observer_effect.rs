//! The effect commands of an observer's script: `narsor-cli task create
//! --data JSON` and `narsor-cli task event TASK --type T [--data JSON]`
//! (R3, R4, PO1).
//!
//! The worker runs a script with `NARSOR_OBSERVER_RUN` (the run's id),
//! `NARSOR_SERVER` and `NARSOR_TOKEN` (the run's token) set. With
//! `NARSOR_OBSERVER_RUN` set, both commands go to the effect routes of
//! that run (`/observer/v1/runs/{run}/…`) and nowhere else:
//!
//! ```text
//!                          NARSOR_OBSERVER_RUN set          not set
//! task create --data       effect route                     usage error
//! task create WORKFLOW …   usage error (the record decides)  the owner's create
//! task event               effect route                     usage error
//! ```
//!
//! The server does not deduplicate: every call creates a task or records an
//! event. The script keeps track in its state of what it already reported.
//!
//! What the script sees:
//!
//! | answer | prints | exit code |
//! | --- | --- | --- |
//! | created, delivered, recorded | the server's line | 0 |
//! | task limit reached, refused data, token not valid | the reason, on stderr | 1 |
//! | usage error | the reason, on stderr | 2 |

use futures::future::BoxFuture;
use serde_json::Value;

use super::commands::{CliError, Result};
use super::observer_text::effect_output;
use super::remote::{failure, TaskCommand};
use crate::api::{ApiClient, ClientError, ServerTarget, TokenSource};
use crate::controller_protocol::{
    ObserverCreateTaskRequest, ObserverEffectResponse, ObserverEventRequest,
};

/// The environment variable naming the observer run a script belongs to.
pub const OBSERVER_RUN_ENV: &str = "NARSOR_OBSERVER_RUN";

/// An effect a script asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectCall {
    /// `task create --data JSON`.
    CreateTask(ObserverCreateTaskRequest),
    /// `task event TASK --type T [--data JSON]`.
    Event {
        /// The task on the observer's watch list.
        task_id: String,
        /// The event.
        request: ObserverEventRequest,
    },
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
        (TaskCommand::Event { .. }, None) => Err(CliError::usage(format!(
            "narsor-cli task event is for the script of an observer run only: \
             {OBSERVER_RUN_ENV} is not set"
        ))),
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
                (workflow.is_some(), "WORKFLOW"),
                (file.is_some(), "--file"),
                (workflow_version.is_some(), "--workflow-version"),
                (!inputs.is_empty(), "--input"),
                (!repositories.is_empty(), "--repository"),
                (!branches.is_empty(), "--branch"),
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
                    "In an observer's script, task create takes only --data: the \
                     observer's record decides the workflow, repositories, backend and profile \
                     (refused: {})",
                    chosen.join(", ")
                )));
            }
            Ok(Some(EffectCall::CreateTask(ObserverCreateTaskRequest {
                data: parse_data(data.as_deref())?,
            })))
        }
        (
            TaskCommand::Event {
                task_id,
                event_type,
                data,
            },
            Some(_),
        ) => Ok(Some(EffectCall::Event {
            task_id: task_id.clone(),
            request: ObserverEventRequest {
                event_type: event_type.clone(),
                data: parse_data(data.as_deref())?,
            },
        })),
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

/// The effect routes of one run, as the script's commands use them: the
/// API client in `narsor-cli`, the service itself in tests.
pub trait EffectApi: Send + Sync {
    /// `POST /observer/v1/runs/{run}/tasks`.
    fn create_task<'a>(
        &'a self,
        run_id: &'a str,
        request: &'a ObserverCreateTaskRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverEffectResponse, ClientError>>;

    /// `POST /observer/v1/runs/{run}/tasks/{task_id}/events`.
    fn deliver_event<'a>(
        &'a self,
        run_id: &'a str,
        task_id: &'a str,
        request: &'a ObserverEventRequest,
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

    fn deliver_event<'a>(
        &'a self,
        run_id: &'a str,
        task_id: &'a str,
        request: &'a ObserverEventRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverEffectResponse, ClientError>> {
        Box::pin(self.observer_deliver_event(run_id, task_id, request))
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
        EffectCall::Event { task_id, request } => api
            .deliver_event(run_id, task_id, request)
            .await
            .map_err(|error| failure("The event was not delivered", error))?,
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
