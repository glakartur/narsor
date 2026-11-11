//! `tui`: the full-screen interface.

use crate::config::{Flags, Resolved};

use super::{CliError, Result};

/// `narsor tui` — the full-screen interface over the control plane `target`
/// names: a remote server, or the local daemon of the state directory
/// (launched when none runs, as for every other command).
///
/// The loop is async but this entry point is not, because the subcommand
/// dispatch calls it from inside the runtime `main` already started. So the
/// current worker thread is handed over with `block_in_place` and the loop is
/// driven on it: spawning a second runtime inside the first would panic, and
/// the interface has to own a thread anyway — it blocks on keys.
pub fn tui(config: &Resolved, flags: &Flags, target: &crate::api::ServerTarget) -> Result<()> {
    let state = config.state_dir();
    let handle = tokio::runtime::Handle::current();
    let refresh_interval = config
        .tui_refresh_interval()
        .map(std::time::Duration::from_secs);
    let connection_name = target.server.clone().unwrap_or_else(|| "local".to_string());
    tokio::task::block_in_place(move || {
        handle.block_on(async {
            let client = crate::api::connect(target, state, flags)
                .await
                .map_err(CliError::failure)?;
            crate::tui::run(client, connection_name, refresh_interval)
                .await
                .map_err(|error| {
                    CliError::failure(format!(
                        "The terminal interface stopped with an error: {error}"
                    ))
                })
        })
    })
}
