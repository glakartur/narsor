//! The configuration check every binary can run on its own part of the
//! file (R14), and the report it prints.
//!
//! Each binary that reads the file has a hidden `--check-config` flag: it
//! resolves its scope, runs its own rules between the settings, prints one
//! [`CheckReport`] as JSON on stdout and exits 0 (no problem) or 1, without
//! starting anything. `narsor-cli config show` reads these reports from the
//! binaries installed next to it and merges them (R15).
//!
//! # Redaction happens here
//!
//! The report replaces the value of every sensitive setting with
//! [`MASK`] while it is built, so the raw value never leaves the process
//! that resolved it; a problem message that quotes it is masked the same
//! way.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::resolve::{ConfigError, Resolved};
use super::settings::{render_value, SettingKey, Source, MASK};

/// The flag that runs the check.
pub const CHECK_CONFIG_FLAG: &str = "--check-config";

/// The version of the report's JSON shape, shared with `config show`.
pub const CHECK_REPORT_VERSION: u32 = 2;

/// The exit code of a check that found no problem.
pub const CHECK_EXIT_OK: i32 = 0;

/// The exit code of a check that found problems.
pub const CHECK_EXIT_PROBLEMS: i32 = 1;

/// One problem of a configuration: what is wrong and, when it is about one
/// setting, that setting's path.
///
/// The message names every path it is about, for a human reader; the path
/// is the one setting a listing marks with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    path: Option<String>,
    message: String,
}

impl Problem {
    /// A problem of `setting`.
    pub fn at(setting: SettingKey, message: impl Into<String>) -> Self {
        Self {
            path: Some(setting.key().to_string()),
            message: message.into(),
        }
    }

    /// The same problem with `prefix` in front of its message.
    pub fn prefixed(self, prefix: &str) -> Self {
        Self {
            message: format!("{prefix}{}", self.message),
            ..self
        }
    }

    /// The setting it is about, if one.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// What is wrong.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The resolution errors of `config`, as problems.
    pub fn of_resolution(config: &Resolved) -> Vec<Problem> {
        config.errors().iter().map(Problem::from).collect()
    }
}

impl From<&ConfigError> for Problem {
    fn from(error: &ConfigError) -> Self {
        Self {
            path: error.key().map(str::to_string),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// What one binary found in its part of the configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    /// [`CHECK_REPORT_VERSION`].
    pub version: u32,
    /// The binary that checked.
    pub binary: String,
    /// The file that won the cascade, if any.
    pub file: Option<String>,
    /// Files that exist but lost.
    #[serde(default)]
    pub shadowed: Vec<String>,
    /// Every setting of the binary's scope, by path.
    pub settings: BTreeMap<String, CheckedSetting>,
    /// Every problem, the ones that mark a setting included.
    #[serde(default)]
    pub errors: Vec<CheckError>,
}

/// One setting of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckedSetting {
    /// The effective value as text, [`MASK`] for a sensitive setting;
    /// `None` when the only value was refused and the built-in stands in.
    pub value: Option<String>,
    /// Where the value came from; `None` with `value`.
    pub source: Option<CheckedSource>,
    /// The problem of this setting, if any.
    pub error: Option<String>,
    /// Whether the value is masked.
    pub sensitive: bool,
}

/// Where a value came from: an object, so nothing has to parse a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckedSource {
    /// `flag`, `env`, `file` or `built-in`.
    pub kind: String,
    /// The flag, variable or file; `None` for a built-in.
    pub detail: Option<String>,
}

/// One problem of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckError {
    /// The setting it marks, if one.
    pub path: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl CheckReport {
    /// The report of `binary` on `config` with `problems`: the resolution
    /// errors and the binary's rules between the settings.
    ///
    /// A setting is marked with the first problem at its path, except a
    /// resolution error that a higher layer overruled: with a bad value in
    /// the file and a good one in the environment, the environment's value
    /// is in force and the row says so. A resolution error of a setting that
    /// fell back to its built-in value hides that value, which nobody
    /// chose.
    pub fn build(binary: &str, config: &Resolved, problems: &[Problem]) -> Self {
        let secrets: Vec<String> = config
            .scope()
            .settings()
            .filter(|setting| setting.is_sensitive())
            .map(|setting| config.value(setting).to_string())
            .filter(|value| !value.trim().is_empty())
            .collect();
        let redact = |message: &str| {
            secrets.iter().fold(message.to_string(), |text, secret| {
                text.replace(secret, MASK)
            })
        };

        let mut settings = BTreeMap::new();
        for setting in config.scope().settings() {
            let source = config.source(setting);
            let resolution_errors: Vec<String> = config
                .errors()
                .iter()
                .filter(|error| error.key() == Some(setting.key()))
                .map(ToString::to_string)
                .collect();
            let masked = *source == Source::BuiltIn && !resolution_errors.is_empty();
            let error = problems
                .iter()
                .filter(|problem| problem.path() == Some(setting.key()))
                .find(|problem| masked || !resolution_errors.contains(&problem.message))
                .map(|problem| redact(problem.message()));
            let entry = if masked {
                CheckedSetting {
                    value: None,
                    source: None,
                    error,
                    sensitive: setting.is_sensitive(),
                }
            } else {
                CheckedSetting {
                    value: Some(render_value(config.value(setting), setting.is_sensitive())),
                    source: Some(CheckedSource {
                        kind: source.label().to_string(),
                        detail: source.detail(),
                    }),
                    error,
                    sensitive: setting.is_sensitive(),
                }
            };
            settings.insert(setting.key().to_string(), entry);
        }

        Self {
            version: CHECK_REPORT_VERSION,
            binary: binary.to_string(),
            file: config.file().path().map(|path| path.display().to_string()),
            shadowed: config
                .file()
                .shadowed
                .iter()
                .map(|candidate| candidate.path.display().to_string())
                .collect(),
            settings,
            errors: problems
                .iter()
                .map(|problem| CheckError {
                    path: problem.path().map(str::to_string),
                    message: redact(problem.message()),
                })
                .collect(),
        }
    }

    /// Whether the binary would start on this configuration.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// The exit code of the check.
    pub fn exit_code(&self) -> i32 {
        if self.is_ok() {
            CHECK_EXIT_OK
        } else {
            CHECK_EXIT_PROBLEMS
        }
    }

    /// The report as JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("the report is plain data")
    }

    /// A report read back from JSON; an error says why it is not one.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let report: Self = serde_json::from_str(text)
            .map_err(|error| format!("the output is not a check report: {error}"))?;
        if report.version != CHECK_REPORT_VERSION {
            return Err(format!(
                "the check report has version {}, {CHECK_REPORT_VERSION} was expected",
                report.version
            ));
        }
        Ok(report)
    }

    /// Prints the report on stdout and ends the process with its exit code:
    /// the end of every binary's `--check-config`.
    pub fn print_and_exit(&self) -> ! {
        println!("{}", self.to_json());
        std::process::exit(self.exit_code());
    }
}
