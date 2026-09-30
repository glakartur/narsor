//! What settings exist, what they are worth when nothing says otherwise, and
//! how to name where a value came from.
//!
//! This module is the single source of truth for the configuration surface and
//! it does no input or output at all: the cascade ([`super::cascade`]) finds a
//! file, the parser ([`super::file`]) reads one, and resolution
//! ([`super::resolve`]) layers them — but all three ask *this* module which
//! keys exist, what their environment variables are called, and what the
//! built-in value is.
//!
//! [`Source`] is deliberately a mirror of
//! [`WorkflowOrigin`](crate::workflows::WorkflowOrigin): a variant that carries
//! the concrete thing (a path, a variable name, a flag name), a `label()` for
//! the word in a table, and a `Display` that renders "word (concrete)". R8 is
//! the reason it carries the concrete thing rather than a category: an operator
//! asking where a value came from needs the path of the file that was actually
//! read, not the word "file".

use std::fmt;
use std::path::{Path, PathBuf};

/// Prefix of every environment variable that carries a setting.
///
/// `ORCHESTRATOR_ACTION_ID` lives outside this prefix on purpose: it is a
/// contract towards workflow scripts, not a setting (see
/// [`ACTION_ID_ENV_VAR`](crate::engine::actions::ACTION_ID_ENV_VAR)).
pub const ENV_PREFIX: &str = "NARSOR_";

/// What is printed instead of the value of a sensitive setting.
pub const MASK: &str = "********";

/// The logging levels a setting may name, in increasing verbosity.
///
/// A closed set rather than a full `EnvFilter` directive: `RUST_LOG` is the
/// tool for directives, and it wins over this setting anyway (D-K).
pub const LOG_LEVELS: [&str; 6] = [LOG_OFF, "error", "warn", "info", "debug", "trace"];

/// The level that turns logging off entirely.
///
/// Load-bearing rather than cosmetic: it is the value that decides whether a
/// subscriber is installed at all, which is what keeps an unconfigured run
/// silent (D-N).
pub const LOG_OFF: &str = "off";

/// The variable that carries a full `tracing` filter directive.
///
/// It belongs to the registry rather than to the resolver because this module
/// is where the names of the variables live. It beats `NARSOR_LOG_LEVEL`
/// because it is the more precise instrument: it takes per-target directives
/// such as `narsor::engine=debug`, where the setting takes a bare level. There
/// is no sense in the sharper tool losing (D-K).
pub const RUST_LOG: &str = "RUST_LOG";

/// How a duration setting spells "no limit at all".
///
/// Writable in the file so that a value inherited from `/etc/narsor/` can be
/// revoked rather than only replaced.
pub const NO_LIMIT: &str = "none";

/// How often the terminal interface re-reads the daemon when nothing says
/// otherwise (KTD16): often enough that a waiting task is noticed without
/// `r`, rarely enough that an idle interface does not hammer the daemon.
pub const DEFAULT_TUI_REFRESH_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Every setting this version has.
///
/// An enum rather than a table of strings: the compiler then refuses a match
/// that forgets a setting, which is what keeps `config show` complete (R7) as
/// the set grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingKey {
    /// Where the workflow definitions and the repository registry are read
    /// from. Shareable between processes, and mountable read-only.
    CatalogDir,
    /// Where executions are kept. Separable for mounting, not for sharing.
    StateDir,
    /// Where task workspaces — the git checkouts — are created. Local scratch.
    WorkDir,
    /// The agent harness used by an `agent` action that names none itself.
    AgentHarness,
    /// The time limit of an `agent` action that declares none itself.
    AgentTimeout,
    /// The logging level, when `RUST_LOG` does not say otherwise.
    LogLevel,
    /// How often the terminal interface re-reads the daemon on its own
    /// (KTD16), on top of the refreshes the live stream triggers.
    TuiRefreshInterval,
}

impl SettingKey {
    /// Every setting, in the order `config show` prints them.
    pub const ALL: [SettingKey; 7] = [
        SettingKey::CatalogDir,
        SettingKey::StateDir,
        SettingKey::WorkDir,
        SettingKey::AgentHarness,
        SettingKey::AgentTimeout,
        SettingKey::LogLevel,
        SettingKey::TuiRefreshInterval,
    ];

    /// The three roots, in the order `config show` prints them.
    pub const ROOTS: [SettingKey; 3] = [
        SettingKey::CatalogDir,
        SettingKey::StateDir,
        SettingKey::WorkDir,
    ];

