//! Creating tasks: fresh, sealed and replayed.

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::{parse_identifier, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::domain::{
    normalize_label, LabelError, ResourceProfile, TaskId, TICKET_MAX_CHARS, TITLE_MAX_CHARS,
};
use crate::environment::{LaunchError, LaunchIntent};
use crate::orchestration::{NewTask, OwnerRef};

impl ProtocolService {
    /// Creates a queued task for `owner`, idempotently by task id.
    ///
    /// An id already taken by another owner's task is answered `NotFound`,
    /// exactly like every other request about that task: a conflict would
    /// confirm to this owner that the other task exists.
    pub fn create_task(
        &self,
        owner: &OwnerRef,
        request: CreateTaskRequest,
    ) -> ProtocolResult<TaskSummary> {
        let request_digest = request_digest(&request)?;
        let task_id = match request.task_id {
            Some(task_id) => parse_identifier(TaskId::parse(task_id))?,
            None => TaskId::parse(format!("task-{}", uuid::Uuid::new_v4().simple()))
                .expect("a generated task id is always valid"),
        };
        match self.control.task(&task_id) {
            Ok(existing) if existing.owner != *owner => return Err(ProtocolError::NotFound),
            // A retry of the identical request: the task it created, even if
            // the configuration it resolved against — the default backend, a
            // limit, an image — changed since. Resolving again could only
            // produce a spurious conflict (U10).
            Ok(existing) if existing.request_digest.as_deref() == Some(&request_digest) => {
                return self.summarize(&existing);
            }
            Ok(_) | Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        // The owner's durable default backend (R10) is read here, once, for
        // a request that names none; the configured one applies when the
        // owner never chose. The task is pinned to the result for good.
        let backend = match request.backend_id {
            Some(backend) => Some(backend),
            None => self
                .control
                .default_backend()?
                .and_then(|selection| selection.backend)
                .map(|backend| backend.to_string()),
        };
        // Resolution reads configuration only; a refusal here returns before
        // the control plane is asked to store anything, so an invalid backend
        // limit, an unregistered secret, an unsupported harness or a missing
        // image leaves no task and no assignment behind (U6).
        let launch = request.launch;
        let ticket = normalize_label("ticket", launch.ticket.as_deref(), TICKET_MAX_CHARS)
            .map_err(label_refusal)?;
        let title = normalize_label("title", launch.title.as_deref(), TITLE_MAX_CHARS)
            .map_err(label_refusal)?;
        let resolved = self
            .launch
            .resolve(&LaunchIntent {
                backend,
                workflow_id: launch.workflow_id,
                workflow_version: launch.workflow_version,
                inputs: launch.inputs,
                workspaces: launch.workspaces,
                profile: launch.profile,
                harness: launch.harness,
                resources: launch.resources.map(|resources| {
                    ResourceProfile::new(resources.cpu_millicores, resources.memory_mib)
                }),
                ticket,
                title,
            })
            .map_err(launch_refusal)?;
        self.create_sealed_task(owner, task_id, request_digest, resolved)
    }

    /// Persists an already-resolved task through the same idempotent
    /// admission path as [`Self::create_task`].
    ///
    /// The local API resolves workflow references before they become a
    /// generic control-plane request: local file paths are meaningful to the
    /// daemon, but must never be stored as remote launch input.  The caller
    /// supplies the digest of its stable submission envelope, while this
    /// method retains all task-id ownership, replay, queueing, and scheduler
    /// semantics in one place.
    pub fn create_sealed_task(
        &self,
        owner: &OwnerRef,
        task_id: TaskId,
        request_digest: String,
        resolved: crate::environment::ResolvedLaunch,
    ) -> ProtocolResult<TaskSummary> {
        match self.control.task(&task_id) {
            Ok(existing) if existing.owner != *owner => return Err(ProtocolError::NotFound),
            Ok(existing) if existing.request_digest.as_deref() == Some(&request_digest) => {
                return self.summarize(&existing);
            }
            Ok(_) | Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        let record = self
            .control
            .create_task(
                NewTask::new(
                    task_id.clone(),
                    owner.clone(),
                    resolved.backend_id,
                    resolved.spec,
                )
                .with_request_digest(request_digest),
            )
            .map_err(|error| match error {
                // Created by another owner between the check above and now.
                crate::orchestration::OrchestrationError::TaskConflict(_)
                    if self.someone_else_owns(owner, &task_id).unwrap_or(true) =>
                {
                    ProtocolError::NotFound
                }
                other => other.into(),
            })?;
        self.notify(&record.task_id);
        self.reschedule(&record.assignment.backend_id);
        self.owned_summary(owner, &record.task_id)
    }

    /// Returns an existing sealed submission before a local caller performs
    /// any current workflow or environment resolution.  The submission
    /// envelope, not today's catalogue, defines replay identity.
    pub fn replay_sealed_task(
        &self,
        owner: &OwnerRef,
        task_id: &TaskId,
        request_digest: &str,
    ) -> ProtocolResult<Option<TaskSummary>> {
        match self.control.task(task_id) {
            Ok(existing) if existing.owner != *owner => Err(ProtocolError::NotFound),
            Ok(existing) if existing.request_digest.as_deref() == Some(request_digest) => {
                self.summarize(&existing).map(Some)
            }
            Ok(existing) => Err(ProtocolError::conflict(
                "task_conflict",
                format!(
                    "task '{}' already exists with a different submission",
                    existing.task_id
                ),
            )),
            Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

/// Digest of a create request — what an identical retry presents again.
fn request_digest(request: &CreateTaskRequest) -> ProtocolResult<String> {
    let value = serde_json::to_value(request)
        .map_err(|error| ProtocolError::Internal(format!("cannot digest a request: {error}")))?;
    Ok(crate::domain::canonical_sha256(&value))
}

/// A launch that did not resolve, as the owner sees it: a `400` naming the
/// reason, or a bare `500` when the catalogue could not even be read.
pub(crate) fn launch_refusal(error: LaunchError) -> ProtocolError {
    if error.is_internal() {
        ProtocolError::Internal(error.to_string())
    } else {
        ProtocolError::bad_request(error.code(), error.to_string())
    }
}

/// A ticket or title that failed validation, as the owner sees it.
fn label_refusal(error: LabelError) -> ProtocolError {
    ProtocolError::bad_request("invalid_task_label", error.to_string())
}
