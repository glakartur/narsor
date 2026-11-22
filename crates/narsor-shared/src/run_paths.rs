//! The rendezvous directory `{state}/run/` (R7, KTD6): the only files that
//! clients and `narsor-server` share.
//!
//! ```text
//! {state}/run/                 0700
//!     daemon.json              narsor-server writes; clients read (and delete when stale)
//!     daemon.lock (+ .guard)   clients write and read (autostart)
//!     narsor-server.log        a client creates it for autostart; the server
//!                              writes it through the inherited descriptor;
//!                              the client reads its tail for error text
//! ```
//!
//! Only `narsor-server` and the client crates construct [`RunPaths`] (KTD9).
//! Both go through [`RunPaths::prepare`] before they write here, so the
//! directory is checked the same way on each side.

use std::fmt;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// The rendezvous directory's name below the state root.
pub const RUN_DIR: &str = "run";

/// The only permissions the rendezvous directory may keep.
const RUN_DIR_MODE: u32 = 0o700;

/// Every path of the rendezvous directory, built from the state root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    dir: PathBuf,
}

impl RunPaths {
    /// The rendezvous directory below the state root `state_root`.
    pub fn new(state_root: impl AsRef<Path>) -> Self {
        Self {
            dir: state_root.as_ref().join(RUN_DIR),
        }
    }

    /// `{state}/run`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `daemon.json`: the discovery file of the running server.
    pub fn daemon_file(&self) -> PathBuf {
        self.dir.join("daemon.json")
    }

    /// `daemon.lock`: held by the one client that starts a server.
    pub fn daemon_lock_file(&self) -> PathBuf {
        self.dir.join("daemon.lock")
    }

    /// `daemon.lock.guard`: serializes taking and reaping `daemon.lock`.
    pub fn daemon_lock_guard_file(&self) -> PathBuf {
        self.dir.join("daemon.lock.guard")
    }

    /// `narsor-server.log`: the output of a server a client started.
    pub fn server_log_file(&self) -> PathBuf {
        self.dir.join("narsor-server.log")
    }

    /// Makes sure the directory exists, belongs to this user, is not a
    /// symbolic link and has mode `0700`.
    ///
    /// A missing directory is created `0700` (the state root too, when it
    /// is missing). One of this user with wider permissions is tightened.
    /// A symbolic link, or a directory of another user, is refused: the
    /// discovery file and its token must never be written where somebody
    /// else can read or replace them.
    pub fn prepare(&self) -> Result<(), RunDirError> {
        if let Some(parent) = self.dir.parent() {
            std::fs::create_dir_all(parent).map_err(|error| self.io(error))?;
        }
        match std::fs::DirBuilder::new()
            .mode(RUN_DIR_MODE)
            .create(&self.dir)
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(self.io(error)),
        }
        let metadata = std::fs::symlink_metadata(&self.dir).map_err(|error| self.io(error))?;
        let found = Found {
            is_symlink: metadata.file_type().is_symlink(),
            is_dir: metadata.is_dir(),
            owner: metadata.uid(),
            mode: metadata.mode(),
        };
        match verdict(&found, current_uid()) {
            Verdict::Keep => Ok(()),
            Verdict::Tighten => {
                std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(RUN_DIR_MODE))
                    .map_err(|error| self.io(error))
            }
            Verdict::Refuse(reason) => Err(RunDirError {
                dir: self.dir.clone(),
                problem: Problem::Refused(reason),
            }),
        }
    }

    fn io(&self, error: std::io::Error) -> RunDirError {
        RunDirError {
            dir: self.dir.clone(),
            problem: Problem::Io(error),
        }
    }
}

/// The rendezvous directory cannot be used.
#[derive(Debug)]
pub struct RunDirError {
    dir: PathBuf,
    problem: Problem,
}

#[derive(Debug)]
enum Problem {
    Io(std::io::Error),
    Refused(Refusal),
}

/// Why an existing rendezvous directory is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Symlink,
    NotADirectory,
    OtherOwner(u32),
}

impl fmt::Display for RunDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = self.dir.display();
        match &self.problem {
            Problem::Io(error) => write!(f, "cannot prepare the directory '{dir}': {error}"),
            Problem::Refused(Refusal::Symlink) => write!(
                f,
                "refusing '{dir}': it is a symbolic link; remove it so narsor can create \
                 the directory itself"
            ),
            Problem::Refused(Refusal::NotADirectory) => write!(
                f,
                "refusing '{dir}': it is not a directory; remove it so narsor can create \
                 the directory itself"
            ),
            Problem::Refused(Refusal::OtherOwner(uid)) => write!(
                f,
                "refusing '{dir}': it belongs to another user (uid {uid}); it must belong \
                 to you"
            ),
        }
    }
}

impl std::error::Error for RunDirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.problem {
            Problem::Io(error) => Some(error),
            Problem::Refused(_) => None,
        }
    }
}

/// What [`RunPaths::prepare`] found at the directory's path.
struct Found {
    is_symlink: bool,
    is_dir: bool,
    owner: u32,
    mode: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Keep,
    Tighten,
    Refuse(Refusal),
}

fn verdict(found: &Found, me: u32) -> Verdict {
    if found.is_symlink {
        return Verdict::Refuse(Refusal::Symlink);
    }
    if !found.is_dir {
        return Verdict::Refuse(Refusal::NotADirectory);
    }
    if found.owner != me {
        return Verdict::Refuse(Refusal::OtherOwner(found.owner));
    }
    if found.mode & 0o777 != RUN_DIR_MODE {
        return Verdict::Tighten;
    }
    Verdict::Keep
}

fn current_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "narsor-run-paths-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().mode() & 0o777
    }

    /// A missing run directory is created, and one this user left wider
    /// is tightened: either way only the owner may enter it.
    #[test]
    fn the_run_dir_is_owner_only_whether_missing_or_left_wider() {
        for existing_mode in [None, Some(0o755)] {
            let root = TempRoot::new(&format!("prepare-{existing_mode:?}"));
            let run = RunPaths::new(root.0.join("state"));
            if let Some(mode) = existing_mode {
                std::fs::create_dir_all(run.dir()).unwrap();
                std::fs::set_permissions(run.dir(), std::fs::Permissions::from_mode(mode)).unwrap();
            }

            run.prepare().expect("prepare the run directory");

            assert_eq!(mode_of(run.dir()), 0o700, "{existing_mode:?}");
            assert_eq!(run.daemon_file(), root.0.join("state/run/daemon.json"));
        }
    }

    #[test]
    fn refuses_a_symlinked_run_dir_readably() {
        let root = TempRoot::new("symlink");
        let elsewhere = root.0.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.0.join(RUN_DIR)).unwrap();

        let error = RunPaths::new(&root.0)
            .prepare()
            .expect_err("a symlink is refused");

        let message = error.to_string();
        assert!(message.contains("symbolic link"), "{message}");
        assert!(message.contains(&root.0.join(RUN_DIR).display().to_string()));
    }

    #[test]
    fn refuses_a_run_dir_of_another_user() {
        let found = Found {
            is_symlink: false,
            is_dir: true,
            owner: 1001,
            mode: 0o700,
        };

        assert_eq!(
            verdict(&found, 1000),
            Verdict::Refuse(Refusal::OtherOwner(1001))
        );
    }
}
