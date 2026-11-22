//! The backup taken before anything moves: a zstd-compressed tar archive of
//! the old-layout entries (not of the whole root, which also holds the
//! catalogue with the built-in values).
//!
//! Each entry is stored under its absolute path without the leading `/`, as
//! GNU tar does, so `tar --zstd -xpf <archive> -C /` puts every file back
//! where it was. Modes, times and symbolic links (as links) are kept. A
//! socket, a FIFO or a device is left out with a warning.
//!
//! Before writing, [`estimate`] adds up the sizes and [`free_space`] reads
//! what the target filesystem has left; the caller refuses when the space
//! is short.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};

use crate::mover::is_special;

/// The file name extension of the archive.
pub const EXTENSION: &str = "tar.zst";

/// Bytes a tar header and its padding take per entry, roughly.
const ENTRY_OVERHEAD: u64 = 1024;

/// What [`write`] put in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The archive.
    pub path: PathBuf,
    /// How many entries it holds.
    pub entries: u64,
    /// Its size on disk.
    pub bytes: u64,
}

/// The size, uncompressed, an archive of `sources` takes at most: their
/// file sizes plus a header per entry.
pub fn estimate(sources: &[PathBuf]) -> u64 {
    fn walk(path: &Path) -> u64 {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return 0;
        };
        let own = ENTRY_OVERHEAD
            + if metadata.is_file() {
                metadata.len()
            } else {
                0
            };
        if !metadata.is_dir() {
            return own;
        }
        let children = fs::read_dir(path)
            .map(|entries| entries.flatten().map(|entry| walk(&entry.path())).sum())
            .unwrap_or(0);
        own + children
    }
    sources.iter().map(|source| walk(source)).sum()
}

/// The bytes an unprivileged user may still write on the filesystem of
/// `dir`, or of its nearest existing ancestor.
pub fn free_space(dir: &Path) -> io::Result<u64> {
    let mut probe = dir;
    while !probe.exists() {
        probe = probe.parent().unwrap_or(Path::new("/"));
    }
    let path = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs fills the struct this function owns.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// The archive name for a run started at `now`:
/// `layout-migration-<UTC timestamp>.tar.zst`.
pub fn file_name(now: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "layout-migration-{}.{EXTENSION}",
        now.format("%Y%m%dT%H%M%SZ")
    )
}

/// Writes the archive of `sources` to `path` (its directory created `0700`,
/// the archive `0600`), then reads it back to check that it holds every
/// entry. An error names the file it happened on; a partial archive is
/// deleted.
pub fn write(path: &Path, sources: &[PathBuf]) -> Result<Written, String> {
    let at = |path: &Path, error: io::Error| format!("{}: {error}", path.display());
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|error| at(dir, error))?;
    }
    let partial = crate::mover::with_suffix(path, ".partial");
    let result = write_archive(&partial, sources).and_then(|entries| {
        let found = count_entries(&partial).map_err(|error| at(&partial, error))?;
        if found != entries {
            return Err(format!(
                "{}: the archive holds {found} entries, {entries} were written",
                partial.display()
            ));
        }
        fs::rename(&partial, path).map_err(|error| at(path, error))?;
        let bytes = fs::metadata(path).map_err(|error| at(path, error))?.len();
        Ok(Written {
            path: path.to_path_buf(),
            entries,
            bytes,
        })
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_archive(partial: &Path, sources: &[PathBuf]) -> Result<u64, String> {
    let at = |path: &Path, error: io::Error| format!("{}: {error}", path.display());
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(partial)
        .map_err(|error| at(partial, error))?;
    let encoder = zstd::stream::write::Encoder::new(file, 3).map_err(|error| at(partial, error))?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    let mut entries = 0;
    for source in sources {
        let absolute = std::path::absolute(source).map_err(|error| at(source, error))?;
        append_tree(&mut builder, &absolute, &mut entries)?;
    }
    let encoder = builder.into_inner().map_err(|error| at(partial, error))?;
    let mut file = encoder.finish().map_err(|error| at(partial, error))?;
    file.flush().map_err(|error| at(partial, error))?;
    file.sync_all().map_err(|error| at(partial, error))?;
    Ok(entries)
}

/// Appends `path` and, for a directory, everything below it.
fn append_tree<W: io::Write>(
    builder: &mut tar::Builder<W>,
    path: &Path,
    entries: &mut u64,
) -> Result<(), String> {
    let at = |error: io::Error| format!("{}: {error}", path.display());
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(at(error)),
    };
    if is_special(&metadata.file_type()) {
        tracing::warn!(path = %path.display(), "backup: leaving out a socket, FIFO or device");
        return Ok(());
    }
    builder
        .append_path_with_name(path, archive_name(path))
        .map_err(at)?;
    *entries += 1;
    if metadata.is_dir() {
        let mut children = fs::read_dir(path)
            .map_err(at)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(at)?;
        children.sort();
        for child in children {
            append_tree(builder, &child, entries)?;
        }
    }
    Ok(())
}

/// `path` without its root: `/srv/n/auth` is stored as `srv/n/auth`.
fn archive_name(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .collect()
}

fn count_entries(path: &Path) -> io::Result<u64> {
    let decoder = zstd::stream::read::Decoder::new(fs::File::open(path)?)?;
    let mut archive = tar::Archive::new(decoder);
    let mut count = 0;
    for entry in archive.entries()? {
        entry?;
        count += 1;
    }
    Ok(count)
}
