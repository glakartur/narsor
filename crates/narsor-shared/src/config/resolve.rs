//! Giving every setting a value and a reason.
//!
//! For each setting the binary's [`Scope`] reads, in this order: the command-line flag, the
//! environment variable, the file that won the cascade, and finally the value
//! compiled into the binary. The first hit wins — **per setting**, not per
//! file (R2). A file that sets one key therefore does not displace a flag that
//! sets another, and each row of `config show` can name the one thing that
//! decided it (R8).
//!
//! The three server directories are then joined to `global.root` when they
//! are relative (R10); their source stays the source of their own key.
//!
//! # Why this returns values and errors together
//!
//! Every subcommand refuses to start on a broken configuration file — except
//! `config show`, which is the only tool there is for diagnosing exactly that
//! (D-D). A `Result` would force that decision here, where it cannot be made.
//! So [`Resolved`] carries both, and the caller decides.
//!
//! # Why the environment is a parameter
//!
//! The variables are read once, in `main`, and handed in as a map. Reaching
//! for `std::env` deep inside resolution would leave the layering testable
//! only through `std::env::set_var`, which races every other test in the same
//! binary and produces failures that come and go. The seam is the same one
//! [`WorkflowResolver::with_user_dir`](crate::workflows::WorkflowResolver::with_user_dir)
//! provides.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::config::{AgentArgs, HarnessHome, HomeMode};

use super::cascade::{CascadeError, ConfigCascade, Resolution};
use super::file::{self, FileError, FileValues};
use super::local_controller::LocalMode;
use super::scope::Scope;
use super::settings::{SettingKey, SettingValue, Source, ENV_PREFIX, LOG_OFF, RUST_LOG};

/// The environment, as a map read once rather than consulted repeatedly.
pub type Environment = BTreeMap<String, String>;

/// The environment of this process, as a map.
///
/// An empty value counts as unset, because that is how Kubernetes renders an
/// unset key of a ConfigMap — and how `XDG_CONFIG_HOME` is already treated
/// elsewhere in this crate (D-J).
pub fn environment_from_process() -> Environment {
    // Only the variables that can carry a setting: a container's environment
    // runs to hundreds of entries, and copying all of them to answer five
    // lookups is work done on the start of every single subcommand.
    std::env::vars()
        .filter(|(name, value)| {
            !value.is_empty() && (name == RUST_LOG || name.starts_with(ENV_PREFIX))
        })
        .collect()
}

/// What the command line provided.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    /// `--root`.
    pub root: Option<PathBuf>,
    /// `--config`.
    pub config: Option<PathBuf>,
}

impl Flags {
    /// The value this setting was given on the command line, and the flag that
    /// gave it.
    ///
    /// Both together, from one match: a flag's name and its value are one
    /// fact, and stating them in two places would need a runtime check that
    /// they agree.
    fn value(&self, setting: SettingKey) -> Option<(SettingValue, &'static str)> {
        // `--root` sets `global.root` only; the server directories follow it
        // because they are relative to it (R9, R10).
        match setting {
            SettingKey::GlobalRoot => self
                .root
                .clone()
                .map(|root| (SettingValue::Path(root), "--root")),
            _ => None,
        }
    }
}

