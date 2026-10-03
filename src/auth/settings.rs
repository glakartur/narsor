//! The server's security configuration: which mode, where to listen and —
//! in network mode — whom to trust.
//!
//! The settings themselves live in the ordinary configuration registry
//! ([`crate::config::SettingKey`]), so they reach the daemon through the same
//! file, environment and `config show` as every other setting. Each value is
//! checked on its own there (with the parsers below); what only makes sense
//! across several of them — network mode needs an issuer, an audience and a
//! TLS policy; trusted mode must stay on loopback — is checked here, once, by
//! [`SecuritySettings::validate`], before the daemon touches anything.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use ipnet::IpNet;
use reqwest::Url;

use crate::config::{settings, Resolved, SettingKey};

use super::oidc::OidcSettings;

/// How the server authenticates its users (R35, R36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerMode {
    /// One machine, one person: a random per-process token in a `0600`
    /// discovery file, served on loopback only.
    Trusted,
    /// OIDC users, one installation owner, TLS terminated by a trusted proxy.
    Network,
}

/// Who terminates TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsTermination {
    /// Nobody: acceptable only on loopback.
    None,
    /// A reverse proxy at one of the trusted addresses. The server itself
    /// speaks plain HTTP to it and refuses every other peer.
    Proxy,
}

/// The security settings as configured, before cross-checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecuritySettings {
    /// The mode.
    pub mode: ServerMode,
    /// `host:port` to listen on.
    pub bind: String,
    /// The trusted OIDC issuer, matched exactly.
    pub oidc_issuer: Option<String>,
    /// The audience every token must carry.
    pub oidc_audience: Option<String>,
    /// The installation owner's subject, when bound by deployment config.
    pub owner_subject: Option<String>,
    /// Who terminates TLS.
    pub tls_termination: TlsTermination,
    /// Addresses or CIDR ranges of the TLS-terminating proxies.
    pub trusted_proxies: Vec<String>,
    /// Browser origins allowed to call the API.
    pub cors_allowed_origins: Vec<String>,
}

impl SecuritySettings {
    /// Trusted mode on a system-chosen loopback port: what an installation
    /// without any security setting gets, and what in-process tests use.
    pub fn trusted_loopback() -> Self {
        Self {
            mode: ServerMode::Trusted,
            bind: settings::DEFAULT_SERVER_BIND.to_string(),
            oidc_issuer: None,
            oidc_audience: None,
            owner_subject: None,
            tls_termination: TlsTermination::None,
            trusted_proxies: Vec::new(),
            cors_allowed_origins: Vec::new(),
        }
    }

    /// The settings the resolved configuration holds. Each value already
    /// passed its own check while the configuration was resolved.
    pub fn from_config(config: &Resolved) -> Self {
        let mode = match config.text(SettingKey::ServerMode) {
            Some(settings::SERVER_MODE_NETWORK) => ServerMode::Network,
            _ => ServerMode::Trusted,
        };
        let tls_termination = match config.text(SettingKey::TlsTermination) {
            Some(settings::TLS_TERMINATION_PROXY) => TlsTermination::Proxy,
            _ => TlsTermination::None,
        };
        Self {
            mode,
            bind: config
                .text(SettingKey::ServerBind)
                .unwrap_or(settings::DEFAULT_SERVER_BIND)
                .to_string(),
            oidc_issuer: config.text(SettingKey::OidcIssuer).map(str::to_string),
            oidc_audience: config.text(SettingKey::OidcAudience).map(str::to_string),
            owner_subject: config.text(SettingKey::OwnerSubject).map(str::to_string),
            tls_termination,
            trusted_proxies: config.list(SettingKey::TrustedProxies),
            cors_allowed_origins: config.list(SettingKey::CorsAllowedOrigins),
        }
    }

    /// Cross-checks the settings and resolves the bind address.
    ///
    /// Every problem is reported at once, so an operator fixing a
    /// deployment does not discover them one restart at a time.
    pub fn validate(&self) -> Result<SecurityPlan, SecurityConfigError> {
        let mut problems = Vec::new();
        let cors = match self
            .cors_allowed_origins
            .iter()
            .map(|origin| parse_origin(origin))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(origins) => CorsPolicy {
                allowed_origins: origins,
            },
            Err(message) => {
                problems.push(format!("cors_allowed_origins: {message}"));
                CorsPolicy::deny()
            }
        };
        let bind = match resolve_bind(&self.bind) {
            Ok(addresses) => Some(addresses),
            Err(message) => {
                problems.push(format!("server_bind: {message}"));
                None
            }
        };

