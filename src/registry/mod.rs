//! Registry of the repositories a workflow may name by id.
//!
//! `WORKFLOW_SCHEMA.md` §3.2 describes `globals.workspace.repository` as "the
//! id of a repository registered in the system". This module is that system: a
//! small, file-backed catalogue mapping a stable id to a git remote, so a
//! definition can say `repository: acme-api` instead of embedding a URL that
//! changes when the hosting does.
//!
//! ```text
//! {catalog}/repositories/{id}.yaml   -- one file per repository
//! ```
//!
//! The file name is the identifier. A directory rather than one document
//! because entries do not depend on each other: uniqueness becomes `EEXIST`,
//! isolation becomes two paths, and a corrupt file costs one repository
//! instead of the catalogue. YAML rather than JSON because the catalogue is
//! meant to be written by a person and by a deployment, beside the workflow
//! definitions it sits next to.
//!
//! # Resolution precedence
//!
//! When a workflow runs, the expanded `repository` value is looked up here
//! **first**. A hit provisions from the entry's `remote` (and its
//! `default_branch` when the workflow declares no `ref`); a miss falls back to
//! treating the value as a raw git remote — a URL or a local path — which is
//! what the CLI did before a registry existed. Nothing that worked before stops
//! working, and a registered id simply wins over that fallback.
//!
//! # Why the provisioner's own validation
//!
//! An entry is validated with the very rules the workspace provisioner applies,
//! not with a copy of them: a remote starting with `-` is read by `git` as an
//! option and the `ext::` transport makes `git` execute an arbitrary command,
//! so both are refused when the entry is registered rather than when it is
//! used. Duplicated validation drifts, and drift here is a command-execution
//! hole.

pub mod store;

pub use store::{
    infer_provider, RegistryError, RegistryFailure, RegistryListing, RepositoryEntry,
    RepositoryRegistry, Result, REPOSITORIES_DIR,
};
