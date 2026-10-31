//! The harnesses Narsor knows, in one list (R0).
//!
//! Every place that names a harness — the engine adapters, the configuration,
//! the server, the TUI and the CLI — takes the id and the names from here, so
//! no second spelling can drift away from the one the adapter registers under.

/// The `harness:` value of Claude Code.
pub const CLAUDE_CODE: &str = "claude-code";

/// The `harness:` value of Codex.
pub const CODEX: &str = "codex";

/// The `harness:` value of OpenCode.
pub const OPENCODE: &str = "opencode";

/// The `harness:` value of GitHub Copilot.
pub const GITHUB_COPILOT: &str = "github-copilot";

/// One harness: the id on the wire and the names a person reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessInfo {
    /// The `harness:` value, e.g. `claude-code`.
    pub id: &'static str,
    /// The name where room is short, e.g. `claude`.
    pub short_name: &'static str,
    /// The full name, e.g. `Claude Code`.
    pub display_name: &'static str,
}

/// Every known harness, in the order clients list them.
pub const HARNESSES: &[HarnessInfo] = &[
    HarnessInfo {
        id: CLAUDE_CODE,
        short_name: "claude",
        display_name: "Claude Code",
    },
    HarnessInfo {
        id: CODEX,
        short_name: "codex",
        display_name: "Codex",
    },
    HarnessInfo {
        id: OPENCODE,
        short_name: "opencode",
        display_name: "OpenCode",
    },
    HarnessInfo {
        id: GITHUB_COPILOT,
        short_name: "copilot",
        display_name: "GitHub Copilot",
    },
];

/// The harness with `id`, or `None` when Narsor does not know it.
pub fn find(id: &str) -> Option<&'static HarnessInfo> {
    HARNESSES.iter().find(|harness| harness.id == id)
}

/// The full name of the harness with `id`; the id itself when Narsor does not
/// know it.
pub fn display_name(id: &str) -> &str {
    find(id).map_or(id, |harness| harness.display_name)
}

#[cfg(test)]
mod tests {
    use super::{display_name, find, CLAUDE_CODE, HARNESSES};
    use std::collections::HashSet;

    #[test]
    fn claude_code_is_found_with_its_names() {
        let harness = find("claude-code").expect("claude-code is known");
        assert_eq!(harness.id, CLAUDE_CODE);
        assert_eq!(harness.short_name, "claude");
        assert_eq!(harness.display_name, "Claude Code");
    }

    #[test]
    fn an_unknown_harness_is_not_found() {
        assert_eq!(find("unknown-harness"), None);
        assert_eq!(display_name("unknown-harness"), "unknown-harness");
    }

    #[test]
    fn ids_and_short_names_are_unique() {
        let ids: HashSet<_> = HARNESSES.iter().map(|harness| harness.id).collect();
        let short_names: HashSet<_> = HARNESSES.iter().map(|harness| harness.short_name).collect();
        assert_eq!(ids.len(), HARNESSES.len());
        assert_eq!(short_names.len(), HARNESSES.len());
    }

    #[test]
    fn harnesses_are_listed_in_the_client_order() {
        let ids: Vec<_> = HARNESSES.iter().map(|harness| harness.id).collect();
        assert_eq!(ids, ["claude-code", "codex", "opencode", "github-copilot"]);
    }
}
