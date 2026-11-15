//! What the owner does with their observers through `narsor-cli observer`
//! (R18, R21): create, list, show, save a script version, approve, pause,
//! resume, delete, and keep the watch list.
//!
//! An observer is named by its name or its id. Every method takes the
//! caller's [`OwnerRef`]: another owner's observer is answered exactly like
//! one that does not exist (`404`), so a caller learns nothing of it.
//!
//! ```text
//! create         config checked, "create a task" against the catalogue → draft
//! script set     next immutable version; never approved by saving (AE6)
//! approve        the schedule runs that version only (R11)
//! watch add      watch_list observer, own task, object under an allowed prefix
//! ```

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::tasks::launch_refusal;
use crate::controller_protocol::service::{parse_identifier, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::domain::{TaskId, WorkflowDefinition};
use crate::observers::{
    CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverError, ObserverId, ObserverName,
    ObserverRecord, ObserverRun, RunId, SecretRef, WatchEntry, WatchMode, WatchObjectPolicy,
    WatchOrigin, WatchWrite,
};
use crate::orchestration::{OrchestrationError, OwnerRef};

/// How many of the latest runs `observer show` lists.
pub const SHOWN_RUNS: u64 = 10;

impl ProtocolService {
    /// Creates a draft observer of `owner` (R1, R2, R3). A "create a task"
    /// effect is checked against the catalogue's workflow, whose version
    /// it takes when the request names none.
    pub fn create_observer(
        &self,
        owner: &OwnerRef,
        request: CreateObserverRequest,
    ) -> ProtocolResult<ObserverView> {
        let name = ObserverName::parse(request.name).map_err(ProtocolError::from)?;
        let mut workflow: Option<WorkflowDefinition> = None;
        let effect = match request.effect {
            ObserverEffectRequest::CreateTask {
                workflow_id,
                workflow_version,
                workspaces,
                backend_id,
                profile,
                mapping,
            } => {
                let definition = self
                    .launch
                    .workflow_definition(&workflow_id)
                    .map_err(launch_refusal)?;
                let effect = CreateTaskEffect {
                    workflow_id,
                    workflow_version: workflow_version.unwrap_or(definition.version),
                    workspaces,
                    backend_id,
                    profile,
                    mapping,
                };
                workflow = Some(definition);
                ObserverEffect::CreateTask(effect)
            }
            ObserverEffectRequest::DeliverEvent => ObserverEffect::DeliverEvent,
        };
        let mut config = ObserverConfig::new(name, request.source, effect);
        if let Some(condition) = request.condition {
            config.condition = condition;
        }
        if let Some(interval_secs) = request.interval_secs {
            config.schedule.interval_secs = interval_secs;
        }
        if request.watch_list {
            config.watch_mode = WatchMode::WatchList;
        }
        config.secrets = request
            .secrets
            .into_iter()
            .map(|secret| SecretRef {
                name: secret.name,
                env: secret.env,
            })
            .collect();
        if let Some(limit) = request.task_limit_per_run {
            config.task_limit_per_run = limit;
        }
        config.watch_objects = WatchObjectPolicy {
            allowed_prefixes: request.allowed_object_prefixes,
            max_len: request
                .max_object_len
                .unwrap_or(WatchObjectPolicy::default().max_len),
        };
        config.import_backlog = request.import_backlog;
        let record =
            self.observers
                .create(owner.clone(), config, workflow.as_ref(), self.control.now())?;
        tracing::info!(observer = %record.observer_id, name = %record.config.name, "observers: created");
        Ok(observer_view(&record, self.control.now()))
    }

    /// The observers of `owner`, by name.
    pub fn observers_of(&self, owner: &OwnerRef) -> ProtocolResult<ObserverListResponse> {
        let now = self.control.now();
        Ok(ObserverListResponse {
            observers: self
                .observers
                .list()?
                .iter()
                .filter(|record| &record.owner == owner)
                .map(|record| observer_view(record, now))
                .collect(),
        })
    }

    /// One observer of `owner` in detail, with its latest runs.
    pub fn observer_detail(
        &self,
        owner: &OwnerRef,
        observer: &str,
    ) -> ProtocolResult<ObserverDetailView> {
        let record = self.owned_observer(owner, observer)?;
        let runs = self.latest_runs(&record)?;
        let mapping = match &record.config.effect {
            ObserverEffect::CreateTask(effect) => effect.mapping.clone(),
            ObserverEffect::DeliverEvent => Default::default(),
        };
        Ok(ObserverDetailView {
            observer: observer_view(&record, self.control.now()),
            condition: record.config.condition.clone(),
            mapping,
            secrets: record
                .config
                .secrets
                .iter()
                .map(|secret| ObserverSecretRef {
                    name: secret.name.clone(),
                    env: secret.env.clone(),
                })
                .collect(),
            allowed_object_prefixes: record.config.watch_objects.allowed_prefixes.clone(),
            scripts: record.scripts.iter().map(script_view).collect(),
            runs: runs.iter().map(run_view).collect(),
        })
    }

    /// Saves `script` as the next, immutable script version of an observer
    /// of `owner`. Saving never approves it (R11, AE6).
    pub fn add_observer_script(
        &self,
        owner: &OwnerRef,
        observer: &str,
        script: &str,
    ) -> ProtocolResult<ObserverScriptView> {
        let record = self.owned_observer(owner, observer)?;
        let version = self
            .observers
            .add_script(&record.observer_id, script, self.control.now())?;
        let record = self.observers.load(&record.observer_id)?;
        let listed = record
            .script_version(version)
            .ok_or_else(|| ProtocolError::Internal("a saved script is not listed".into()))?;
        tracing::info!(observer = %record.observer_id, version, "observers: script version saved");
        Ok(script_view(listed))
    }

    /// [`Self::approve_observer_version`] for an observer of `owner`.
    pub fn approve_observer_version_for(
        &self,
        owner: &OwnerRef,
        observer: &str,
        version: u32,
    ) -> ProtocolResult<ObserverView> {
        let record = self.owned_observer(owner, observer)?;
        let record = self.approve_observer_version(&record.observer_id, version)?;
        tracing::info!(observer = %record.observer_id, version, "observers: script version approved");
        Ok(observer_view(&record, self.control.now()))
    }

    /// [`Self::pause_observer`] for an observer of `owner`.
    pub fn pause_observer_for(
        &self,
        owner: &OwnerRef,
        observer: &str,
    ) -> ProtocolResult<ObserverView> {
        let record = self.owned_observer(owner, observer)?;
        let record = self.pause_observer(&record.observer_id)?;
        Ok(observer_view(&record, self.control.now()))
    }

    /// [`Self::resume_observer`] for an observer of `owner`.
    pub fn resume_observer_for(
        &self,
        owner: &OwnerRef,
        observer: &str,
    ) -> ProtocolResult<ObserverView> {
        let record = self.owned_observer(owner, observer)?;
        let record = self.resume_observer(&record.observer_id)?;
        Ok(observer_view(&record, self.control.now()))
    }

    /// Deletes an observer of `owner` with every file of it: its scripts,
    /// runs and watch entries. A run in progress loses its
    /// token at once. The tasks it created stay. Its id is never reused.
    pub fn delete_observer(
        &self,
        owner: &OwnerRef,
        observer: &str,
    ) -> ProtocolResult<ObserverView> {
        let record = self.owned_observer(owner, observer)?;
        if !self.observers.delete(&record.observer_id)? {
            return Err(ProtocolError::NotFound);
        }
        tracing::info!(observer = %record.observer_id, name = %record.config.name, "observers: deleted");
        Ok(observer_view(&record, self.control.now()))
    }

    /// The watch entries of an observer of `owner`, oldest first.
    pub fn observer_watches(
        &self,
        owner: &OwnerRef,
        observer: &str,
    ) -> ProtocolResult<ObserverWatchListResponse> {
        let record = self.owned_observer(owner, observer)?;
        let mut entries = self.observers.list_watches(&record.observer_id)?;
        entries.sort_by(|left, right| {
            (left.created_at, &left.entry_id).cmp(&(right.created_at, &right.entry_id))
        });
        Ok(ObserverWatchListResponse {
            watches: entries.iter().map(watch_view).collect(),
        })
    }

    /// Adds the owner's watch entry for `request.object` and the task
    /// `request.task_id` (R8), idempotently: an entry for the same task and
    /// object is answered as it is, with `false`.
    ///
    /// The observer must keep a watch list, the task must be the caller's,
    /// and the object must pass the observer's object policy, so the
    /// source's token never goes to a host the owner did not allow.
    pub fn add_observer_watch(
        &self,
        owner: &OwnerRef,
        observer: &str,
        request: AddObserverWatchRequest,
    ) -> ProtocolResult<(ObserverWatchView, bool)> {
        let record = self.owned_observer(owner, observer)?;
        if record.config.watch_mode != WatchMode::WatchList {
            return Err(ProtocolError::conflict(
                "observer_keeps_no_watch_list",
                format!(
                    "observer '{}' keeps no watch list (create it with --watch-list)",
                    record.config.name
                ),
            ));
        }
        let task_id = parse_identifier(TaskId::parse(request.task_id))?;
        let unknown_task = || {
            ProtocolError::bad_request("unknown_task", format!("task '{task_id}' does not exist"))
        };
        match self.control.task(&task_id) {
            Ok(task) if &task.owner == owner => {}
            // Another owner's task is indistinguishable from none.
            Ok(_) | Err(OrchestrationError::TaskNotFound(_)) => return Err(unknown_task()),
            Err(error) => return Err(error.into()),
        }
        record
            .config
            .watch_objects
            .check(&request.object)
            .map_err(watch_refusal)?;
        let entry = WatchEntry::new(
            task_id,
            None,
            request.object,
            request.event_types,
            WatchOrigin::Operator,
            self.control.now(),
        );
        entry.validate().map_err(watch_refusal)?;
        let lock = self.observers.lock(&record.observer_id)?;
        match self.observers.create_watch(&lock, &entry)? {
            WatchWrite::Created => {
                tracing::info!(observer = %record.observer_id, entry = %entry.entry_id, "observers: the owner added a watch entry");
                Ok((watch_view(&entry), true))
            }
            WatchWrite::Exists(existing) => Ok((watch_view(&existing), false)),
        }
    }

    /// Removes the watch entry `entry_id` of an observer of `owner`: its
    /// object is not looked at for that task any more.
    pub fn remove_observer_watch(
        &self,
        owner: &OwnerRef,
        observer: &str,
        entry_id: &str,
    ) -> ProtocolResult<ObserverWatchView> {
        let record = self.owned_observer(owner, observer)?;
        let lock = self.observers.lock(&record.observer_id)?;
        let entry = self
            .observers
            .list_watches(&record.observer_id)?
            .into_iter()
            .find(|entry| entry.entry_id == entry_id)
            .ok_or(ProtocolError::NotFound)?;
        if !self.observers.remove_watch(&lock, &entry.entry_id)? {
            return Err(ProtocolError::NotFound);
        }
        tracing::info!(observer = %record.observer_id, entry = %entry.entry_id, "observers: the owner removed a watch entry");
        Ok(watch_view(&entry))
    }

    /// The observer `observer` (an id when it has the id's form, else a
    /// name) of `owner`; not found for one of another owner. Names never
    /// have the id's form ([`ObserverName::parse`]), so no name shadows an
    /// id.
    fn owned_observer(&self, owner: &OwnerRef, observer: &str) -> ProtocolResult<ObserverRecord> {
        let found = if ObserverName::looks_like_id(observer) {
            match ObserverId::parse(observer) {
                Ok(observer_id) => match self.observers.load(&observer_id) {
                    Ok(record) => Some(record),
                    Err(ObserverError::NotFound(_)) => None,
                    Err(error) => return Err(error.into()),
                },
                Err(_) => None,
            }
        } else {
            self.observers.find_by_name(observer)?
        };
        match found {
            Some(record) if &record.owner == owner => Ok(record),
            _ => Err(ProtocolError::NotFound),
        }
    }

    /// The latest [`SHOWN_RUNS`] runs of `record`, newest first. Runs are
    /// numbered without gaps, so only the files shown are read.
    fn latest_runs(&self, record: &ObserverRecord) -> ProtocolResult<Vec<ObserverRun>> {
        let last = record.last_run_sequence;
        let first = last.saturating_sub(SHOWN_RUNS - 1).max(1);
        let mut runs = Vec::new();
        for sequence in (first..=last).rev() {
            match self
                .observers
                .load_run(&RunId::new(record.observer_id.clone(), sequence))
            {
                Ok(run) => runs.push(run),
                Err(ObserverError::RunNotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(runs)
    }
}

/// A refused watch object or event type, as the owner reads it.
fn watch_refusal(error: ObserverError) -> ProtocolError {
    match error {
        ObserverError::Invalid { .. } => {
            ProtocolError::bad_request("invalid_watch_entry", error.to_string())
        }
        other => other.into(),
    }
}

/// The wire name of a `snake_case` enum value (`needs_attention`, say).
fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unrecognized".to_string())
}

/// What `observer list` shows of `record` at `now`.
fn observer_view(record: &ObserverRecord, now: DateTime<Utc>) -> ObserverView {
    let (effect, workflow_id) = match &record.config.effect {
        ObserverEffect::CreateTask(effect) => ("create_task", Some(effect.workflow_id.clone())),
        ObserverEffect::DeliverEvent => ("deliver_event", None),
    };
    ObserverView {
        observer_id: record.observer_id.to_string(),
        name: record.config.name.to_string(),
        source: record.config.source.clone(),
        state: wire_name(&record.state),
        effect: effect.to_string(),
        workflow_id,
        watch_list: record.config.watch_mode == WatchMode::WatchList,
        interval_secs: record.config.schedule.interval_secs,
        task_limit_per_run: record.config.task_limit_per_run,
        approved_version: record.approved_version,
        latest_version: record.latest_script_version(),
        next_run_at: record.next_run_at,
        waiting_for_claim_since: record.waiting_for_claim_since(),
        worker_missing: record.worker_missing(now),
        last_claim_at: record.last_claim_at,
        active_run: record.active_run,
        last_run: record.last_run.as_ref().map(|run| ObserverLastRunView {
            sequence: run.sequence,
            status: wire_name(&run.status),
            finished_at: run.finished_at,
            error: run.error.clone(),
        }),
        consecutive_failures: record.consecutive_failures,
        created_at: record.created_at,
    }
}

fn script_view(script: &crate::observers::ScriptVersion) -> ObserverScriptView {
    ObserverScriptView {
        version: script.version,
        created_at: script.created_at,
        sha256: script.sha256.clone(),
    }
}

fn run_view(run: &ObserverRun) -> ObserverRunView {
    ObserverRunView {
        run_id: run.run_id().to_string(),
        sequence: run.sequence,
        script_version: run.script_version,
        status: wire_name(&run.status),
        claimed_at: run.claimed_at,
        finished_at: run.finished_at,
        exit_code: run.exit_code,
        error: run.error.clone(),
        tasks_created: run.tasks_created,
        stderr: run.stderr.clone(),
    }
}

fn watch_view(entry: &WatchEntry) -> ObserverWatchView {
    ObserverWatchView {
        entry_id: entry.entry_id.clone(),
        task_id: entry.task_id.to_string(),
        step: entry.step.clone(),
        object: entry.object.clone(),
        event_types: entry.event_types.clone(),
        origin: wire_name(&entry.origin),
        created_at: entry.created_at,
    }
}
