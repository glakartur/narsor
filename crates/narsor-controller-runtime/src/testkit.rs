//! Instruments that turn *any* adapter into a contract-test subject (U4).
//!
//! * [`Probed`] wraps an [`ExecutionBackend`], counts what it really did
//!   (creations, deletions — the "exactly once" of archive and of reconnect)
//!   and injects platform faults: failed or lost provisioning, failed
//!   observations, slow deletions. Faults are injected *in front of* the
//!   adapter, so the same scenario means the same thing for the fake, the
//!   local and the Kubernetes adapter.
//! * [`SeverableLink`] wraps a [`ControllerLink`] and can cut the
//!   controller off the control plane and reconnect it, without touching
//!   the backend or the runners.
//!
//! Production code never uses either; they live in the library so that
//! every adapter's integration tests (`tests/backend_contract.rs`, U5's
//! `tests/kubernetes_adapter.rs`, which uses the adapter owned by
//! `narsor-controller`) share one implementation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::backend::{
    ArchiveProgress, BackendError, BackendErrorKind, BackendResult, ExecutionBackend, Provisioned,
    Removal, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase,
};
use crate::backend::{BackendCapabilities, BackendKind};
use crate::controller::ControllerLink;
use narsor_protocol::controller::{
    AckResponse, CommandAck, CommandsResponse, HeartbeatResponse, LeaseView, ReportRequest,
    ReportResponse, RunnerCredentialRequest, RunnerCredentialResponse,
};
use narsor_protocol::identifier::TaskId;
use narsor_shared::api_error::ClientError;

/// What a [`Probed`] adapter really did, per kind of effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeCounters {
    /// Workloads created (a returned existing one is not counted).
    pub workloads_created: u64,
    /// Task storages created.
    pub storages_created: u64,
    /// Workload deletions issued (each removal of a present workload once).
    pub workload_deletions: u64,
    /// Storage deletions issued.
    pub storage_deletions: u64,
    /// Launches (provision or resume) attempted while a workload of the
    /// task still existed. A correct controller never does that: it is how
    /// a second workload would come to be (R7).
    pub launches_over_existing: u64,
    /// `provision` calls that reached the adapter.
    pub provision_calls: u64,
    /// `resume` calls that reached the adapter.
    pub resume_calls: u64,
    /// `observe` calls, including failed ones.
    pub observe_calls: u64,
}

/// Faults to inject, consumed as calls arrive.
#[derive(Debug, Default)]
struct Faults {
    /// Launches to fail before reaching the adapter, with how.
    failing_launches: Vec<BackendErrorKind>,
    /// Launches that reach the adapter but whose answer is lost (a timeout
    /// after the create happened).
    lost_launch_answers: u32,
    /// Observations to fail.
    failing_observes: u32,
    /// Extra removal calls every deletion takes before it really happens.
    deletion_delay: u32,
}

#[derive(Debug, Default)]
struct ProbeState {
    counters: ProbeCounters,
    faults: Faults,
    /// Deletions under way: resource name → removal calls still to wait.
    pending_deletions: HashMap<String, u32>,
    /// Resources whose deletion this probe already counted.
    deleting: HashSet<String>,
}

/// An adapter with counters and fault injection in front of it.
///
/// Clones share counters and faults; [`Self::sharing`] puts another
/// adapter instance (a second controller's view of the same platform)
/// behind the same probe.
#[derive(Debug, Clone)]
pub struct Probed<B> {
    inner: B,
    state: Arc<Mutex<ProbeState>>,
}