    /// The key as it is written in the configuration file.
    pub fn key(self) -> &'static str {
        match self {
            SettingKey::CatalogDir => "catalog_dir",
            SettingKey::StateDir => "state_dir",
            SettingKey::WorkDir => "work_dir",
            SettingKey::AgentHarness => "agent_harness",
            SettingKey::AgentTimeout => "agent_timeout",
            SettingKey::LogLevel => "log_level",
            SettingKey::TuiRefreshInterval => "tui_refresh_interval",
        }
    }

    /// The setting written under that key, if there is one.
    pub fn from_key(key: &str) -> Option<SettingKey> {
        SettingKey::ALL.into_iter().find(|entry| entry.key() == key)
    }

    /// The environment variable that carries it.
    ///
    /// Derived mechanically from the key so that the mapping is total (R6) and
    /// so that a new setting cannot arrive without one.
    pub fn env_var(self) -> String {
        format!("{ENV_PREFIX}{}", self.key().to_uppercase())
    }

    /// Whether the value is masked when printed.
    ///
    /// No setting is sensitive in this version; the flag exists because
    /// sensitivity has to be an attribute of the entry rather than a rule
    /// guessed from the key's name (R11).
    pub fn is_sensitive(self) -> bool {
        match self {
            SettingKey::CatalogDir
            | SettingKey::StateDir
            | SettingKey::WorkDir
            | SettingKey::AgentHarness
            | SettingKey::AgentTimeout
            | SettingKey::LogLevel
            | SettingKey::TuiRefreshInterval => false,
        }
    }

    /// The value in force when nothing else provides one (R1).
    ///
    /// The logging level is `off` rather than `info` on purpose: with
    /// `RUST_LOG` unset today no subscriber is installed and every subcommand
    /// is silent (see `src/main.rs`). A built-in of `info` would turn logging
    /// on for every command at once and make `tui` create a data directory and
    /// a log file on every run — a change of behaviour no requirement asks for.
    pub fn built_in(self) -> SettingValue {
        match self {
            SettingKey::CatalogDir | SettingKey::StateDir | SettingKey::WorkDir => {
                SettingValue::Path(PathBuf::from(crate::persistence::paths::DEFAULT_ROOT))
            }
            SettingKey::AgentHarness => {
                SettingValue::Text(crate::engine::actions::agent::BUILT_IN_HARNESS.to_string())
            }
            SettingKey::AgentTimeout => SettingValue::Duration(None),
            SettingKey::LogLevel => SettingValue::Text(LOG_OFF.to_string()),
            SettingKey::TuiRefreshInterval => {
                SettingValue::Duration(Some(DEFAULT_TUI_REFRESH_SECS))
            }
        }
    }

    /// Whether the value is a duration, which the file may also write as a
    /// bare number of seconds or as an explicit null.
    pub fn is_duration(self) -> bool {
        matches!(
            self,
            SettingKey::AgentTimeout | SettingKey::TuiRefreshInterval
        )
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.key())
    }
}

// ---------------------------------------------------------------------------
// Settings that used to exist
// ---------------------------------------------------------------------------

/// A key that was a setting once and is not one now.
///
/// Its own table rather than a variant of [`SettingKey`], because it is not a
/// setting: it has no value, no source and no row in `config show`. What it
/// has is a name that an operator may still have in a file or a deployment,
/// and one job — to say what happened instead of being mistaken for a typo.
///
/// Both places that could see the name have to consult this. The file parser
/// would otherwise answer with the generic "unknown key, known keys are …",
/// which lists the successors without saying that they *are* successors. The
/// environment is worse: resolution asks for one variable per known setting,
/// so a retired variable is not looked up at all and would be ignored in
/// complete silence — which is exactly the failure this table exists to
/// prevent.
pub const RETIRED_KEYS: &[RetiredKey] = &[RetiredKey {
    key: "data_dir",
    replacement: "It was split into 'catalog_dir' (workflow definitions and the repository \
                  registry), 'state_dir' (executions) and 'work_dir' (task workspaces). Set \
                  the ones you need, or pass --root to point all three at one directory.",
}];

/// One withdrawn setting and the sentence that explains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredKey {
    /// The key as it was written in the file.
    pub key: &'static str,
    /// What to do instead, as one sentence for an operator.
    pub replacement: &'static str,
}

impl RetiredKey {
    /// The environment variable that used to carry it.
    pub fn env_var(&self) -> String {
        format!("{ENV_PREFIX}{}", self.key.to_uppercase())
    }

    /// The whole message, for a key written in a file.
    pub fn message(&self) -> String {
        format!(
            "Setting '{}' no longer exists. {}",
            self.key, self.replacement
        )
    }

