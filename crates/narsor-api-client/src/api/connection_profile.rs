use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const MAX_PROFILE_BYTES: usize = 1024 * 1024;
const PROFILE_FILE: &str = "connection.yaml";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    server_url: String,
}

/// Resolves an HTTPS URL to the normalized endpoint and its trust origin.
pub fn normalize_https_endpoint(raw: &str) -> Result<(String, String), String> {
    let parsed = reqwest::Url::parse(raw.trim())
        .map_err(|error| format!("'{raw}' is not a URL: {error}"))?;
    if parsed.scheme() != "https" {
        return Err("connection profiles require an https:// server URL".to_string());
    }

    let normalized = super::client::normalize_base_url(raw)?;
    let parsed = reqwest::Url::parse(&normalized)
        .map_err(|error| format!("'{normalized}' is not a URL: {error}"))?;
    Ok((normalized, parsed.origin().ascii_serialization()))
}

/// The per-user connection profile path, or `None` when no config home exists.
pub fn default_connection_profile_path() -> Option<PathBuf> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(config_home).join("narsor").join(PROFILE_FILE));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".config/narsor")
                .join(PROFILE_FILE)
        })
}

/// Reads and validates a connection profile, returning its normalized endpoint.
pub fn read_connection_profile(path: &Path) -> Result<Option<String>, String> {
    let link = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read connection profile '{path:?}': {error}"
            ))
        }
    };
    let metadata = if link.file_type().is_symlink() {
        fs::metadata(path)
            .map_err(|error| format!("could not read connection profile '{path:?}': {error}"))?
    } else {
        link
    };
    if !metadata.is_file() {
        return Err(format!(
            "connection profile '{path:?}' is not a regular file"
        ));
    }
    if metadata.len() > MAX_PROFILE_BYTES as u64 {
        return Err(format!(
            "connection profile '{path:?}' exceeds the {MAX_PROFILE_BYTES} byte limit"
        ));
    }
    let mut contents = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take((MAX_PROFILE_BYTES + 1) as u64)
                .read_to_end(&mut contents)
        })
        .map_err(|error| format!("could not read connection profile '{path:?}': {error}"))?;
    if contents.len() > MAX_PROFILE_BYTES {
        return Err(format!(
            "connection profile '{path:?}' exceeds the {MAX_PROFILE_BYTES} byte limit"
        ));
    }
    let profile: ProfileFile = serde_yaml::from_slice(&contents)
        .map_err(|error| format!("could not parse connection profile '{path:?}': {error}"))?;
    normalize_https_endpoint(&profile.server_url)
        .map(|(endpoint, _)| Some(endpoint))
        .map_err(|error| format!("invalid server URL in connection profile '{path:?}': {error}"))
}

/// Serializes a non-secret profile containing only its validated server URL.
pub fn format_connection_profile(server_url: &str) -> Result<String, String> {
    let (server_url, _) = normalize_https_endpoint(server_url)?;
    serde_yaml::to_string(&ProfileFile { server_url })
        .map_err(|error| format!("could not serialize connection profile: {error}"))
}

/// Atomically writes a validated connection profile without touching token sources.
pub fn write_connection_profile(path: &Path, server_url: &str) -> Result<(), String> {
    let (server_url, _) = normalize_https_endpoint(server_url)?;
    let _ = read_connection_profile(path)?;
    let contents = serde_yaml::to_string(&ProfileFile { server_url })
        .map_err(|error| format!("could not serialize connection profile: {error}"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        format!("could not create connection profile directory '{parent:?}': {error}")
    })?;

    let temporary = parent.join(format!(".{PROFILE_FILE}.{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("could not create temporary connection profile: {error}"))?;
    let result = (|| {
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("could not write connection profile: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not sync connection profile: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("could not replace connection profile '{path:?}': {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
