//! The protocol's server-side handling, independent of HTTP.
//!
//! [`ProtocolService`] is the one door owners, controllers and runners go
//! through to the [`ControlPlane`]: it scopes every request to the caller
//! (an owner sees only its tasks, a controller only its backend's, a runner
//! only its own incarnation's), turns records into the normalized wire
//! views, and announces every accepted mutation on an in-process channel —
//! which is what wakes long-polls and live event streams.

use std::sync::{Arc, Mutex};

use std::time::Duration as StdDuration;

use chrono::Duration;
use tokio::sync::broadcast;

mod enrollment;
mod lease;
mod operator;
mod reads;
mod runner_channel;
mod scheduling;
mod tasks;
#[cfg(test)]
mod tests;

use super::credentials::{ControllerAuthenticator, CredentialStore, RunnerAuthenticator};
use super::error::{ProtocolError, ProtocolResult};
use super::wire::*;
use crate::domain::TaskId;
use crate::environment::LaunchResolver;
use crate::materials::{MaterialLimits, MaterialStore};
use crate::orchestration::{ControlPlane, ResumePolicy};
use reads::Relay;

/// Converts a configured period; one too large for the calendar counts as
/// the largest one.
fn chrono_duration(period: StdDuration) -> Duration {
    Duration::from_std(period).unwrap_or(Duration::MAX)
}

pub use reads::ReaderLeasePass;
pub use scheduling::{AutoResumePass, SlotReleasePass};
pub use tasks::{task_actions, task_panel, task_workspaces};

/// The owner every request authenticated by the local daemon token acts
/// as. Trusted loopback mode has exactly one owner; network mode uses the
/// verified OIDC `(issuer, subject)` instead
/// ([`crate::auth::OidcIdentity::owner_ref`]).
pub const LOCAL_OWNER: &str = "local";

/// How many change notifications may queue for a slow listener before it
/// lags — a lagging listener rescans instead of losing anything.
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// Raw bytes a relayed conversation page holds when the owner asks for no
/// particular budget, and the most it may ask for. Smaller than the local
/// API's budget: every page crosses two network hops.
const DEFAULT_PAGE_BUDGET: u64 = 256 * 1024;
const MAX_PAGE_BUDGET: u64 = 1024 * 1024;

/// Events a relayed or stored read returns when the owner asks for no
/// particular number, and the most it may ask for.
const DEFAULT_EVENTS_LIMIT: u32 = 500;
const MAX_EVENTS_LIMIT: u32 = 2000;

/// Conversation page requests owners are waiting on (KTD4's relay).
type PageRelay = Relay<ConversationPageRequest, ConversationPage>;

/// Execution event requests owners are waiting on (U4), relayed the same way.
type EventsRelay = Relay<ExecutionEventsRequest, ExecutionEventsPage>;

/// Conductor-state requests owners are waiting on (U5), relayed the same way.
type ConductorRelay = Relay<ConductorReadRequest, crate::conductor::ConductorView>;

/// Timing policy of the protocol.
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
    /// Lease validity when the controller does not ask for one.
    pub default_lease_ttl: Duration,
    /// Longest lease validity granted.
    pub max_lease_ttl: Duration,
    /// Validity of a runner credential, from issue or renewal.
    pub runner_credential_ttl: Duration,
    /// How long after its expiry a runner credential may still be renewed
    /// (and only renewed): longer than a host sleeps or the control plane
    /// is down, after which the runner could never be heard again.
    pub runner_renewal_grace: Duration,
    /// Enrollment code validity when the operator does not ask for one.
    pub default_enrollment_ttl: Duration,
    /// Longest enrollment code validity granted.
    pub max_enrollment_ttl: Duration,
    /// A runner not heard from for longer is shown stale.
    pub runner_stale_after: Duration,
    /// A controller without a heartbeat for longer is shown stale.
    pub controller_stale_after: Duration,
    /// Longest long-poll a controller or runner may ask for.
    pub max_long_poll: std::time::Duration,
    /// How long an owner waits for the live runner to upload a requested
    /// conversation page or events page before the relay gives up.
    pub conversation_page_timeout: std::time::Duration,
    /// How workload losses are counted for the automatic resume: the
    /// attempts' delays (their number is the limit) and the stability
    /// window. Installed on the service's control plane.
    pub resume: ResumePolicy,
    /// Whether the control plane frees the slot of a task whose work ended
    /// or waits for its owner (D62). On everywhere but in tests that hold a
    /// finished task's runner alive on purpose to look at it.
    pub slot_release: bool,
    /// How long a read of a stopped task waits for its read-only runner to
    /// connect before it answers "starting" and leaves the retry to the
    /// client (D63).
    pub reader_connect_wait: std::time::Duration,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            default_lease_ttl: Duration::seconds(60),
            max_lease_ttl: Duration::minutes(10),
            runner_credential_ttl: Duration::minutes(15),
            runner_renewal_grace: Duration::hours(24),
            default_enrollment_ttl: Duration::minutes(15),
            max_enrollment_ttl: Duration::hours(24),
            runner_stale_after: Duration::seconds(90),
            controller_stale_after: Duration::seconds(90),
            max_long_poll: std::time::Duration::from_secs(30),
            conversation_page_timeout: std::time::Duration::from_secs(20),
            resume: ResumePolicy::default(),
            slot_release: true,
            reader_connect_wait: std::time::Duration::from_secs(2),
        }
    }
}

