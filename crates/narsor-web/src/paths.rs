//! `narsor-web`'s own state: `{state}/narsor-web/` below the state root
//! (`server.state`).

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

/// The web interface's own directory below the state root.
pub const WEB_STATE_DIR: &str = "narsor-web";

/// Every path of `narsor-web`'s state, built from the state root, to which
/// it appends only its own directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebStatePaths {
    dir: PathBuf,
}

impl WebStatePaths {
    /// The web interface's state below the state root `state_root`.
    pub fn new(state_root: impl AsRef<Path>) -> Self {
        Self {
            dir: state_root.as_ref().join(WEB_STATE_DIR),
        }
    }

    /// The web interface's state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates the directory, readable by the operator only when new.
    pub fn create(&self) -> std::io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
    }

    /// `web-session`: the session token, for the operator only.
    pub fn session_file(&self) -> PathBuf {
        self.dir.join("web-session")
    }

    /// `web-open.html`: the launch page `--open` hands to the browser.
    pub fn launch_page(&self) -> PathBuf {
        self.dir.join("web-open.html")
    }
}
