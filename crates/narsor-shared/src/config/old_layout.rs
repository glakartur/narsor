//! Where an installation kept its files before every binary got a directory
//! of its own below the state and work roots (KTD6, KTD10).
//!
//! The rule for the roots themselves did not change: an explicit relative
//! `server.state` / `server.work` is joined to `global.root`, an absolute one
//! is used as it is. Only the built-in value changed, from `.` (the
//! `global.root` itself, which also holds the catalogue) to `state` and
//! `work`. So the old root is the resolved one when the setting was given
//! explicitly (flag, environment, file), and `global.root` when it took the
//! built-in value.
//!
//! The one-time layout migration (`narsor-migrate`) moves what it finds
//! there; `narsor-server` only looks whether anything is left to move
//! ([`OldRoots::unmigrated_entries`]) and refuses to start until it is moved.

use std::path::{Path, PathBuf};

use super::resolve::Resolved;
use super::settings::{SettingKey, Source};

/// The roots an installation used before the per-binary layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldRoots {
    /// The old state root: `auth/`, `orchestration/`, `daemon.json`,
    /// `local-controller/` (credential side), …
    pub state: PathBuf,
    /// The old work root: `local-controller/` (storage side).
    pub work: PathBuf,
}

/// The old directory of the local controller below an old root.
pub const OLD_LOCAL_CONTROLLER_DIR: &str = "local-controller";

/// The server's entries of the old state root; each keeps its name in
/// `state/narsor-server/`. `narsor.log` is the old log: kept, nobody
/// writes it any more.
pub const OLD_SERVER_ENTRIES: [&str; 6] = [
    "auth",
    "orchestration",
    "materials",
    "observer-queues",
    "agent-harness.json",
    "narsor.log",
];

/// The file the migration writes, last, into a binary's new state directory
/// once that binary's files are moved.
pub const LAYOUT_MARKER_FILE: &str = "layout-migrated";

/// The old state root of the installation `config` resolves; needs only
/// `server.state` in the scope (the web interface has no `server.work`).
pub fn old_state_root(config: &Resolved) -> PathBuf {
    old_root(config, SettingKey::StateDir, config.state_dir())
}

fn old_root(config: &Resolved, setting: SettingKey, resolved: &Path) -> PathBuf {
    match config.source(setting) {
        Source::BuiltIn => config.root().to_path_buf(),
        _ => resolved.to_path_buf(),
    }
}

impl OldRoots {
    /// The old roots of the installation `config` resolves.
    pub fn of(config: &Resolved) -> Self {
        Self {
            state: old_state_root(config),
            work: old_root(config, SettingKey::WorkDir, config.work_dir()),
        }
    }

    /// `{old state}/local-controller`: the old controller directory with
    /// its credential, token store and image entries.
    pub fn local_controller_state(&self) -> PathBuf {
        self.state.join(OLD_LOCAL_CONTROLLER_DIR)
    }

    /// `{old work}/local-controller`: the old controller directory with
    /// task storages, workloads, readers and per-task records. The same
    /// directory as [`Self::local_controller_state`] unless the roots were
    /// configured apart.
    pub fn local_controller_work(&self) -> PathBuf {
        self.work.join(OLD_LOCAL_CONTROLLER_DIR)
    }

    /// The entries of the old layout still at the old roots: the server's
    /// ([`OLD_SERVER_ENTRIES`]) and the local controller's old directories.
    /// Empty for a fresh installation and for one already moved.
    pub fn unmigrated_entries(&self) -> Vec<PathBuf> {
        OLD_SERVER_ENTRIES
            .iter()
            .map(|name| self.state.join(name))
            .chain([self.local_controller_state(), self.local_controller_work()])
            .filter(|path| path.symlink_metadata().is_ok())
            .fold(Vec::new(), |mut found, path| {
                if !found.contains(&path) {
                    found.push(path);
                }
                found
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::resolve::{resolve_with, Environment, Flags};
    use crate::config::{ConfigCascade, Scope};

    fn resolved(variables: &[(&str, &str)]) -> Resolved {
        let environment: Environment = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let cascade = ConfigCascade::with_dirs(None, None, None);
        resolve_with(&Flags::default(), &environment, &cascade, Scope::SERVER)
    }

    #[test]
    fn built_in_roots_were_the_installation_root_and_explicit_ones_stay() {
        type Case = (
            &'static [(&'static str, &'static str)],
            &'static str,
            &'static str,
        );
        let cases: [Case; 3] = [
            (&[("NARSOR_GLOBAL__ROOT", "/srv/n")], "/srv/n", "/srv/n"),
            (
                &[
                    ("NARSOR_GLOBAL__ROOT", "/srv/n"),
                    ("NARSOR_SERVER__STATE", "/var/lib/n"),
                    ("NARSOR_SERVER__WORK", "scratch"),
                ],
                "/var/lib/n",
                "/srv/n/scratch",
            ),
            // An explicit `state` is the new built-in value, but it was
            // explicit before too: it stays where it was.
            (
                &[
                    ("NARSOR_GLOBAL__ROOT", "/srv/n"),
                    ("NARSOR_SERVER__STATE", "state"),
                ],
                "/srv/n/state",
                "/srv/n",
            ),
        ];
        for (variables, state, work) in cases {
            let roots = OldRoots::of(&resolved(variables));
            assert_eq!(roots.state, Path::new(state), "{variables:?}");
            assert_eq!(roots.work, Path::new(work), "{variables:?}");
        }
    }
}
