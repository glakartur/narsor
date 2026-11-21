//! The directories of `narsor-controller local`, below the state and work
//! roots it is given (KTD6, KTD7).
//!
//! ```text
//! {state}/narsor-controller-local/    ControllerStatePaths
//!     credential, credential.lock     the controller identity and its lock
//!     controller-id                   the image label of this controller (KTD8)
//!     secrets/                        the token store (Docker mode)
//!     images/                         task image cache entries (Docker mode)
//!     tasks/{storage}.json            per-task records (KTD14)
//! {work}/narsor-controller-local/     ControllerWorkPaths
//!     storage/{storage}/              task storage
//!     workloads/{workload}/           one live workload per task
//!     readers/{reader}/               the task's read-only runner (D63)
//! ```
//!
//! Each type is built from a *root* and appends only this controller's
//! directory name ([`CONTROLLER_DIR`]): whoever starts the controller hands
//! it the roots and never constructs these paths itself.

use std::path::{Path, PathBuf};

/// The controller's own directory below the state root and below the work
/// root.
pub const CONTROLLER_DIR: &str = "narsor-controller-local";

/// `root` made absolute against the current directory, once: container
/// mounts and workdirs need absolute host paths. An absolute root, or one
/// that cannot be made absolute, is kept as it is.
fn absolute(root: &Path) -> PathBuf {
    if root.is_absolute() {
        return root.to_path_buf();
    }
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The controller's state: `{state}/narsor-controller-local/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerStatePaths {
    dir: PathBuf,
}

impl ControllerStatePaths {
    /// The controller's state below the state root `state_root`.
    pub fn new(state_root: impl AsRef<Path>) -> Self {
        Self {
            dir: absolute(state_root.as_ref()).join(CONTROLLER_DIR),
        }
    }

    /// The controller's state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `credential`: the controller identity, written on enrollment.
    pub fn credential_file(&self) -> PathBuf {
        self.dir.join("credential")
    }

    /// `credential.lock`: held while a controller uses the credential.
    pub fn credential_lock_file(&self) -> PathBuf {
        self.dir.join("credential.lock")
    }

    /// `controller-id`: the [`CONTROLLER_LABEL`] value of the images this
    /// controller builds (KTD8).
    ///
    /// [`CONTROLLER_LABEL`]: super::container::image::CONTROLLER_LABEL
    pub fn controller_id_file(&self) -> PathBuf {
        self.dir.join("controller-id")
    }

    /// `secrets/`: the token store (KTD10).
    pub fn secrets_dir(&self) -> PathBuf {
        self.dir.join("secrets")
    }

    /// `images/`: the task image cache entries and the refresh marker.
    pub fn images_dir(&self) -> PathBuf {
        self.dir.join("images")
    }

    /// `tasks/`: the per-task records (KTD14).
    pub fn tasks_dir(&self) -> PathBuf {
        self.dir.join("tasks")
    }
}

/// The controller's work: `{work}/narsor-controller-local/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerWorkPaths {
    dir: PathBuf,
}

impl ControllerWorkPaths {
    /// The controller's work below the work root `work_root`.
    pub fn new(work_root: impl AsRef<Path>) -> Self {
        Self {
            dir: absolute(work_root.as_ref()).join(CONTROLLER_DIR),
        }
    }

    /// The controller's work directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `storage/`: one task storage per task.
    pub fn storage_dir(&self) -> PathBuf {
        self.dir.join("storage")
    }

    /// `workloads/`: one directory per live working runner.
    pub fn workloads_dir(&self) -> PathBuf {
        self.dir.join("workloads")
    }

    /// `readers/`: one directory per live read-only runner.
    pub fn readers_dir(&self) -> PathBuf {
        self.dir.join("readers")
    }
}
