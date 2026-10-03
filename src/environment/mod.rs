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
//! | owner default backend (R10) | `PUT /control/v1/default-backend`, else setting `default_backend` |
//! | owner default profile (R16) | setting `default_profile` |
//! | backends (R3) | `{catalog}/backends/{id}.yaml` — [`BackendRecord`] |
//! | profiles (R16) | `{catalog}/profiles/{id}.yaml` — [`ProfileRecord`] |
//! | runtime bundle, tools (R20) | `{catalog}/runtime.yaml` — [`RuntimeCatalog`] |
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
//! * [`resolver`] — request + configuration → backend + launch spec.

pub mod backend;
pub mod catalog;
pub mod image;
pub mod profile;
pub mod resolver;

pub use backend::{
    BackendCapabilities, BackendKind, BackendLimits, BackendRecord, BackendSecret,
    SecretDeliveryMode, TrustScope, MAX_CONCURRENT_TASKS_LIMIT,
};
pub use catalog::{CatalogError, EnvironmentCatalog};
pub use image::{ImageResolver, NoTagResolution, StaticImageResolver};
pub use profile::{ProfileRecord, RuntimeCatalog, DEFAULT_PROFILE};
pub use resolver::{
    CatalogLaunchResolver, HarnessDefaults, LaunchError, LaunchIntent, LaunchResolver,
    OwnerDefaults, ResolvedLaunch, StaticHarnessDefaults,
};
