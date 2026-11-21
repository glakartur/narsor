//! The terminal interface's own state: `{state}/narsor/` below the state root
//! (`server.state`).

use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// The terminal interface's own directory below the state root.
pub const TUI_STATE_DIR: &str = "narsor";

/// Every path of the terminal interface's state, built from the state root,
/// to which it appends only its own directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiStatePaths {
    dir: PathBuf,
}

impl TuiStatePaths {
    /// The terminal interface's state below the state root `state_root`.
    pub fn new(state_root: impl AsRef<Path>) -> Self {
        Self {
            dir: state_root.as_ref().join(TUI_STATE_DIR),
        }
    }

    /// The terminal interface's state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `narsor.log`: the trace log written when `RUST_LOG` is set.
    pub fn log_file(&self) -> PathBuf {
        self.dir.join("narsor.log")
    }

    /// Opens the log for appending, creating the directory (operator only
    /// when new) and the file (`0600` when new).
    pub fn open_log(&self) -> std::io::Result<std::fs::File> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.log_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_written_in_the_tuis_own_directory_not_the_rendezvous_one() {
        let root = std::env::temp_dir().join(format!("narsor-tui-paths-{}", uuid::Uuid::new_v4()));
        let paths = TuiStatePaths::new(&root);

        std::io::Write::write_all(&mut paths.open_log().expect("open the log"), b"line\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("narsor").join("narsor.log")).unwrap(),
            "line\n"
        );
        assert!(!root.join("run").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