impl<B: ExecutionBackend> Probed<B> {
    /// Wraps `inner` with zeroed counters and no faults.
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(ProbeState::default())),
        }
    }

    /// Wraps `inner` behind this probe's counters and faults.
    pub fn sharing<C: ExecutionBackend>(&self, inner: C) -> Probed<C> {
        Probed {
            inner,
            state: Arc::clone(&self.state),
        }
    }

    /// The wrapped adapter.
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// The counters so far.
    pub fn counters(&self) -> ProbeCounters {
        self.lock().counters.clone()
    }

    /// The next launches fail with these kinds, in order, without reaching
    /// the adapter.
    pub fn fail_launches(&self, kinds: impl IntoIterator<Item = BackendErrorKind>) {
        self.lock().faults.failing_launches.extend(kinds);
    }

    /// The next `count` launches reach the adapter, but their answers are
    /// lost on the way back (reported as transient failures).
    pub fn lose_launch_answers(&self, count: u32) {
        self.lock().faults.lost_launch_answers += count;
    }

    /// The next `count` observations fail transiently.
    pub fn fail_observes(&self, count: u32) {
        self.lock().faults.failing_observes += count;
    }

    /// Every deletion from now on takes `calls` extra removal calls, during
    /// which the resource is observed as being deleted.
    pub fn delay_deletions(&self, calls: u32) {
        self.lock().faults.deletion_delay = calls;
    }

    /// Removes every injected fault (deletions already under way finish on
    /// their own schedule).
    pub fn clear_faults(&self) {
        self.lock().faults = Faults::default();
    }

    fn lock(&self) -> MutexGuard<'_, ProbeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The delay gate of one removal call for `name` (the resource
    /// exists): `None` to go ahead now, or the answer of a deletion the
    /// probe is still holding back.
    fn delay(&self, name: &str) -> Option<Removal> {
        let mut state = self.lock();
        let delay = state.faults.deletion_delay;
        let already_deleting = state.deleting.contains(name);
        match state.pending_deletions.get_mut(name) {
            Some(0) => {
                state.pending_deletions.remove(name);
                None
            }
            Some(remaining) => {
                *remaining -= 1;
                Some(Removal::InProgress)
            }
            None if delay > 0 && !already_deleting => {
                state.pending_deletions.insert(name.to_string(), delay - 1);
                Some(Removal::Initiated)
            }
            None => None,
        }
    }

    /// Forgets a deletion whose resource is gone.
    fn forget(&self, name: &str) {
        let mut state = self.lock();
        state.pending_deletions.remove(name);
        state.deleting.remove(name);
    }

    /// Counts what a removal did, issuing each resource's deletion once.
    fn count_removal(&self, name: &str, removal: Removal, storage: bool) {
        let mut state = self.lock();
        if removal.issued() && state.deleting.insert(name.to_string()) {
            if storage {
                state.counters.storage_deletions += 1;
            } else {
                state.counters.workload_deletions += 1;
            }
        }
        if removal.is_complete() {
            state.deleting.remove(name);
        }
    }

    async fn launch(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        resume: bool,
    ) -> BackendResult<Provisioned> {
        {
            let mut state = self.lock();
            if resume {
                state.counters.resume_calls += 1;
            } else {
                state.counters.provision_calls += 1;
            }
            if !state.faults.failing_launches.is_empty() {
                let kind = state.faults.failing_launches.remove(0);
                return Err(BackendError {
                    kind,
                    message: "injected launch failure".into(),
                });
            }
        }
        if self
            .inner
            .observe(task)
            .await
            .is_ok_and(|o| o.workload.is_some())
        {
            self.lock().counters.launches_over_existing += 1;
        }
        let result = if resume {
            self.inner.resume(task, launch).await
        } else {
            self.inner.provision(task, launch).await
        };
        let mut state = self.lock();
        if let Ok(provisioned) = &result {
            if provisioned.created_workload {
                state.counters.workloads_created += 1;
            }
            if provisioned.created_storage {
                state.counters.storages_created += 1;
            }
        }
        if result.is_ok() && state.faults.lost_launch_answers > 0 {
            state.faults.lost_launch_answers -= 1;
            return Err(BackendError::transient("injected lost answer"));
        }
        result
    }
}

