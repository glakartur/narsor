//! `narsor-cli observer …`: observers of external sources.
//!
//! | command | who | what |
//! | --- | --- | --- |
//! | `observer create\|list\|show\|delete` | the owner | manages observers |
//! | `observer script set --file` | the owner | saves the next, immutable script version (never approved by saving) |
//! | `observer approve --version\|pause\|resume` | the owner | decides what runs on schedule |
//!
//! The owner's commands go through [`ObserverApi`], implemented on the API
//! client, and print through the pure functions of
//! [`super::observer_text`]: both are what the tests drive.
//!
//! Every command is a call of the control plane's API. The worker that
//! claims due runs and executes their scripts is a program of its own,
//! `narsor-observer-worker`, which `narsor-server` starts (D20).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use clap::Subcommand;
use futures::future::BoxFuture;

use super::commands::{CliError, Result};
use super::observer_text::{
    observer_changed_output, observer_list_output, observer_show_output, script_saved_output,
};
use super::remote::{failure, parse_workspaces, split_pair};
use crate::api::{ApiClient, ClientError, ServerTarget};
use crate::config::Flags;
use crate::controller_protocol::{
    ApproveObserverRequest, CreateObserverRequest, ObserverDetailView, ObserverEffectRequest,
    ObserverListResponse, ObserverMode, ObserverScriptView, ObserverSecretRef, ObserverView,
    SetObserverScriptRequest,
};
use crate::domain::without_unsafe_chars;

/// What `narsor-cli observer` can be asked to do.
#[derive(Debug, Subcommand)]
pub enum ObserverCommand {
    /// Creates a draft observer. Nothing runs until a script version is
    /// saved and approved.
    Create(Box<CreateArgs>),

    /// Lists your observers: state, script versions, last and next run,
    /// the last error, and a warning when no worker claims due runs.
    List,

    /// Shows one observer: its configuration (secret references, never
    /// values), script versions and the errors of failed or lost runs.
    Show {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
    },

    /// Saves script versions.
    Script {
        /// What to do.
        #[command(subcommand)]
        command: ScriptCommand,
    },

    /// Approves a script version: from now on the schedule runs it, and
    /// only it. A draft observer becomes active.
    Approve {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
        /// The script version to approve.
        #[arg(long, value_name = "N")]
        version: u32,
    },

    /// Stops the observer's schedule; a run in progress finishes.
    Pause {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
    },

    /// Puts a paused observer, or one waiting for you (needs_attention,
    /// error), back on its schedule.
    Resume {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
    },

    /// Deletes the observer with its scripts and run errors. The tasks it created stay. Cannot be undone.
    Delete {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
    },
}

/// `narsor-cli observer create`.
#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// A unique name.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// The source it polls (`jira`, `gitlab.example.com`, …), as text.
    #[arg(long, value_name = "TEXT")]
    pub source: String,

    /// The queue it serves: the `watch.queue` of the gates whose wait
    /// episodes it checks. Its own name when omitted.
    #[arg(long, value_name = "QUEUE")]
    pub queue: Option<String>,

    /// When its script runs: `on_episodes` (for the entries of its queue
    /// only) or `always` (on every tick). Every observer of a queue has the
    /// same mode. `always` with --workflow, `on_episodes` without, when
    /// omitted.
    #[arg(long, value_name = "MODE", value_parser = ["on_episodes", "always"])]
    pub mode: Option<String>,

    /// Create a task of this catalogue workflow for each `task create` call
    /// of the script.
    #[arg(long, value_name = "WORKFLOW")]
    pub workflow: Option<String>,

    /// The workflow's version; the catalogue's when omitted.
    #[arg(long = "workflow-version", value_name = "N", requires = "workflow")]
    pub workflow_version: Option<u32>,

    /// A workflow input taken from a field of the effect data,
    /// `INPUT=FIELD` (repeatable). Only mapped fields reach the task.
    #[arg(long = "map", value_name = "INPUT=FIELD", requires = "workflow")]
    pub mappings: Vec<String>,

    /// Registered repository for a workspace of the workflow,
    /// `WORKSPACE=ID` (repeatable).
    #[arg(
        long = "repository",
        value_name = "WORKSPACE=ID",
        requires = "workflow"
    )]
    pub repositories: Vec<String>,

    /// Branch for a workspace of the workflow, `WORKSPACE=BRANCH`
    /// (repeatable).
    #[arg(
        long = "branch",
        value_name = "WORKSPACE=BRANCH",
        requires = "workflow"
    )]
    pub branches: Vec<String>,

    /// Backend of the tasks; your default backend when omitted.
    #[arg(long, value_name = "BACKEND", requires = "workflow")]
    pub backend: Option<String>,

    /// Environment profile of the tasks; the default when omitted.
    #[arg(long, value_name = "PROFILE", requires = "workflow")]
    pub profile: Option<String>,

    /// Seconds between runs (at least 60); 300 when neither --interval nor
    /// --cron is given.
    #[arg(long, value_name = "SECONDS", conflicts_with = "cron")]
    pub interval: Option<u64>,

    /// A cron expression of five fields (minute hour day-of-month month
    /// day-of-week), in UTC, like '*/10 * * * *'. No seconds field, no
    /// aliases.
    #[arg(long, value_name = "EXPRESSION")]
    pub cron: Option<String>,

    /// Parameters handed to the script as they are: a JSON object.
    #[arg(long, value_name = "JSON")]
    pub condition: Option<String>,

    /// A secret of the worker's secrets directory handed to the script as
    /// an environment variable, `NAME=ENV` (repeatable).
    #[arg(long = "secret", value_name = "NAME=ENV")]
    pub secrets: Vec<String>,

    /// Most tasks one run may create; past it the observer waits for you.
    #[arg(long = "task-limit", value_name = "N")]
    pub task_limit: Option<u32>,

    /// Let the first run import what already matches the condition rather
    /// than only remember where to start from.
    #[arg(long)]
    pub import_backlog: bool,
}

