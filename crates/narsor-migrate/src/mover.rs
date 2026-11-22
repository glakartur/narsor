//! Moving one entry of an installation to its new place, safely and so that
//! a repeat finishes what a crash left (KTD10).
//!
//! - Within one filesystem an entry is renamed, which is atomic.
//! - Across filesystems it is copied to a staging name beside the target
//!   (`<target>.narsor-moving`), the copy is compared with the source, the
//!   original modes and times are applied again (a `0600` credential stays
//!   `0600`), the source is renamed aside (`<source>.narsor-moved`), the copy
//!   takes the target name and the source is deleted last.
//! - A source that is a symbolic link is refused: the migration moves the
//!   installation's own files, never what a link points at.
//! - A socket, a FIFO or a device inside a copied tree is not copied: a
//!   warning names it. Nothing listens on it once the old programs stopped.
//! - A directory without owner write permission (a read-only checkout, a
//!   Go module cache) is made writable for its owner when the copied source
//!   or an incomplete copy must be deleted.
//!
//! Every step leaves a state a repeat recognises: source gone and target
//! present is done; the source renamed aside means the copy was complete;
//! a staging copy beside an untouched source is incomplete and made again.
//! Source and target both present is a conflict the caller must not
//! settle by guessing, so it is refused.
//!
//! The migration of each binary ends with its marker ([`write_marker`]):
//! the tool does not migrate again what a marker says is done.

use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
    PermissionsExt as _,
};
use std::path::{Path, PathBuf};

use filetime::FileTime;

pub use narsor_shared::config::old_layout::LAYOUT_MARKER_FILE as MARKER_FILE;

/// Suffix of the copy being made across filesystems, beside the target.
pub const STAGING_SUFFIX: &str = ".narsor-moving";

/// Suffix of a source whose copy is complete, until it is deleted.
pub const MOVED_SUFFIX: &str = ".narsor-moved";

/// Why an entry could not be moved: names the entry and what failed.
#[derive(Debug)]
pub struct MoveError {
    /// The entry that could not be moved.
    pub source: PathBuf,
    /// Where it was to go.
    pub target: PathBuf,
    /// What went wrong, naming the file it went wrong on.
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
        if exists(&aside).map_err(&fail)? {
            if !exists(target).map_err(&fail)? {
                fs::rename(&staging, target).map_err(|error| {
                    fail(format!(
                        "cannot finish the copy {}: {error}",
                        staging.display()
                    ))
                })?;
            }
            remove_tree(&aside)
                .map_err(|error| fail(format!("cannot delete the copied source: {error}")))?;
            return Ok(Moved::Moved);
        }

        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(if exists(target).map_err(&fail)? {
                    Moved::AlreadyThere
                } else {
                    Moved::Absent
                });
            }
            Err(error) => return Err(fail(at(source, error))),
        };
        if metadata.file_type().is_symlink() {
            return Err(fail(
                "it is a symbolic link; move what it points at by hand and run again".into(),
            ));
        }
        if exists(target).map_err(&fail)? {
            return Err(fail(
                "the target exists too; keep the one you want, remove the other and run again"
                    .into(),
            ));
        }
        if let Some(parent) = target.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|error| fail(at(parent, error)))?;
        }
        // A copy beside a source still in place never completed.
        if exists(&staging).map_err(&fail)? {
            remove_tree(&staging)
                .map_err(|error| fail(format!("cannot delete the incomplete copy: {error}")))?;
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
        fs::rename(source, &aside).map_err(|error| fail(at(source, error)))?;
        fs::rename(&staging, target).map_err(|error| fail(at(&staging, error)))?;
        remove_tree(&aside)
            .map_err(|error| fail(format!("cannot delete the copied source: {error}")))?;
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

/// `path` with `suffix` appended to its last component.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether anything (a link included) is at `path`.
fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(at(path, error)),
    }
}

/// An error message naming the file it happened on.
fn at(path: &Path, error: io::Error) -> String {
    format!("{}: {error}", path.display())
}

/// Whether `file_type` is a socket, a FIFO or a device: nothing the copy
/// can carry over.
pub fn is_special(file_type: &fs::FileType) -> bool {
    file_type.is_socket()
        || file_type.is_fifo()
        || file_type.is_char_device()
        || file_type.is_block_device()
}

