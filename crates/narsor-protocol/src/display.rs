//! What a client shows about a task: the header values of its workspaces
//! ([`WorkspaceHeader`]), the loss states of an interrupted task, and the
//! sanitizers that make text from outside safe to print.

use crate::controller::{ResumeState, RunnerState};
use serde::{Deserialize, Serialize};

/// Appended to captured output when the server truncates it at the byte cap.
pub const TRUNCATION_MARKER: &str = "\n… [truncated by narsor]";

/// Longest title the conductor generates, in characters. The new-task form
/// stops the operator's title at the same length.
pub const GENERATED_TITLE_MAX_CHARS: usize = 50;

/// What a gate shows for a step that gave no reason.
pub const NO_REASON: &str = "brak uzasadnienia";

/// Prefix attached to an automatic-resume exhaustion reason.
pub const EXHAUSTION_REASON_PREFIX: &str = "could not resume automatically: ";

/// Fallback loss text when the controller supplied no reason.
pub const UNSPECIFIED_LOSS: &str = "the workload was lost";

/// The default control-plane retry limit shown to clients while a task is
/// waiting for its next automatic resume attempt.
pub const RESUME_ATTEMPT_LIMIT: u32 = 5;

/// What an interrupted task waits for, derived from protocol state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LossState {
    AutoResume { attempt: u32, limit: u32 },
    Exhausted(String),
    LostWhilePausing,
}

impl LossState {
    /// Whether the owner has to act.
    pub fn needs_owner(&self) -> bool {
        !matches!(self, Self::AutoResume { .. })
    }
}

/// The reason named by a resume exhaustion, without its protocol prefix.
pub fn exhaustion_loss(reason: &str) -> &str {
    reason
        .strip_prefix(EXHAUSTION_REASON_PREFIX)
        .unwrap_or(reason)
}

/// What a task in `runner_state` with `resume` waits for, or `None` when it is
/// not interrupted or its loss is unknown.
pub fn loss_state(runner_state: RunnerState, resume: &ResumeState) -> Option<LossState> {
    if runner_state != RunnerState::Interrupted {
        return None;
    }
    if let Some(exhausted) = &resume.exhausted {
        return Some(LossState::Exhausted(exhausted.reason.clone()));
    }
    if resume.next_attempt_at.is_some() {
        return Some(LossState::AutoResume {
            attempt: resume.consecutive_losses.max(1),
            limit: RESUME_ATTEMPT_LIMIT,
        });
    }
    match &resume.last_loss {
        Some(loss) if loss.from == RunnerState::PauseRequested => Some(LossState::LostWhilePausing),
        _ => None,
    }
}

/// What a task shows about one of its workspaces: the fixed header values a
/// client renders as one row. The list of them is the task's
/// `context.workspace.<name>.*` contract (`docs/dev/workflow-schema.md`).
///
/// `Default` is a workspace with nothing known yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceHeader {
    /// The workspace name from the workflow's `globals.workspaces`.
    #[serde(default)]
    pub name: String,

    /// The repository name, or the folder path of a local workspace.
    #[serde(default)]
    pub repo: String,

    /// The branch the work is on; `None` while it is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// The branch the changes are counted against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,

    /// Lines added against the base branch; `None` while not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,

    /// Lines removed against the base branch; `None` while not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<u64>,

    /// The pull request number, without a prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,

    /// The pull request address, as the workflow set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

impl WorkspaceHeader {
    /// The same header made safe to print in a terminal: control characters
    /// (an ESC that starts an escape sequence among them) are removed from
    /// every text, and a `pr_url` that is not an `http(s)` address is
    /// dropped. Applied wherever a header comes from outside the control
    /// plane's own data: a remote runner reports it.
    pub fn sanitized(self) -> Self {
        let plain = |text: String| without_unsafe_chars(&text);
        Self {
            name: plain(self.name),
            repo: plain(self.repo),
            branch: self.branch.map(plain),
            base_branch: self.base_branch.map(plain),
            added: self.added,
            removed: self.removed,
            pr: self.pr.map(plain),
            pr_url: self.pr_url.as_deref().and_then(safe_link_url),
        }
    }

