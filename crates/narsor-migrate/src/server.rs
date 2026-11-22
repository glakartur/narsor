//! The server's half of the one-time move to the per-binary layout (R10,
//! KTD10).
//!
//! Before every binary had a directory of its own, the server kept its
//! files directly in the state root, which by default was `global.root`
//! itself, next to the catalogue. [`move_entries`] moves them, by allowlist
//! only ([`OLD_SERVER_ENTRIES`]), to `state/narsor-server/`;
//! [`delete_stale_files`] deletes the discovery and autostart files of the
//! old daemon, its instance lock and the old web session files. The old
//! `executions/`, `launch-specs/` and `observers/` stay where they are for
//! the operator to delete ([`LEFT_IN_PLACE`]).

use std::fs;
use std::io;
use std::path::PathBuf;

use narsor_shared::config::old_layout::{OldRoots, OLD_SERVER_ENTRIES};
use narsor_store::paths::ServerStatePaths;

use crate::mover::{Moved, Mover};

/// Files of the old daemon, its clients and the old web interface that a
/// stopped installation does not need: deleted, never moved. The web
/// interface writes a new session at every start.
pub const STALE_FILES: [&str; 6] = [
    "daemon.json",
    "daemon.lock",
    "daemon.lock.guard",
    "daemon.instance.lock",
    "web-session",
    "web-open.html",
];

/// The old instance lock, held by a running old daemon.
pub const OLD_INSTANCE_LOCK: &str = "daemon.instance.lock";

/// Records of removed features: neither read nor moved, left for the
/// operator to delete, but kept in the backup.
pub const LEFT_IN_PLACE: [&str; 3] = ["executions", "launch-specs", "observers"];

/// Moves the server's old entries from the old state root to `paths`, one
/// by one; returns the names that moved. A failure names the entry.
pub fn move_entries(
    old: &OldRoots,
    paths: &ServerStatePaths,
    mover: Mover,
) -> Result<Vec<String>, String> {
    let mut moved = Vec::new();
    for name in OLD_SERVER_ENTRIES {
        let source = old.state.join(name);
        let target = paths.dir().join(name);
        match mover.move_entry(&source, &target) {
            Ok(Moved::Moved) => moved.push(name.to_string()),
            Ok(_) => {}
            Err(error) => return Err(format!("the move to the new layout stopped: {error}")),
        }
    }
    Ok(moved)
}

/// Deletes the [`STALE_FILES`] at the old state root; returns those
/// deleted. One that cannot be deleted is left with a warning.
pub fn delete_stale_files(old: &OldRoots) -> Vec<PathBuf> {
    let mut deleted = Vec::new();
    for name in STALE_FILES {
        let path = old.state.join(name);
        match fs::remove_file(&path) {
            Ok(()) => deleted.push(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "migration: cannot delete an old file; leaving it");
            }
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Temp;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    /// An old installation with the default layout: one directory with
    /// the catalogue and the server's files.
    fn old_installation(root: &Path) -> OldRoots {
        for dir in ["workflows", "auth", "orchestration/tasks"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("auth/owner.json"), "{}").unwrap();
        OldRoots {
            state: root.to_path_buf(),
            work: root.to_path_buf(),
        }
    }

    #[test]
    fn a_failed_move_stops_naming_the_entry_and_a_repeat_finishes() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the permissions this test relies on
        }
        let temp = Temp::new();
        let old = old_installation(&temp.0);
        let paths = ServerStatePaths::new(temp.0.join("state"));
        // The new directory cannot take entries.
        fs::create_dir_all(paths.dir()).unwrap();
        fs::set_permissions(paths.dir(), fs::Permissions::from_mode(0o500)).unwrap();

        let error = move_entries(&old, &paths, Mover::new()).unwrap_err();

        fs::set_permissions(paths.dir(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            error.contains(&*temp.0.join("auth").to_string_lossy()),
            "{error}"
        );
        // Once the cause is gone, the next run finishes.
        move_entries(&old, &paths, Mover::new()).unwrap();
        assert!(paths.owner_file().is_file());
        assert!(temp.0.join("workflows").is_dir(), "catalogue untouched");
    }
}
