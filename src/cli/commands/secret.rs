//! `secret set`: stores a secret in the token store of the local
//! controller, which Docker-mode tasks take their secrets from (KTD10).
//!
//! The store belongs to the controller the daemon starts, beside its
//! credential under the daemon's state directory. `narsor-cli` resolves that
//! directory from the same configuration as the daemon and runs the
//! `narsor-controller` installed next to it, which writes the store, with
//! `--credential-file` naming that credential. The value goes through stdin
//! only: it never appears on a command line or in the output.

use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::api::launch::sibling_executable;
use crate::config::local_controller;

use super::{CliError, Result};

/// The binary that owns the token store.
pub const CONTROLLER_BINARY: &str = "narsor-controller";

/// The `narsor-controller` arguments that store secret `name` for the
/// daemon whose state directory is `state_dir`; the value comes on stdin.
pub fn secret_set_args(state_dir: &Path, name: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["local", "secret", "set", "--credential-file"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(local_controller::credential_file(state_dir).into_os_string());
    args.push("--".into());
    args.push(name.into());
    args
}

/// The `narsor-controller` installed next to this executable; `role` says
/// why it is needed when it is missing.
pub(super) fn controller_program(role: &str) -> Result<PathBuf> {
    sibling_executable(CONTROLLER_BINARY)
        .map_err(|error| {
            CliError::failure(format!("cannot look for {CONTROLLER_BINARY}: {error}"))
        })?
        .ok_or_else(|| {
            CliError::failure(format!(
                "{CONTROLLER_BINARY} is not installed next to narsor-cli; {role}"
            ))
        })
}

/// `narsor-cli secret set <name>`: hands stdin to the controller installed
/// next to this executable, which stores it.
pub fn secret_set(state_dir: &Path, name: &str) -> Result<()> {
    let program = controller_program("it keeps the token store")?;
    if std::io::stdin().is_terminal() {
        eprintln!(
            "Type the value of '{name}', then Enter and Ctrl-D. It shows as you type; to keep it \
             off the screen, use `narsor-cli secret set {name} < file` or a pipe."
        );
    }
    let status = Command::new(&program)
        .args(secret_set_args(state_dir, name))
        .stdin(Stdio::inherit())
        .status()
        .map_err(|error| CliError::failure(format!("cannot run {}: {error}", program.display())))?;
    // The controller has said why on stderr.
    match status.code() {
        Some(0) => Ok(()),
        Some(2) => Err(CliError::usage(format!(
            "narsor-cli: secret '{name}' was not stored"
        ))),
        _ => Err(CliError::failure(format!(
            "narsor-cli: secret '{name}' was not stored ({CONTROLLER_BINARY} ended with {status})"
        ))),
    }
}
