use std::{
    io::{self, IsTerminal, Write},
    path::Path,
};

use crate::{
    api::connection_profile::{
        default_connection_profile_path, format_connection_profile, normalize_https_endpoint,
        read_connection_profile, write_connection_profile,
    },
    cli::commands::{CliError, Result},
};

pub fn export(server: Option<&str>, output: Option<&Path>) -> Result<()> {
    let server = server
        .filter(|server| !server.trim().is_empty())
        .ok_or_else(|| {
            CliError::usage(
                "connection-profile export needs --server or an imported connection profile",
            )
        })?;
    let contents = format_connection_profile(server).map_err(CliError::usage)?;
    if let Some(output) = output {
        write_connection_profile(output, server).map_err(CliError::failure)?;
        eprintln!("Exported connection profile to '{}'.", output.display());
    } else {
        print!("{contents}");
    }
    Ok(())
}

pub fn import(source: &Path, trust_server: bool) -> Result<()> {
    let server = read_connection_profile(source)
        .map_err(CliError::failure)?
        .ok_or_else(|| {
            CliError::usage(format!(
                "connection profile '{}' does not exist",
                source.display()
            ))
        })?;
    let (server, origin) = normalize_https_endpoint(&server).map_err(CliError::failure)?;

    eprintln!("Connection profile endpoint: {server}");
    eprintln!("Origin to trust: {origin}");
    if !trust_server && !confirm_trust(&origin)? {
        return Err(CliError::usage(
            "connection profile import was not confirmed",
        ));
    }

    let destination = default_connection_profile_path().ok_or_else(|| {
        CliError::failure("cannot determine XDG_CONFIG_HOME or HOME for the connection profile")
    })?;
    write_connection_profile(&destination, &server).map_err(CliError::failure)?;
    eprintln!(
        "Imported connection profile to '{}'.",
        destination.display()
    );
    Ok(())
}

fn confirm_trust(origin: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(CliError::usage(format!(
            "non-interactive profile import requires --trust-server to trust '{origin}'"
        )));
    }

    eprint!("Trust {origin} and save this connection profile? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| CliError::failure(format!("could not display trust prompt: {error}")))?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).map_err(|error| {
        CliError::failure(format!("could not read trust confirmation: {error}"))
    })?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}
