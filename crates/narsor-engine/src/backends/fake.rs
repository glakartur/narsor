//! The in-memory contract-test adapter (U4, AE2).
//!
//! A [`FakeBackend`] models a container platform in a map: task storage
//! that outlives compute, workloads with platform UIDs, phases and resource
//! versions, deterministic names. Instead of running a real `narsor-runner`
//! each workload runs a *simulated runner* — a task that speaks the real
//! runner protocol over HTTP with the credential the controller issued:
//! it reports a first snapshot (so the controller sees the runner up),
//! long-polls its directive, acknowledges a pause with a safe-point snapshot
//! and exits 0, exactly as the runner contract in [`crate::runner`] says.
//! The simulated work counts up an event cursor kept on the fake *storage*,
//! so a resumed incarnation continues it the way a real one continues the
//! Engine's log.
//!
//! What the fake deliberately does not do is inject faults or count calls:
//! `narsor-controller-runtime::testkit::Probed` does both for any adapter, so
//! every adapter — this one, the local one, U5's Kubernetes one — runs the
//! same contract suite with the same faults.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;

use super::{
    ArchiveProgress, BackendError, BackendResult, ExecutionBackend, Provisioned, Removal,
    StorageState, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase, WorkloadState,
};
use crate::api::ClientError;
use crate::controller_protocol::{RunnerClient, RunnerDirective};
use crate::domain::{ExecutionStatus, TaskId};
use crate::environment::{BackendCapabilities, BackendKind};
use crate::orchestration::RunnerSnapshot;

/// Pause between a simulated runner's retries and polls.
const RUNNER_RETRY: Duration = Duration::from_millis(50);

/// Long-poll wait of a simulated runner, in seconds.
const RUNNER_POLL_SECS: u64 = 1;

/// The in-memory platform. Cloning shares it: two clones are two
/// controllers' views of one backend.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    platform: Arc<Mutex<Platform>>,
    simulate_runners: bool,
}

#[derive(Debug, Default)]
struct Platform {
    storages: HashMap<String, FakeStorage>,
    workloads: HashMap<String, FakeWorkload>,
    next_uid: u64,
}

#[derive(Debug, Default)]
struct FakeStorage {
    /// The simulated runner's progress, kept with the work it describes.
    event_cursor: u64,
}

#[derive(Debug)]
struct FakeWorkload {
    state: WorkloadState,
    runner: Option<JoinHandle<()>>,
}

