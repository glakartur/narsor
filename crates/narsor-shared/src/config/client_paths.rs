//! Client-side paths outside the state root. The rendezvous directory is
//! [`crate::run_paths::RunPaths`].

use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/narsor/workflows`, falling back to `$HOME/.config`.
/// Empty `XDG_CONFIG_HOME` is treated as unset, matching server resolution.
pub fn user_workflows_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("narsor").join("workflows"))
}
