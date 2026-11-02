//! Application configuration: where the file is, what it says, and where every
//! effective value came from.
//!
//! Two axes that are easy to confuse and are kept apart here:
//!
//! 1. **Which file wins** — [`cascade`] walks `--config`, the user's config
//!    directory and `/etc/narsor/` and stops at the first one that exists. The
//!    files are never merged: a file that loses is shadowed whole, and
//!    `config show` lists it so that the operator can see it lost.
//! 2. **Where one value comes from** — [`resolve`] asks, per setting and in
//!    order, the command line, the environment, the winning file and finally
//!    the value compiled into the binary (R2). The first hit wins, *per
//!    setting*, which is why a file that sets one key does not displace a flag
//!    that sets another.
//!
//! [`settings`] is the registry both axes consult, and it does no input or
//! output at all.
//!
//! # Why resolution does not return a `Result`
//!
//! A broken configuration file stops every subcommand — except `config show`,
//! which is the one tool that exists to diagnose exactly that situation
//! (D-D). So resolution hands back values *and* errors together, and the
//! caller decides whether to refuse to start or to print what it has.

pub mod cascade;
pub mod client_paths;
pub mod duration;
pub mod file;
pub mod harness_home;
pub mod resolve;
pub(crate) mod security_validation;
pub mod settings;

pub use cascade::{CascadeError, ConfigCascade, Resolution};
pub use client_paths::ClientPaths;
pub use file::{FileError, FileValues, ParsedFile};
pub use harness_home::{
    HarnessHome, HomeMode, API_KEY_ENVS, CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV,
};
pub use resolve::AgentDefaults;
pub use resolve::{resolve, ConfigError, Environment, Flags, Resolved};
pub use security_validation::is_loopback_host;
pub use settings::{
    agent_args_for, agent_args_key, render_value, AgentArgs, SettingKey, SettingValue, Source,
    AGENT_ARGS_ENV, DEFAULT_BACKEND, DEFAULT_TUI_REFRESH_SECS, ENV_PREFIX, MASK, NO_LIMIT,
};
