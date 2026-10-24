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
//! The simulated workflow runs no step. It stands in the initial state of
//! the launch spec's workflow, `Running`, for as long as nobody moves it: an
//! owner's `Transition` to another state of the workflow takes it there, and
//! a terminal state completes it. So a task runs until a test decides
//! otherwise, and the owner's offer treats it as any running task.
//! [`run_simulated_runner`] and [`run_simulated_reader`] are public so a
//! stub runner *process* (the contract tests of the process-based adapters)
//! speaks the protocol with the same code.
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
    ArchiveProgress, BackendError, BackendResult, ExecutionBackend, Provisioned, ReaderStart,
    Removal, StorageState, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase,
    WorkloadState,
};
use crate::api::ClientError;
use crate::controller_protocol::{RunnerClient, RunnerDirective, RunnerPollQuery};
use crate::domain::{ExecutionStatus, TaskId, TaskLaunchSpec, WorkflowDefinition};
use crate::environment::{BackendCapabilities, BackendKind};
use crate::orchestration::{OperatorCommand, RunnerSnapshot};

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
    /// Read-only runners (D63), by their own names.
    readers: HashMap<String, FakeWorkload>,
    next_uid: u64,
}

#[derive(Debug, Default)]
struct FakeStorage {
    /// The simulated runner's progress, kept with the work it describes.
    event_cursor: u64,
    /// The workflow state the simulated work entered last.
    workflow_state: Option<String>,
}

#[derive(Debug)]
struct FakeWorkload {
    state: WorkloadState,
    runner: Option<JoinHandle<()>>,
}