impl Drop for Platform {
    fn drop(&mut self) {
        for workload in self.workloads.values_mut() {
            if let Some(runner) = workload.runner.take() {
                runner.abort();
            }
        }
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBackend {
    /// An empty platform whose workloads run simulated runners.
    pub fn new() -> Self {
        Self {
            platform: Arc::new(Mutex::new(Platform::default())),
            simulate_runners: true,
        }
    }

    /// An empty platform whose workloads stay silent: nothing ever reports,
    /// so a task stays Provisioning (for tests of the controller alone).
    pub fn without_runners() -> Self {
        Self {
            simulate_runners: false,
            ..Self::new()
        }
    }

    /// The platform loses the task's workload unexpectedly (a node failure,
    /// an eviction): the runner dies without a word and the workload shows
    /// terminated with no exit code. Returns whether there was one to lose.
    pub fn kill_workload(&self, task: &TaskResources) -> bool {
        let mut platform = self.lock();
        let Some(workload) = platform.workloads.get_mut(&task.workload) else {
            return false;
        };
        if let Some(runner) = workload.runner.take() {
            runner.abort();
        }
        if !workload.state.phase.is_alive() {
            return false;
        }
        workload.state.phase = WorkloadPhase::Exited { code: None };
        workload.state.resource_generation += 1;
        true
    }

    fn lock(&self) -> MutexGuard<'_, Platform> {
        self.platform.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn create(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        create_storage: bool,
    ) -> BackendResult<Provisioned> {
        let mut platform = self.lock();
        let created_storage = !platform.storages.contains_key(&task.storage);
        if created_storage {
            if !create_storage {
                return Err(BackendError::storage_missing(format!(
                    "task storage '{}' does not exist",
                    task.storage
                )));
            }
            platform
                .storages
                .insert(task.storage.clone(), FakeStorage::default());
        }
        if let Some(existing) = platform.workloads.get(&task.workload) {
            return Ok(Provisioned {
                workload: existing.state.clone(),
                created_workload: false,
                created_storage,
            });
        }
        platform.next_uid += 1;
        let state = WorkloadState {
            uid: format!("fake-{}-{}", task.workload, platform.next_uid),
            resource_generation: 1,
            incarnation: launch.incarnation.clone(),
            launch_generation: launch.generation,
            phase: WorkloadPhase::Starting,
        };
        let runner = self.simulate_runners.then(|| {
            tokio::spawn(simulated_runner(
                Arc::downgrade(&self.platform),
                task.workload.clone(),
                state.uid.clone(),
                task.storage.clone(),
                launch.clone(),
            ))
        });
        platform.workloads.insert(
            task.workload.clone(),
            FakeWorkload {
                state: state.clone(),
                runner,
            },
        );
        Ok(Provisioned {
            workload: state,
            created_workload: true,
            created_storage,
        })
    }

    fn remove_workload(platform: &mut Platform, name: &str) -> Removal {
        match platform.workloads.remove(name) {
            Some(mut workload) => {
                if let Some(runner) = workload.runner.take() {
                    runner.abort();
                }
                Removal::Removed
            }
            None => Removal::AlreadyAbsent,
        }
    }
}

impl ExecutionBackend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Fake
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            pause_resume: true,
            persistent_storage: true,
            archive: true,
        }
    }

    fn resources(&self, task_id: &TaskId, resource_key: &str) -> BackendResult<TaskResources> {
        // The key is a task id; anything else cannot be named safely.
        let key = TaskId::parse(resource_key)
            .map_err(|error| BackendError::rejected(error.to_string()))?;
        Ok(TaskResources {
            task_id: task_id.clone(),
            resource_key: resource_key.to_string(),
            workload: format!("narsor-task-{key}"),
            storage: format!("narsor-task-{key}-storage"),
        })
    }

    async fn observe(&self, task: &TaskResources) -> BackendResult<TaskObservation> {
        let platform = self.lock();
        Ok(TaskObservation {
            workload: platform
                .workloads
                .get(&task.workload)
                .map(|workload| workload.state.clone()),
            storage: if platform.storages.contains_key(&task.storage) {
                StorageState::Present
            } else {
                StorageState::Absent
            },
        })
    }

    async fn provision(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.create(task, launch, true)
    }

    async fn resume(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.create(task, launch, false)
    }

    async fn release_compute(&self, task: &TaskResources) -> BackendResult<Removal> {
        Ok(Self::remove_workload(&mut self.lock(), &task.workload))
    }

    async fn archive(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
        let mut platform = self.lock();
        let workload = Self::remove_workload(&mut platform, &task.workload);
        let storage = match platform.storages.remove(&task.storage) {
            Some(_) => Removal::Removed,
            None => Removal::AlreadyAbsent,
        };
        Ok(ArchiveProgress { workload, storage })
    }
}

/// A runner that does no work but keeps every promise of the runner
/// protocol (see the module docs).
async fn simulated_runner(
    platform: Weak<Mutex<Platform>>,
    workload: String,
    uid: String,
    storage: String,
    launch: WorkloadLaunch,
) {
    let client = RunnerClient::new(&launch.control_plane_url, launch.credential.expose());
    let mut runner = SimulatedRunner {
        platform,
        workload,
        uid,
        storage,
        client,
        sequence: 0,
    };
    let code = runner.run().await;
    runner.exit(code);
}