/// `narsor-cli observer script …`.
#[derive(Debug, Subcommand)]
pub enum ScriptCommand {
    /// Saves a shell script as the observer's next version. Saving never
    /// approves it.
    Set {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
        /// The script file.
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
    },
}

/// Runs one `narsor-cli observer` command.
pub async fn observer(
    command: ObserverCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    // Parse what is local before connecting: a usage error never needs the
    // control plane.
    let create = match &command {
        ObserverCommand::Create(args) => Some(create_observer_request(args)?),
        _ => None,
    };
    let client = crate::api::connect(target, state, flags)
        .await
        .map_err(CliError::failure)?;
    let output = operate(&client, command, create, Utc::now).await?;
    print!("{output}");
    Ok(())
}

/// The owner's observer routes, as the commands use them: the API client
/// in `narsor-cli`, the service itself in tests.
pub trait ObserverApi: Send + Sync {
    /// The caller's observers.
    fn observers(&self) -> BoxFuture<'_, std::result::Result<ObserverListResponse, ClientError>>;

    /// Creates a draft observer.
    fn create_observer<'a>(
        &'a self,
        request: &'a CreateObserverRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>>;

    /// One observer in detail.
    fn observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverDetailView, ClientError>>;

    /// Deletes an observer.
    fn delete_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>>;

    /// Saves the next script version.
    fn set_observer_script<'a>(
        &'a self,
        observer: &'a str,
        request: &'a SetObserverScriptRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverScriptView, ClientError>>;

    /// Approves a script version.
    fn approve_observer<'a>(
        &'a self,
        observer: &'a str,
        request: &'a ApproveObserverRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>>;

    /// Stops the schedule.
    fn pause_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>>;

    /// Puts the observer back on its schedule.
    fn resume_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>>;
}

impl ObserverApi for ApiClient {
    fn observers(&self) -> BoxFuture<'_, std::result::Result<ObserverListResponse, ClientError>> {
        Box::pin(ApiClient::observers(self))
    }

    fn create_observer<'a>(
        &'a self,
        request: &'a CreateObserverRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>> {
        Box::pin(ApiClient::create_observer(self, request))
    }

    fn observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverDetailView, ClientError>> {
        Box::pin(ApiClient::observer(self, observer))
    }

    fn delete_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>> {
        Box::pin(ApiClient::delete_observer(self, observer))
    }

    fn set_observer_script<'a>(
        &'a self,
        observer: &'a str,
        request: &'a SetObserverScriptRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverScriptView, ClientError>> {
        Box::pin(ApiClient::set_observer_script(self, observer, request))
    }

    fn approve_observer<'a>(
        &'a self,
        observer: &'a str,
        request: &'a ApproveObserverRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>> {
        Box::pin(ApiClient::approve_observer(self, observer, request))
    }

    fn pause_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>> {
        Box::pin(ApiClient::pause_observer(self, observer))
    }

    fn resume_observer<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverView, ClientError>> {
        Box::pin(ApiClient::resume_observer(self, observer))
    }
}

/// Seconds between runs of an observer created with neither `--interval`
/// nor `--cron`.
pub const DEFAULT_INTERVAL_SECS: u64 = 300;

