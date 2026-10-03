//! How a client reaches the control plane (U8): the local daemon it
//! discovers or launches, or a remote control plane at a URL.
//!
//! The two are not interchangeable guesses. Without a server URL a client
//! uses the trusted, loopback daemon of its state directory — exactly what
//! every local command has always done ([`super::ensure_daemon_running`]).
//! A daemon there that runs in network mode has no local token to hand out
//! ([`DiscoveryError::NetworkMode`]); the client then says so and asks for
//! the URL and an OIDC token instead of silently trying something else.

use crate::config::Flags;
use crate::persistence::StatePaths;

use super::client::ApiClient;
use super::discovery::{ensure_daemon_running, DiscoveryError};
use super::token::TokenSource;

/// Where the control plane is, as the operator named it.
#[derive(Debug, Clone, Default)]
pub struct ServerTarget {
    /// The control plane's base URL (`--server` / `NARSOR_SERVER`); `None`
    /// means the local daemon of the state directory.
    pub server: Option<String>,
    /// The token to present to `server`. Ignored for the local daemon, whose
    /// token is in its protected discovery file.
    pub token: Option<TokenSource>,
}

impl ServerTarget {
    /// Whether this targets a remote control plane.
    pub fn is_remote(&self) -> bool {
        self.server.is_some()
    }
}

/// A client for `target`: the remote control plane when it names one,
/// else the local daemon of `state`, launched when none runs.
///
/// The error is a sentence for the operator.
pub async fn connect(
    target: &ServerTarget,
    state: &StatePaths,
    flags: &Flags,
) -> Result<ApiClient, String> {
    match &target.server {
        Some(server) => {
            let token = target.token.clone().ok_or_else(|| {
                format!(
                    "no token for {server}: set NARSOR_TOKEN, NARSOR_TOKEN_FILE (--token-file) \
                     or NARSOR_TOKEN_COMMAND (--token-command) to an OIDC access token for the \
                     control plane's audience"
                )
            })?;
            ApiClient::remote(server, token).map_err(|error| error.to_string())
        }
        None => ensure_daemon_running(state, flags)
            .await
            .map_err(|error| match error {
                DiscoveryError::NetworkMode => format!(
                    "{error}: pass --server <https-url> (or NARSOR_SERVER) together with a token \
                     (NARSOR_TOKEN_FILE, NARSOR_TOKEN_COMMAND or NARSOR_TOKEN)"
                ),
                other => format!("Could not reach the narsor daemon: {other}"),
            }),
    }
}
