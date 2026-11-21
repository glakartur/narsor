//! Moving one entry of an installation to its new place, safely and so that
//! a repeat finishes what a crash left (KTD10).
//!
//! The one-time layout migrations of `narsor-server` and
//! `narsor-controller local` move their files entry by entry with
//! [`Mover::move_entry`]:
//!
//! - Within one filesystem an entry is renamed, which is atomic.
//! - Across filesystems it is copied to a staging name beside the target
//!   (`<target>.narsor-moving`), the copy is compared with the source, the
//!   original modes are applied again (a `0600` credential stays `0600`), the
//!   source is renamed aside (`<source>.narsor-moved`), the copy takes the
//!   target name and the source is deleted last.
//! - A source that is a symbolic link is refused: the migration moves the
//!   installation's own files, never what a link points at.
//!
//! Every step leaves a state a repeat recognises: source gone and target
//! present is done; the source renamed aside means the copy was complete;
//! a staging copy beside an untouched source is incomplete and made again.
//! Source and target both present is a conflict the caller must not
//! settle by guessing, so it is refused.
//!
//! The migration of each binary ends with its marker ([`write_marker`]):
//! a binary that finds its marker does not migrate again.

use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// The file each binary writes, last, into its new state directory once its
/// migration is complete.
pub const MARKER_FILE: &str = "layout-migrated";

/// Suffix of the copy being made across filesystems, beside the target.
const STAGING_SUFFIX: &str = ".narsor-moving";

/// Suffix of a source whose copy is complete, until it is deleted.
pub const MOVED_SUFFIX: &str = ".narsor-moved";

/// Why an entry could not be moved: names the entry and what failed.
#[derive(Debug)]
pub struct MoveError {
    /// The entry that could not be moved.
    pub source: PathBuf,
    /// Where it was to go.
    pub target: PathBuf,
    /// What went wrong.
    pub reason: String,
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot move {} to {}: {}",
            self.source.display(),
            self.target.display(),
            self.reason
        )
    }
}

impl std::error::Error for MoveError {}

/// What [`Mover::move_entry`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    /// The entry is now at its target.
    Moved,
    /// An earlier run had moved it already.
    AlreadyThere,
    /// There was nothing to move.
    Absent,
}

/// Moves entries by rename, or by copy across filesystems.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mover {
    always_copy: bool,
}

impl Mover {
    /// Renames where it can and copies across filesystems.
    pub fn new() -> Self {
        Self::default()
    }

    /// Always copies, as across filesystems: the seam for tests on one
    /// filesystem.
    pub fn copying() -> Self {
        Self { always_copy: true }
    }

    /// Moves `source` (a file or a directory tree) to `target`, creating
    /// the target's parent (`0700`) when it is missing.
    pub fn move_entry(&self, source: &Path, target: &Path) -> Result<Moved, MoveError> {
        let fail = |reason: String| MoveError {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            reason,
        };
        let aside = with_suffix(source, MOVED_SUFFIX);
        let staging = with_suffix(target, STAGING_SUFFIX);

        // A copy that was complete when its source was renamed aside.
        if exists(&aside).map_err(|error| fail(error.to_string()))? {
            if !exists(target).map_err(|error| fail(error.to_string()))? {
                fs::rename(&staging, target).map_err(|error| {
                    fail(format!(
                        "cannot finish the copy {}: {error}",
                        staging.display()
                    ))
                })?;
            }
            remove_any(&aside).map_err(|error| {
                fail(format!(
                    "cannot delete the copied source {}: {error}",
                    aside.display()
                ))
            })?;
            return Ok(Moved::Moved);
        }

        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(
                    if exists(target).map_err(|error| fail(error.to_string()))? {
                        Moved::AlreadyThere
                    } else {
                        Moved::Absent
                    },
                );
            }
            Err(error) => return Err(fail(error.to_string())),
        };
        if metadata.file_type().is_symlink() {
            return Err(fail(
                "it is a symbolic link; move what it points at by hand and start again".into(),
            ));
        }
        if exists(target).map_err(|error| fail(error.to_string()))? {
            return Err(fail(
                "the target exists too; keep the one you want, remove the other and start again"
                    .into(),
            ));
        }
        if let Some(parent) = target.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|error| fail(format!("cannot create {}: {error}", parent.display())))?;
        }
        // A copy beside a source still in place never completed.
        if exists(&staging).map_err(|error| fail(error.to_string()))? {
            remove_any(&staging).map_err(|error| {
                fail(format!(
                    "cannot delete the incomplete copy {}: {error}",
                    staging.display()
                ))
            })?;
        }
        if !self.always_copy {
            match fs::rename(source, target) {
                Ok(()) => return Ok(Moved::Moved),
                Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
                Err(error) => return Err(fail(error.to_string())),
            }
        }
        // Across filesystems: copy, compare, then swap.
        copy_tree(source, &staging).map_err(|error| fail(format!("copy failed: {error}")))?;
        same_tree(source, &staging)
            .map_err(|error| fail(format!("the copy differs from the source: {error}")))?;
        fs::rename(source, &aside).map_err(|error| fail(error.to_string()))?;
        fs::rename(&staging, target).map_err(|error| fail(error.to_string()))?;
        remove_any(&aside).map_err(|error| {
            fail(format!(
                "cannot delete the copied source {}: {error}",
                aside.display()
            ))
        })?;
        Ok(Moved::Moved)
    }
}

