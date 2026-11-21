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
use std::path::{Path, PathBuf};

use super::cascade::ConfigCascade;
use super::check::Problem;
use super::resolve::{self, Environment, Flags, Resolved};
use super::scope::Scope;
use super::settings::{SettingKey, Source, ENV_PREFIX};

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

/// The credential file of the local controller of a server whose state
/// directory (`server.state`) is `state_dir`. Its directory also holds the
/// controller's token store, which `narsor-cli secret set` reaches through
/// it.
pub fn credential_file(state_dir: &Path) -> PathBuf {
    state_dir.join("local-controller").join("credential")
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

/// How the local backend runs a task's runner (`controller.local.mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LocalMode {
    /// A `narsor-runner` process on the host, as local tasks have always run.
    #[default]
    Process,
    /// A `narsor-runner` in a Docker container on the local daemon, built
    /// from the task image the control plane sealed into the task.
    Docker,
}

impl LocalMode {
    /// The word in configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            LocalMode::Process => "process",
            LocalMode::Docker => "docker",
        }
    }
}

/// The rules between the controller's settings that need nothing but the
/// configuration: the controller refuses to start on them, its
/// `--check-config` reports them, and a trusted server checks them before
/// it starts its controller.
///
/// A container always runs with a private HOME on task storage (KTD5), so a
/// `host` HOME somebody set explicitly cannot go with Docker mode. The
/// built-in `host` does not count: Docker mode replaces it.
pub fn problems(config: &Resolved) -> Vec<Problem> {
    let mut problems = Vec::new();
    let home_source = config.source(SettingKey::LocalHome);
    if config.local_mode() == LocalMode::Docker
        && config.text(SettingKey::LocalHome) == Some("host")
        && !matches!(home_source, Source::BuiltIn)
    {
        problems.push(Problem::at(
            SettingKey::LocalHome,
            format!(
                "{} is 'docker' but {} is 'host' (from {home_source}): a container always runs \
                 with a private HOME on task storage. Remove {} or set it to 'private'",
                SettingKey::LocalMode,
                SettingKey::LocalHome,
                SettingKey::LocalHome,
            ),
        ));
    }
    problems
}