    /// What a client shows for the pull request of this workspace.
    pub fn pr_label(&self) -> PrLabel {
        let url = self.pr_url.as_deref().and_then(safe_link_url);
        let number = self
            .pr
            .as_deref()
            .map(str::trim)
            .filter(|pr| !pr.is_empty())
            .map(|pr| pr.trim_start_matches('#').to_string())
            .or_else(|| url.as_deref().and_then(number_in_url))
            .map(|number| format!("#{number}"));
        PrLabel { number, url }
    }
}

/// How a client shows a pull request: the number with its `#` prefix and the
/// link. Both are `None` when there is no pull request. A link exists only
/// for an absolute `http(s)` address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrLabel {
    pub number: Option<String>,
    pub url: Option<String>,
}

impl PrLabel {
    /// `PR #42`, or `PR` when the number is not known.
    pub fn text(&self) -> String {
        match &self.number {
            Some(number) => format!("PR {number}"),
            None => "PR".to_string(),
        }
    }
}

/// The number at the end of a pull request address, such as the `42` of
/// `.../merge_requests/42`.
fn number_in_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let (_, tail) = path.split_once("://")?;
    let (_, path) = tail.split_once('/')?;
    let segment = path.trim_end_matches('/').rsplit('/').next()?;
    (!segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| segment.to_string())
}

/// `text` without the characters a terminal must never be handed from an
/// untrusted source (see `is_unsafe_char`).
pub fn without_unsafe_chars(text: &str) -> String {
    text.chars().filter(|ch| !is_unsafe_char(*ch)).collect()
}

/// Like [`without_unsafe_chars`] for text that is meant to span lines (a
/// chat message, a diagnosis): line feeds stay, tabs become a space, and
/// carriage returns go with the other control characters.
pub fn without_unsafe_chars_multiline(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            _ if is_unsafe_char(ch) => None,
            _ => Some(ch),
        })
        .collect()
}

/// The longest `detail` a runner's acknowledgement may carry, in characters.
pub const MAX_DETAIL_CHARS: usize = 2_000;

/// A runner-supplied `detail` made safe to store and print: unsafe
/// characters removed (see [`without_unsafe_chars_multiline`]) and cut to
/// [`MAX_DETAIL_CHARS`] characters, on a character boundary.
pub fn bounded_detail(text: &str) -> String {
    without_unsafe_chars_multiline(text)
        .chars()
        .take(MAX_DETAIL_CHARS)
        .collect()
}

/// Cleans every string of `value` (and every object key) in place, keeping
/// line breaks in the strings: [`without_unsafe_chars_multiline`] for text,
/// [`without_unsafe_chars`] for names.
pub fn without_unsafe_chars_json(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::String(text) => *text = without_unsafe_chars_multiline(text),
        Value::Array(items) => items.iter_mut().for_each(without_unsafe_chars_json),
        Value::Object(map) => {
            *map = std::mem::take(map)
                .into_iter()
                .map(|(key, mut value)| {
                    without_unsafe_chars_json(&mut value);
                    (without_unsafe_chars(&key), value)
                })
                .collect();
        }
        _ => {}
    }
}

/// Whether `ch` could spoof or disturb what a terminal shows: a control
/// character, or an invisible Unicode format character (zero-width and
/// directional marks, bidi overrides and isolates, line and paragraph
/// separators, the byte order mark) that `char::is_control` does not flag.
fn is_unsafe_char(ch: char) -> bool {
    ch.is_control() || is_invisible_format(ch)
}