impl<B: ExecutionBackend> ExecutionBackend for Probed<B> {
    fn kind(&self) -> BackendKind {
        self.inner.kind()
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    fn resources(&self, task_id: &TaskId, resource_key: &str) -> BackendResult<TaskResources> {
        self.inner.resources(task_id, resource_key)
    }

    async fn observe(&self, task: &TaskResources) -> BackendResult<TaskObservation> {
        {
            let mut state = self.lock();
            state.counters.observe_calls += 1;
            if state.faults.failing_observes > 0 {
                state.faults.failing_observes -= 1;
                return Err(BackendError::transient("injected observation failure"));
            }
        }
        let mut observed = self.inner.observe(task).await?;
        let state = self.lock();
        // A deletion the probe is still delaying shows as one in progress.
        if state.pending_deletions.contains_key(&task.workload) {
            if let Some(workload) = &mut observed.workload {
                workload.phase = WorkloadPhase::Deleting;
            }
        }
        Ok(observed)
    }

    async fn provision(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.launch(task, launch, false).await
    }

    async fn resume(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.launch(task, launch, true).await
    }

    async fn release_compute(&self, task: &TaskResources) -> BackendResult<Removal> {
        if self.inner.observe(task).await?.workload.is_none() {
            self.forget(&task.workload);
        } else if let Some(answer) = self.delay(&task.workload) {
            self.count_removal(&task.workload, answer, false);
            return Ok(answer);
        }
        let removal = self.inner.release_compute(task).await?;
        self.count_removal(&task.workload, removal, false);
        Ok(removal)
    }

    async fn archive(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
        let observed = self.inner.observe(task).await?;
        if observed.workload.is_none() && !observed.storage.exists() {
            self.forget(&task.workload);
            self.forget(&task.storage);
        } else if let Some(answer) = self.delay(&task.workload) {
            // One gate for both resources: archive is one deletion.
            if observed.workload.is_some() {
                self.count_removal(&task.workload, answer, false);
            }
            if observed.storage.exists() {
                self.count_removal(&task.storage, answer, true);
            }
            return Ok(ArchiveProgress {
                workload: answer,
                storage: answer,
            });
        }
        let progress = self.inner.archive(task).await?;
        self.count_removal(&task.workload, progress.workload, false);
        self.count_removal(&task.storage, progress.storage, true);
        Ok(progress)
    }
}

/// A [`ControllerLink`] that can be cut off and reconnected.
///
/// While severed, every call fails as the control plane being unreachable
/// ([`ClientError::Unavailable`]) — the controller's view of a network
/// partition. Clones share the switch.
#[derive(Debug, Clone)]
pub struct SeverableLink<L> {
    inner: L,
    connected: Arc<AtomicBool>,
}

impl<L> SeverableLink<L> {
    /// A connected link over `inner`.
    pub fn new(inner: L) -> Self {
        Self {
            inner,
            connected: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Cuts the controller off.
    pub fn sever(&self) {
        self.connected.store(false, Ordering::SeqCst);
    }

    /// Reconnects it.
    pub fn restore(&self) {
        self.connected.store(true, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), ClientError> {
        if self.connected.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ClientError::Unavailable("link severed".into()))
        }
    }
}

impl<L: ControllerLink> ControllerLink for SeverableLink<L> {
    async fn acquire_lease(&self, ttl_secs: Option<u64>) -> Result<LeaseView, ClientError> {
        self.check()?;
        self.inner.acquire_lease(ttl_secs).await
    }

    async fn renew_lease(
        &self,
        epoch: u64,
        ttl_secs: Option<u64>,
    ) -> Result<LeaseView, ClientError> {
        self.check()?;
        self.inner.renew_lease(epoch, ttl_secs).await
    }

    async fn release_lease(&self, epoch: u64) -> Result<LeaseView, ClientError> {
        self.check()?;
        self.inner.release_lease(epoch).await
    }

    async fn heartbeat(&self) -> Result<HeartbeatResponse, ClientError> {
        self.check()?;
        self.inner.heartbeat().await
    }

    async fn commands(&self, wait_secs: u64, all: bool) -> Result<CommandsResponse, ClientError> {
        self.check()?;
        let response = self.inner.commands(wait_secs, all).await;
        // A long-poll that outlived the partition must not deliver.
        self.check()?;
        response
    }

    async fn acknowledge(&self, acks: Vec<CommandAck>) -> Result<AckResponse, ClientError> {
        self.check()?;
        self.inner.acknowledge(acks).await
    }

    async fn report(&self, report: &ReportRequest) -> Result<ReportResponse, ClientError> {
        self.check()?;
        self.inner.report(report).await
    }

    async fn issue_runner_credential(
        &self,
        task_id: &str,
        request: &RunnerCredentialRequest,
    ) -> Result<RunnerCredentialResponse, ClientError> {
        self.check()?;
        self.inner.issue_runner_credential(task_id, request).await
    }
}
