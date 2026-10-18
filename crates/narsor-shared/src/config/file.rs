//! Reading the configuration file: every value, or every error.
//!
//! # Why this parses in two phases
//!
//! Phase one deserializes the file into a plain [`serde_yaml::Mapping`] —
//! nothing typed, nothing rejected. Phase two walks that mapping against the
//! registry in [`super::settings`], checking each key and each value on its
//! own and collecting *all* the complaints into one deterministically ordered
//! list.
//!
//! The obvious alternative — a typed struct with `#[serde(deny_unknown_fields)]`,
//! which is the house rule in [`crate::workflow::schema`] — is a trap here. It
//! aborts deserialization at the first unknown key, so a file with two
//! mistakes reports one, and a file whose only mistake is an unknown key never
//! produces a mapping at all. An unknown key is precisely the case `config
//! show` exists to diagnose (D-D, D-E), so it has to survive into phase two.
//! The same guarantee the house rule gives is obtained here by comparing the
//! key set against the registry.
//!
//! Reporting every violation at once, in a fixed order, is the rule
//! [`crate::workflow::validator`] already follows.
//!
//! # What stops the read before any of that
//!
//! A file over [`MAX_FILE_BYTES`], or one that is not UTF-8, is refused with a
//! named error. The size limit is not paranoia: YAML has anchors and aliases,
//! so a small file on a mounted volume can expand into a large document in
//! memory — on the start of *every* subcommand (D-L). R5 allows a refusal, so
//! this refuses rather than letting the expansion happen.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use narsor_protocol::identifier::BackendId;

use super::settings::{self, SettingKey, SettingValue, LOG_LEVELS, NO_LIMIT};

/// The largest configuration file that will be parsed.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// The byte-order mark, which an editor on another platform may leave behind.
const BOM: char = '\u{feff}';

/// What one file said, key by key.
///
/// A key the file does not mention is simply absent: this type says what the
/// *file* provides, and turning absence into a built-in value is the job of
/// the layer above (see [`super::resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileValues {
    values: BTreeMap<SettingKey, SettingValue>,
}

impl FileValues {
    /// The value the file gives this setting, if it gives one.
    pub fn get(&self, key: SettingKey) -> Option<&SettingValue> {
        self.values.get(&key)
    }

    /// Whether the file set nothing at all — an empty file, or one with
    /// nothing but comments (D-H).
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Everything that can be wrong with a configuration file.
///
/// Every variant names the file, because a message that does not is useless
/// to an operator holding three of them.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// The file could not be read.
    #[error("Could not read configuration file '{path}': {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },

    /// The file is bigger than the limit.
    #[error(
        "Configuration file '{path}' is {bytes} bytes, over the {limit} byte limit; \
         it was not parsed."
    )]
    TooLarge {
        /// The file.
        path: PathBuf,
        /// Its size.
        bytes: u64,
        /// The limit it passed.
        limit: u64,
    },

    /// The bytes are not UTF-8.
    ///
    /// Its own message because the one the system produces for the same
    /// situation reads like a permission problem.
    #[error("Configuration file '{path}' is not valid UTF-8 text.")]
    NotUtf8 {
        /// The file.
        path: PathBuf,
    },

    /// The YAML does not parse.
    ///
    /// The line and the column come from `serde_yaml`'s own message, which is
    /// why `{source}` is kept in the format rather than flattened into a
    /// string earlier.
    #[error("Could not parse configuration file '{path}': {source}")]
    Syntax {
        /// The file.
        path: PathBuf,
        /// The parser's complaint, with the location.
        #[source]
        source: serde_yaml::Error,
    },

    /// The document is something other than a mapping of settings.
    #[error("Configuration file '{path}' must be a mapping of settings, not {found}.")]
    NotAMapping {
        /// The file.
        path: PathBuf,
        /// What was found instead.
        found: String,
    },

    /// A key no setting answers to.
    ///
    /// The list of keys that *would* have been accepted is derived from the
    /// registry when the message is rendered, rather than carried: it is the
    /// same list every time, and carrying it would allocate it once per
    /// offending key.
    #[error(
        "Configuration file '{path}' sets unknown key '{key}'; known keys are: {}.",
        known_keys()
    )]
    UnknownKey {
        /// The file.
        path: PathBuf,
        /// The key as written.
        key: String,
    },

    /// A key that was a setting in an earlier version and is not one now.
    ///
    /// Separate from [`FileError::UnknownKey`] because the two call for
    /// different sentences: an unknown key is answered with the list of keys
    /// that would have worked, while a retired one is answered by naming what
    /// replaced it. Falling through to the generic message would print the
    /// successors without ever saying they *are* the successors.
    #[error("Configuration file '{path}': {message}")]
    RetiredKey {
        /// The file.
        path: PathBuf,
        /// The key as written.
        key: String,
        /// What happened to it, and what to set instead.
        message: String,
    },

    /// A known key whose value does not fit.
    #[error("Configuration file '{path}', key '{key}': {message}")]
    BadValue {
        /// The file.
        path: PathBuf,
        /// The key.
        key: String,
        /// What is wrong with the value.
        message: String,
    },
}

