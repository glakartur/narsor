//! Client-side invocation of the server's explicit lifecycle command.

use std::io::Write;
use std::process::Command;

use crate::cli::commands::{CliError, Result};
use crate::config::Flags;

pub fn stop_daemon(flags: &Flags) -> Result<()> {
    let executable = crate::api::launch::server_executable().map_err(|error| {
        CliError::failure(format!(
            "could not resolve narsor-server executable: {error}"
        ))
    })?;
    let mut command = Command::new(executable);
    command.arg("stop");
    if let Some(root) = &flags.root {
        command.arg("--root").arg(root);
    }
    if let Some(config) = &flags.config {
        command.arg("--config").arg(config);
    }
    let output = command
        .output()
        .map_err(|error| CliError::failure(format!("could not run narsor-server stop: {error}")))?;
    std::io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| CliError::failure(format!("could not write stop output: {error}")))?;
    if output.status.success() {
        std::io::stderr()
            .write_all(&output.stderr)
            .map_err(|error| {
                CliError::failure(format!("could not write stop error output: {error}"))
            })?;
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(CliError {
            message: if message.is_empty() {
                format!("narsor-server stop exited with {}", output.status)
            } else {
                message
            },
            exit_code: output.status.code().unwrap_or(1),
        })
    }
}