/// Anything that went wrong while working out the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Finding the file failed.
    #[error(transparent)]
    Cascade(#[from] CascadeError),

    /// Reading or validating the file failed.
    #[error(transparent)]
    File(#[from] FileError),

    /// A setting given in the environment does not hold up.
    #[error("Environment variable {variable}: {message}")]
    Env {
        /// The setting it was meant to set, so the row can be marked the same
        /// way a bad value in the file is.
        key: &'static str,
        /// The variable.
        variable: String,
        /// What is wrong with its value.
        message: String,
    },
}

impl ConfigError {
    /// The setting this is about, for the errors that are about one.
    pub fn key(&self) -> Option<&str> {
        match self {
            ConfigError::File(error) => error.key(),
            ConfigError::Env { key, .. } => Some(key),
            ConfigError::Cascade(_) => None,
        }
    }

    /// Whether this is the operator naming a file that is not there — the one
    /// configuration failure that is a usage error, and the one `config show`
    /// does not survive either (D-D).
    pub fn is_usage(&self) -> bool {
        match self {
            ConfigError::Cascade(error) => error.is_usage(),
            // Not a usage error: it stops every subcommand, but `config show`
            // has to survive it and print the table, because that is the one
            // tool for diagnosing a configuration (D-D, D-J).
            ConfigError::File(_) | ConfigError::Env { .. } => false,
        }
    }
}

/// Every setting of one scope, with its value and where that value came from.
#[derive(Debug)]
pub struct Resolved {
    values: BTreeMap<SettingKey, (SettingValue, Source)>,
    scope: Scope,
    file: Resolution,
    errors: Vec<ConfigError>,
}

impl Resolved {
    /// The effective value of one setting of the scope.
    ///
    /// Total by construction: every setting has a value compiled into the
    /// binary, so there is no "unset" to handle (R1). Asking for a setting
    /// outside the scope is a programming error, not a runtime fallback.
    pub fn value(&self, setting: SettingKey) -> &SettingValue {
        &self.entry(setting).0
    }

    /// Where that value came from.
    pub fn source(&self, setting: SettingKey) -> &Source {
        &self.entry(setting).1
    }

    fn entry(&self, setting: SettingKey) -> &(SettingValue, Source) {
        self.values.get(&setting).unwrap_or_else(|| {
            panic!("setting '{setting}' is outside the scope this binary resolved")
        })
    }

    /// The scope that was resolved.
    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// The base directory of the installation (`global.root`).
    pub fn root(&self) -> &Path {
        self.root_of(SettingKey::GlobalRoot)
    }

    /// What the cascade found, for a listing that names the shadowed files.
    pub fn file(&self) -> &Resolution {
        &self.file
    }

    /// Everything that went wrong, in a deterministic order.
    pub fn errors(&self) -> &[ConfigError] {
        &self.errors
    }

    /// Whether the configuration is usable as it stands.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Whether one of the failures is a usage error.
    pub fn has_usage_error(&self) -> bool {
        self.errors.iter().any(ConfigError::is_usage)
    }

    /// Where workflow definitions and the repository registry are read from,
    /// already joined to the root when it was written relative.
    pub fn catalog_dir(&self) -> &Path {
        self.root_of(SettingKey::CatalogDir)
    }

    /// The state root (`server.state`): every binary keeps its state in a
    /// directory of its own below it.
    pub fn state_dir(&self) -> &Path {
        self.root_of(SettingKey::StateDir)
    }

    /// The work root (`server.work`): every binary keeps its work in a
    /// directory of its own below it.
    pub fn work_dir(&self) -> &Path {
        self.root_of(SettingKey::WorkDir)
    }

    /// One of the directory settings, which are all paths.
    fn root_of(&self, setting: SettingKey) -> &Path {
        self.value(setting)
            .as_path()
            .expect("a directory is a path")
    }

    /// The agent harness an action that names none falls back to.
    pub fn agent_harness(&self) -> &str {
        self.value(SettingKey::AgentHarness)
            .as_text()
            .expect("the harness is text")
    }

    /// How often the terminal interface re-reads the daemon on its own
    /// (KTD16); `None` turns the periodic read off.
    pub fn tui_refresh_interval(&self) -> Option<u64> {
        self.value(SettingKey::TuiRefreshInterval)
            .as_duration()
            .expect("the refresh interval is a duration")
    }

    /// The backend a remote task goes to when its request names none (R10).
    pub fn default_backend(&self) -> &str {
        self.value(SettingKey::DefaultBackend)
            .as_text()
            .expect("the default backend is text")
    }

    /// The profile a remote task uses when its request names none (R16).
    pub fn default_profile(&self) -> &str {
        self.value(SettingKey::DefaultProfile)
            .as_text()
            .expect("the default profile is text")
    }

    /// Which HOME and harness configuration a local task's runner gets
    /// (`controller.local.*`).
    pub fn local_harness_home(&self) -> HarnessHome {
        let mode = match self.text(SettingKey::LocalHome) {
            Some("private") => HomeMode::Private,
            _ => HomeMode::Host,
        };
        HarnessHome {
            mode,
            codex_home: self.text(SettingKey::LocalCodexHome).map(PathBuf::from),
            claude_config_dir: self
                .text(SettingKey::LocalClaudeConfigDir)
                .map(PathBuf::from),
            claude_profile_source: self
                .text(SettingKey::LocalClaudeProfileSource)
                .map(PathBuf::from),
        }
    }

    /// How the local backend runs a task's runner (`controller.local.mode`).
    pub fn local_mode(&self) -> LocalMode {
        match self.text(SettingKey::LocalMode) {
            Some("docker") => LocalMode::Docker,
            _ => LocalMode::Process,
        }
    }

    /// How long a built task image is used before its component versions
    /// are checked again (`controller.local.docker.update_interval`), in
    /// seconds; `None` never checks again.
    pub fn local_docker_update_interval(&self) -> Option<u64> {
        self.value(SettingKey::LocalDockerUpdateInterval)
            .as_duration()
            .expect("the update interval is a duration")
    }

    /// The observer worker the server starts (`server.observer.*`), `None`
    /// when it starts none.
    pub fn observer_worker(&self) -> Option<ObserverWorkerConfig> {
        if self.text(SettingKey::ObserverEnabled) != Some("true") {
            return None;
        }
        Some(ObserverWorkerConfig {
            server: self.text(SettingKey::ObserverServerUrl).map(str::to_string),
            secrets_dir: self.text(SettingKey::ObserverSecretsDir).map(PathBuf::from),
            script_user: self
                .text(SettingKey::ObserverScriptUser)
                .map(str::to_string),
            insecure_same_user: self.text(SettingKey::ObserverInsecureSameUser) == Some("true"),
        })
    }

    /// One text setting as written, `None` when it is empty (not set).
    pub fn text(&self, setting: SettingKey) -> Option<&str> {
        self.value(setting)
            .as_text()
            .filter(|text| !text.trim().is_empty())
    }

    /// A list setting's entries, in the order written.
    pub fn list(&self, setting: SettingKey) -> Vec<String> {
        self.text(setting)
            .map(super::file::split_list)
            .unwrap_or_default()
    }

    /// The `runner.agent_args` setting: extra arguments by harness.
    pub fn agent_args(&self) -> AgentArgs {
        self.text(SettingKey::AgentArgs)
            .and_then(|text| serde_yaml::from_str(text).ok())
            .unwrap_or_default()
    }

    /// The model tier the conductor runs on; `None` is the harness's default.
    pub fn conductor_model(&self) -> Option<String> {
        self.text(SettingKey::ConductorModel)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    /// The effort the conductor runs with; `None` is the harness's default.
    pub fn conductor_effort(&self) -> Option<String> {
        self.text(SettingKey::ConductorEffort)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    /// The limit of tasks processed at once across all backends, if any.
    pub fn max_concurrent_tasks(&self) -> Option<u32> {
        self.text(SettingKey::MaxConcurrentTasks)
            .and_then(|text| text.parse().ok())
    }

    /// The `tracing` filter to install, or `None` for "install nothing".
    ///
    /// `None` keeps today's behaviour exactly: with nothing configured no
    /// subscriber is installed and every subcommand is silent (D-N).
    pub fn log_directive(&self) -> Option<String> {
        let directive = self
            .value(SettingKey::LogLevel)
            .as_text()
            .expect("the level is text");
        if directive == LOG_OFF {
            None
        } else {
            Some(directive.to_string())
        }
    }
}

/// What the server starts its observer worker with (`server.observer.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverWorkerConfig {
    /// The API's URL for the worker and its scripts; `None`: the server's
    /// own listening address.
    pub server: Option<String>,
    /// The secrets directory, `None` for none.
    pub secrets_dir: Option<PathBuf>,
    /// The system user scripts run as.
    pub script_user: Option<String>,
    /// Scripts run as the server's own user (a trial only).
    pub insecure_same_user: bool,
}

/// Works out every setting of `scope`, reading the environment that was
/// handed in.
pub fn resolve(flags: &Flags, environment: &Environment, scope: Scope) -> Resolved {
    let cascade = ConfigCascade::new(flags.config.clone());
    resolve_with(flags, environment, &cascade, scope)
}

/// The same, against a cascade named explicitly.
///
/// The seam a test uses so that it reaches neither `~/.config` nor `/etc`.
pub fn resolve_with(
    flags: &Flags,
    environment: &Environment,
    cascade: &ConfigCascade,
    scope: Scope,
) -> Resolved {
    let mut errors = Vec::new();

    let (file_resolution, file_values) = match cascade.resolve() {
        Ok(resolution) => match resolution.path() {
            Some(path) => {
                let parsed = file::load(path, &scope);
                // The keys that held up stay in force even when others did
                // not: `config show` prints the good rows beside the bad ones,
                // and dropping them would make the table lie (D-D).
                errors.extend(parsed.errors.into_iter().map(ConfigError::File));
                (resolution.clone(), parsed.values)
            }
            // No file anywhere is a normal, silent state (R4).
            None => (resolution, FileValues::default()),
        },
        Err(error) => {
            errors.push(ConfigError::Cascade(error));
            (Resolution::default(), FileValues::default())
        }
    };

    let file_path = file_resolution.path().map(Path::to_path_buf);

    let mut values = BTreeMap::new();
    for setting in scope.settings() {
        let (value, source) = resolve_one(
            setting,
            flags,
            environment,
            &file_values,
            file_path.as_deref(),
            &mut errors,
        );
        values.insert(setting, (value, source));
    }

    join_directories_to_root(&mut values);

    Resolved {
        values,
        scope,
        file: file_resolution,
        errors,
    }
}

/// Joins every relative server directory of the scope to `global.root`.
///
/// An absolute directory is used as it is. `.` components of the relative
/// value are dropped, so the built-in `.` of the catalogue is the root
/// itself (`./.narsor`, not `./.narsor/.`).
fn join_directories_to_root(values: &mut BTreeMap<SettingKey, (SettingValue, Source)>) {
    let Some(root) = values
        .get(&SettingKey::GlobalRoot)
        .and_then(|(value, _)| value.as_path())
        .map(Path::to_path_buf)
    else {
        return;
    };
    for directory in SettingKey::ROOTS {
        let Some((SettingValue::Path(path), _)) = values.get_mut(&directory) else {
            continue;
        };
        if path.is_absolute() {
            continue;
        }
        let relative: PathBuf = path
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect();
        *path = if relative.as_os_str().is_empty() {
            root.clone()
        } else {
            root.join(relative)
        };
    }
}

/// One setting, through the four layers in order.
fn resolve_one(
    setting: SettingKey,
    flags: &Flags,
    environment: &Environment,
    file_values: &FileValues,
    file_path: Option<&Path>,
    errors: &mut Vec<ConfigError>,
) -> (SettingValue, Source) {
    if let Some((value, flag)) = flags.value(setting) {
        return (value, Source::Flag(flag));
    }

    // RUST_LOG is consulted before NARSOR_LOG_LEVEL, and `config show` then
    // names the variable that actually took effect (D-K).
    if setting == SettingKey::LogLevel {
        if let Some(directive) = environment.get(RUST_LOG) {
            return (
                SettingValue::Text(directive.clone()),
                Source::Env(RUST_LOG.to_string()),
            );
        }
    }

    let variable = setting.env_var();
    if let Some(text) = environment.get(&variable) {
        match file::value_from_text(setting, text) {
            Ok(value) => return (value, Source::Env(variable)),
            Err(message) => errors.push(ConfigError::Env {
                key: setting.key(),
                variable,
                message,
            }),
        }
    }

    if let (Some(value), Some(path)) = (file_values.get(setting), file_path) {
        // The source carries the path of the file that was actually read, not
        // the word "file": naming it is the whole of R8.
        return (value.clone(), Source::File(path.to_path_buf()));
    }

    (setting.built_in(), Source::BuiltIn)
}
