//! Filesystem paths the client needs for its own log and daemon discovery.
//! No execution, orchestration, credential, or material storage paths are
//! exposed here.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    root: PathBuf,
}

impl ClientPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn log_file(&self) -> PathBuf {
        self.root.join("narsor.log")
    }

    pub fn daemon_file(&self) -> PathBuf {
        self.root.join("daemon.json")
    }

    pub fn daemon_lock_file(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }
}

/// `$XDG_CONFIG_HOME/narsor/workflows`, falling back to `$HOME/.config`.
/// Empty `XDG_CONFIG_HOME` is treated as unset, matching server resolution.
pub fn user_workflows_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("narsor").join("workflows"))
}