/// The create request of `observer create`, checked locally: a mapping,
/// workspace, secret or condition that does not parse is a usage error.
pub fn create_observer_request(args: &CreateArgs) -> Result<CreateObserverRequest> {
    let effect = match &args.workflow {
        None => None,
        Some(workflow) => {
            let mut mapping = BTreeMap::new();
            for pair in &args.mappings {
                let (input, field) = split_pair("map", pair)?;
                let field = field.trim();
                if field.is_empty() {
                    return Err(CliError::usage(format!(
                        "Invalid --map '{pair}' — expected INPUT=FIELD."
                    )));
                }
                mapping.insert(input.to_string(), field.to_string());
            }
            Some(ObserverEffectRequest::CreateTask {
                workflow_id: workflow.clone(),
                workflow_version: args.workflow_version,
                workspaces: parse_workspaces(&args.repositories, &args.branches, &[])?,
                backend_id: args.backend.clone(),
                profile: args.profile.clone(),
                mapping,
            })
        }
    };
    let mode = match args.mode.as_deref() {
        None => None,
        Some("on_episodes") => Some(ObserverMode::OnEpisodes),
        Some("always") => Some(ObserverMode::Always),
        Some(other) => {
            return Err(CliError::usage(format!(
                "Invalid --mode '{}' — expected on_episodes or always.",
                without_unsafe_chars(other)
            )))
        }
    };
    // The server needs one of the two; the CLI keeps its default interval.
    let interval_secs = match (&args.interval, &args.cron) {
        (None, None) => Some(DEFAULT_INTERVAL_SECS),
        (interval, _) => *interval,
    };
    let condition = args
        .condition
        .as_deref()
        .map(|raw| match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(value @ serde_json::Value::Object(_)) => Ok(value),
            _ => Err(CliError::usage(
                "Invalid --condition — expected a JSON object, like '{\"project\": \"ABC\"}'.",
            )),
        })
        .transpose()?;
    let secrets = args
        .secrets
        .iter()
        .map(|pair| {
            split_pair("secret", pair).map(|(name, env)| ObserverSecretRef {
                name: name.to_string(),
                env: env.trim().to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(CreateObserverRequest {
        name: args.name.clone(),
        source: args.source.clone(),
        condition,
        queue: args.queue.clone(),
        mode,
        interval_secs,
        cron: args.cron.clone(),
        effect,
        secrets,
        task_limit_per_run: args.task_limit,
        import_backlog: args.import_backlog,
    })
}

/// Runs one of the owner's observer commands on `api` and returns what it
/// prints. `create` is [`create_observer_request`] of a `create` command,
/// and `now` the clock ages are measured with.
pub async fn operate<A, N>(
    api: &A,
    command: ObserverCommand,
    create: Option<CreateObserverRequest>,
    now: N,
) -> Result<String>
where
    A: ObserverApi + ?Sized,
    N: Fn() -> DateTime<Utc>,
{
    match command {
        ObserverCommand::Create(args) => {
            let request = match create {
                Some(request) => request,
                None => create_observer_request(&args)?,
            };
            let created = api
                .create_observer(&request)
                .await
                .map_err(|error| failure("Could not create the observer", error))?;
            let name = without_unsafe_chars(&created.name);
            Ok(format!(
                "Created observer '{}' ({}): state {}. Next: `narsor-cli observer script set {} \
                 --file SCRIPT`, then `narsor-cli observer approve`.\n",
                name,
                without_unsafe_chars(&created.observer_id),
                without_unsafe_chars(&created.state),
                name
            ))
        }
        ObserverCommand::List => {
            let listing = api
                .observers()
                .await
                .map_err(|error| failure("Could not list the observers", error))?;
            Ok(observer_list_output(&listing, now()))
        }
        ObserverCommand::Show { observer } => {
            let detail = api.observer(&observer).await.map_err(|error| {
                failure(&format!("Could not read observer '{observer}'"), error)
            })?;
            Ok(observer_show_output(&detail, now()))
        }
        ObserverCommand::Script {
            command: ScriptCommand::Set { observer, file },
        } => {
            let script = std::fs::read_to_string(&file).map_err(|error| {
                CliError::failure(format!("Could not read {}: {error}", file.display()))
            })?;
            let saved = api
                .set_observer_script(&observer, &SetObserverScriptRequest { script })
                .await
                .map_err(|error| failure("Could not save the script", error))?;
            Ok(script_saved_output(&observer, &saved))
        }
        ObserverCommand::Approve { observer, version } => {
            let approved = api
                .approve_observer(&observer, &ApproveObserverRequest { version })
                .await
                .map_err(|error| failure("Could not approve the version", error))?;
            Ok(observer_changed_output(
                &format!("Approved version {version} of"),
                &approved,
            ))
        }
        ObserverCommand::Pause { observer } => {
            let paused = api
                .pause_observer(&observer)
                .await
                .map_err(|error| failure("Could not pause the observer", error))?;
            Ok(observer_changed_output("Paused", &paused))
        }
        ObserverCommand::Resume { observer } => {
            let resumed = api
                .resume_observer(&observer)
                .await
                .map_err(|error| failure("Could not resume the observer", error))?;
            Ok(observer_changed_output("Resumed", &resumed))
        }
        ObserverCommand::Delete { observer } => {
            let deleted = api
                .delete_observer(&observer)
                .await
                .map_err(|error| failure("Could not delete the observer", error))?;
            Ok(format!(
                "Deleted observer '{}' with its scripts and run errors; \
                 the tasks it created stay.\n",
                without_unsafe_chars(&deleted.name)
            ))
        }
    }
}
