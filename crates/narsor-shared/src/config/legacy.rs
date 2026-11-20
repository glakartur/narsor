//! Names of the flat layout an un-migrated configuration still uses (R26).
//!
//! The tree has no transition: a flat key at the top of the file is an
//! unknown root and an old `NARSOR_<FLAT_KEY>` variable is not a setting, so
//! both are ignored (AE4, AE7). Ignored silently, an un-migrated
//! installation would run on built-in values (trusted mode, observer off)
//! with nothing to say why. The server and `narsor-web` therefore warn at
//! start — a warning, never a refusal — naming every old name they found.
//!
//! The old names come from [`SettingKey::former_key`] and
//! [`REMOVED_FLAT_KEYS`], the one list of them.

use std::fmt;
use std::path::Path;

use super::file::MAX_FILE_BYTES;
use super::resolve::{Environment, Resolved};
use super::settings::{SettingKey, ENV_PREFIX, REMOVED_FLAT_KEYS};

/// Where an old name was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldNamePlace {
    /// A top-level key of the configuration file.
    File,
    /// An environment variable.
    Environment,
}

/// One old name in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldName {
    /// The key or the variable, as written.
    pub name: String,
    /// Where it was found.
    pub place: OldNamePlace,
    /// The setting that replaces it, `None` for a removed key.
    pub replacement: Option<SettingKey>,
}

impl fmt::Display for OldName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let now = match (&self.place, self.replacement) {
            (_, None) => "removed".to_string(),
            (OldNamePlace::File, Some(setting)) => format!("now {}", setting.key()),
            (OldNamePlace::Environment, Some(setting)) => format!("now {}", setting.env_var()),
        };
        write!(formatter, "{} ({now})", self.name)
    }
}

/// The old flat keys with the setting that replaces each, if any.
fn flat_keys() -> impl Iterator<Item = (&'static str, Option<SettingKey>)> {
    SettingKey::ALL
        .into_iter()
        .filter_map(|setting| setting.former_key().map(|key| (key, Some(setting))))
        .chain(REMOVED_FLAT_KEYS.into_iter().map(|key| (key, None)))
}

/// The variable an old flat key was read from: `NARSOR_` plus the key in
/// capitals (`server_mode` was `NARSOR_SERVER_MODE`).
fn flat_variable(key: &str) -> String {
    format!("{ENV_PREFIX}{}", key.to_uppercase())
}

/// Every old name among these top-level keys of the file and these
/// variables, file keys first, each group in the order of the old list.
pub fn old_names(top_level_keys: &[String], environment: &Environment) -> Vec<OldName> {
    let in_file = flat_keys()
        .filter(|(key, _)| top_level_keys.iter().any(|written| written == key))
        .map(|(key, replacement)| OldName {
            name: key.to_string(),
            place: OldNamePlace::File,
            replacement,
        });
    let in_environment = flat_keys()
        .map(|(key, replacement)| (flat_variable(key), replacement))
        .filter(|(variable, _)| environment.contains_key(variable))
        .map(|(variable, replacement)| OldName {
            name: variable,
            place: OldNamePlace::Environment,
            replacement,
        });
    in_file.chain(in_environment).collect()
}

/// The top-level keys of the file at `path`, best effort: a file that
/// cannot be read or parsed has none (resolution reports why).
pub fn top_level_keys(path: &Path) -> Vec<String> {
    let too_large = std::fs::metadata(path).map_or(true, |meta| meta.len() > MAX_FILE_BYTES);
    if too_large {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(serde_yaml::Value::Mapping(mapping)) =
        serde_yaml::from_str(text.trim_start_matches('\u{feff}'))
    else {
        return Vec::new();
    };
    mapping
        .keys()
        .filter_map(|key| key.as_str().map(str::to_string))
        .collect()
}

/// The startup warning about old names in the winning file and in
/// `environment`, `None` when there are none.
pub fn warning(resolved: &Resolved, environment: &Environment) -> Option<String> {
    let keys = resolved
        .file()
        .path()
        .map(top_level_keys)
        .unwrap_or_default();
    let found = old_names(&keys, environment);
    if found.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let in_file: Vec<String> = found
        .iter()
        .filter(|name| name.place == OldNamePlace::File)
        .map(ToString::to_string)
        .collect();
    if let (false, Some(path)) = (in_file.is_empty(), resolved.file().path()) {
        parts.push(format!("in {}: {}", path.display(), in_file.join(", ")));
    }
    let in_environment: Vec<String> = found
        .iter()
        .filter(|name| name.place == OldNamePlace::Environment)
        .map(ToString::to_string)
        .collect();
    if !in_environment.is_empty() {
        parts.push(format!("in the environment: {}", in_environment.join(", ")));
    }
    Some(format!(
        "the configuration uses names of the old flat layout, which are ignored ({}); \
         write them as the tree of sections",
        parts.join("; ")
    ))
}
