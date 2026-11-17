//! What the owner does with their observers through `narsor-cli observer`
//! (R18, R21): create, list, show, save a script version, approve, pause,
//! resume and delete.
//!
//! An observer is named by its name or its id. Every method takes the
//! caller's [`OwnerRef`]: another owner's observer is answered exactly like
//! one that does not exist (`404`), so a caller learns nothing of it.
//!
//! ```text
//! create         config checked, "create a task" against the catalogue,
//!                the mode against the owner's queue (R26)            → draft
//! script set     next immutable version; never approved by saving (AE6)
//! approve        the schedule runs that version only (R11)
//! ```

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::tasks::launch_refusal;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::WorkflowDefinition;
use crate::observers::{
    CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverError, ObserverId, ObserverMode,
    ObserverName, ObserverRecord, RunFailure, ScheduleConfig, SecretRef,
};
use crate::orchestration::OwnerRef;

impl ProtocolService {
    /// Creates a draft observer of `owner` (R1, R2, R3). A "create a task"
    /// effect is checked against the catalogue's workflow, whose version
    /// it takes when the request names none.
    ///
    /// The queue is the observer's name when the request names none; the
    /// mode is `always` for an observer that creates tasks and
    /// `on_episodes` for any other. The schedule must be exactly one of an
    /// interval and a cron expression (R28).
    pub fn create_observer(
        &self,
        owner: &OwnerRef,
        request: CreateObserverRequest,
    ) -> ProtocolResult<ObserverView> {
        let name = ObserverName::parse(request.name).map_err(ProtocolError::from)?;
        let mut workflow: Option<WorkflowDefinition> = None;
        let schedule = ScheduleConfig::from_parts(request.interval_secs, request.cron)
            .map_err(ProtocolError::from)?;
        let effect = match request.effect {
            None => ObserverEffect::None,
            Some(ObserverEffectRequest::CreateTask {
                workflow_id,
                workflow_version,
                workspaces,
                backend_id,
                profile,
                mapping,
            }) => {
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
        };
        let mode = request.mode.unwrap_or(match effect {
            ObserverEffect::CreateTask(_) => ObserverMode::Always,
            ObserverEffect::None => ObserverMode::OnEpisodes,
        });
        let mut config = ObserverConfig::new(name, request.source, effect, mode);
        if let Some(queue) = request.queue {
            config.queue = queue;
        }
        config.schedule = schedule;
        if let Some(condition) = request.condition {
            config.condition = condition;
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

    /// One observer of `owner` in detail, with the kept execution errors of
    /// its runs (a successful run leaves none).
    pub fn observer_detail(
        &self,
        owner: &OwnerRef,
        observer: &str,
    ) -> ProtocolResult<ObserverDetailView> {
        let record = self.owned_observer(owner, observer)?;
        let errors = self.observers.list_failures(&record.observer_id)?;
        let mapping = match &record.config.effect {
            ObserverEffect::CreateTask(effect) => effect.mapping.clone(),
            ObserverEffect::None => Default::default(),
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
            scripts: record.scripts.iter().map(script_view).collect(),
            errors: errors.iter().map(run_error_view).collect(),
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

    /// Deletes an observer of `owner` with every file of it: its scripts
    /// and run errors. A run in progress loses its token at once. The tasks
    /// it created stay, and so do the entries of its queue (they belong to
    /// the gates). Its id is never reused.
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
        ObserverEffect::None => ("none", None),
    };
    ObserverView {
        observer_id: record.observer_id.to_string(),
        name: record.config.name.to_string(),
        source: record.config.source.clone(),
        state: wire_name(&record.state),
        effect: effect.to_string(),
        workflow_id,
        queue: record.config.queue.clone(),
        mode: record.config.mode.to_string(),
        interval_secs: record.config.schedule.interval_secs(),
        cron: record
            .config
            .schedule
            .cron()
            .map(|cron| cron.as_str().to_string()),
        task_limit_per_run: record.config.task_limit_per_run,
        approved_version: record.approved_version,
        latest_version: record.latest_script_version(),
        next_run_at: record.next_run_at,
        waiting_for_claim_since: record.waiting_for_claim_since(),
        worker_missing: record.worker_missing(now),
        last_claim_at: record.last_claim_at,
        active_run: record.active_run.as_ref().map(|active| active.sequence),
        last_run: record.last_run.as_ref().map(|run| ObserverLastRunView {
            sequence: run.sequence,
            status: wire_name(&run.status),
            finished_at: run.finished_at,
            error: run.error.clone(),
        }),
        consecutive_failures: record.consecutive_failures,
        note: record.note.clone(),
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

fn run_error_view(failure: &RunFailure) -> ObserverRunErrorView {
    ObserverRunErrorView {
        run_id: failure.run_id().to_string(),
        sequence: failure.sequence,
        script_version: failure.script_version,
        status: wire_name(&failure.status),
        claimed_at: failure.claimed_at,
        finished_at: failure.finished_at,
        exit_code: failure.exit_code,
        error: failure.error.clone(),
        stdout: failure.stdout.clone(),
        stderr: failure.stderr.clone(),
        entry_id: failure.entry_id.clone(),
    }
}