impl FileError {
    /// The file the message is about.
    pub fn path(&self) -> &Path {
        match self {
            FileError::Read { path, .. }
            | FileError::TooLarge { path, .. }
            | FileError::NotUtf8 { path }
            | FileError::Syntax { path, .. }
            | FileError::NotAMapping { path, .. }
            | FileError::UnknownKey { path, .. }
            | FileError::RetiredKey { path, .. }
            | FileError::BadValue { path, .. } => path,
        }
    }

    /// The key the message is about, for the errors that have one.
    ///
    /// This is what lets `config show` mark the offending row rather than
    /// printing the complaint beside the table.
    pub fn key(&self) -> Option<&str> {
        match self {
            FileError::UnknownKey { key, .. }
            | FileError::RetiredKey { key, .. }
            | FileError::BadValue { key, .. } => Some(key),
            _ => None,
        }
    }
}

/// What one file yielded: the values that held up, and every complaint.
///
/// Both, rather than one or the other, because `config show` prints the rows
/// that are fine beside the rows that are not (D-D) — a key the operator got
/// right is still in force, and hiding it would make the table lie.
#[derive(Debug, Default)]
pub struct ParsedFile {
    /// Every value that held up.
    pub values: FileValues,
    /// Every complaint, sorted by key.
    pub errors: Vec<FileError>,
}

impl ParsedFile {
    /// Nothing wrong with it.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// The values, if there is nothing to complain about.
    pub fn into_result(self) -> Result<FileValues, Vec<FileError>> {
        if self.errors.is_empty() {
            Ok(self.values)
        } else {
            Err(self.errors)
        }
    }

    /// A whole-file failure: nothing could be read at all.
    fn failed(error: FileError) -> Self {
        Self {
            values: FileValues::default(),
            errors: vec![error],
        }
    }
}

