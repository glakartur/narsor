//! A served control plane for client tests of remote tasks (U8): the real
//! API server with a hand-moved clock, an owner client, and helpers that
//! play the controller and the runner over their real protocols — so a
//! client test sees exactly what a remote backend would make it see.
//!
//! Needs `mod environment_support;` next to `mod remote_support;` in the
//! test crate.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use narsor::api::{self, ApiClient, ApiServices};
use narsor::controller_protocol::{
    ControllerClient, CreateEnrollmentRequest, CreateTaskRequest, LaunchRequest, ProtocolService,
    ReportRequest, RunnerClient, RunnerCredentialRequest, TaskSummary,
};
use narsor::domain::ExecutionStatus;
use narsor::engine::{Engine, PendingHumanInput, Timeline};
use narsor::orchestration::{
    Clock, ControlPlane, ConversationAttemptSummary, ConversationAvailability, ExecutionCursor,
    Observation, ObservedSummary, PendingInputRef, ResourceIdentity, RunnerSnapshot,
};
use narsor::persistence::paths::StatePaths;
use narsor::persistence::ExecutionStore;

use super::environment_support;

/// A temporary directory that deletes itself.
pub struct TempRoot(PathBuf);

impl TempRoot {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-remote-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A clock the test moves by hand.
#[derive(Debug)]
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    pub fn advance(&self, by: chrono::Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// The served control plane.
pub struct RemoteHarness {
    pub root: TempRoot,
    pub clock: Arc<ManualClock>,
    pub handle: api::ApiHandle,
    pub owner: ApiClient,
}

/// A task a controller brought to `Running`, with its runner.
pub struct Running {
    pub controller: ControllerClient,
    pub epoch: u64,
    pub runner: RunnerClient,
}

impl RemoteHarness {
    pub async fn start() -> Self {
        let root = TempRoot::new();
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap(),
        )));
        let control = ControlPlane::with_clock(StatePaths::new(root.path()), clock.clone());
        let services = ApiServices {
            control: Some(Arc::new(ProtocolService::new(
                control,
                environment_support::seeded_resolver(root.path()),
            ))),
            ..ApiServices::default()
        };
        let handle = api::start_with_services(
            Arc::new(Engine::new(ExecutionStore::new(root.path()))),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            services,
        )
        .await
        .expect("server starts");
        let owner = handle.client();
        Self {
            root,
            clock,
            handle,
            owner,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.handle.addr())
    }

    pub async fn create(&self, task: &str, backend: Option<&str>) -> TaskSummary {
        self.owner
            .create_control_task(&create_request(task, backend))
            .await
            .expect("create task")
    }

    /// An enrolled controller holding `backend`'s lease.
    pub async fn controller(&self, backend: &str) -> (ControllerClient, u64) {
        let code = self
            .owner
            .create_enrollment(backend, &CreateEnrollmentRequest::default())
            .await
            .expect("enrollment code");
        let (controller, _) = ControllerClient::enroll(self.base_url(), &code.enrollment_code)
            .await
            .expect("enroll");
        let lease = controller.acquire_lease(None).await.expect("lease");
        (controller, lease.epoch)
    }

    /// Creates `task` on `backend` and drives it to `Running` the way a
    /// controller must; the runner reports `snapshot`.
    pub async fn running(&self, task: &str, backend: &str, first: RunnerSnapshot) -> Running {
        self.create(task, Some(backend)).await;
        let (controller, epoch) = self.controller(backend).await;
        controller
            .report(&report(task, epoch, 1, Observation::StartAccepted, None))
            .await
            .expect("start accepted");
        let credential = controller
            .issue_runner_credential(
                task,
                &RunnerCredentialRequest {
                    lease_epoch: epoch,
                    generation: 1,
                    incarnation: "pod-1".to_string(),
                },
            )
            .await
            .expect("runner credential");
        let runner = RunnerClient::new(self.base_url(), credential.credential);
        runner.snapshot(&first).await.expect("first snapshot");
        controller
            .report(&report(task, epoch, 1, Observation::RunnerReady, Some(1)))
            .await
            .expect("runner ready");
        Running {
            controller,
            epoch,
            runner,
        }
    }
}

pub fn create_request(task: &str, backend: Option<&str>) -> CreateTaskRequest {
    CreateTaskRequest {
        task_id: Some(task.to_string()),
        backend_id: backend.map(str::to_string),
        launch: LaunchRequest {
            workflow_id: "implement-feature".to_string(),
            workflow_version: 1,
            inputs: serde_json::json!({ "ticket": "NAR-1" }),
            workspace: None,
            profile: None,
            harness: None,
            resources: None,
        },
    }
}

pub fn report(
    task: &str,
    epoch: u64,
    generation: u64,
    observation: Observation,
    resource_generation: Option<u64>,
) -> ReportRequest {
    ReportRequest {
        task_id: task.to_string(),
        lease_epoch: epoch,
        desired_generation: generation,
        observation,
        resource: resource_generation.map(|resource_generation| ResourceIdentity {
            uid: "pod-1".to_string(),
            resource_generation,
        }),
        summary: ObservedSummary {
            workload_present: true,
            storage_present: true,
            runner_status: None,
            detail: None,
        },
        observed_at: Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 1).unwrap(),
    }
}

/// A pending request with one required text field `reason`.
pub fn pending(id: &str) -> PendingInputRef {
    let form: PendingHumanInput = serde_json::from_value(serde_json::json!({
        "execution_id": "exec-1",
        "state_id": "review",
        "action_execution_id": "act-7",
        "instruction": "Approve the plan?",
        "fields": [{ "name": "reason", "type": "text", "required": true }],
        "outputs": [{ "label": "approved" }],
        "episode": id,
        "requested_at": "2026-10-03T12:00:00Z",
    }))
    .expect("pending input form");
    PendingInputRef {
        id: id.to_string(),
        attempt_id: Some("act-7".to_string()),
        form,
    }
}

/// A snapshot at `sequence` with a live conversation of attempt `act-6`.
pub fn snapshot(sequence: u64, pending_input: Option<&str>) -> RunnerSnapshot {
    RunnerSnapshot {
        snapshot_sequence: sequence,
        execution_status: if pending_input.is_some() {
            ExecutionStatus::Paused
        } else {
            ExecutionStatus::Running
        },
        execution_cursor: ExecutionCursor {
            execution_id: Some("exec-1".to_string()),
            current_state: Some("review".to_string()),
            action_execution_id: Some("act-7".to_string()),
        },
        event_cursor: sequence * 3,
        pending_input: pending_input.map(pending),
        conversation: ConversationAvailability::Live {
            attempts: vec![ConversationAttemptSummary {
                attempt_id: "act-6".to_string(),
                state_id: Some("plan".to_string()),
                entries: 2,
                closed: true,
                last_entry_at: None,
            }],
        },
        timeline: Some(Timeline {
            default_position: Some("plan #1".to_string()),
            ..Timeline::default()
        }),
        safe_point: false,
        problem: None,
        display: None,
        observed_at: Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 5).unwrap(),
    }
}
