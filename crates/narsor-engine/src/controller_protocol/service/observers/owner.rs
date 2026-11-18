//! What the owner does with their observers through `narsor-cli observer`,
//! the TUI and the WebUI (R18, R21, R31, R35): create, list, show, update
//! the queue, mode and schedule, save a script version, approve, pause,
//! resume and delete; and what they read of their queues (R23, R24).
//!
//! An observer is named by its name or its id. Every method takes the
//! caller's [`OwnerRef`]: another owner's observer, or queue, is answered
//! exactly like one that does not exist (`404`), so a caller learns nothing
//! of it (R7).
//!
//! ```text
//! create         config checked, "create a task" against the catalogue,
//!                the mode against the owner's queue (R26)            → draft
//! update         queue, mode, schedule; the mode against the queue it
//!                then serves (R26), the schedule as at create (R28)
//! script set     next immutable version; never approved by saving (AE6)
//! approve        the schedule runs that version only (R11)
//! delete         its scripts and run errors; the entries of its queue
//!                when no other observer of the owner serves it (R33)
//! queue list     the names of the owner's observers' queues and of the
//!                owner's entries, with counts
//! queue show     its observers, its entries, and per entry and observer
//!                the last run time and the newest error (R23)
//! ```

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::tasks::launch_refusal;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::WorkflowDefinition;
use crate::observers::{
    CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverError, ObserverId, ObserverMode,
    ObserverName, ObserverRecord, QueueEntry, RunFailure, ScheduleConfig, SecretRef,
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
        let mut config = ObserverConfig::new(name, effect, mode);
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
                .owned_observers(owner)?
                .iter()
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
        Ok(ObserverDetailView {
            observer: observer_view(&record, self.control.now()),
            condition: record.config.condition.clone(),
            scripts: record.scripts.iter().map(script_view).collect(),
            errors: errors.iter().map(run_error_view).collect(),
        })
    }

    /// Changes the queue, the mode or the schedule of an observer of
    /// `owner` (R31, R35); what the request leaves out stays. The new
    /// configuration is checked as a new observer's: a plain queue name, a
    /// schedule of exactly one interval or cron expression (R28), and the
    /// mode of the queue it then serves (R26, refused with the queue's
    /// mode). State, scripts and runs are kept.
    pub fn update_observer(
        &self,
        owner: &OwnerRef,
        observer: &str,
        request: UpdateObserverRequest,
    ) -> ProtocolResult<ObserverView> {
        if request.is_empty() {
            return Err(ProtocolError::bad_request(
                "invalid_request",
                "an update names at least one of queue, mode, interval_secs and cron",
            ));
        }
        let record = self.owned_observer(owner, observer)?;
        let mut config = record.config.clone();
        if let Some(queue) = request.queue {
            config.queue = queue;
        }
        if let Some(mode) = request.mode {
            config.mode = mode;
        }
        if request.interval_secs.is_some() || request.cron.is_some() {
            config.schedule = ScheduleConfig::from_parts(request.interval_secs, request.cron)
                .map_err(ProtocolError::from)?;
        }
        // The effect is unchanged, so the store does not check it against
        // the catalogue again.
        let record =
            self.observers
                .update_config(&record.observer_id, config, None, self.control.now())?;
        tracing::info!(observer = %record.observer_id, queue = %record.config.queue, mode = %record.config.mode, "observers: configuration updated");
        Ok(observer_view(&record, self.control.now()))
    }

    /// The queues of `owner`, by name, with their counts of observers and
    /// entries (R23, R24). A queue is a name that an observer of `owner`
    /// serves or an entry of a gate of `owner` names (R6, R7).
    pub fn observer_queues_of(
        &self,
        owner: &OwnerRef,
    ) -> ProtocolResult<ObserverQueueListResponse> {
        let observers = self.owned_observers(owner)?;
        let entries = self.owned_entries(owner)?;
        let names: BTreeSet<&str> = observers
            .iter()
            .map(|record| record.config.queue.as_str())
            .chain(entries.iter().map(|entry| entry.queue.as_str()))
            .collect();
        let queues = names
            .into_iter()
            .map(|queue| {
                let served: Vec<&ObserverRecord> = observers
                    .iter()
                    .filter(|record| record.config.queue == queue)
                    .collect();
                ObserverQueueSummary {
                    queue: queue.to_string(),
                    mode: queue_mode(&served),
                    observers: count(served.len()),
                    entries: count(entries.iter().filter(|entry| entry.queue == queue).count()),
                }
            })
            .collect();
        Ok(ObserverQueueListResponse { queues })
    }

    /// One queue of `owner`: its observers, its entries, and per entry and
    /// observer the last run time and the newest error (R23). Not found
    /// when no observer of `owner` serves it and no entry of `owner` names
    /// it, another owner's queue of that name included (R7).
    pub fn observer_queue(
        &self,
        owner: &OwnerRef,
        queue: &str,
    ) -> ProtocolResult<ObserverQueueView> {
        let observers: Vec<ObserverRecord> = self
            .owned_observers(owner)?
            .into_iter()
            .filter(|record| record.config.queue == queue)
            .collect();
        let entries = self.observers.queue_entries(owner, queue)?;
        if observers.is_empty() && entries.is_empty() {
            return Err(ProtocolError::NotFound);
        }
        // The newest error of each observer per entry: the store keeps one
        // per entry (R23), newest first.
        let mut errors: BTreeMap<(&ObserverId, String), RunFailure> = BTreeMap::new();
        for record in &observers {
            for failure in self.observers.list_failures(&record.observer_id)? {
                if let Some(entry_id) = failure.entry_id.clone() {
                    errors
                        .entry((&record.observer_id, entry_id))
                        .or_insert(failure);
                }
            }
        }
        let now = self.control.now();
        let served: Vec<&ObserverRecord> = observers.iter().collect();
        Ok(ObserverQueueView {
            queue: queue.to_string(),
            mode: queue_mode(&served),
            entries: entries
                .iter()
                .map(|entry| entry_view(entry, &observers, &errors))
                .collect(),
            observers: observers
                .iter()
                .map(|record| observer_view(record, now))
                .collect(),
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

    /// Script version `version` of an observer of `owner`, with its text: a
    /// view shows it before the owner approves it (R32) and opens it to
    /// edit the next version (R31).
    pub fn observer_script(
        &self,
        owner: &OwnerRef,
        observer: &str,
        version: u32,
    ) -> ProtocolResult<ObserverScriptTextView> {
        let record = self.owned_observer(owner, observer)?;
        let listed = record.script_version(version).ok_or_else(|| {
            ProtocolError::from(ObserverError::ScriptNotFound {
                observer_id: record.observer_id.clone(),
                version,
            })
        })?;
        let script = self.observers.load_script(&record.observer_id, version)?;
        Ok(ObserverScriptTextView {
            version: listed.version,
            created_at: listed.created_at,
            sha256: listed.sha256.clone(),
            script,
        })
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
    /// and run errors (R33). A run in progress loses its token at once.
    /// The tasks it created stay. The entries of its queue go with the
    /// queue's last observer of `owner`: no observer is left to answer
    /// them, and the gates wait for a person. Should an observer of the
    /// queue come back, its tick adds the entries of the waiting tasks
    /// again. Its id is never reused.
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
        self.drop_unserved_queue_entries(owner, &record.config.queue);
        Ok(observer_view(&record, self.control.now()))
    }

    /// Removes the entries of `owner`'s queue `queue` when no observer of
    /// `owner` serves it any more. A failure is logged: the delete itself
    /// is done, and an entry left over harms nothing (no run takes it).
    fn drop_unserved_queue_entries(&self, owner: &OwnerRef, queue: &str) {
        let removed = (|| -> Result<usize, ObserverError> {
            if !self.observers.queue_observers(owner, queue)?.is_empty() {
                return Ok(0);
            }
            let lock = self.observers.lock_queue_entries()?;
            let mut removed = 0;
            for entry in self.observers.queue_entries(owner, queue)? {
                if self.observers.remove_queue_entry(&lock, &entry.entry_id)? {
                    removed += 1;
                }
            }
            Ok(removed)
        })();
        match removed {
            Ok(0) => {}
            Ok(removed) => {
                tracing::info!(%queue, removed, "observers: the queue's last observer is deleted; its entries are removed");
            }
            Err(error) => {
                tracing::warn!(%queue, %error, "observers: cannot remove the entries of a queue without observers");
            }
        }
    }

    /// The observers of `owner`, by name.
    fn owned_observers(&self, owner: &OwnerRef) -> ProtocolResult<Vec<ObserverRecord>> {
        Ok(self
            .observers
            .list()?
            .into_iter()
            .filter(|record| &record.owner == owner)
            .collect())
    }

    /// The queue entries of `owner`'s gates, oldest first.
    fn owned_entries(&self, owner: &OwnerRef) -> ProtocolResult<Vec<QueueEntry>> {
        Ok(self
            .observers
            .list_queue_entries()?
            .into_iter()
            .filter(|entry| &entry.owner == owner)
            .collect())
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

/// What `observer list` and the queues view show of `record` at `now`.
fn observer_view(record: &ObserverRecord, now: DateTime<Utc>) -> ObserverView {
    let (effect, workflow_id, mapping) = match &record.config.effect {
        ObserverEffect::CreateTask(effect) => (
            "create_task",
            Some(effect.workflow_id.clone()),
            effect.mapping.clone(),
        ),
        ObserverEffect::None => ("none", None, BTreeMap::new()),
    };
    ObserverView {
        observer_id: record.observer_id.to_string(),
        name: record.config.name.to_string(),
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
        secrets: record
            .config
            .secrets
            .iter()
            .map(|secret| ObserverSecretRef {
                name: secret.name.clone(),
                env: secret.env.clone(),
            })
            .collect(),
        mapping,
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

/// The mode of a queue's observers (one for all, R26); `None` when none
/// serves it.
fn queue_mode(observers: &[&ObserverRecord]) -> Option<String> {
    observers
        .first()
        .map(|record| record.config.mode.to_string())
}

/// A count on the wire.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// What the queues view shows of `entry`: per observer of its queue, a run
/// in progress, the last run time and the newest error (R23).
fn entry_view(
    entry: &QueueEntry,
    observers: &[ObserverRecord],
    errors: &BTreeMap<(&ObserverId, String), RunFailure>,
) -> QueueEntryView {
    QueueEntryView {
        entry_id: entry.entry_id.clone(),
        task_id: entry.task_id.to_string(),
        step: entry.step.clone(),
        episode: entry.episode.clone(),
        started_at: entry.created_at,
        params: entry.params.clone(),
        observers: observers
            .iter()
            .map(|record| QueueEntryObserverView {
                observer: record.config.name.to_string(),
                running: record
                    .active_run
                    .as_ref()
                    .and_then(|active| active.entry.as_ref())
                    .is_some_and(|run| run.entry_id == entry.entry_id),
                last_run_at: record.entry_last_runs.get(&entry.entry_id).copied(),
                last_error: errors
                    .get(&(&record.observer_id, entry.entry_id.clone()))
                    .map(|failure| QueueEntryErrorView {
                        run_id: failure.run_id().to_string(),
                        status: wire_name(&failure.status),
                        finished_at: failure.finished_at,
                        exit_code: failure.exit_code,
                        error: failure.error.clone(),
                    }),
            })
            .collect(),
    }
}
