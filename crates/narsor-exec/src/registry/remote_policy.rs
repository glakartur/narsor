//! The rule for a Git remote a machine reaches on behalf of someone it does
//! not trust: an `https://` URL naming a host beyond this machine and its
//! private network. Anything else could read a path of this disk, use the
//! user's SSH keys, or let the caller probe the private network through
//! git's error messages.

/// How a refusal names the remote and states the rule, for each caller.
pub struct RemoteWording {
    subject: &'static str,
    not_https_rule: &'static str,
    internal_rule: &'static str,
}

impl RemoteWording {
    /// A control plane beyond this machine launching a task.
    pub const UNTRUSTED_SERVER: Self = Self {
        subject: "Git workspace",
        not_https_rule: "a control plane beyond this machine may only have repositories \
                         cloned over https://",
        internal_rule: "a control plane beyond this machine may only have repositories \
                        cloned from beyond it",
    };

    /// Listing the branches of a remote.
    pub const BRANCH_LISTING: Self = Self {
        subject: "Git remote",
        not_https_rule: "only repositories reachable over https:// are allowed here",
        internal_rule: "only repositories beyond it are allowed here",
    };
}

/// Refuses a Git remote other than an `https://` URL naming a host beyond
/// this machine and its private network.
pub fn check_https_remote(remote: &str, wording: &RemoteWording) -> Result<(), String> {
    let not_https = || {
        format!(
            "the {} '{}' is not an https:// URL; {}",
            wording.subject,
            without_credentials(remote),
            wording.not_https_rule
        )
    };
    // Git reads the text, not a parser's view of it: to git `https:host/x`
    // is an SSH remote, where a URL parser sees `https://host/x`.
    let plain = !remote
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\');
    if !plain || !remote.starts_with("https://") {
        return Err(not_https());
    }
    let url = reqwest::Url::parse(remote).map_err(|_| not_https())?;
    match url.host_str() {
        Some(host) if is_internal(host) => Err(format!(
            "the {} '{}' names {host}, this machine or its private network; {}",
            wording.subject,
            without_credentials(remote),
            wording.internal_rule
        )),
        Some(_) => Ok(()),
        None => Err(not_https()),
    }
}

/// Whether `host` is this machine or an address of a private network: a
/// clone of it would let the server probe them through git's errors. A name
/// is not resolved; only `localhost` is known to be this machine.
fn is_internal(host: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr};

    let internal_v4 = |ip: &Ipv4Addr| {
        let [a, b, ..] = ip.octets();
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            // Shared address space (RFC 6598), carrier-grade NAT.
            || (a == 100 && (64..128).contains(&b))
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => internal_v4(&ip),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                // Unique local (fc00::/7) and link-local (fe80::/10).
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(|ip| internal_v4(&ip))
        }
        Err(_) => {
            let name = bare.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    }
}

/// `remote` as a refusal may show it: without a user name or password.
pub fn without_credentials(remote: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(remote) {
        if !url.username().is_empty() || url.password().is_some() {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            return url.to_string();
        }
    }
    // What a parser does not see as credentials may still be some.
    match remote.rsplit_once('@') {
        Some((_, host)) => format!("...@{host}"),
        None => remote.to_string(),
    }
}
