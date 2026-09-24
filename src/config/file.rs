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
use std::path::{Path, PathBuf};

use crate::engine::HarnessRegistry;
use crate::workflow::schema::parse_duration;

use super::settings::{SettingKey, SettingValue, LOG_LEVELS, NO_LIMIT};

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
    #[error("Configuration file '{path}' sets unknown key '{key}'; known keys are: {}.", known.join(", "))]
    UnknownKey {
        /// The file.
        path: PathBuf,
        /// The key as written.
        key: String,
        /// Every key that would have been accepted.
        known: Vec<String>,
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
            | FileError::BadValue { path, .. } => path,
        }
    }

    /// The key the message is about, for the errors that have one.
    ///
    /// This is what lets `config show` mark the offending row rather than
    /// printing the complaint beside the table.
    pub fn key(&self) -> Option<&str> {
        match self {
            FileError::UnknownKey { key, .. } | FileError::BadValue { key, .. } => Some(key),
            _ => None,
        }
    }
}

/// Reads and parses one configuration file.
pub fn load(path: &Path) -> Result<FileValues, Vec<FileError>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) => {
            return Err(vec![FileError::Read {
                path: path.to_path_buf(),
                source,
            }])
        }
    };

    // Checked before the read, so that an oversized file is never held in
    // memory even once.
    if metadata.len() > MAX_FILE_BYTES {
        return Err(vec![FileError::TooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            limit: MAX_FILE_BYTES,
        }]);
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) => {
            return Err(vec![FileError::Read {
                path: path.to_path_buf(),
                source,
            }])
        }
    };

    let contents = match String::from_utf8(bytes) {
        Ok(contents) => contents,
        Err(_) => {
            return Err(vec![FileError::NotUtf8 {
                path: path.to_path_buf(),
            }])
        }
    };

    parse(path, &contents)
}

/// Parses the text of a configuration file.
///
/// Separate from [`load`] so that the two phases can be exercised without a
/// filesystem; `path` is carried only to name the file in the messages.
pub fn parse(path: &Path, contents: &str) -> Result<FileValues, Vec<FileError>> {
    let contents = contents.strip_prefix(BOM).unwrap_or(contents);

    // Phase one: no types, no rejections — just the document.
    let document: serde_yaml::Value = match serde_yaml::from_str(contents) {
        Ok(document) => document,
        Err(source) => {
            return Err(vec![FileError::Syntax {
                path: path.to_path_buf(),
                source,
            }])
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
            return Err(vec![FileError::NotAMapping {
                path: path.to_path_buf(),
                found: describe(&other),
            }])
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
                known: known_keys(),
            });
            continue;
        };

        let Some(setting) = SettingKey::from_key(&key) else {
            errors.push(FileError::UnknownKey {
                path: path.to_path_buf(),
                key,
                known: known_keys(),
            });
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

    if errors.is_empty() {
        Ok(FileValues { values })
    } else {
        // Sorted by key so that the same file always produces the same report,
        // whatever order the document happened to use.
        errors.sort_by(|left, right| {
            left.key()
                .unwrap_or_default()
                .cmp(right.key().unwrap_or_default())
        });
        Err(errors)
    }
}

/// Every key the file may set, in registry order.
fn known_keys() -> Vec<String> {
    SettingKey::ALL
        .iter()
        .map(|key| key.key().to_string())
        .collect()
}

/// Validates one value against the setting it was written under.
///
/// The `Err` is the sentence that goes after "key 'x': ", so it names what was
/// expected rather than only what was found.
fn value_of(setting: SettingKey, raw: &serde_yaml::Value) -> Result<SettingValue, String> {
    match setting {
        SettingKey::DataDir => {
            let text = text_of(raw)?;
            if text.trim().is_empty() {
                return Err("a directory path was expected, but the value is empty".to_string());
            }
            Ok(SettingValue::Path(PathBuf::from(text)))
        }

        SettingKey::AgentHarness => {
            let text = text_of(raw)?;
            let supported = HarnessRegistry::defaults()
                .supported()
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if supported.iter().any(|name| name == &text) {
                Ok(SettingValue::Text(text))
            } else {
                Err(format!(
                    "unknown agent harness '{text}'; supported harnesses are: {}",
                    supported.join(", ")
                ))
            }
        }

        SettingKey::AgentTimeout => {
            // Null and the spelled-out word both mean "no limit", so a value
            // inherited from /etc/narsor/ can be revoked and not only replaced.
            if raw.is_null() {
                return Ok(SettingValue::Duration(None));
            }
            if let Some(seconds) = raw.as_u64() {
                return Ok(SettingValue::Duration(Some(seconds)));
            }
            let text = text_of(raw)?;
            if text.trim().eq_ignore_ascii_case(NO_LIMIT) {
                return Ok(SettingValue::Duration(None));
            }
            parse_duration(&text)
                .map(|seconds| SettingValue::Duration(Some(seconds)))
                .map_err(|error| format!("{error}; or '{NO_LIMIT}' for no limit at all"))
        }

        SettingKey::LogLevel => {
            let text = text_of(raw)?;
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
