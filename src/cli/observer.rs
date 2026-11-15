//! `narsor-cli observer …`: observers of external sources.
//!
//! | command | who | what |
//! | --- | --- | --- |
//! | `observer create\|list\|show\|delete` | the owner | manages observers |
//! | `observer script set --file` | the owner | saves the next, immutable script version (never approved by saving) |
//! | `observer approve --version\|pause\|resume` | the owner | decides what runs on schedule |
//! | `observer watch add\|list\|remove` | the owner | keeps the watch list of PR/MR objects |
//! | `observer worker` | `narsor-server`, which starts it | claims due runs and executes their scripts ([`super::observer_worker`]) |
//!
//! The owner's commands go through [`ObserverApi`], implemented on the API
//! client, and print through the pure functions of
//! [`super::observer_text`]: both are what the tests drive.
//!
//! The worker talks to the control plane with the key the server hands it
//! in [`OBSERVER_WORKER_KEY_ENV`] when it starts the worker, never with the
//! owner's token (D5, PO5): the key claims runs and reports their results,
//! nothing else. Nobody else makes or holds such a key.

use std::collections::BTreeMap;
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
    check_secrets_dir, current_uid, ProcessRunner, ScriptUser, Worker, WorkerOptions, WorkerStop,
    POLL_INTERVAL,
};
use super::remote::{failure, parse_workspaces, split_pair};
use crate::api::{ApiClient, ClientError, ServerTarget, TokenSource};
use crate::config::Flags;
use crate::controller_protocol::{
    AddObserverWatchRequest, ApproveObserverRequest, CreateObserverRequest, ObserverDetailView,
    ObserverEffectRequest, ObserverListResponse, ObserverScriptView, ObserverSecretRef,
    ObserverView, ObserverWatchListResponse, ObserverWatchView, SetObserverScriptRequest,
    OBSERVER_WORKER_KEY_ENV,
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
    /// and executes their scripts in a clean environment. narsor-server
    /// starts it, with --server and the key in NARSOR_OBSERVER_WORKER_KEY.
    Worker(WorkerArgs),
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
    /// The directory of the observers' secrets: one subdirectory per
    /// observer owner, one file per secret reference name in it. Must be
    /// accessible to its owner only.
    #[arg(long, value_name = "DIRECTORY")]
    pub secrets_dir: Option<PathBuf>,

    /// Run scripts as this system user, which must not be able to read the
    /// secrets directory or the worker's environment. Required unless
    /// `--insecure-same-user` is given: isolation is the default.
    #[arg(
        long,
        value_name = "USER",
        required_unless_present = "insecure_same_user"
    )]
    pub script_user: Option<String>,

    /// Run scripts as the worker's own user. A script can then read the
    /// worker key and every observer's secrets: for a trial on a
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

/// Runs one `narsor-cli observer` command.
pub async fn observer(
    command: ObserverCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    match command {
        ObserverCommand::Worker(args) => worker(args, target).await,
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
        ObserverCommand::Worker(_) => Err(CliError::usage(
            "narsor-cli observer worker is not an owner command of an observer",
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

async fn worker(args: WorkerArgs, target: &ServerTarget) -> Result<()> {
    let Some(server) = target.server.clone() else {
        return Err(CliError::usage(
            "narsor-cli observer worker needs the control plane's URL: --server or NARSOR_SERVER",
        ));
    };
    let key = worker_key()?;
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
                "Cannot run scripts as '{name}': it is the worker's own user, which holds \
                 the key and can read the secrets"
            )));
        }
        options.script_user = Some(user);
    } else if args.insecure_same_user {
        tracing::warn!(
            "observer worker: --insecure-same-user: scripts run as the worker's own user and \
             can read the worker key and every observer's secrets; use --script-user outside \
             a trial"
        );
        eprintln!(
            "Warning: scripts run as the worker's own user (--insecure-same-user) and can read \
             the worker key and the secrets. Use --script-user in production."
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
    let client = ApiClient::remote(&server, TokenSource::Static(key))
        .map_err(|error| failure("Cannot reach the control plane", error))?;
    let worker = Worker::new(client, ProcessRunner, options);
    tracing::info!(server = %server, "observer worker: started");
    worker
        .serve(args.once, POLL_INTERVAL, shutdown_signal())
        .await
        .map_err(|stop: WorkerStop| CliError::failure(stop.to_string()))
}

/// The key `narsor-server` handed the worker in [`OBSERVER_WORKER_KEY_ENV`].
/// A variable, not a flag: an argument would show in `ps`. The worker
/// never passes it on: a script's environment is an explicit list.
fn worker_key() -> Result<String> {
    match std::env::var(OBSERVER_WORKER_KEY_ENV) {
        Ok(key) if !key.trim().is_empty() => Ok(key.trim().to_string()),
        _ => Err(CliError::usage(format!(
            "narsor-cli observer worker needs the key narsor-server gives it in \
             {OBSERVER_WORKER_KEY_ENV}; enable the worker in the server's configuration \
             (observer_worker: on) instead of starting it by hand"
        ))),
    }
}

/// Resolves at `SIGTERM` (the server's stop) or Ctrl-C.
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