/// Reads and parses one configuration file.
pub fn load(path: &Path) -> ParsedFile {
    // Opened once and then asked its own size: the check has to come before
    // the read — using the length of what was read would mean holding an
    // oversized file in memory to discover it was oversized — but the size of
    // an open handle costs no second lookup of the path.
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(source) => {
            return ParsedFile::failed(FileError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    match file.metadata() {
        Ok(metadata) if metadata.len() > MAX_FILE_BYTES => {
            return ParsedFile::failed(FileError::TooLarge {
                path: path.to_path_buf(),
                bytes: metadata.len(),
                limit: MAX_FILE_BYTES,
            })
        }
        Ok(_) => {}
        Err(source) => {
            return ParsedFile::failed(FileError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    }

    let mut bytes = Vec::new();
    if let Err(source) = file.read_to_end(&mut bytes) {
        return ParsedFile::failed(FileError::Read {
            path: path.to_path_buf(),
            source,
        });
    }

    let contents = match String::from_utf8(bytes) {
        Ok(contents) => contents,
        Err(_) => {
            return ParsedFile::failed(FileError::NotUtf8 {
                path: path.to_path_buf(),
            })
        }
    };

    parse(path, &contents)
}

/// Parses the text of a configuration file.
///
/// Separate from [`load`] so that the two phases can be exercised without a
/// filesystem; `path` is carried only to name the file in the messages.
pub fn parse(path: &Path, contents: &str) -> ParsedFile {
    let contents = contents.strip_prefix(BOM).unwrap_or(contents);

    // Phase one: no types, no rejections — just the document.
    let document: serde_yaml::Value = match serde_yaml::from_str(contents) {
        Ok(document) => document,
        Err(source) => {
            return ParsedFile::failed(FileError::Syntax {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let mapping = match document {
        // An empty file, or one holding nothing but comments, parses to null
        // and means "every value built in" (D-H). That is a statement, not an
        // absence, and `config show` still reports the file as the one in
        // force.
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        serde_yaml::Value::Mapping(mapping) => mapping,
        other => {
            return ParsedFile::failed(FileError::NotAMapping {
                path: path.to_path_buf(),
                found: describe(&other),
            })
        }
    };

    // Phase two: every key against the registry, every value on its own, and
    // every complaint collected rather than the first one raised.
    let mut values = BTreeMap::new();
    let mut errors = Vec::new();

    for (raw_key, raw_value) in mapping {
        let Some(key) = raw_key.as_str().map(ToString::to_string) else {
            errors.push(FileError::UnknownKey {
                path: path.to_path_buf(),
                key: describe(&raw_key),
            });
            continue;
        };

        let Some(setting) = SettingKey::from_key(&key) else {
            // Checked before the generic complaint, so a key that used to work
            // is told what replaced it rather than being read as a typo.
            if let Some(withdrawn) = settings::retired(&key) {
                errors.push(FileError::RetiredKey {
                    path: path.to_path_buf(),
                    key,
                    message: withdrawn.message(),
                });
            } else {
                errors.push(FileError::UnknownKey {
                    path: path.to_path_buf(),
                    key,
                });
            }
            continue;
        };

        match value_of(setting, &raw_value) {
            Ok(value) => {
                values.insert(setting, value);
            }
            Err(message) => errors.push(FileError::BadValue {
                path: path.to_path_buf(),
                key,
                message,
            }),
        }
    }

    // Sorted by key so that the same file always produces the same report,
    // whatever order the document happened to use.
    errors.sort_by(|left, right| {
        left.key()
            .unwrap_or_default()
            .cmp(right.key().unwrap_or_default())
    });

    ParsedFile {
        values: FileValues { values },
        errors,
    }
}

/// Every key the file may set, in registry order, ready to print.
fn known_keys() -> String {
    SettingKey::ALL
        .iter()
        .map(|key| key.key())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validates one value against the setting it was written under.
///
/// The `Err` is the sentence that goes after "key 'x': ", so it names what was
/// expected rather than only what was found.
fn value_of(setting: SettingKey, raw: &serde_yaml::Value) -> Result<SettingValue, String> {
    // The shapes YAML offers that text does not: an explicit null and a bare
    // integer, both of which a duration accepts.
    if setting.is_duration() {
        if raw.is_null() {
            return Ok(SettingValue::Duration(None));
        }
        if let Some(seconds) = raw.as_u64() {
            return duration_value(setting, seconds);
        }
    }

    // A list may be written as a YAML sequence of texts; it is held as the
    // same comma-separated text the environment variable carries, so both
    // are validated by one path.
    if setting.is_list() {
        if let Some(items) = raw.as_sequence() {
            let texts = items.iter().map(text_of).collect::<Result<Vec<_>, _>>()?;
            return value_from_text(setting, &texts.join(","));
        }
    }

    value_from_text(setting, &text_of(raw)?)
}

/// The entries of a comma-separated list setting, trimmed, empties dropped.
pub fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Validates one value written as text.
///
/// Shared with [`super::resolve`], so that a setting given in the environment
/// is held to exactly the same contract as the same setting given in the file
/// — R6 asks for the two to be interchangeable, and they are only
/// interchangeable if they are validated alike.
pub fn value_from_text(setting: SettingKey, text: &str) -> Result<SettingValue, String> {
    match setting {
        SettingKey::CatalogDir | SettingKey::StateDir | SettingKey::WorkDir => {
            if text.trim().is_empty() {
                return Err("a directory path was expected, but the value is empty".to_string());
            }
            Ok(SettingValue::Path(PathBuf::from(text)))
        }

        SettingKey::AgentHarness => {
            let supported = ["claude-code", "codex", "opencode"];
            if supported.contains(&text) {
                Ok(SettingValue::Text(text.to_string()))
            } else {
                Err(format!(
                    "unknown agent harness '{text}'; supported harnesses are: {}",
                    supported.join(", ")
                ))
            }
        }

        SettingKey::AgentTimeout | SettingKey::TuiRefreshInterval => {
            // The spelled-out word means "no limit" (for the refresh: no
            // periodic read at all), so a value inherited from /etc/narsor/
            // can be revoked and not only replaced.
            if text.trim().eq_ignore_ascii_case(NO_LIMIT) {
                return Ok(SettingValue::Duration(None));
            }
            let seconds = super::duration::parse_duration(text)
                .map_err(|error| format!("{error}; or '{NO_LIMIT}' for no limit at all"))?;
            duration_value(setting, seconds)
        }

        SettingKey::DefaultBackend => BackendId::parse(text)
            .map(|id| SettingValue::Text(id.to_string()))
            .map_err(|error| error.to_string()),

        SettingKey::DefaultProfile => BackendId::parse(text)
            .map(|_| SettingValue::Text(text.to_string()))
            // The same character set as a backend id; only the word differs.
            .map_err(|error| error.to_string().replacen("backend", "profile", 1)),

        SettingKey::ServerMode => one_of(
            text,
            "server mode",
            &[settings::SERVER_MODE_TRUSTED, settings::SERVER_MODE_NETWORK],
        ),

        SettingKey::TlsTermination => one_of(
            text,
            "TLS termination",
            &[
                settings::TLS_TERMINATION_NONE,
                settings::TLS_TERMINATION_PROXY,
            ],
        ),

        SettingKey::ServerBind => super::security_validation::parse_bind(text)
            .map(|_| SettingValue::Text(text.trim().to_string())),

        SettingKey::OidcIssuer => {
            if text.trim().is_empty() {
                return Ok(SettingValue::Text(String::new()));
            }
            super::security_validation::parse_issuer(text)
                .map(|_| SettingValue::Text(text.trim().to_string()))
        }

        SettingKey::OidcAudience | SettingKey::OwnerSubject => {
            Ok(SettingValue::Text(text.trim().to_string()))
        }

        SettingKey::TrustedProxies => {
            let items = split_list(text);
            for item in &items {
                super::security_validation::parse_proxy(item)?;
            }
            Ok(SettingValue::Text(items.join(",")))
        }

        SettingKey::CorsAllowedOrigins => {
            let items = split_list(text);
            for item in &items {
                super::security_validation::parse_origin(item)?;
            }
            Ok(SettingValue::Text(items.join(",")))
        }

        SettingKey::LocalWorkspaceRoots => {
            let items = split_list(text);
            for item in &items {
                if !std::path::Path::new(item).is_absolute() {
                    return Err(format!(
                        "'{item}' is not an absolute path; list absolute directories"
                    ));
                }
            }
            Ok(SettingValue::Text(items.join(",")))
        }

        SettingKey::LocalHome => one_of(text, "local HOME mode", &settings::LOCAL_HOME_MODES),

        SettingKey::LocalCodexHome
        | SettingKey::LocalClaudeConfigDir
        | SettingKey::LocalClaudeProfileSource => {
            let dir = text.trim();
            if !dir.is_empty() && !std::path::Path::new(dir).is_absolute() {
                return Err(format!(
                    "'{dir}' is not an absolute path; name the directory absolutely, \
                     or leave it empty for the harness's own"
                ));
            }
            Ok(SettingValue::Text(dir.to_string()))
        }

        SettingKey::LogLevel => {
            let level = text.trim().to_lowercase();
            if LOG_LEVELS.contains(&level.as_str()) {
                Ok(SettingValue::Text(level))
            } else {
                Err(format!(
                    "unknown logging level '{text}'; allowed levels are: {}",
                    LOG_LEVELS.join(", ")
                ))
            }
        }
    }
}

/// One word out of a closed set, compared without regard to case.
fn one_of(text: &str, what: &str, allowed: &[&str]) -> Result<SettingValue, String> {
    let word = text.trim().to_lowercase();
    if allowed.contains(&word.as_str()) {
        Ok(SettingValue::Text(word))
    } else {
        Err(format!(
            "unknown {what} '{text}'; allowed values are: {}",
            allowed.join(", ")
        ))
    }
}

/// A duration in seconds, checked against what its setting can live with.
///
/// A refresh interval of zero would have the interface re-read the daemon on
/// every redraw; `none` is the way to turn the periodic read off.
fn duration_value(setting: SettingKey, seconds: u64) -> Result<SettingValue, String> {
    if setting == SettingKey::TuiRefreshInterval && seconds == 0 {
        return Err(format!(
            "the refresh interval must be at least one second; or '{NO_LIMIT}' to turn \
             the periodic refresh off"
        ));
    }
    Ok(SettingValue::Duration(Some(seconds)))
}

/// The value as text, or a complaint naming what was found instead.
fn text_of(raw: &serde_yaml::Value) -> Result<String, String> {
    raw.as_str()
        .map(ToString::to_string)
        .ok_or_else(|| format!("a text value was expected, but {} was found", describe(raw)))
}

/// A word for what a YAML value is, for a message about the wrong shape.
fn describe(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::Null => "nothing".to_string(),
        serde_yaml::Value::Bool(_) => "a true/false value".to_string(),
        serde_yaml::Value::Number(_) => "a number".to_string(),
        serde_yaml::Value::String(text) => format!("the text '{text}'"),
        serde_yaml::Value::Sequence(_) => "a list".to_string(),
        serde_yaml::Value::Mapping(_) => "a mapping".to_string(),
        serde_yaml::Value::Tagged(_) => "a tagged value".to_string(),
    }
}