/// Deletes `path` and everything below it. A directory that refuses
/// (`EACCES`/`EPERM`) is given owner read, write and search permission and
/// the step is tried again; a final failure names the file.
pub fn remove_tree(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(at(path, error)),
    };
    let parent = path.parent().unwrap_or(Path::new("/"));
    if !metadata.is_dir() {
        return retry_writable(parent, path, || fs::remove_file(path));
    }
    let entries = retry_writable(path, path, || {
        fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()
    })?;
    for entry in entries {
        remove_tree(&entry.path())?;
    }
    retry_writable(parent, path, || fs::remove_dir(path))
}

/// Runs `step` on `path`; when it is refused for permissions, gives the
/// owner of the directory `dir` full access and runs it once more.
fn retry_writable<T>(
    dir: &Path,
    path: &Path,
    step: impl Fn() -> io::Result<T>,
) -> Result<T, String> {
    match step() {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let mode = fs::symlink_metadata(dir)
                .map_err(|error| at(dir, error))?
                .permissions()
                .mode();
            fs::set_permissions(dir, fs::Permissions::from_mode((mode & 0o7777) | 0o700))
                .map_err(|error| at(dir, error))?;
            step().map_err(|error| at(path, error))
        }
        Err(error) => Err(at(path, error)),
    }
}

/// The times of `metadata` as `filetime` takes them.
fn times(metadata: &fs::Metadata) -> (FileTime, FileTime) {
    (
        FileTime::from_last_access_time(metadata),
        FileTime::from_last_modification_time(metadata),
    )
}

/// The permission bits a copy keeps: read, write and execute only.
const COPIED_MODE_BITS: u32 = 0o777;

/// Copies `source` to `target`: files with their contents, links as links,
/// directories recursively, then every original mode and access and
/// modification time applied again (a directory's last, so a read-only one
/// can still be filled and its time is not changed by the filling). A
/// socket, a FIFO or a device is skipped with a warning. An error names the
/// file.
fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|error| at(source, error))?;
    let file_type = metadata.file_type();
    // Without the set-id and sticky bits: a copy belongs to whoever runs
    // the tool, so a set-id bit kept on it would grant that user's rights.
    let mode = metadata.permissions().mode() & COPIED_MODE_BITS;
    let (atime, mtime) = times(&metadata);
    if file_type.is_symlink() {
        let link = fs::read_link(source).map_err(|error| at(source, error))?;
        std::os::unix::fs::symlink(link, target).map_err(|error| at(target, error))?;
        return filetime::set_symlink_file_times(target, atime, mtime)
            .map_err(|error| at(target, error));
    }
    if file_type.is_dir() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(target)
            .map_err(|error| at(target, error))?;
        for entry in fs::read_dir(source).map_err(|error| at(source, error))? {
            let entry = entry.map_err(|error| at(source, error))?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if file_type.is_file() {
        let mut from = fs::File::open(source).map_err(|error| at(source, error))?;
        let mut to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)
            .map_err(|error| at(target, error))?;
        io::copy(&mut from, &mut to).map_err(|error| at(source, error))?;
        to.sync_all().map_err(|error| at(target, error))?;
    } else {
        tracing::warn!(path = %source.display(), "migration: not copying a socket, FIFO or device");
        return Ok(());
    }
    fs::set_permissions(target, fs::Permissions::from_mode(mode))
        .map_err(|error| at(target, error))?;
    filetime::set_file_times(target, atime, mtime).map_err(|error| at(target, error))
}

