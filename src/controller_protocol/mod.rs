//! The versioned control-plane protocol for owners, environment controllers
//! and task runners (U2: KTD1, KTD3, KTD4).
//!
//! The control plane ([`crate::orchestration::ControlPlane`]) is the only
//! writer of orchestration state. Nobody else touches its directory:
//! controllers and runners talk to it over this protocol, outbound from
//! their own environment, and clients read normalized views of it.
//!
//! # Controller contract
//!
//! 1. **Enroll** once with an operator-issued one-time code and a key pair
//!    the controller generated (`POST /controller/v1/enroll`); the resulting
//!    identity is scoped to one backend and revocable (R37). Every later
//!    request is signed with the private key — fresh, single-use and bound
//!    to its method, path and body ([`crate::auth::pop`]); the key can be
//!    rotated (`POST /controller/v1/key`).
//! 2. **Lease**: acquire the backend's lease (`POST /controller/v1/lease`)
//!    and renew it well before expiry. Every report carries the lease epoch;
//!    a newer acquisition, a release or a revocation fences older epochs.
//!    Heartbeat (`POST /controller/v1/heartbeat`) so clients can tell a
//!    quiet controller from a gone one.
//! 3. **Fetch** desired commands (`GET /controller/v1/commands`, long-poll
//!    with `wait_secs`). After a (re)start ask once with `all=true` to
//!    reconcile every live task; afterwards only commands newer than this
//!    identity's acknowledged generation are returned.
//! 4. **Acknowledge** (`POST /controller/v1/acks`) the generation it has
//!    taken over. Acknowledgements only ever raise; repeats are harmless.
//! 5. **Reconcile and report** (`POST /controller/v1/reports`) resource UID,
//!    resource generation, desired generation and lease epoch. Rules a
//!    controller must keep:
//!    * **report `StartAccepted` and see it accepted *before* creating any
//!      resource.** Archive racing a start completes at once on a task no
//!      controller accepted; a resource created before that report would
//!      leak, and a refused report means "do not create";
//!    * name every resource after the task's `resource_key`, so a retried
//!      create finds the existing resource instead of duplicating it (R7);
//!    * never recreate a lost workload: report `WorkloadLost` and wait for
//!      the owner (R13);
//!    * remove compute for a pause only after the runner acknowledged a
//!      safe point for the current generation
//!      (`runner.safe_point_acknowledged`); the control plane refuses
//!      `PausedAtSafePoint` otherwise (KTD2);
//!    * issue each new runner incarnation its own credential
//!      (`POST /controller/v1/tasks/:id/runner-credentials`) and mount it
//!      read-only; never use exec or port-forward into the workload.
//!
//! # Runner contract
//!
//! A runner holds a short-lived credential bound to its task, its launch
//! generation and its incarnation. A newer incarnation's credential, a
//! newer start/resume or archival makes it stop working. The runner posts
//! snapshots (`POST /runner/v1/snapshots`: cursors and summaries only),
//! long-polls (`GET /runner/v1/poll`) for its directive and for owner
//! answers to exactly the input request it waits on, acknowledges each
//! answer, and renews its credential. Losing the control plane never stops
//! it (R14): it keeps working and reports a later cursor when it can.
//!
//! # Owner contract
//!
//! Clients see [`wire::TaskSummary`]s, issue idempotent owner commands,
//! submit input for the runner's current request, and follow a durable
//! event stream that replays from a cursor before going live. Raw workspace
//! files, secret values and raw transcripts are never part of any of it.
//!
//! * [`wire`] — every request and response;
//! * [`credentials`] — identities, enrollment codes, runner secrets,
//!   controller keys and the pluggable checks;
//! * [`service`] — the server-side handling, independent of HTTP;
//! * [`http`] — routers, credential gates and handlers;
//! * [`client`] — typed controller and runner clients.

pub mod client;
pub mod credentials;
pub mod error;
pub mod http;
pub mod service;
pub mod wire;

pub use client::{ControllerClient, RunnerClient};
pub use credentials::{
    ControllerAuthenticator, ControllerIdentity, CredentialStore, RunnerAuthenticator,
    RunnerIdentity,
};
pub use error::{ProtocolError, ProtocolResult};
pub use service::{ProtocolConfig, ProtocolService, LOCAL_OWNER};
pub use wire::*;
