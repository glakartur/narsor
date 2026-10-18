//! The read-only runner's side of the control plane (D63, R8-R12, R15).
//!
//! An owner reading a stopped task does not read its storage and does not
//! start anything: the read leases a reader on the task's record, the
//! controller — told so by the desired state — starts a read-only runner,
//! and the runner answers over the same relay a live runner does. This
//! module holds what the control plane does about it: leasing, waiting a
//! moment for the reader to connect, ending leases that ran idle, and
//! issuing the reader's credential while the lease holds.

use std::time::Duration as StdDuration;

use super::slot_release::chrono_duration;
use super::ProtocolService;
use crate::controller_protocol::credentials::{ControllerIdentity, RunnerRole};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::{ReaderLease, TaskRecord};

/// How often a read waiting for its reader looks whether it connected.
const CONNECT_CHECK_EVERY: StdDuration = StdDuration::from_millis(100);

/// Where a read of a stopped task stands.
pub(super) enum ReaderReady {
    /// A reader of the current lease is connected: ask it.
    Ready,
    /// The lease is set, the reader is not connected yet: ask again later.
    Starting,
    /// The task is no longer one a reader may read (it resumed, or was
    /// archived, meanwhile).
    NotReadable,
}

/// What one pass over the reader leases did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderLeasePass {
    /// Tasks whose lease this pass ended: the reader ran idle.
    pub ended: Vec<TaskId>,
}

impl ProtocolService {
    /// Leases a reader for the stopped task `record` (or moves the lease
    /// later) and waits a moment for it to connect.
    ///
    /// The wait is [`super::ProtocolConfig::reader_connect_wait`]: a reader
    /// that is already running answers at once, one that is only starting is
    /// not waited for — the owner is told so ([`ReaderReady::Starting`]) and
    /// asks again. The control plane leases and waits; it starts nothing
    /// itself.
    pub(super) async fn reader_ready(&self, record: &TaskRecord) -> ProtocolResult<ReaderReady> {
        let idle = self
            .slot_release_timings(&record.assignment.backend_id)
            .reader_idle;
        let Some((lease, written)) = self
            .control
            .lease_reader(&record.task_id, chrono_duration(idle))?
        else {
            return Ok(ReaderReady::NotReadable);
        };
        if written {
            // Wakes a controller waiting on its backend's commands.
            self.notify(&record.task_id);
        }
        let deadline = tokio::time::Instant::now() + self.config.reader_connect_wait;
        loop {
            if self.reader_connected(&record.task_id, &lease)? {
                return Ok(ReaderReady::Ready);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(ReaderReady::Starting);
            }
            tokio::time::sleep(remaining.min(CONNECT_CHECK_EVERY)).await;
        }
    }

    /// Whether a reader of `lease` has been heard from lately: its
    /// credential was issued after the lease began (an earlier reader's
    /// belongs to an earlier reading), still holds, and it polled within the
    /// staleness window.
    fn reader_connected(&self, task_id: &TaskId, lease: &ReaderLease) -> ProtocolResult<bool> {
        let record = self.control.task(task_id)?;
        let now = self.control.now();
        if !record.reader_lease_active(now) {
            return Ok(false);
        }
        let Some(session) = self.credentials.reader_session(task_id)? else {
            return Ok(false);
        };
        Ok(session.issued_at >= lease.since
            && session.launch_generation == record.generation
            && now < session.expires_at
            && session
                .last_contact_at
                .is_some_and(|contact| now - contact <= self.config.runner_stale_after))
    }

    /// Ends every reader lease that ran out (or whose task left the states
    /// a reader reads in), so the desired state stops asking for a reader
    /// and clients stop seeing a lease that holds nothing. Failures are
    /// logged; the next pass retries.
    pub fn reader_lease_pass(&self) -> ReaderLeasePass {
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "reader leases: cannot list tasks");
                return ReaderLeasePass::default();
            }
        };
        let now = self.control.now();
        let mut pass = ReaderLeasePass::default();
        for task in tasks.iter().filter(|task| task.reader_lease_stale(now)) {
            match self.control.end_reader_lease(&task.task_id) {
                Ok(true) => {
                    self.notify(&task.task_id);
                    pass.ended.push(task.task_id.clone());
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(task = %task.task_id, %error, "reader leases: cannot end a lease");
                }
            }
        }
        pass
    }

    /// Issues the credential of a read-only runner the controller started
    /// for a stopped task (D63).
    ///
    /// The same fences as a working runner's — the lease holder only, for
    /// the task's current generation — and the task must be one a reader
    /// may read with an active lease: a reader nobody waits for is not
    /// started. The credential is a session of its own: it replaces an
    /// earlier reader's and leaves the working runner's alone, and the
    /// task's record does not change.
    pub fn issue_reader_credential(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
        request: RunnerCredentialRequest,
    ) -> ProtocolResult<RunnerCredentialResponse> {
        let record = self.backend_task(identity, task_id)?;
        let lease = self.require_lease(identity)?;
        if lease.epoch != request.lease_epoch {
            return Err(ProtocolError::conflict(
                "stale_lease",
                format!(
                    "lease epoch {} is not the current epoch {}",
                    request.lease_epoch, lease.epoch
                ),
            ));
        }
        if request.generation != record.generation {
            return Err(ProtocolError::conflict(
                "stale_generation",
                format!(
                    "task '{}' is at generation {}, not {}",
                    record.task_id, record.generation, request.generation
                ),
            ));
        }
        if !record.reader_lease_active(self.control.now()) {
            return Err(ProtocolError::conflict(
                "reader_not_wanted",
                format!(
                    "task '{}' ({:?}) has no reader lease: nobody is reading it",
                    record.task_id, record.runner_state
                ),
            ));
        }
        let incarnation = self.checked_incarnation(&request.incarnation)?;
        let (session, credential) = self.credentials.issue_runner_session(
            RunnerRole::Reader,
            &record.task_id,
            incarnation,
            record.generation,
            lease.epoch,
            self.config.runner_credential_ttl,
        )?;
        Ok(RunnerCredentialResponse {
            task_id: session.task_id,
            generation: session.launch_generation,
            incarnation: session.incarnation,
            credential: credential.expose().to_string(),
            expires_at: session.expires_at,
        })
    }
}
