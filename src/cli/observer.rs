//! `narsor-cli observer …`: observers of external sources.
//!
//! | command | who | what |
//! | --- | --- | --- |
//! | `observer create\|list\|show\|delete` | the owner | manages observers |
//! | `observer script set --file` | the owner | saves the next, immutable script version (never approved by saving) |
//! | `observer approve --version\|pause\|resume` | the owner | decides what runs on schedule |
//! | `observer watch add\|list\|remove` | the owner | keeps the watch list of PR/MR objects |
//! | `observer worker` | the worker's system user | claims due runs and executes their scripts ([`super::observer_worker`]) |
//! | `observer worker-token create\|list\|revoke` | the owner | manages the worker's credential |
//!
//! The owner's commands go through [`ObserverApi`], implemented on the API
//! client, and print through the pure functions of
//! [`super::observer_text`]: both are what the tests drive.
//!
//! The worker talks to the control plane with its own credential, never
//! the owner's token (PO5): the credential claims runs and reports their
//! results, nothing else. It is shown once, at `create`; with `--output`
//! it goes straight into a new file of mode `0600`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use clap::Subcommand;
use futures::future::BoxFuture;

use super::commands::{CliError, Result};
use super::observer_text::{
    observer_changed_output, observer_list_output, observer_show_output, script_saved_output,
    watch_added_output, watch_list_output, watch_removed_output,
};
use super::observer_worker::{
    check_credential_file, check_secrets_dir, current_uid, ProcessRunner, ScriptUser, Worker,
    WorkerOptions, WorkerStop, POLL_INTERVAL,
};
use super::remote::{failure, parse_workspaces, split_pair};
use super::task_text::age;
use crate::api::{ApiClient, ClientError, ServerTarget, TokenSource};
use crate::config::Flags;
use crate::controller_protocol::{
    AddObserverWatchRequest, ApproveObserverRequest, CreateObserverRequest,
    CreateObserverWorkerRequest, ObserverDetailView, ObserverEffectRequest, ObserverListResponse,
    ObserverScriptView, ObserverSecretRef, ObserverView, ObserverWatchListResponse,
    ObserverWatchView, ObserverWorkerCreated, ObserverWorkerListResponse, ObserverWorkerView,
    SetObserverScriptRequest,
};
use crate::domain::without_unsafe_chars;

/// What `narsor-cli observer` can be asked to do.
#[derive(Debug, Subcommand)]
pub enum ObserverCommand {
    /// Creates a draft observer. Nothing runs until a script version is
    /// saved and approved.
    Create(CreateArgs),

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

    /// Deletes the observer with its scripts, run errors and watch entries. The tasks it created stay. Cannot be undone.
    Delete {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
    },

    /// Keeps the watch list: the PR/MR objects whose events go to a task.
    Watch {
        /// What to do.
        #[command(subcommand)]
        command: WatchCommand,
    },

    /// Runs the observer worker: claims the runs the control plane has due
    /// and executes their scripts in a clean environment. Needs --server.
    Worker(WorkerArgs),

    /// Creates, lists and revokes the credential of observer workers.
    WorkerToken {
        /// What to do.
        #[command(subcommand)]
        command: WorkerTokenCommand,
    },
}

impl ObserverCommand {
    /// Whether the command runs on a configuration file that does not hold
    /// up: the worker reads none, only its flags.
    pub fn runs_on_broken_config(&self) -> bool {
        matches!(self, ObserverCommand::Worker(_))
    }
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