impl Drop for Platform {
    fn drop(&mut self) {
        for workload in self.workloads.values_mut().chain(self.readers.values_mut()) {
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

    /// The platform loses the task's reader unexpectedly. Returns whether
    /// there was a live one to lose.
    pub fn kill_reader(&self, task: &TaskResources) -> bool {
        let mut platform = self.lock();
        let Some(reader) = platform.readers.get_mut(&task.reader) else {
            return false;
        };
        if let Some(runner) = reader.runner.take() {
            runner.abort();
        }
        if !reader.state.phase.is_alive() {
            return false;
        }
        reader.state.phase = WorkloadPhase::Exited { code: None };
        reader.state.resource_generation += 1;
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
            reader: format!("narsor-task-{key}-reader"),
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

    async fn observe_reader(&self, task: &TaskResources) -> BackendResult<Option<WorkloadState>> {
        Ok(self
            .lock()
            .readers
            .get(&task.reader)
            .map(|reader| reader.state.clone()))
    }

    async fn start_reader(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<ReaderStart> {
        let mut platform = self.lock();
        if !platform.storages.contains_key(&task.storage) {
            return Err(BackendError::storage_missing(format!(
                "task storage '{}' does not exist",
                task.storage
            )));
        }
        if let Some(existing) = platform.readers.get(&task.reader) {
            return Ok(ReaderStart {
                workload: existing.state.clone(),
                created: false,
            });
        }
        platform.next_uid += 1;
        let state = WorkloadState {
            uid: format!("fake-{}-{}", task.reader, platform.next_uid),
            resource_generation: 1,
            incarnation: launch.incarnation.clone(),
            launch_generation: launch.generation,
            phase: WorkloadPhase::Starting,
        };
        let runner = self.simulate_runners.then(|| {
            tokio::spawn(simulated_reader(
                Arc::downgrade(&self.platform),
                task.reader.clone(),
                state.uid.clone(),
                launch.clone(),
            ))
        });
        platform.readers.insert(
            task.reader.clone(),
            FakeWorkload {
                state: state.clone(),
                runner,
            },
        );
        Ok(ReaderStart {
            workload: state,
            created: true,
        })
    }

    async fn stop_reader(&self, task: &TaskResources) -> BackendResult<Removal> {
        Ok(match self.lock().readers.remove(&task.reader) {
            Some(mut reader) => {
                if let Some(runner) = reader.runner.take() {
                    runner.abort();
                }
                Removal::Removed
            }
            None => Removal::AlreadyAbsent,
        })
    }
}

/// A read-only runner of the fake platform (see [`run_simulated_reader`]).
async fn simulated_reader(
    platform: Weak<Mutex<Platform>>,
    name: String,
    uid: String,
    launch: WorkloadLaunch,
) {
    let client = RunnerClient::new(&launch.control_plane_url, launch.credential.expose());
    let code = run_simulated_reader(&client, || {
        update_reader(&platform, &name, &uid, |state| {
            if state.phase == WorkloadPhase::Starting {
                state.phase = WorkloadPhase::Running;
                state.resource_generation += 1;
            }
        });
    })
    .await;
    update_reader(&platform, &name, &uid, |state| {
        state.phase = WorkloadPhase::Exited { code: Some(code) };
        state.resource_generation += 1;
    });
}

fn update_reader(
    platform: &Weak<Mutex<Platform>>,
    name: &str,
    uid: &str,
    change: impl FnOnce(&mut WorkloadState),
) {
    let Some(platform) = platform.upgrade() else {
        return;
    };
    let mut platform = platform.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(reader) = platform.readers.get_mut(name) {
        // A replacement under the same name is not this reader's.
        if reader.state.uid == uid {
            change(&mut reader.state);
            if !reader.state.phase.is_alive() {
                reader.runner = None;
            }
        }
    }
}

/// A working runner of the fake platform (see [`run_simulated_runner`]).
async fn simulated_runner(
    platform: Weak<Mutex<Platform>>,
    workload: String,
    uid: String,
    storage: String,
    launch: WorkloadLaunch,
) {
    let client = RunnerClient::new(&launch.control_plane_url, launch.credential.expose());
    let workload = FakeWorkloadHandle {
        platform,
        workload,
        uid,
        storage,
    };
    let code = run_simulated_runner(&client, &launch.launch_spec, &workload, || {
        workload.update(|state| {
            if state.phase == WorkloadPhase::Starting {
                state.phase = WorkloadPhase::Running;
                state.resource_generation += 1;
            }
        });
    })
    .await;
    workload.update(|state| {
        state.phase = WorkloadPhase::Exited { code: Some(code) };
        state.resource_generation += 1;
    });
}

/// One workload of the fake platform and the storage it works on, as its
/// simulated runner sees them.
struct FakeWorkloadHandle {
    platform: Weak<Mutex<Platform>>,
    workload: String,
    uid: String,
    storage: String,
}

impl FakeWorkloadHandle {
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

    fn with_storage<R>(&self, read: impl FnOnce(&mut FakeStorage) -> R) -> Option<R> {
        let platform = self.platform.upgrade()?;
        let mut platform = platform.lock().unwrap_or_else(PoisonError::into_inner);
        platform.storages.get_mut(&self.storage).map(read)
    }
}

impl SimulatedStorage for FakeWorkloadHandle {
    fn next_event(&self) -> u64 {
        self.with_storage(|storage| {
            storage.event_cursor += 1;
            storage.event_cursor
        })
        .unwrap_or(0)
    }

    fn workflow_state(&self) -> Option<String> {
        self.with_storage(|storage| storage.workflow_state.clone())
            .flatten()
    }

    fn enter_state(&self, state: &str) {
        self.with_storage(|storage| storage.workflow_state = Some(state.to_string()));
    }
}

// ---------------------------------------------------------------------------
// The simulated runner's side of the protocol
// ---------------------------------------------------------------------------

/// Where a simulated runner keeps its work: on the task storage, so a
/// resumed incarnation continues where the last one stood.
pub trait SimulatedStorage: Send + Sync {
    /// Counts one more simulated event; the new event cursor, which never
    /// goes back, not even across incarnations.
    fn next_event(&self) -> u64;

    /// The workflow state the work entered last, if it entered one.
    fn workflow_state(&self) -> Option<String>;

    /// Records that the work entered `state`.
    fn enter_state(&self, state: &str);
}

/// A read-only runner that answers nothing (no simulated runner has a
/// storage's worth of conversation) but keeps the promises of the reader
/// contract: it polls for reads only — which is what tells the control plane
/// it is connected — and leaves on any directive but `continue` or when its
/// credential is refused. `up` is called once, at its first answered poll.
/// Returns the exit code.
pub async fn run_simulated_reader(client: &RunnerClient, up: impl FnOnce()) -> i32 {
    let query = RunnerPollQuery {
        wait_secs: Some(RUNNER_POLL_SECS),
        defer_materials: true,
        ..RunnerPollQuery::default()
    };
    let mut up = Some(up);
    loop {
        match client.poll_with(&query).await {
            Ok(response) => {
                if let Some(up) = up.take() {
                    up();
                }
                if response.directive != RunnerDirective::Continue {
                    return 0;
                }
            }
            Err(ClientError::Unauthorized) => return 1,
            Err(_) => tokio::time::sleep(RUNNER_RETRY).await,
        }
    }
}

/// A runner that does no work but keeps every promise of the runner
/// protocol (see the module docs). `launch_spec` is the launch spec as the
/// workload got it; `up` is called once, when the control plane accepted a
/// first snapshot. Returns the exit code.
pub async fn run_simulated_runner(
    client: &RunnerClient,
    launch_spec: &serde_json::Value,
    storage: &dyn SimulatedStorage,
    up: impl FnOnce(),
) -> i32 {
    let definition = serde_json::from_value::<TaskLaunchSpec>(launch_spec.clone())
        .ok()
        .and_then(|spec| spec.definition().ok());
    let mut runner = SimulatedRunner {
        client,
        storage,
        state: storage
            .workflow_state()
            .or_else(|| definition.as_ref().map(|d| d.initial_state.clone())),
        definition,
        sequence: 0,
    };
    runner.run(up).await
}

struct SimulatedRunner<'a> {
    client: &'a RunnerClient,
    storage: &'a dyn SimulatedStorage,
    definition: Option<WorkflowDefinition>,
    /// The workflow state the work stands in.
    state: Option<String>,
    sequence: u64,
}

impl SimulatedRunner<'_> {
    /// Runs until told to stop; returns the exit code.
    async fn run(&mut self, up: impl FnOnce()) -> i32 {
        // "Up" means: the control plane accepted a first snapshot.
        let mut directive = loop {
            match self.report(false).await {
                Ok(directive) => break directive,
                Err(ClientError::Unauthorized) => return 1,
                Err(_) => tokio::time::sleep(RUNNER_RETRY).await,
            }
        };
        up();
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
                    let mut moved = false;
                    for command in response.commands {
                        let verdict = self.carry_out(&command.command);
                        moved |= verdict.is_ok();
                        let _ = self
                            .client
                            .ack_command(&command.command_id, verdict.is_ok(), verdict.err())
                            .await;
                    }
                    if moved {
                        match self.report(false).await {
                            Ok(directive) => directive,
                            Err(ClientError::Unauthorized) => return 1,
                            Err(_) => response.directive,
                        }
                    } else {
                        response.directive
                    }
                }
                Err(ClientError::Unauthorized) => return 1,
                Err(_) => {
                    tokio::time::sleep(RUNNER_RETRY).await;
                    RunnerDirective::Continue
                }
            };
        }
    }

    /// Carries out an operator command, or says why not. The simulated
    /// workflow can only be moved: it has no step to interrupt or restart.
    fn carry_out(&mut self, command: &OperatorCommand) -> Result<(), String> {
        let OperatorCommand::Transition { target, .. } = command else {
            return Err("the simulated runner runs no step".to_string());
        };
        let known = self
            .definition
            .as_ref()
            .is_some_and(|definition| definition.states.contains_key(target));
        if !known {
            return Err(format!("the workflow has no state '{target}'"));
        }
        self.storage.enter_state(target);
        self.state = Some(target.clone());
        Ok(())
    }

    /// The simulated workflow's status: completed in a terminal state,
    /// running anywhere else.
    fn status(&self) -> ExecutionStatus {
        let terminal = self
            .definition
            .as_ref()
            .zip(self.state.as_ref())
            .and_then(|(definition, state)| definition.states.get(state))
            .is_some_and(|state| state.terminal);
        if terminal {
            ExecutionStatus::Completed
        } else {
            ExecutionStatus::Running
        }
    }

    /// Posts a snapshot of the simulated workflow.
    async fn report(&mut self, safe_point: bool) -> Result<RunnerDirective, ClientError> {
        self.sequence += 1;
        let mut snapshot = RunnerSnapshot {
            snapshot_sequence: self.sequence,
            execution_status: self.status(),
            execution_cursor: Default::default(),
            event_cursor: self.storage.next_event(),
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
            generated_title: None,
            observed_at: Utc::now(),
        };
        snapshot.execution_cursor.current_state = self.state.clone();
        self.client
            .snapshot(&snapshot)
            .await
            .map(|response| response.directive)
    }
}