/// Server-side protocol handling over one [`ControlPlane`].
pub struct ProtocolService {
    control: ControlPlane,
    credentials: CredentialStore,
    controller_auth: Arc<dyn ControllerAuthenticator>,
    runner_auth: Arc<dyn RunnerAuthenticator>,
    config: ProtocolConfig,
    changes: broadcast::Sender<TaskId>,
    launch: Arc<dyn LaunchResolver>,
    pages: Mutex<PageRelay>,
    event_reads: Mutex<EventsRelay>,
    conductor_reads: Mutex<ConductorRelay>,
    materials: MaterialStore,
    harness_usages: scheduling::HarnessUsages,
    /// Most tasks processed at once across all backends; `None` for no limit.
    max_concurrent_tasks: Mutex<Option<u32>>,
}

impl std::fmt::Debug for ProtocolService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolService")
            .field("control", &self.control)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The limit of tasks processed at once when nothing sets one.
pub const DEFAULT_MAX_CONCURRENT_TASKS: u32 = 1;

impl ProtocolService {
    /// A service with the default policy and file-backed bearer credentials.
    ///
    /// `launch` turns every create request into a backend and a sealed
    /// launch spec (U6); there is no default, because a control plane that
    /// accepted unresolved launches would pin tasks to nothing.
    pub fn new(control: ControlPlane, launch: Arc<dyn LaunchResolver>) -> Self {
        Self::with_config(control, launch, ProtocolConfig::default())
    }

    /// A service with an explicit policy.
    ///
    /// The config's [`ProtocolConfig::resume`] replaces the control plane's
    /// own resume policy, so the service and every report it accepts count
    /// losses alike.
    pub fn with_config(
        control: ControlPlane,
        launch: Arc<dyn LaunchResolver>,
        config: ProtocolConfig,
    ) -> Self {
        let control = control.with_resume_policy(config.resume.clone());
        let credentials = CredentialStore::new(control.clone());
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let materials = MaterialStore::new(control.store().paths(), MaterialLimits::default());
        Self {
            materials,
            controller_auth: Arc::new(credentials.clone()),
            runner_auth: Arc::new(credentials.clone()),
            control,
            credentials,
            config,
            changes,
            launch,
            pages: Mutex::new(PageRelay::default()),
            event_reads: Mutex::new(EventsRelay::default()),
            conductor_reads: Mutex::new(ConductorRelay::default()),
            harness_usages: Mutex::default(),
            max_concurrent_tasks: Mutex::new(Some(DEFAULT_MAX_CONCURRENT_TASKS)),
        }
    }

    /// Returns the service with a limit of tasks processed at once across
    /// all backends, `None` for no limit.
    pub fn with_max_concurrent_tasks(self, limit: Option<u32>) -> Self {
        *self.lock_max_concurrent_tasks() = limit;
        self
    }

    /// Replaces how controller requests are verified (the default checks
    /// signed proof-of-possession, [`crate::auth::pop`]).
    pub fn with_controller_authenticator(mut self, auth: Arc<dyn ControllerAuthenticator>) -> Self {
        self.controller_auth = auth;
        self
    }

    /// Replaces the bounds of task materials (U9).
    pub fn with_material_limits(mut self, limits: MaterialLimits) -> Self {
        self.materials = MaterialStore::new(self.control.store().paths(), limits);
        self
    }

    /// Replaces how runner credentials are verified.
    pub fn with_runner_authenticator(mut self, auth: Arc<dyn RunnerAuthenticator>) -> Self {
        self.runner_auth = auth;
        self
    }

    /// The control plane.
    pub fn control_plane(&self) -> &ControlPlane {
        &self.control
    }

    /// The credential store.
    pub fn credentials(&self) -> &CredentialStore {
        &self.credentials
    }

    /// The policy in force.
    pub fn config(&self) -> &ProtocolConfig {
        &self.config
    }

    /// Change notifications: the id of every task whose record changed.
    ///
    /// A notification carries no event — listeners re-read the durable log
    /// from their own cursor, so a lagged or duplicated notification can
    /// never make them skip or repeat one.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskId> {
        self.changes.subscribe()
    }

    /// Announces that `task_id`'s record changed. Public so an in-process
    /// writer that mutates the [`ControlPlane`] directly (U4's local
    /// adapter) wakes streams and long-polls too.
    pub fn notify(&self, task_id: &TaskId) {
        // No receiver is not an error: nobody is listening right now.
        let _ = self.changes.send(task_id.clone());
    }
}

fn parse_identifier<T>(parsed: Result<T, crate::domain::InvalidIdentifier>) -> ProtocolResult<T> {
    parsed.map_err(|error| ProtocolError::bad_request("invalid_identifier", error.to_string()))
}
