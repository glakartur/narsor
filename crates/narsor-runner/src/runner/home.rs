//! Which HOME, and which harness configuration, the tools of a task see.
//!
//! A harness CLI keeps its login next to its sessions: `codex` in
//! `~/.codex/auth.json`, Claude Code in `~/.claude/.credentials.json`. A
//! task that runs on a subscription rather than an API key needs that login,
//! so where the runner points HOME decides whether a harness is logged in at
//! all. It is a launch setting of the backend, read on every start, not part
//! of the task: switching it for a task that already ran leaves the sessions
//! of the earlier mode behind, which is the operator's call to make.

pub use narsor_engine::config::{
    HarnessHome, HomeMode, API_KEY_ENVS, CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV,
};