    /// The whole message, for a variable found in the environment.
    pub fn env_message(&self) -> String {
        format!(
            "Environment variable {} sets '{}', which no longer exists. {}",
            self.env_var(),
            self.key,
            self.replacement
        )
    }
}

/// The retired setting written under this key, if there is one.
pub fn retired(key: &str) -> Option<&'static RetiredKey> {
    RETIRED_KEYS.iter().find(|entry| entry.key == key)
}

/// The retired setting this environment variable used to carry, if any.
pub fn retired_env_var(variable: &str) -> Option<&'static RetiredKey> {
    RETIRED_KEYS
        .iter()
        .find(|entry| entry.env_var() == variable)
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// The value of one setting, in the shape that setting takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// A filesystem path.
    Path(PathBuf),
    /// A word out of a closed set — a harness name, a logging level.
    Text(String),
    /// A number of seconds, or `None` for "no limit at all".
    Duration(Option<u64>),
}

impl SettingValue {
    /// The path, for the settings that are one.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            SettingValue::Path(path) => Some(path),
            _ => None,
        }
    }

    /// The text, for the settings that are one.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            SettingValue::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The number of seconds, for the settings that are a duration.
    ///
    /// The outer `Option` says "this is not a duration"; the inner one says
    /// "no limit".
    pub fn as_duration(&self) -> Option<Option<u64>> {
        match self {
            SettingValue::Duration(seconds) => Some(*seconds),
            _ => None,
        }
    }
}

impl fmt::Display for SettingValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingValue::Path(path) => write!(formatter, "{}", path.display()),
            SettingValue::Text(text) => formatter.write_str(text),
            SettingValue::Duration(Some(seconds)) => write!(formatter, "{seconds}s"),
            SettingValue::Duration(None) => formatter.write_str(NO_LIMIT),
        }
    }
}