struct SimulatedRunner {
    platform: Weak<Mutex<Platform>>,
    workload: String,
    uid: String,
    storage: String,
    client: RunnerClient,
    sequence: u64,
}

impl SimulatedRunner {
    /// Runs until told to stop; returns the exit code.
    async fn run(&mut self) -> i32 {
        // "Up" means: the control plane accepted a first snapshot.
        let mut directive = loop {
            match self.report(false).await {
                Ok(directive) => break directive,
                Err(ClientError::Unauthorized) => return 1,
                Err(_) => tokio::time::sleep(RUNNER_RETRY).await,
            }
        };
        self.update(|state| {
            if state.phase == WorkloadPhase::Starting {
                state.phase = WorkloadPhase::Running;
                state.resource_generation += 1;
            }
        });
        loop {
            match directive {
                RunnerDirective::Continue => {}
                RunnerDirective::Stop => return 0,
                RunnerDirective::PauseAtSafePoint => match self.report(true).await {
                    Ok(RunnerDirective::PauseAtSafePoint | RunnerDirective::Stop) => return 0,
                    Ok(RunnerDirective::Continue) => {}
                    Err(ClientError::Unauthorized) => return 1,
                    Err(_) => {
                        tokio::time::sleep(RUNNER_RETRY).await;
                        continue;
                    }
                },
            }
            directive = match self.client.poll(None, RUNNER_POLL_SECS).await {
                Ok(response) => {
                    // The simulated workflow has long finished and runs no
                    // engine: every operator command gets the verdict a
                    // real runner gives one it cannot act on.
                    for command in response.commands {
                        let _ = self
                            .client
                            .ack_command(
                                &command.command_id,
                                false,
                                Some("the simulated workflow has finished".to_string()),
                            )
                            .await;
                    }
                    response.directive
                }
                Err(ClientError::Unauthorized) => return 1,
                Err(_) => {
                    tokio::time::sleep(RUNNER_RETRY).await;
                    RunnerDirective::Continue
                }
            };
        }
    }

    /// Posts a snapshot of the (instantly completed) simulated workflow.
    async fn report(&mut self, safe_point: bool) -> Result<RunnerDirective, ClientError> {
        self.sequence += 1;
        let event_cursor = self.advance_cursor();
        let snapshot = RunnerSnapshot {
            snapshot_sequence: self.sequence,
            execution_status: ExecutionStatus::Completed,
            execution_cursor: Default::default(),
            event_cursor,
            pending_input: None,
            conversation: Default::default(),
            timeline: None,
            safe_point,
            problem: None,
            attention: Vec::new(),
            conductor_available: false,
            conductor_unavailable_reason: None,
            default_harness_unavailable: None,
            display: None,
            observed_at: Utc::now(),
        };
        self.client
            .snapshot(&snapshot)
            .await
            .map(|response| response.directive)
    }

    /// One more simulated event on the storage; the cursor never goes back,
    /// not even across incarnations.
    fn advance_cursor(&self) -> u64 {
        let Some(platform) = self.platform.upgrade() else {
            return 0;
        };
        let mut platform = platform.lock().unwrap_or_else(PoisonError::into_inner);
        platform
            .storages
            .get_mut(&self.storage)
            .map_or(0, |storage| {
                storage.event_cursor += 1;
                storage.event_cursor
            })
    }

    fn update(&self, change: impl FnOnce(&mut WorkloadState)) {
        let Some(platform) = self.platform.upgrade() else {
            return;
        };
        let mut platform = platform.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(workload) = platform.workloads.get_mut(&self.workload) {
            // A replacement under the same name is not this runner's.
            if workload.state.uid == self.uid {
                change(&mut workload.state);
                if !workload.state.phase.is_alive() {
                    workload.runner = None;
                }
            }
        }
    }

    fn exit(&self, code: i32) {
        self.update(|state| {
            state.phase = WorkloadPhase::Exited { code: Some(code) };
            state.resource_generation += 1;
        });
    }
}
