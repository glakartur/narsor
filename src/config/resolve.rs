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

use super::cascade::{CascadeError, ConfigCascade, Resolution};
use super::file::{self, FileError, FileValues};
use super::settings::{SettingKey, SettingValue, Source};

/// The environment, as a map read once rather than consulted repeatedly.
pub type Environment = BTreeMap<String, String>;

/// The variable that carries a full `tracing` filter directive.
///
/// It beats `NARSOR_LOG_LEVEL` because it is the more precise instrument: it
/// takes per-target directives such as `narsor::engine=debug`, where the
/// setting takes a bare level. There is no sense in the sharper tool losing
/// (D-K).
pub const RUST_LOG: &str = "RUST_LOG";

/// The environment of this process, as a map.
///
/// An empty value counts as unset, because that is how Kubernetes renders an
/// unset key of a ConfigMap — and how `XDG_CONFIG_HOME` is already treated
/// elsewhere in this crate (D-J).
pub fn environment_from_process() -> Environment {
    std::env::vars()
        .filter(|(_, value)| !value.is_empty())
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
    /// The value this setting was given on the command line, if any.
    fn value(&self, setting: SettingKey) -> Option<SettingValue> {
        match setting {
            SettingKey::DataDir => self.root.clone().map(SettingValue::Path),
            SettingKey::AgentHarness | SettingKey::AgentTimeout | SettingKey::LogLevel => None,
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
            ConfigError::Cascade(_) | ConfigError::Env { .. } => None,
        }
    }

    /// Whether this is the operator naming a file that is not there — the one
    /// configuration failure that is a usage error, and the one `config show`
    /// does not survive either (D-D).
    pub fn is_usage(&self) -> bool {
        match self {
            ConfigError::Cascade(error) => error.is_usage(),
            ConfigError::File(_) | ConfigError::Env { .. } => false,
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

    /// The data directory in force.
    pub fn data_dir(&self) -> &Path {
        self.value(SettingKey::DataDir)
            .as_path()
            .expect("the data directory is a path")
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

    /// The `tracing` filter to install, or `None` for "install nothing".
    ///
    /// `None` keeps today's behaviour exactly: with nothing configured no
    /// subscriber is installed and every subcommand is silent (D-N).
    pub fn log_directive(&self) -> Option<String> {
        let directive = self
            .value(SettingKey::LogLevel)
            .as_text()
            .expect("the level is text");
        if directive == "off" {
            None
        } else {
            Some(directive.to_string())
        }
    }
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
            Some(path) => match file::load(path) {
                Ok(values) => (resolution.clone(), values),
                Err(problems) => {
                    errors.extend(problems.into_iter().map(ConfigError::File));
                    (resolution.clone(), FileValues::default())
                }
            },
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
    if let Some(value) = flags.value(setting) {
        let flag = setting.flag().expect("a flag value implies a flag");
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
            Err(message) => errors.push(ConfigError::Env { variable, message }),
        }
    }

    if let (Some(value), Some(path)) = (file_values.get(setting), file_path) {
        // The source carries the path of the file that was actually read, not
        // the word "file": naming it is the whole of R8.
        return (value.clone(), Source::File(path.to_path_buf()));
    }

    (setting.built_in(), Source::BuiltIn)
}