        let plan = match self.mode {
            ServerMode::Trusted => self.trusted_plan(bind, cors, &mut problems),
            ServerMode::Network => self.network_plan(bind, cors, &mut problems),
        };
        match plan {
            Some(plan) if problems.is_empty() => Ok(plan),
            _ => Err(SecurityConfigError { problems }),
        }
    }

    fn trusted_plan(
        &self,
        bind: Option<Vec<SocketAddr>>,
        cors: CorsPolicy,
        problems: &mut Vec<String>,
    ) -> Option<SecurityPlan> {
        // Settings that only mean something in network mode are refused
        // rather than ignored: an operator who set an issuer believes users
        // are authenticated, and a trusted daemon would prove them wrong.
        let network_only = [
            ("oidc_issuer", self.oidc_issuer.is_some()),
            ("oidc_audience", self.oidc_audience.is_some()),
            ("owner_subject", self.owner_subject.is_some()),
            ("trusted_proxies", !self.trusted_proxies.is_empty()),
            (
                "tls_termination",
                self.tls_termination != TlsTermination::None,
            ),
        ];
        for (key, set) in network_only {
            if set {
                problems.push(format!(
                    "{key} is set, but server_mode is 'trusted', which ignores it; set \
                     server_mode: network to authenticate users, or remove {key}"
                ));
            }
        }
        let addresses = bind?;
        for address in &addresses {
            if let Err(reason) = loopback_only(address) {
                problems.push(format!(
                    "server_bind '{}' resolves to {address}, {reason}; trusted mode has no \
                     user authentication beyond a local token and must listen on loopback \
                     (127.0.0.1 or [::1]) only — use server_mode: network to serve other hosts",
                    self.bind
                ));
            }
        }
        Some(SecurityPlan::Trusted {
            bind: *addresses.first()?,
            cors,
        })
    }

    fn network_plan(
        &self,
        bind: Option<Vec<SocketAddr>>,
        cors: CorsPolicy,
        problems: &mut Vec<String>,
    ) -> Option<SecurityPlan> {
        let issuer = match &self.oidc_issuer {
            Some(issuer) => match parse_issuer(issuer) {
                Ok(_) => Some(issuer.trim().to_string()),
                Err(message) => {
                    problems.push(format!("oidc_issuer: {message}"));
                    None
                }
            },
            None => {
                problems.push(
                    "server_mode is 'network' but oidc_issuer is not set; network mode \
                     authenticates every user with the configured OIDC issuer"
                        .to_string(),
                );
                None
            }
        };
        let audience = match &self.oidc_audience {
            Some(audience) => Some(audience.trim().to_string()),
            None => {
                problems.push(
                    "server_mode is 'network' but oidc_audience is not set; without it a \
                     token issued for any other application of the issuer would be accepted"
                        .to_string(),
                );
                None
            }
        };
        if self.tls_termination != TlsTermination::Proxy {
            problems.push(
                "server_mode is 'network' but no TLS policy is configured; set \
                 tls_termination: proxy and list the TLS-terminating proxies in \
                 trusted_proxies (the server does not serve TLS itself)"
                    .to_string(),
            );
        }
        let proxies = self
            .trusted_proxies
            .iter()
            .map(|proxy| parse_proxy(proxy))
            .collect::<Result<Vec<_>, _>>();
        let proxy = match proxies {
            Ok(proxies) if proxies.is_empty() => {
                problems.push(
                    "server_mode is 'network' but trusted_proxies is empty; list the \
                     addresses of the proxies that terminate TLS in front of the server"
                        .to_string(),
                );
                None
            }
            Ok(trusted) => Some(ProxyPolicy { trusted }),
            Err(message) => {
                problems.push(format!("trusted_proxies: {message}"));
                None
            }
        };
        let bind = *bind?.first()?;
        Some(SecurityPlan::Network(NetworkPlan {
            bind,
            oidc: OidcSettings::new(issuer?, audience?),
            owner_subject: self
                .owner_subject
                .as_ref()
                .map(|subject| subject.trim().to_string()),
            proxy: proxy?,
            cors,
        }))
    }
}

/// The security configuration did not hold up; the daemon refuses to start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the server's security configuration is not usable: {}",
    problems.join("; ")
)]
pub struct SecurityConfigError {
    /// Every problem found.
    pub problems: Vec<String>,
}

/// What the server will do, after validation.
#[derive(Debug, Clone)]
pub enum SecurityPlan {
    /// Loopback with a random token.
    Trusted {
        /// A loopback address.
        bind: SocketAddr,
        /// Browser origins allowed (usually none).
        cors: CorsPolicy,
    },
    /// OIDC users behind a TLS-terminating proxy.
    Network(NetworkPlan),
}