    /// Create a task of this catalogue workflow for each `task create` call
    /// of the script.
    #[arg(
        long,
        value_name = "WORKFLOW",
        required_unless_present = "deliver_events",
        conflicts_with = "deliver_events"
    )]
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

    /// Deliver events to the tasks on its watch list instead of creating
    /// tasks (implies --watch-list).
    #[arg(long)]
    pub deliver_events: bool,

    /// Look only at the objects of its watch entries; no entries, no run.
    #[arg(long)]
    pub watch_list: bool,

    /// A watch entry's object must start with this (repeatable): a URL
    /// prefix ending with the `/` after its host, so the source's token
    /// never goes to another host.
    #[arg(long = "allow-prefix", value_name = "PREFIX")]
    pub allowed_prefixes: Vec<String>,

    /// The longest object of a watch entry.
    #[arg(long = "max-object-len", value_name = "N")]
    pub max_object_len: Option<usize>,

    /// Seconds between runs (at least 60).
    #[arg(long, value_name = "SECONDS")]
    pub interval: Option<u64>,

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

/// `narsor-cli observer watch …`.
#[derive(Debug, Subcommand)]
pub enum WatchCommand {
    /// Watches an object (a PR/MR URL) for one of your tasks.
    Add {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
        /// The task its events go to.
        #[arg(long, value_name = "TASK_ID")]
        task: String,
        /// The object, under one of the observer's allowed prefixes.
        #[arg(long, value_name = "OBJECT")]
        object: String,
        /// The event types to catch, comma-separated or repeated.
        #[arg(
            long = "events",
            value_name = "TYPE",
            value_delimiter = ',',
            required = true
        )]
        events: Vec<String>,
    },

    /// Lists the observer's watch entries.
    List {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
    },

    /// Removes a watch entry.
    Remove {
        /// The observer's name or id.
        #[arg(value_name = "OBSERVER")]
        observer: String,
        /// The entry's id, or the start of it that `watch list` shows.
        #[arg(value_name = "ENTRY")]
        entry: String,
    },
}

/// `narsor-cli observer worker`.
#[derive(Debug, clap::Args)]
pub struct WorkerArgs {
    /// The file holding the worker credential (`observer worker-token
    /// create --output`). Must be readable by its owner only.
    #[arg(long, value_name = "FILE")]
    pub credential_file: PathBuf,

    /// The directory of the observers' secrets: one file per secret
    /// reference name. Must be accessible to its owner only.
    #[arg(long, value_name = "DIRECTORY")]
    pub secrets_dir: Option<PathBuf>,

    /// Run scripts as this system user, which must not be able to read the
    /// credential file or the secrets directory. Required unless
    /// `--insecure-same-user` is given: isolation is the default.
    #[arg(
        long,
        value_name = "USER",
        required_unless_present = "insecure_same_user"
    )]
    pub script_user: Option<String>,

    /// Run scripts as the worker's own user. A script can then read the
    /// worker credential and every observer's secrets: for a trial on a
    /// developer machine only, never in production.
    #[arg(long, conflicts_with = "script_user")]
    pub insecure_same_user: bool,

    /// Where to make the run directories (the system's temporary directory
    /// by default).
    #[arg(long, value_name = "DIRECTORY")]
    pub work_dir: Option<PathBuf>,

    /// The `PATH` of scripts (the directory of narsor-cli, then the
    /// system's directories, by default).
    #[arg(long, value_name = "PATH")]
    pub script_path: Option<String>,

    /// Claim once, handle what was due, and exit.
    #[arg(long)]
    pub once: bool,
}

