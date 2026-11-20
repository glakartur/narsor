//! The settings of `narsor-web` and the rules between them.
//!
//! Three keys of the shared configuration (`config show` lists them):
//!
//! - `web.bind` — where the browser interface listens;
//! - `web.upstream` — `local` (the local daemon) or the server's URL;
//! - `web.token` — `server`: `narsor-web` holds the server's token and gives
//!   the browser a session token of its own; `passthrough`: the browser's
//!   own token goes to the server unchanged.
//!
//! The rules refuse a start rather than run unsafely:
//!
//! - The address must be loopback. `narsor-web` does not serve TLS yet, and
//!   a browser treats plain HTTP as a secure context only on loopback. A
//!   remote setup puts a TLS proxy in front of a loopback `narsor-web`.
//! - In the `server` mode anyone who reaches `narsor-web` acts as the owner
//!   of the server. The session token and the check of `Host` and `Origin`
//!   (see [`crate::gateway`]) keep other sites and DNS rebinding out.

use std::net::{SocketAddr, ToSocketAddrs as _};

use narsor_shared::config::settings::{
    SettingKey, WEB_TOKEN_PASSTHROUGH, WEB_TOKEN_SERVER, WEB_UPSTREAM_LOCAL,
};
use narsor_shared::config::{Problem, Resolved};

/// Where the API is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// The local daemon of the state directory, discovered or launched.
    Local,
    /// A server at this URL.
    Url(String),
}

/// Whose token goes to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenMode {
    /// `narsor-web` holds the server's token; the browser holds a session
    /// token of `narsor-web`.
    Server,
    /// The browser's own token, passed on unchanged.
    Passthrough,
}

impl TokenMode {
    /// The word of the configuration.
    pub fn word(self) -> &'static str {
        match self {
            TokenMode::Server => WEB_TOKEN_SERVER,
            TokenMode::Passthrough => WEB_TOKEN_PASSTHROUGH,
        }
    }
}

/// The checked settings.
#[derive(Debug, Clone)]
pub struct WebSettings {
    pub bind: SocketAddr,
    pub upstream: Upstream,
    pub token: TokenMode,
}

/// `narsor-web`'s configuration step: its settings from `config`, or every
/// problem at once — the resolution errors and the rules between the
/// settings — before anything starts (R7, R8).
pub fn configure(config: &Resolved) -> Result<WebSettings, Vec<Problem>> {
    let mut problems = Problem::of_resolution(config);
    match WebSettings::from_config(config) {
        Ok(settings) if problems.is_empty() => Ok(settings),
        Ok(_) => Err(problems),
        Err(rules) => {
            problems.extend(rules);
            Err(problems)
        }
    }
}

impl WebSettings {
    /// The settings from the resolved configuration, or every problem at once.
    pub fn from_config(config: &Resolved) -> Result<Self, Vec<Problem>> {
        Self::parse(
            config.text(SettingKey::WebBind).unwrap_or_default(),
            config.text(SettingKey::WebUpstream).unwrap_or_default(),
            config.text(SettingKey::WebToken).unwrap_or_default(),
        )
    }

    /// The settings from the three texts.
    pub fn parse(bind: &str, upstream: &str, token: &str) -> Result<Self, Vec<Problem>> {
        let mut problems = Vec::new();
        let bind = match resolve_loopback(bind) {
            Ok(address) => Some(address),
            Err(problem) => {
                problems.push(Problem::at(
                    SettingKey::WebBind,
                    format!("{}: {problem}", SettingKey::WebBind),
                ));
                None
            }
        };
        let upstream = match upstream.trim() {
            WEB_UPSTREAM_LOCAL | "" => Upstream::Local,
            url => Upstream::Url(url.trim_end_matches('/').to_string()),
        };
        let token = match token.trim() {
            WEB_TOKEN_PASSTHROUGH => TokenMode::Passthrough,
            WEB_TOKEN_SERVER | "" => TokenMode::Server,
            other => {
                problems.push(Problem::at(
                    SettingKey::WebToken,
                    format!(
                        "{}: '{WEB_TOKEN_SERVER}' or '{WEB_TOKEN_PASSTHROUGH}' was expected, \
                         but '{other}' was found",
                        SettingKey::WebToken
                    ),
                ));
                TokenMode::Server
            }
        };
        if token == TokenMode::Passthrough && upstream == Upstream::Local {
            let token = SettingKey::WebToken;
            problems.push(Problem::at(
                token,
                format!(
                    "{token}: 'passthrough' with {}: 'local' would hand the local daemon's token \
                     to the browser; use {token}: 'server'",
                    SettingKey::WebUpstream
                ),
            ));
        }
        match bind {
            Some(bind) if problems.is_empty() => Ok(Self {
                bind,
                upstream,
                token,
            }),
            _ => Err(problems),
        }
    }
}

/// The one loopback address `text` names.
fn resolve_loopback(text: &str) -> Result<SocketAddr, String> {
    let text = text.trim();
    let addresses: Vec<SocketAddr> = text
        .to_socket_addrs()
        .map_err(|error| format!("'{text}' does not resolve: {error}"))?
        .collect();
    let address = *addresses
        .first()
        .ok_or_else(|| format!("'{text}' resolves to no address"))?;
    let ip = address.ip();
    let mapped = matches!(ip, std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some());
    if !ip.is_loopback() || mapped {
        return Err(format!(
            "'{text}' is not a loopback address. narsor-web does not serve TLS yet: listen on \
             127.0.0.1 and put a TLS proxy in front of it to reach it from other machines"
        ));
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_local_daemon_with_a_session_token() {
        let settings = WebSettings::parse("127.0.0.1:8484", "local", "server").unwrap();
        assert_eq!(settings.bind, "127.0.0.1:8484".parse().unwrap());
        assert_eq!(settings.upstream, Upstream::Local);
        assert_eq!(settings.token, TokenMode::Server);
    }

    #[test]
    fn a_url_upstream_loses_its_trailing_slash() {
        let settings =
            WebSettings::parse("127.0.0.1:0", "https://narsor.example.com/", "passthrough")
                .unwrap();
        assert_eq!(
            settings.upstream,
            Upstream::Url("https://narsor.example.com".into())
        );
        assert_eq!(settings.token, TokenMode::Passthrough);
    }

    #[test]
    fn an_address_beyond_loopback_is_refused_until_tls_exists() {
        for bind in ["0.0.0.0:8484", "[::]:8484", "192.168.1.10:8484"] {
            let problems = WebSettings::parse(bind, "local", "server").unwrap_err();
            assert!(
                problems[0].message().contains("not a loopback address"),
                "{bind}: {problems:?}"
            );
        }
    }

    #[test]
    fn passthrough_to_the_local_daemon_is_refused() {
        let problems = WebSettings::parse("127.0.0.1:1", "local", "passthrough").unwrap_err();
        assert_eq!(problems[0].path(), Some("web.token"), "{problems:?}");
        assert!(
            problems[0].message().contains("web.upstream"),
            "{problems:?}"
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = WebSettings::parse("0.0.0.0:1", "local", "maybe").unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
    }
}
