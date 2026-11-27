//! Backends, profiles and the runtime catalogue, resolved into the pinned
//! environment of each new task (U6: R3, R10, R15-R21, R37, R39-R42; KTD5,
//! KTD6).
//!
//! The *configuration* side lives here and only the control plane reads it;
//! what it resolves to — [`ResolvedEnvironment`](crate::domain::ResolvedEnvironment)
//! inside a [`TaskLaunchSpec`](crate::domain::TaskLaunchSpec) — lives in the
//! domain, because the runner and the controller read it too.
//!
//! # Where things are configured
//!
//! | What | Where |
//! | --- | --- |
//! | owner default backend (R10) | `PUT /control/v1/default-backend`, else setting `server.defaults.backend` |
//! | owner default profile (R16) | setting `server.defaults.profile` |
//! | backends (R3) | `{catalog}/backends/{id}.yaml` — [`BackendRecord`] |
//! | profiles (R16) | `{catalog}/profiles/{id}.yaml` — [`ProfileRecord`] |
//! | runtime bundle, tools, task image (R20) | `{catalog}/runtime/runtime.yaml` (older catalogues: `{catalog}/runtime.yaml`) — [`RuntimeCatalog`]; its scripts beside it in `{catalog}/runtime/components/` |
//! | repository base image (R15) | `{catalog}/repositories/{id}.yaml` `base_image` |
//! | workflow / step image (R15) | workflow `globals.image`, state `image` |
//!
//! Secrets are configured as references on the backend that holds them
//! (R19); profiles and repositories name them; the control plane never sees
//! a value.
//!
//! * [`backend`] — backend records, limits, secret allow lists;
//! * [`profile`] — profiles and the runtime catalogue;
//! * [`catalog`] — reading and writing those files;
//! * [`image`] — the pluggable tag-to-digest lookup;
//! * [`resolver`] — the launch resolver contract: request + configuration →
//!   backend + launch spec. The implementation is `crate::launch`.

pub mod backend;
pub mod catalog;
pub mod image;
pub mod profile;
pub mod resolver;

pub use backend::{
    BackendCapabilities, BackendKind, BackendLimits, BackendRecord, BackendSecret,
    SecretDeliveryMode, SlotReleaseTimings, TrustScope,
};
pub use catalog::{CatalogError, EnvironmentCatalog, LocatedRuntime};
pub use image::{ImageResolver, NoTagResolution, StaticImageResolver};
pub use profile::{ProfileRecord, RuntimeCatalog, DEFAULT_PROFILE};
pub use resolver::{
    LaunchError, LaunchIntent, LaunchResolver, ResolvedLaunch, TaskDefaultHarness, WorkspaceRequest,
};
