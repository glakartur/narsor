use std::net::IpAddr;

use ipnet::IpNet;
use reqwest::Url;

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
        "http" => Err(format!("'{text}' uses plain HTTP; an issuer must be reached over HTTPS (plain HTTP is accepted only for a loopback test issuer)")),
        other => Err(format!("'{text}' uses the unsupported scheme '{other}'")),
    }
}

/// Whether `url` names this machine: a loopback IP address, or exactly
/// `localhost`. Never a DNS lookup, so no name that merely resolves to a
/// loopback address (`127.0.0.1.nip.io`, `localhost.example.com`) counts.
pub fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

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
        return Err(format!("'{text}' trusts every address, which would let any client claim to have arrived over HTTPS"));
    }
    Ok(range)
}

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