/// `narsor-cli observer worker-token …`.
#[derive(Debug, Subcommand)]
pub enum WorkerTokenCommand {
    /// Creates a worker credential, shown once.
    Create {
        /// A label for the worker (a host name, say).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Write the credential to this new file (mode 0600) instead of
        /// printing it.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },

    /// Lists the worker credentials, revoked ones included.
    List,

    /// Revokes a worker credential: the worker is refused from now on.
    Revoke {
        /// The credential's id (`wkr-…`).
        #[arg(value_name = "WORKER_ID")]
        worker_id: String,
    },
}

/// Runs one `narsor-cli observer` command.
pub async fn observer(
    command: ObserverCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    match command {
        ObserverCommand::Worker(args) => worker(args, target).await,
        ObserverCommand::WorkerToken { command } => {
            let client = crate::api::connect(target, state, flags)
                .await
                .map_err(CliError::failure)?;
            let output = worker_token(&client, command, Utc::now).await?;
            print!("{output}");
            Ok(())
        }
        command => {
            // Parse what is local before connecting: a usage error never
            // needs the control plane.
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
    }
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

    /// The watch entries.
    fn observer_watches<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverWatchListResponse, ClientError>>;

    /// Adds a watch entry.
    fn add_observer_watch<'a>(
        &'a self,
        observer: &'a str,
        request: &'a AddObserverWatchRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverWatchView, ClientError>>;

    /// Removes a watch entry.
    fn remove_observer_watch<'a>(
        &'a self,
        observer: &'a str,
        entry_id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverWatchView, ClientError>>;
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

    fn observer_watches<'a>(
        &'a self,
        observer: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverWatchListResponse, ClientError>> {
        Box::pin(ApiClient::observer_watches(self, observer))
    }

    fn add_observer_watch<'a>(
        &'a self,
        observer: &'a str,
        request: &'a AddObserverWatchRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverWatchView, ClientError>> {
        Box::pin(ApiClient::add_observer_watch(self, observer, request))
    }

    fn remove_observer_watch<'a>(
        &'a self,
        observer: &'a str,
        entry_id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverWatchView, ClientError>> {
        Box::pin(ApiClient::remove_observer_watch(self, observer, entry_id))
    }
}

/// The create request of `observer create`, checked locally: a mapping,
/// workspace, secret or condition that does not parse is a usage error.
pub fn create_observer_request(args: &CreateArgs) -> Result<CreateObserverRequest> {
    let effect = match &args.workflow {
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
            ObserverEffectRequest::CreateTask {
                workflow_id: workflow.clone(),
                workflow_version: args.workflow_version,
                workspaces: parse_workspaces(&args.repositories, &args.branches, &[])?,
                backend_id: args.backend.clone(),
                profile: args.profile.clone(),
                mapping,
            }
        }
        None if args.deliver_events => ObserverEffectRequest::DeliverEvent,
        None => {
            return Err(CliError::usage(
                "narsor-cli observer create needs --workflow WORKFLOW or --deliver-events",
            ))
        }
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
        interval_secs: args.interval,
        effect,
        watch_list: args.watch_list || args.deliver_events,
        secrets,
        task_limit_per_run: args.task_limit,
        allowed_object_prefixes: args.allowed_prefixes.clone(),
        max_object_len: args.max_object_len,
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
                "Deleted observer '{}' with its scripts, run errors and watch entries; \
                 the tasks it created stay.\n",
                without_unsafe_chars(&deleted.name)
            ))
        }
        ObserverCommand::Watch { command } => watch(api, command, now()).await,
        ObserverCommand::Worker(_) | ObserverCommand::WorkerToken { .. } => Err(CliError::usage(
            "narsor-cli observer worker and worker-token are not owner commands of an observer",
        )),
    }
}

