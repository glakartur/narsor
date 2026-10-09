//! The `display` section of a workflow and what it renders to.
//!
//! A workflow declares how its tasks introduce themselves — a title and a few
//! links (ticket, pull request) — as templates ([`DisplaySpec`]). Rendering
//! against a task's inputs and variables yields a [`TaskDisplay`], the plain
//! values a client shows. `WORKFLOW_SCHEMA.md` §3.6 is the external contract.

use serde::{Deserialize, Serialize};

/// What a link shows when it has no text to show.
pub const EMPTY_LINK_TEXT: &str = "—";

/// The declared, still templated `display` of a workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplaySpec {
    /// Template of the task title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Links shown next to the title, in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<DisplayLinkSpec>,
}

/// One declared link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayLinkSpec {
    /// What the link is (`Jira`, `PR`); unique within the section.
    pub label: String,

    /// Template of the visible text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Template of the target; only `http` and `https` survive rendering.
    pub url: String,
}

/// The rendered `display` of one task.
///
/// `Default` is "nothing to show": an empty title makes a client fall back to
/// the task id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDisplay {
    /// Rendered title; empty when the workflow declares none or it cannot
    /// be rendered yet.
    #[serde(default)]
    pub title: String,

    /// Rendered links, one per declared link, in declaration order.
    #[serde(default)]
    pub links: Vec<DisplayLink>,
}

impl TaskDisplay {
    /// The same display made safe to print in a terminal: control characters
    /// (an ESC that starts an escape sequence among them) are removed from
    /// the title, labels and texts, and a URL that is not an `http(s)` one
    /// is dropped. Applied wherever a display comes from outside the
    /// control plane's own rendering — a remote runner reports one.
    pub fn sanitized(self) -> Self {
        let plain = |text: String| text.chars().filter(|ch| !is_unsafe_char(*ch)).collect();
        Self {
            title: plain(self.title),
            links: self
                .links
                .into_iter()
                .map(|link| DisplayLink {
                    label: plain(link.label),
                    text: plain(link.text),
                    url: link.url.as_deref().and_then(safe_link_url),
                })
                .collect(),
        }
    }
}

/// One rendered link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayLink {
    /// The declared label.
    pub label: String,

    /// Visible text; [`EMPTY_LINK_TEXT`] when there is none.
    pub text: String,

    /// Target, or `None` while it is unknown or not an `http(s)` URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// Whether `ch` could spoof or disturb what a terminal shows: a control
/// character, or an invisible Unicode format character (zero-width and
/// directional marks, bidi overrides and isolates, line and paragraph
/// separators, the byte order mark) that `char::is_control` does not flag.
fn is_unsafe_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{00AD}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

/// Accepts `value` as a link target only when it is an absolute `http` or
/// `https` URL with a host and nothing a terminal or a shell could misread.
///
/// A rendered URL may carry anything a runner variable held, and a client
/// hands it to the operator's browser, so the scheme is allow-listed
/// (`javascript:`, `file:` and the like never pass).
pub fn safe_link_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value
        .chars()
        .any(|ch| is_unsafe_char(ch) || ch.is_whitespace())
    {
        return None;
    }
    let (scheme, rest) = value.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    (!host.is_empty() && !host.starts_with(':')).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{safe_link_url, DisplayLink, TaskDisplay};

    #[test]
    fn sanitizing_strips_invisible_format_characters() {
        let display = TaskDisplay {
            title: "NAR-1\u{202E}gnp.exe\u{200B}\u{2028}".into(),
            links: vec![DisplayLink {
                label: "PR\u{2066}".into(),
                text: "go\u{FEFF}".into(),
                url: Some("https://x.example/\u{202E}a".into()),
            }],
        }
        .sanitized();
        assert_eq!(display.title, "NAR-1gnp.exe");
        assert_eq!(display.links[0].label, "PR");
        assert_eq!(display.links[0].text, "go");
        assert_eq!(display.links[0].url, None);
    }

    #[test]
    fn keeps_http_and_https() {
        assert_eq!(
            safe_link_url("https://jira.example/browse/NAR-1").as_deref(),
            Some("https://jira.example/browse/NAR-1")
        );
        assert!(safe_link_url("HTTP://localhost:8080").is_some());
    }

    #[test]
    fn drops_other_schemes_and_malformed_urls() {
        for value in [
            "",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://host/file",
            "https://",
            "https:///path",
            "https://:80/",
            "//host/path",
            "https://host/a b",
            "https://host/\u{1b}[31m",
            "jira.example/browse",
        ] {
            assert!(safe_link_url(value).is_none(), "{value:?} must be dropped");
        }
    }
}