/// The validated network-mode configuration.
#[derive(Debug, Clone)]
pub struct NetworkPlan {
    /// Where to listen.
    pub bind: SocketAddr,
    /// The trusted issuer and audience.
    pub oidc: OidcSettings,
    /// The owner bound by deployment config, if any.
    pub owner_subject: Option<String>,
    /// Which peers may connect.
    pub proxy: ProxyPolicy,
    /// Which browser origins may call.
    pub cors: CorsPolicy,
}

/// The TLS-termination policy: only these peers, and only for requests the
/// proxy says arrived over HTTPS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyPolicy {
    /// Address ranges of the proxies.
    pub trusted: Vec<IpNet>,
}

impl ProxyPolicy {
    /// Whether a connection from `peer` comes from a trusted proxy. An
    /// IPv4-mapped IPv6 peer is judged as the IPv4 address it carries, so a
    /// dual-stack socket cannot dodge an IPv4 range.
    pub fn allows(&self, peer: IpAddr) -> bool {
        let peer = canonical(peer);
        self.trusted.iter().any(|range| range.contains(&peer))
    }
}

/// The explicit CORS policy. Empty denies: no CORS header is ever sent, so
/// a browser refuses every cross-origin call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsPolicy {
    /// Exact origins (`scheme://host[:port]`), never a wildcard.
    pub allowed_origins: Vec<String>,
}

impl CorsPolicy {
    /// Deny every cross-origin request.
    pub fn deny() -> Self {
        Self::default()
    }
}

/// Checks the shape of a bind address: `host:port`.
pub fn parse_bind(text: &str) -> Result<(), String> {
    let text = text.trim();
    let (host, port) = text
        .rsplit_once(':')
        .ok_or_else(|| format!("'{text}' is not host:port (for example 127.0.0.1:7400)"))?;
    if host.is_empty() {
        return Err(format!("'{text}' names no host"));
    }
    port.parse::<u16>()
        .map_err(|_| format!("'{text}' has no valid port number"))?;
    Ok(())
}

/// Resolves a bind address to every socket address it names.
///
/// Resolution, not parsing, is what decides whether a name like `localhost`
/// is loopback: the check has to see what the socket will actually bind.
pub fn resolve_bind(text: &str) -> Result<Vec<SocketAddr>, String> {
    parse_bind(text)?;
    let addresses: Vec<SocketAddr> = text
        .trim()
        .to_socket_addrs()
        .map_err(|error| format!("'{}' does not resolve: {error}", text.trim()))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("'{}' resolves to no address", text.trim()));
    }
    Ok(addresses)
}

/// Whether `address` keeps the socket on this machine: a loopback address,
/// and neither a wildcard nor an IPv4-mapped IPv6 one (a mapped address
/// binds an IPv6 socket whose dual-stack behaviour depends on the host).
fn loopback_only(address: &SocketAddr) -> Result<(), &'static str> {
    let ip = address.ip();
    if ip.is_unspecified() {
        return Err("a wildcard address that listens on every interface");
    }
    if let IpAddr::V6(v6) = ip {
        if v6.to_ipv4_mapped().is_some() {
            return Err("an IPv4-mapped IPv6 address");
        }
    }
    if !ip.is_loopback() {
        return Err("which is not a loopback address");
    }
    Ok(())
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// Checks an OIDC issuer URL: HTTPS without query, fragment or user info —
/// or plain HTTP to a loopback host, for a local test issuer only.
pub fn parse_issuer(text: &str) -> Result<Url, String> {
    let url = Url::parse(text.trim()).map_err(|error| format!("'{text}' is not a URL: {error}"))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "'{text}' must not carry a query or a fragment (OIDC issuer identifiers never do)"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("'{text}' must not carry user information"));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback_host(&url) => Ok(url),
        "http" => Err(format!(
            "'{text}' uses plain HTTP; an issuer must be reached over HTTPS (plain HTTP is \
             accepted only for a loopback test issuer)"
        )),
        other => Err(format!("'{text}' uses the unsupported scheme '{other}'")),
    }
}

/// Whether `url` points at this machine. Used to allow a plain-HTTP issuer
/// (and its JWKS) only where no network can see the traffic.
pub(crate) fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