/// Whether `dir` holds the migration marker.
pub fn marker_present(dir: &Path) -> bool {
    dir.join(MARKER_FILE).is_file()
}

/// Writes the migration marker into `dir` (created `0700` when missing),
/// durably: the last step of a migration.
pub fn write_marker(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let target = dir.join(MARKER_FILE);
    let temporary = with_suffix(&target, ".tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(b"1\n")?;
    file.sync_all()?;
    fs::rename(&temporary, &target)?;
    fs::File::open(dir)?.sync_all()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether anything (a link included) is at `path`.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Copies `source` to `target`: files with their contents, links as links,
/// directories recursively, then every original mode applied again (a
/// directory's last, so a read-only one can still be filled).
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let file_type = metadata.file_type();
    let mode = metadata.permissions().mode() & 0o7777;
    if file_type.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)
    } else if file_type.is_dir() {
        fs::DirBuilder::new().mode(0o700).create(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        fs::set_permissions(target, fs::Permissions::from_mode(mode))
    } else if file_type.is_file() {
        let mut from = fs::File::open(source)?;
        let mut to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        fs::set_permissions(target, fs::Permissions::from_mode(mode))
    } else {
        Err(io::Error::other(format!(
            "{} is neither a file, a directory nor a link",
            source.display()
        )))
    }
}

/// Checks that `copy` holds what `source` holds: the same entries, the
/// same bytes, the same links and the same modes.
fn same_tree(source: &Path, copy: &Path) -> Result<(), String> {
    let differs = |what: &str| format!("{} ({what})", copy.display());
    let left = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    let right = fs::symlink_metadata(copy).map_err(|error| error.to_string())?;
    if left.file_type() != right.file_type() {
        return Err(differs("type"));
    }
    if !left.file_type().is_symlink()
        && left.permissions().mode() & 0o7777 != right.permissions().mode() & 0o7777
    {
        return Err(differs("mode"));
    }
    if left.file_type().is_symlink() {
        let targets = (fs::read_link(source), fs::read_link(copy));
        return match targets {
            (Ok(left), Ok(right)) if left == right => Ok(()),
            _ => Err(differs("link")),
        };
    }
    if left.is_dir() {
        let names = |dir: &Path| -> Result<Vec<std::ffi::OsString>, String> {
            let mut names = fs::read_dir(dir)
                .map_err(|error| error.to_string())?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            names.sort();
            Ok(names)
        };
        let entries = names(source)?;
        if entries != names(copy)? {
            return Err(differs("entries"));
        }
        for name in entries {
            same_tree(&source.join(&name), &copy.join(&name))?;
        }
        return Ok(());
    }
    if left.len() != right.len() || !same_bytes(source, copy).map_err(|e| e.to_string())? {
        return Err(differs("contents"));
    }
    Ok(())
}