async fn watch<A: ObserverApi + ?Sized>(
    api: &A,
    command: WatchCommand,
    now: DateTime<Utc>,
) -> Result<String> {
    match command {
        WatchCommand::Add {
            observer,
            task,
            object,
            events,
        } => {
            let event_types: Vec<String> = events
                .iter()
                .map(|event| event.trim().to_string())
                .filter(|event| !event.is_empty())
                .collect();
            if event_types.is_empty() {
                return Err(CliError::usage(
                    "narsor-cli observer watch add needs at least one event type (--events)",
                ));
            }
            let entry = api
                .add_observer_watch(
                    &observer,
                    &AddObserverWatchRequest {
                        task_id: task,
                        object,
                        event_types,
                    },
                )
                .await
                .map_err(|error| failure("Could not add the watch entry", error))?;
            Ok(watch_added_output(&entry))
        }
        WatchCommand::List { observer } => {
            let listing = api
                .observer_watches(&observer)
                .await
                .map_err(|error| failure("Could not list the watch entries", error))?;
            Ok(watch_list_output(&listing, now))
        }
        WatchCommand::Remove { observer, entry } => {
            let listing = api
                .observer_watches(&observer)
                .await
                .map_err(|error| failure("Could not list the watch entries", error))?;
            let matching: Vec<&ObserverWatchView> = listing
                .watches
                .iter()
                .filter(|watch| watch.entry_id.starts_with(entry.as_str()))
                .collect();
            let entry_id = match matching.as_slice() {
                [only] => only.entry_id.clone(),
                [] => {
                    return Err(CliError::failure(format!(
                        "Observer '{}' has no watch entry '{}'",
                        without_unsafe_chars(&observer),
                        without_unsafe_chars(&entry)
                    )))
                }
                _ => {
                    return Err(CliError::usage(format!(
                        "'{}' starts more than one watch entry; give more of its id",
                        without_unsafe_chars(&entry)
                    )))
                }
            };
            let removed = api
                .remove_observer_watch(&observer, &entry_id)
                .await
                .map_err(|error| failure("Could not remove the watch entry", error))?;
            Ok(watch_removed_output(&removed))
        }
    }
}

/// The owner's worker-credential routes, as `observer worker-token` uses
/// them: the API client in `narsor-cli`, the service itself in tests.
pub trait WorkerTokenApi: Send + Sync {
    /// Creates a worker credential, shown this once.
    fn create_observer_worker<'a>(
        &'a self,
        request: &'a CreateObserverWorkerRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverWorkerCreated, ClientError>>;

    /// The worker credentials, revoked ones included.
    fn observer_workers(
        &self,
    ) -> BoxFuture<'_, std::result::Result<ObserverWorkerListResponse, ClientError>>;

    /// Revokes a worker credential.
    fn revoke_observer_worker<'a>(
        &'a self,
        worker_id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverWorkerView, ClientError>>;
}

impl WorkerTokenApi for ApiClient {
    fn create_observer_worker<'a>(
        &'a self,
        request: &'a CreateObserverWorkerRequest,
    ) -> BoxFuture<'a, std::result::Result<ObserverWorkerCreated, ClientError>> {
        Box::pin(ApiClient::create_observer_worker(self, request))
    }

    fn observer_workers(
        &self,
    ) -> BoxFuture<'_, std::result::Result<ObserverWorkerListResponse, ClientError>> {
        Box::pin(ApiClient::observer_workers(self))
    }

    fn revoke_observer_worker<'a>(
        &'a self,
        worker_id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverWorkerView, ClientError>> {
        Box::pin(ApiClient::revoke_observer_worker(self, worker_id))
    }
}

