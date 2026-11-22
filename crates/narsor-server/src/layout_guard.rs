//! The refusal to start on an installation of the old layout (D82, KTD10).
//!
//! The server moves no files: `narsor-migrate`, which the installer runs,
//! moves an installation once and writes the server's marker last. A server
//! that finds entries of the old layout at the old roots and no marker
//! refuses to start, naming the command to run; it would otherwise start as
//! a new, empty installation beside the old files. A fresh installation has
//! no old entries and starts without a marker.

use std::path::{Path, PathBuf};

use narsor_engine::persistence::ServerStatePaths;
use narsor_shared::config::old_layout::{OldRoots, LAYOUT_MARKER_FILE};
use narsor_shared::config::{Resolved, SettingKey, Source};

/// The program that moves an old installation.
const MIGRATE_PROGRAM: &str = "narsor-migrate";

/// `Err` with the message to print when `config` names an installation of
/// the old layout that `narsor-migrate` has not moved yet.
pub fn refuse_old_layout(config: &Resolved, paths: &ServerStatePaths) -> Result<(), String> {
    if paths.dir().join(LAYOUT_MARKER_FILE).is_file() {
        return Ok(());
    }
    let found = OldRoots::of(config).unmigrated_entries();
    if found.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = found
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect();
    Err(format!(
        "this installation still has files of an older Narsor in the old layout:\n{}\n\
         Move them once with the installer of this version, or run:\n  {}\n\
         (it backs them up first; add --dry-run to see the plan). Nothing was started.",
        listed.join("\n"),
        migrate_command(config)
    ))
}

/// The `narsor-migrate` command for the installation `config` resolves: the
/// program beside this one, with the flags this server was given. Settings
/// from the environment or the found configuration file are found again.
fn migrate_command(config: &Resolved) -> String {
    let program = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(MIGRATE_PROGRAM)))
        .filter(|program| program.is_file())
        .unwrap_or_else(|| PathBuf::from(MIGRATE_PROGRAM));
    let mut command = vec![quote(&program)];
    if matches!(config.source(SettingKey::GlobalRoot), Source::Flag(_)) {
        command.extend(["--root".to_string(), quote(config.root())]);
    }
    if let Some(file) = config.file().path() {
        command.extend(["--config".to_string(), quote(file)]);
    }
    command.join(" ")
}

/// `path` quoted for a POSIX shell when it needs quoting.
fn quote(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@".contains(c))
    {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}
