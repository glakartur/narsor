//! What the server hands the local controller it starts (R21).
//!
//! The controller owns the `controller` section; the server does not parse
//! it. The server passes on what decides that section instead: the file
//! that won its own cascade (`--config <file>`, or `--no-config` when no file
//! won, so the child cannot find a file that appeared later) and its own
//! variables of the controller's sections (`NARSOR_GLOBAL__*`,
//! `NARSOR_CONTROLLER__*`). Every other `NARSOR_*` variable stays behind.
//!
//! [`resolve`] is what the controller resolves from exactly that, which
//! lets the server check the controller's configuration before it binds
//! its API, and refuse to start on its errors, without starting it.

use std::ffi::OsString;
use std::path::Path;

use super::cascade::ConfigCascade;
use super::resolve::{self, Environment, Flags, Resolved};
use super::scope::Scope;
use super::settings::ENV_PREFIX;

/// The controller's scope.
pub const SCOPE: Scope = Scope::CONTROLLER;

/// The prefixes of the variables the server forwards to its controller:
/// one per section of [`SCOPE`].
pub fn forwarded_prefixes() -> Vec<String> {
    debug_assert!(
        SCOPE.extra().is_empty(),
        "an extra setting of the controller would need its variable forwarded by name"
    );
    SCOPE
        .sections()
        .iter()
        .map(|section| section.env_prefix())
        .collect()
}

/// Whether the server hands the variable `name` to its controller: every
/// variable outside the `NARSOR_` namespace (the child's own preparation
/// decides about those), and of that namespace only the controller's
/// sections.
pub fn is_forwarded(name: &str) -> bool {
    !name.starts_with(ENV_PREFIX)
        || forwarded_prefixes()
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()))
}

/// The part of `environment` the controller child gets.
pub fn forwarded_environment(environment: &Environment) -> Environment {
    environment
        .iter()
        .filter(|(name, _)| is_forwarded(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// The controller arguments that name its configuration file: the server's
/// winning `file`, or `--no-config` when none won.
pub fn config_args(file: Option<&Path>) -> Vec<OsString> {
    match file {
        Some(path) => vec!["--config".into(), path.as_os_str().to_owned()],
        None => vec!["--no-config".into()],
    }
}

/// The controller's configuration from `file` (none: no file at all) and
/// the variables of `environment` the controller reads.
///
/// The controller started with [`config_args`] resolves exactly this, and
/// the server's pre-start check calls it with the same file and its own
/// environment.
pub fn resolve(file: Option<&Path>, environment: &Environment) -> Resolved {
    let cascade = ConfigCascade::with_dirs(file.map(Path::to_path_buf), None, None);
    resolve::resolve_with(
        &Flags::default(),
        &forwarded_environment(environment),
        &cascade,
        SCOPE,
    )
}