/// Checks one trusted proxy: an IP address or a CIDR range, never "every
/// address".
pub fn parse_proxy(text: &str) -> Result<IpNet, String> {
    let text = text.trim();
    let range = match text.parse::<IpNet>() {
        Ok(range) => range,
        Err(_) => text
            .parse::<IpAddr>()
            .map(IpNet::from)
            .map_err(|_| format!("'{text}' is neither an IP address nor a CIDR range"))?,
    };
    if range.prefix_len() == 0 {
        return Err(format!(
            "'{text}' trusts every address, which would let any client claim to have \
             arrived over HTTPS"
        ));
    }
    Ok(range)
}

/// Checks one CORS origin and returns its canonical form: `scheme://host`
/// with a port only when it is not the scheme's default. A wildcard is
/// refused on purpose — the API carries bearer credentials.
pub fn parse_origin(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text == "*" || text.eq_ignore_ascii_case("null") {
        return Err(format!(
            "'{text}' is not allowed; list the exact origins that may call the API"
        ));
    }
    let url = Url::parse(text).map_err(|error| format!("'{text}' is not an origin: {error}"))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(format!("'{text}' is not an http(s) origin"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "'{text}' is not a bare origin (scheme://host[:port], no path)"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("'{text}' must not carry user information"));
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted(bind: &str) -> SecuritySettings {
        SecuritySettings {
            bind: bind.to_string(),
            ..SecuritySettings::trusted_loopback()
        }
    }

    fn network() -> SecuritySettings {
        SecuritySettings {
            mode: ServerMode::Network,
            bind: "0.0.0.0:0".to_string(),
            oidc_issuer: Some("https://issuer.example".to_string()),
            oidc_audience: Some("narsor".to_string()),
            owner_subject: None,
            tls_termination: TlsTermination::Proxy,
            trusted_proxies: vec!["10.0.0.0/8".to_string()],
            cors_allowed_origins: Vec::new(),
        }
    }

    #[test]
    fn trusted_mode_accepts_loopback_and_refuses_everything_else() {
        assert!(trusted("127.0.0.1:0").validate().is_ok());
        assert!(trusted("[::1]:0").validate().is_ok());
        assert!(trusted("localhost:0").validate().is_ok());
        for refused in ["0.0.0.0:0", "[::]:0", "[::ffff:127.0.0.1]:0", "192.0.2.1:0"] {
            let error = trusted(refused).validate().expect_err(refused);
            assert!(error.to_string().contains("loopback"), "{error}");
        }
    }

    #[test]
    fn trusted_mode_refuses_network_only_settings() {
        let settings = SecuritySettings {
            oidc_issuer: Some("https://issuer.example".to_string()),
            ..trusted("127.0.0.1:0")
        };
        let error = settings.validate().expect_err("issuer in trusted mode");
        assert!(error.to_string().contains("oidc_issuer"), "{error}");
    }

    #[test]
    fn network_mode_needs_issuer_audience_and_a_tls_policy() {
        assert!(network().validate().is_ok());
        let bare = SecuritySettings {
            oidc_issuer: None,
            oidc_audience: None,
            tls_termination: TlsTermination::None,
            trusted_proxies: Vec::new(),
            ..network()
        };
        let error = bare.validate().expect_err("nothing configured").to_string();
        for named in [
            "oidc_issuer",
            "oidc_audience",
            "tls_termination",
            "trusted_proxies",
        ] {
            assert!(error.contains(named), "{named} missing from: {error}");
        }
    }

    #[test]
    fn issuers_must_be_https_unless_loopback() {
        assert!(parse_issuer("https://issuer.example/realms/a").is_ok());
        assert!(parse_issuer("http://127.0.0.1:8080").is_ok());
        assert!(parse_issuer("http://issuer.example").is_err());
        assert!(parse_issuer("https://issuer.example/?a=b").is_err());
        assert!(parse_issuer("https://issuer.example/#frag").is_err());
    }

    #[test]
    fn proxies_and_origins_are_explicit() {
        assert!(parse_proxy("10.1.2.3").is_ok());
        assert!(parse_proxy("10.0.0.0/8").is_ok());
        assert!(parse_proxy("0.0.0.0/0").is_err());
        assert!(parse_proxy("proxy.example").is_err());
        assert_eq!(
            parse_origin("https://app.example").unwrap(),
            "https://app.example"
        );
        assert!(parse_origin("*").is_err());
        assert!(parse_origin("https://app.example/path").is_err());

        let policy = ProxyPolicy {
            trusted: vec![parse_proxy("10.0.0.0/8").unwrap()],
        };
        assert!(policy.allows("10.9.9.9".parse().unwrap()));
        assert!(policy.allows("::ffff:10.9.9.9".parse().unwrap()));
        assert!(!policy.allows("192.0.2.1".parse().unwrap()));
    }
}
