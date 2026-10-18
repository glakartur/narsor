//! Giving every setting a value and a reason.
//!
//! For each setting in the registry, in this order: the command-line flag, the
//! environment variable, the file that won the cascade, and finally the value
//! compiled into the binary. The first hit wins — **per setting**, not per
//! file (R2). A file that sets one key therefore does not displace a flag that
//! sets another, and each row of `config show` can name the one thing that
//! decided it (R8).
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
use std::path::{Path, PathBuf};

use crate::config::{HarnessHome, HomeMode};

use super::cascade::{CascadeError, ConfigCascade, Resolution};
use super::file::{self, FileError, FileValues};
use super::settings::{self, SettingKey, SettingValue, Source, ENV_PREFIX, LOG_OFF, RUST_LOG};

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
        match setting {
            // One flag, three settings. `--root` stays the single knob for
            // local work and for tests, and it stays a *flag* rather than a
            // fourth setting the three derive from: a derived value would need
            // a source variant of its own ("derived from data_dir") and break
            // the one-setting-one-key-one-source model the whole preview
            // stands on (D-B).
            SettingKey::CatalogDir | SettingKey::StateDir | SettingKey::WorkDir => self
                .root
                .clone()
                .map(|root| (SettingValue::Path(root), "--root")),
            SettingKey::AgentHarness
            | SettingKey::AgentTimeout
            | SettingKey::LogLevel
            | SettingKey::TuiRefreshInterval
            | SettingKey::DefaultBackend
            | SettingKey::DefaultProfile
            | SettingKey::ServerMode
            | SettingKey::ServerBind
            | SettingKey::OidcIssuer
            | SettingKey::OidcAudience
            | SettingKey::OwnerSubject
            | SettingKey::TlsTermination
            | SettingKey::TrustedProxies
            | SettingKey::CorsAllowedOrigins
            | SettingKey::LocalWorkspaceRoots
            | SettingKey::LocalHome
            | SettingKey::LocalCodexHome
            | SettingKey::LocalClaudeConfigDir
            | SettingKey::LocalClaudeProfileSource => None,
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

    /// A variable that carried a setting in an earlier version.
    ///
    /// Its own variant because it cannot be reported by the loop that asks for
    /// one variable per known setting: that loop never looks the name up, so
    /// the variable would be ignored in silence — the one outcome R6 forbids.
    #[error("{message}")]
    RetiredEnv {
        /// The key it used to set, so the row can be marked.
        key: &'static str,
        /// The variable.
        variable: String,
        /// What happened to it, and what to set instead.
        message: String,
    },

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
            ConfigError::Env { key, .. } | ConfigError::RetiredEnv { key, .. } => Some(key),
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
            ConfigError::File(_) | ConfigError::Env { .. } | ConfigError::RetiredEnv { .. } => {
                false
            }
        }
    }
}

/// Every setting, with its value and where that value came from.
#[derive(Debug)]
pub struct Resolved {
    values: BTreeMap<SettingKey, (SettingValue, Source)>,
    file: Resolution,
    errors: Vec<ConfigError>,
}

impl Resolved {
    /// The effective value of one setting.
    ///
    /// Total by construction: every setting has a value compiled into the
    /// binary, so there is no "unset" to handle (R1).
    pub fn value(&self, setting: SettingKey) -> &SettingValue {
        &self
            .values
            .get(&setting)
            .expect("every setting is resolved")
            .0
    }

    /// Where that value came from.
    pub fn source(&self, setting: SettingKey) -> &Source {
        &self
            .values
            .get(&setting)
            .expect("every setting is resolved")
            .1
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

    /// Where workflow definitions and the repository registry are read from.
    pub fn catalog_dir(&self) -> &Path {
        self.root_of(SettingKey::CatalogDir)
    }

    /// Where executions are kept.
    pub fn state_dir(&self) -> &Path {
        self.root_of(SettingKey::StateDir)
    }

    /// Where task workspaces are created.
    pub fn work_dir(&self) -> &Path {
        self.root_of(SettingKey::WorkDir)
    }

    /// The client's narrow filesystem paths for logging and daemon discovery.
    pub fn client_paths(&self) -> crate::config::client_paths::ClientPaths {
        crate::config::client_paths::ClientPaths::new(self.state_dir())
    }

    /// One of the three roots, which are all paths.
    fn root_of(&self, setting: SettingKey) -> &Path {
        self.value(setting).as_path().expect("a root is a path")
    }

    /// The agent harness an action that names none falls back to.
    pub fn agent_harness(&self) -> &str {
        self.value(SettingKey::AgentHarness)
            .as_text()
            .expect("the harness is text")
    }

    /// The time limit an agent action that declares none falls back to.
    ///
    /// `None` is "no limit", which is what such an action gets today.
    pub fn agent_timeout(&self) -> Option<u64> {
        self.value(SettingKey::AgentTimeout)
            .as_duration()
            .expect("the timeout is a duration")
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

    /// Which HOME and harness configuration a local task's runner gets.
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

    /// What the engine needs from the configuration for an `agent` action.
    pub fn agent_defaults(&self) -> AgentDefaults {
        AgentDefaults {
            harness: self.agent_harness().to_string(),
            timeout_secs: self.agent_timeout(),
        }
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

/// The configuration values an engine needs for an agent action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefaults {
    /// The fallback harness name.
    pub harness: String,
    /// The fallback timeout, or no limit.
    pub timeout_secs: Option<u64>,
}

/// Works out every setting, reading the environment that was handed in.
pub fn resolve(flags: &Flags, environment: &Environment) -> Resolved {
    let cascade = ConfigCascade::new(flags.config.clone());
    resolve_with(flags, environment, &cascade)
}

/// The same, against a cascade named explicitly.
///
/// The seam a test uses so that it reaches neither `~/.config` nor `/etc`.
pub fn resolve_with(flags: &Flags, environment: &Environment, cascade: &ConfigCascade) -> Resolved {
    let mut errors = Vec::new();

    let (file_resolution, file_values) = match cascade.resolve() {
        Ok(resolution) => match resolution.path() {
            Some(path) => {
                let parsed = file::load(path);
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

    // Swept separately, and before the settings: the loop below asks for one
    // variable per *known* setting, so a retired one is never looked up there
    // and would pass unnoticed (D-I).
    for withdrawn in settings::RETIRED_KEYS {
        let variable = withdrawn.env_var();
        if environment.contains_key(&variable) {
            errors.push(ConfigError::RetiredEnv {
                key: withdrawn.key,
                variable,
                message: withdrawn.env_message(),
            });
        }
    }

    let mut values = BTreeMap::new();
    for setting in SettingKey::ALL {
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

    Resolved {
        values,
        file: file_resolution,
        errors,
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