fn same_bytes(left: &Path, right: &Path) -> io::Result<bool> {
    let (mut left, mut right) = (fs::File::open(left)?, fs::File::open(right)?);
    let (mut a, mut b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let read = left.read(&mut a)?;
        if read == 0 {
            return Ok(right.read(&mut b)? == 0);
        }
        let mut filled = 0;
        while filled < read {
            let more = right.read(&mut b[filled..read])?;
            if more == 0 {
                return Ok(false);
            }
            filled += more;
        }
        if a[..read] != b[..read] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-layout-move-{}-{}",
                std::process::id(),
                uuid_like()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn uuid_like() -> u128 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        now + u128::from(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn private_tree(root: &Path) -> PathBuf {
        let source = root.join("old").join("secrets");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("credential"), "token").unwrap();
        fs::set_permissions(source.join("credential"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(source.join("nested/data"), "data").unwrap();
        std::os::unix::fs::symlink("credential", source.join("link")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        source
    }

    /// Both ways keep contents, links and the private modes.
    #[test]
    fn a_tree_keeps_its_contents_and_modes_by_rename_and_by_copy() {
        for mover in [Mover::new(), Mover::copying()] {
            let temp = Temp::new();
            let source = private_tree(&temp.0);
            let target = temp.0.join("new").join("secrets");

            assert_eq!(mover.move_entry(&source, &target).unwrap(), Moved::Moved);

            assert!(!source.exists());
            assert_eq!(
                fs::read_to_string(target.join("credential")).unwrap(),
                "token"
            );
            assert_eq!(
                fs::read_to_string(target.join("nested/data")).unwrap(),
                "data"
            );
            assert_eq!(
                fs::read_link(target.join("link")).unwrap(),
                Path::new("credential")
            );
            let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o777;
            assert_eq!(mode(&target.join("credential")), 0o600, "{mover:?}");
            assert_eq!(mode(&target), 0o700, "{mover:?}");
            assert!(!with_suffix(&target, STAGING_SUFFIX).exists());
            assert!(!with_suffix(&source, MOVED_SUFFIX).exists());
            assert_eq!(
                mover.move_entry(&source, &target).unwrap(),
                Moved::AlreadyThere
            );
        }
    }

    #[test]
    fn a_symbolic_link_as_the_source_is_refused() {
        let temp = Temp::new();
        let real = private_tree(&temp.0);
        let source = temp.0.join("old").join("linked");
        std::os::unix::fs::symlink(&real, &source).unwrap();

        let error = Mover::new()
            .move_entry(&source, &temp.0.join("new").join("linked"))
            .unwrap_err();

        assert_eq!(error.source, source);
        assert!(error.to_string().contains("symbolic link"), "{error}");
        assert!(real.join("credential").exists());
    }

    /// A crash at each point of the copy path is finished by a repeat.
    #[test]
    fn a_repeat_finishes_an_interrupted_copy() {
        enum Crash {
            WhileCopying,
            AfterSourceAside,
        }
        for crash in [Crash::WhileCopying, Crash::AfterSourceAside] {
            let temp = Temp::new();
            let source = private_tree(&temp.0);
            let target = temp.0.join("new").join("secrets");
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            let staging = with_suffix(&target, STAGING_SUFFIX);
            match crash {
                Crash::WhileCopying => {
                    fs::create_dir_all(&staging).unwrap();
                    fs::write(staging.join("credential"), "tok").unwrap();
                }
                Crash::AfterSourceAside => {
                    copy_tree(&source, &staging).unwrap();
                    fs::rename(&source, with_suffix(&source, MOVED_SUFFIX)).unwrap();
                }
            }

            Mover::copying().move_entry(&source, &target).unwrap();

            assert_eq!(
                fs::read_to_string(target.join("credential")).unwrap(),
                "token"
            );
            assert!(!staging.exists());
            assert!(!source.exists());
            assert!(!with_suffix(&source, MOVED_SUFFIX).exists());
        }
    }

    #[test]
    fn source_and_target_both_present_is_refused() {
        let temp = Temp::new();
        let source = private_tree(&temp.0);
        let target = temp.0.join("new").join("secrets");
        fs::create_dir_all(&target).unwrap();

        let error = Mover::new().move_entry(&source, &target).unwrap_err();

        assert!(error.to_string().contains("target exists"), "{error}");
        assert!(source.join("credential").exists());
    }
}