/// Checks that `copy` holds what `source` holds: the same entries (sockets,
/// FIFOs and devices left out), the same bytes, the same links, the same
/// modes and the same modification times (to the second, which every
/// filesystem keeps).
fn same_tree(source: &Path, copy: &Path) -> Result<(), String> {
    let differs = |what: &str| format!("{} ({what})", copy.display());
    let left = fs::symlink_metadata(source).map_err(|error| at(source, error))?;
    let right = fs::symlink_metadata(copy).map_err(|error| at(copy, error))?;
    if left.file_type() != right.file_type() {
        return Err(differs("type"));
    }
    if left.file_type().is_symlink() {
        let targets = (fs::read_link(source), fs::read_link(copy));
        return match targets {
            (Ok(left), Ok(right)) if left == right => Ok(()),
            _ => Err(differs("link")),
        };
    }
    if left.permissions().mode() & COPIED_MODE_BITS != right.permissions().mode() & COPIED_MODE_BITS
    {
        return Err(differs("mode"));
    }
    if left.is_dir() {
        let names = |dir: &Path| -> Result<Vec<std::ffi::OsString>, String> {
            let mut names = Vec::new();
            for entry in fs::read_dir(dir).map_err(|error| at(dir, error))? {
                let entry = entry.map_err(|error| at(dir, error))?;
                let kind = entry
                    .file_type()
                    .map_err(|error| at(&entry.path(), error))?;
                if !is_special(&kind) {
                    names.push(entry.file_name());
                }
            }
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
    } else {
        let same = left.len() == right.len()
            && same_bytes(source, copy).map_err(|error| at(copy, error))?;
        if !same {
            return Err(differs("contents"));
        }
    }
    // Last, after the entries were read: reading does not change it.
    if left.mtime() != right.mtime() {
        return Err(differs("modification time"));
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
    use crate::test_support::Temp;

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

    /// Both ways keep contents, links, the private modes and the times.
    #[test]
    fn a_tree_keeps_its_contents_modes_and_times_by_rename_and_by_copy() {
        for mover in [Mover::new(), Mover::copying()] {
            let temp = Temp::new();
            let source = private_tree(&temp.0);
            let old_time = FileTime::from_unix_time(1_600_000_000, 0);
            filetime::set_file_times(source.join("nested/data"), old_time, old_time).unwrap();
            filetime::set_file_times(source.join("nested"), old_time, old_time).unwrap();
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
            for path in [target.join("nested/data"), target.join("nested")] {
                assert_eq!(
                    fs::metadata(&path).unwrap().mtime(),
                    1_600_000_000,
                    "{mover:?} {}",
                    path.display()
                );
            }
            assert!(!with_suffix(&target, STAGING_SUFFIX).exists());
            assert!(!with_suffix(&source, MOVED_SUFFIX).exists());
            assert_eq!(
                mover.move_entry(&source, &target).unwrap(),
                Moved::AlreadyThere
            );
        }
    }

    /// The copy path leaves out what cannot be copied and deletes what its
    /// owner made read-only.
    #[test]
    fn the_copy_skips_sockets_and_fifos_and_deletes_read_only_directories() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the permissions this test relies on
        }
        type Plant = fn(&Path);
        let cases: [(&str, Plant); 3] = [
            ("socket", |dir| {
                std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
            }),
            ("fifo", |dir| {
                let path =
                    std::ffi::CString::new(dir.join("fifo").into_os_string().into_encoded_bytes())
                        .unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }),
            ("read-only", |dir| {
                let locked = dir.join("read-only");
                fs::create_dir(&locked).unwrap();
                fs::write(locked.join("file"), "kept").unwrap();
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
            }),
        ];
        for (name, plant) in cases {
            let temp = Temp::new();
            let source = private_tree(&temp.0);
            plant(&source);
            let target = temp.0.join("new").join("secrets");

            let moved = Mover::copying().move_entry(&source, &target);

            assert_eq!(moved.unwrap(), Moved::Moved, "{name}");
            assert!(!source.exists(), "{name}: the source is deleted");
            assert!(
                !with_suffix(&source, MOVED_SUFFIX).exists(),
                "{name}: nothing left aside"
            );
            assert_eq!(
                fs::read_to_string(target.join("credential")).unwrap(),
                "token"
            );
            match name {
                "read-only" => {
                    assert_eq!(
                        fs::read_to_string(target.join("read-only/file")).unwrap(),
                        "kept"
                    );
                    let mode = fs::metadata(target.join("read-only")).unwrap().mode() & 0o777;
                    assert_eq!(mode, 0o500, "the copy keeps the mode");
                    fs::set_permissions(
                        target.join("read-only"),
                        fs::Permissions::from_mode(0o700),
                    )
                    .unwrap();
                }
                _ => assert!(!target.join(name).exists(), "{name} is not copied"),
            }
        }
    }

    /// A copy belongs to whoever runs the tool: a set-id bit kept on it
    /// would grant that user's rights to the file.
    #[test]
    fn a_copy_drops_the_set_id_bits() {
        let temp = Temp::new();
        let source = private_tree(&temp.0);
        fs::write(source.join("tool"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(source.join("tool"), fs::Permissions::from_mode(0o6755)).unwrap();
        let target = temp.0.join("new").join("secrets");

        Mover::copying().move_entry(&source, &target).unwrap();

        let mode = fs::metadata(target.join("tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
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