/// Renders a value for a human, masking it when the entry says it is sensitive.
///
/// Takes the flag rather than the [`SettingKey`] so that masking stays an
/// attribute of the entry (R11) and stays testable while no real setting is
/// sensitive.
pub fn render_value(value: &SettingValue, sensitive: bool) -> String {
    if sensitive {
        MASK.to_string()
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Where a value came from
// ---------------------------------------------------------------------------

/// Where the effective value of one setting came from.
///
/// Each variant carries the concrete thing rather than a category, because the
/// question this whole layer answers is "where did *this* value come from" and
/// "file" alone does not answer it (R8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A command-line flag, by the name the operator typed.
    Flag(&'static str),
    /// An environment variable, by name.
    Env(String),
    /// The configuration file that won the cascade, by path.
    File(PathBuf),
    /// Nothing provided one; this is the value compiled into the binary.
    BuiltIn,
}

impl Source {
    /// The word printed in the source column.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Flag(_) => "flag",
            Source::Env(_) => "env",
            Source::File(_) => "file",
            Source::BuiltIn => "built-in",
        }
    }

    /// The concrete thing behind the word — `None` for a built-in, which has
    /// nothing to name.
    pub fn detail(&self) -> Option<String> {
        match self {
            Source::Flag(flag) => Some((*flag).to_string()),
            Source::Env(name) => Some(name.clone()),
            Source::File(path) => Some(path.display().to_string()),
            Source::BuiltIn => None,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.detail() {
            Some(detail) => write!(formatter, "{} ({})", self.label(), detail),
            None => formatter.write_str(self.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_holds_every_setting_the_requirements_name() {
        let keys: Vec<&str> = SettingKey::ALL.iter().map(|key| key.key()).collect();
        assert_eq!(
            keys,
            vec![
                "catalog_dir",
                "state_dir",
                "work_dir",
                "agent_harness",
                "agent_timeout",
                "log_level",
                "tui_refresh_interval"
            ],
            "the registry is the one place the settings live, and the three \
             roots lead because that is the order `config show` prints"
        );
    }

    #[test]
    fn data_dir_is_retired_rather_than_forgotten() {
        // It must not come back as a setting: a key that both exists and is
        // withdrawn would be reported twice and resolved once.
        assert_eq!(SettingKey::from_key("data_dir"), None);

        let withdrawn = retired("data_dir").expect("data_dir is a retired key");
        assert_eq!(withdrawn.env_var(), "NARSOR_DATA_DIR");
        assert_eq!(
            retired_env_var("NARSOR_DATA_DIR").map(|entry| entry.key),
            Some("data_dir")
        );

        // The message names all three successors, which is the whole job.
        for successor in ["catalog_dir", "state_dir", "work_dir"] {
            assert!(
                withdrawn.message().contains(successor),
                "the message must name {successor}: {}",
                withdrawn.message()
            );
            assert!(
                withdrawn.env_message().contains(successor),
                "the variable's message must name {successor} too"
            );
        }
        assert!(withdrawn.env_message().contains("NARSOR_DATA_DIR"));
    }

    #[test]
    fn no_retired_key_is_also_a_live_setting() {
        for withdrawn in RETIRED_KEYS {
            assert_eq!(
                SettingKey::from_key(withdrawn.key),
                None,
                "'{}' cannot be both withdrawn and in force",
                withdrawn.key
            );
        }
    }

    #[test]
    fn every_setting_has_a_prefixed_upper_case_environment_variable() {
        for key in SettingKey::ALL {
            let variable = key.env_var();
            assert!(
                variable.starts_with(ENV_PREFIX),
                "{variable} must carry the {ENV_PREFIX} prefix"
            );
            assert_eq!(
                variable,
                variable.to_uppercase(),
                "{variable} must be upper case"
            );
        }
    }

    #[test]
    fn the_environment_variable_names_are_all_distinct() {
        let mut names: Vec<String> = SettingKey::ALL.iter().map(|key| key.env_var()).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "two settings must not share a variable"
        );
    }

    #[test]
    fn every_key_resolves_back_to_its_setting() {
        for key in SettingKey::ALL {
            assert_eq!(SettingKey::from_key(key.key()), Some(key));
        }
        assert_eq!(SettingKey::from_key("nonsense"), None);
    }

    #[test]
    fn the_built_in_logging_level_is_off_not_info() {
        // D-N: `info` would turn logging on for every subcommand at once and
        // make `tui` create a log file no one asked for.
        assert_eq!(
            SettingKey::LogLevel.built_in(),
            SettingValue::Text("off".to_string())
        );
    }

    #[test]
    fn the_built_in_values_are_the_ones_the_plan_names() {
        for root in SettingKey::ROOTS {
            assert_eq!(
                root.built_in(),
                SettingValue::Path(PathBuf::from("./.narsor")),
                "{root} shares the one built-in root, so an unconfigured run \
                 writes where it always did"
            );
        }
        assert_eq!(
            SettingKey::AgentHarness.built_in(),
            SettingValue::Text("claude-code".to_string())
        );
        assert_eq!(
            SettingKey::AgentTimeout.built_in(),
            SettingValue::Duration(None),
            "no limit, which is what an agent action without one gets today"
        );
        assert_eq!(
            SettingKey::TuiRefreshInterval.built_in(),
            SettingValue::Duration(Some(10)),
            "KTD16: the interface re-reads the daemon every ten seconds"
        );
    }

    #[test]
    fn a_file_source_renders_the_path_and_a_built_in_renders_one_word() {
        let file = Source::File(PathBuf::from("/etc/narsor/config.yaml"));
        assert_eq!(file.to_string(), "file (/etc/narsor/config.yaml)");
        assert_eq!(Source::BuiltIn.to_string(), "built-in");
        assert_eq!(Source::Flag("--root").to_string(), "flag (--root)");
        assert_eq!(
            Source::Env("NARSOR_STATE_DIR".to_string()).to_string(),
            "env (NARSOR_STATE_DIR)"
        );
    }

    #[test]
    fn a_sensitive_entry_masks_its_value_and_leaves_its_source_alone() {
        let value = SettingValue::Text("hunter2".to_string());
        assert_eq!(render_value(&value, true), MASK);

        // The source is a separate thing and is never touched by masking.
        let source = Source::Env("NARSOR_SECRET".to_string());
        assert_eq!(source.to_string(), "env (NARSOR_SECRET)");
    }

    #[test]
    fn an_entry_that_is_not_sensitive_is_never_masked() {
        // Even when the value looks like a secret: sensitivity is an attribute
        // of the entry, never a guess about the value (D-M).
        let value = SettingValue::Text("token-abc123".to_string());
        assert_eq!(render_value(&value, false), "token-abc123");

        for key in SettingKey::ALL {
            assert!(
                !key.is_sensitive(),
                "{key} is not sensitive in this version"
            );
        }
    }

    #[test]
    fn a_duration_renders_seconds_or_the_word_for_no_limit() {
        assert_eq!(SettingValue::Duration(Some(1800)).to_string(), "1800s");
        assert_eq!(SettingValue::Duration(None).to_string(), NO_LIMIT);
    }
}