/// Runs one `narsor-cli observer worker-token` command on `api` and
/// returns what it prints; `now` is the clock ages are measured with. A
/// credential written to `--output` goes into a new file of mode `0600`
/// only: an existing file is refused before the credential is created.
pub async fn worker_token<A, N>(api: &A, command: WorkerTokenCommand, now: N) -> Result<String>
where
    A: WorkerTokenApi + ?Sized,
    N: Fn() -> DateTime<Utc>,
{
    let mut out = String::new();
    match command {
        WorkerTokenCommand::Create { name, output } => {
            if let Some(path) = &output {
                if path.symlink_metadata().is_ok() {
                    return Err(CliError::usage(format!(
                        "{} already exists; the credential goes into a new file only",
                        path.display()
                    )));
                }
            }
            let created = api
                .create_observer_worker(&CreateObserverWorkerRequest { name })
                .await
                .map_err(|error| failure("Could not create a worker credential", error))?;
            out.push_str(&format!("Worker: {}\n", created.worker.worker_id));
            match output {
                Some(path) => {
                    write_new_secret_file(&path, &created.credential).map_err(|error| {
                        CliError::failure(format!(
                            "Could not write {}: {error}; revoke {} and create another",
                            path.display(),
                            created.worker.worker_id
                        ))
                    })?;
                    out.push_str(&format!(
                        "Credential written to {} (mode 0600).\n",
                        path.display()
                    ));
                }
                None => {
                    out.push_str(&format!("Credential: {}\n", created.credential));
                    eprintln!(
                        "Shown once. Put it in a file of mode 0600 readable by the worker's \
                         user only, and pass it with --credential-file."
                    );
                }
            }
        }
        WorkerTokenCommand::List => {
            let listing = api
                .observer_workers()
                .await
                .map_err(|error| failure("Could not list the worker credentials", error))?;
            if listing.workers.is_empty() {
                out.push_str("No worker credentials.\n");
            }
            let now = now();
            for worker in &listing.workers {
                let state = worker.revoked_at.map_or("active".to_string(), |at| {
                    format!("revoked {} ago", age(now, at))
                });
                out.push_str(&format!(
                    "{:<40} {:<24} created {} ago, {state}\n",
                    without_unsafe_chars(&worker.worker_id),
                    without_unsafe_chars(worker.name.as_deref().unwrap_or("-")),
                    age(now, worker.created_at)
                ));
            }
        }
        WorkerTokenCommand::Revoke { worker_id } => {
            let worker = api
                .revoke_observer_worker(&worker_id)
                .await
                .map_err(|error| failure("Could not revoke the worker credential", error))?;
            out.push_str(&format!(
                "Revoked worker credential {}.\n",
                without_unsafe_chars(&worker.worker_id)
            ));
        }
    }
    Ok(out)
}

/// Writes `secret` to a file that must not exist yet, mode `0600`.
fn write_new_secret_file(path: &Path, secret: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{secret}")
}

async fn worker(args: WorkerArgs, target: &ServerTarget) -> Result<()> {
    let Some(server) = target.server.clone() else {
        return Err(CliError::usage(
            "narsor-cli observer worker needs the control plane's URL: --server or NARSOR_SERVER",
        ));
    };
    check_credential_file(&args.credential_file).map_err(CliError::failure)?;
    if let Some(dir) = &args.secrets_dir {
        check_secrets_dir(dir).map_err(CliError::failure)?;
    }
    let mut options = WorkerOptions::new(server.clone());
    options.secrets_dir = args.secrets_dir;
    if let Some(name) = &args.script_user {
        let user = ScriptUser::lookup(name).map_err(|error| {
            CliError::failure(format!("Cannot run scripts as '{name}': {error}"))
        })?;
        if user.uid == current_uid() {
            return Err(CliError::failure(format!(
                "Cannot run scripts as '{name}': it is the worker's own user, which can read \
                 the credential and the secrets"
            )));
        }
        options.script_user = Some(user);
    } else if args.insecure_same_user {
        tracing::warn!(
            "observer worker: --insecure-same-user: scripts run as the worker's own user and \
             can read the worker credential and every observer's secrets; use --script-user \
             outside a trial"
        );
        eprintln!(
            "Warning: scripts run as the worker's own user (--insecure-same-user) and can read \
             the worker credential and the secrets. Use --script-user in production."
        );
    } else {
        // clap requires one of the two; refuse rather than run unisolated.
        return Err(CliError::usage(
            "narsor-cli observer worker needs --script-user USER (or --insecure-same-user for a \
             trial)",
        ));
    }
    if let Some(dir) = args.work_dir {
        options.work_dir = dir;
    }
    if let Some(path) = args.script_path {
        options.path = path;
    }
    let client = ApiClient::remote(&server, TokenSource::File(args.credential_file))
        .map_err(|error| failure("Cannot reach the control plane", error))?;
    let worker = Worker::new(client, ProcessRunner, options);
    tracing::info!(server = %server, "observer worker: started");
    worker
        .serve(args.once, POLL_INTERVAL, shutdown_signal())
        .await
        .map_err(|stop: WorkerStop| CliError::failure(stop.to_string()))
}

/// Resolves at Ctrl-C or `SIGTERM` (systemd's stop).
async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