/// Whether `ch` is an invisible Unicode format character: zero-width and
/// directional marks, bidi overrides and isolates, line and paragraph
/// separators, the soft hyphen and the byte order mark. `char::is_control`
/// does not flag them, yet they can reorder or hide what a terminal shows.
pub fn is_invisible_format(ch: char) -> bool {
    matches!(
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
    use super::{safe_link_url, without_unsafe_chars_multiline, WorkspaceHeader};

    fn header_with_pr(pr: Option<&str>, pr_url: Option<&str>) -> WorkspaceHeader {
        WorkspaceHeader {
            name: "code".into(),
            pr: pr.map(str::to_string),
            pr_url: pr_url.map(str::to_string),
            ..WorkspaceHeader::default()
        }
    }

    #[test]
    fn sanitizing_strips_control_and_invisible_characters_from_texts() {
        let header = WorkspaceHeader {
            name: "co\u{1b}de".into(),
            repo: "NAR-1\u{202E}gnp.exe\u{200B}\u{2028}".into(),
            branch: Some("feat/\u{1b}[31mx".into()),
            base_branch: Some("main\u{FEFF}".into()),
            pr: Some("4\u{2066}2".into()),
            ..WorkspaceHeader::default()
        }
        .sanitized();
        assert_eq!(header.name, "code");
        assert_eq!(header.repo, "NAR-1gnp.exe");
        assert_eq!(header.branch.as_deref(), Some("feat/[31mx"));
        assert_eq!(header.base_branch.as_deref(), Some("main"));
        assert_eq!(header.pr.as_deref(), Some("42"));
    }

    #[test]
    fn sanitizing_drops_a_pr_url_that_is_not_http() {
        let header = header_with_pr(None, Some("javascript:alert(1)")).sanitized();
        assert_eq!(header.pr_url, None);
        let header = header_with_pr(None, Some("https://x.example/\u{202E}a")).sanitized();
        assert_eq!(header.pr_url, None);
        let header = header_with_pr(None, Some("https://x.example/pull/1")).sanitized();
        assert_eq!(header.pr_url.as_deref(), Some("https://x.example/pull/1"));
    }

    #[test]
    fn a_header_without_a_workspaces_field_in_json_has_defaults() {
        let header: WorkspaceHeader = serde_json::from_str(r#"{"name":"code"}"#).unwrap();
        assert_eq!(header.name, "code");
        assert_eq!(header.repo, "");
        assert_eq!(header.branch, None);
        assert_eq!(header.added, None);
    }

    #[test]
    fn the_pr_number_comes_from_the_end_of_the_address_when_pr_is_missing() {
        let label = header_with_pr(None, Some("https://github.com/o/r/pull/7")).pr_label();
        assert_eq!(label.number.as_deref(), Some("#7"));
        assert_eq!(label.url.as_deref(), Some("https://github.com/o/r/pull/7"));

        let label = header_with_pr(
            None,
            Some("https://gitlab.example/g/api/-/merge_requests/42/"),
        )
        .pr_label();
        assert_eq!(label.number.as_deref(), Some("#42"));
    }

    #[test]
    fn an_address_without_a_number_gives_a_link_and_no_number() {
        let label = header_with_pr(None, Some("https://host/review/abc")).pr_label();
        assert_eq!(label.number, None);
        assert_eq!(label.url.as_deref(), Some("https://host/review/abc"));

        let label = header_with_pr(None, Some("https://host")).pr_label();
        assert_eq!(label.number, None);
    }

    #[test]
    fn a_pr_number_wins_over_the_address() {
        let label = header_with_pr(Some("42"), Some("https://host/pull/7")).pr_label();
        assert_eq!(label.number.as_deref(), Some("#42"));
        assert_eq!(label.url.as_deref(), Some("https://host/pull/7"));
    }

    #[test]
    fn an_unsafe_address_is_never_a_link_or_a_number_source() {
        let label = header_with_pr(None, Some("javascript:alert(1)")).pr_label();
        assert_eq!(label.number, None);
        assert_eq!(label.url, None);

        let label = header_with_pr(Some("42"), Some("javascript:alert(1)")).pr_label();
        assert_eq!(label.number.as_deref(), Some("#42"));
        assert_eq!(label.url, None);

        let label = header_with_pr(None, Some("file:///etc/7")).pr_label();
        assert_eq!(label.number, None);
        assert_eq!(label.url, None);
    }

    #[test]
    fn no_pr_and_no_address_give_an_empty_label() {
        let label = header_with_pr(None, None).pr_label();
        assert_eq!(label.number, None);
        assert_eq!(label.url, None);
        let label = header_with_pr(Some("  "), None).pr_label();
        assert_eq!(label.number, None);
    }

    #[test]
    fn multiline_text_keeps_its_line_feeds_and_nothing_a_terminal_acts_on() {
        assert_eq!(
            without_unsafe_chars_multiline("a\nb\tc\r\n\u{1b}[31md\u{202E}e\u{200B}"),
            "a\nb c\n[31mde"
        );
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
