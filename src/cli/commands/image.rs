//! `image refresh`: asks the local controller to check the versions of its
//! Docker-mode task images at their next use, even inside the update
//! interval.
//!
//! Like `secret set`, `narsor-cli` resolves the daemon's state root and runs
//! the `narsor-controller` installed next to it with `--state-root`; the
//! controller writes its refresh marker in its own directory below it.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::local_controller;

use super::secret::{controller_program, CONTROLLER_BINARY};
use super::{CliError, Result};

/// The `narsor-controller` arguments that ask for a refresh of the task
/// images of the daemon whose state root is `state_dir`.
pub fn image_refresh_args(state_dir: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["local", "image", "refresh"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.extend(local_controller::root_args(state_dir, None));
    args
}

/// `narsor-cli image refresh`.
pub fn image_refresh(state_dir: &Path) -> Result<()> {
    let program = controller_program("it builds the task images")?;
    let status = Command::new(&program)
        .args(image_refresh_args(state_dir))
        .stdin(Stdio::null())
        .status()
        .map_err(|error| CliError::failure(format!("cannot run {}: {error}", program.display())))?;
    // The controller has said why on stderr.
    if status.success() {
        Ok(())
    } else {
        Err(CliError::failure(format!(
            "narsor-cli: the task images were not marked for a refresh ({CONTROLLER_BINARY} \
             ended with {status})"
        )))
    }
}
