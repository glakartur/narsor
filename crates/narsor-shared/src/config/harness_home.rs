//! Which HOME, and which harness configuration, the tools of a task see.
//!
//! A harness CLI keeps its login next to its sessions: `codex` in
//! `~/.codex/auth.json`, Claude Code in `~/.claude/.credentials.json`. A
//! task that runs on a subscription rather than an API key needs that login,
//! so where the runner points HOME decides whether a harness is logged in at
//! all. It is a launch setting of the backend, read on every start, not part
//! of the task: switching it for a task that already ran leaves the sessions
//! of the earlier mode behind, which is the operator's call to make.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Variable `codex` reads its configuration directory from.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";
/// Variable Claude Code reads its configuration directory from.
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Variables that make a harness bill an API key instead of the
/// subscription it is logged in with. The runner never passes them on from
/// its own environment; a task that wants one gets it as a secret.
pub const API_KEY_ENVS: [&str; 3] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CODEX_API_KEY"];

/// Where HOME points for every action of a task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum HomeMode {
    /// A private HOME on task storage (R17): nothing of the host leaks in,
    /// and a harness logs in only through what the task is given.
    #[default]
    Private,
    /// The HOME the runner was started with, the way a harness run from the
    /// console sees it — logins included.
    Host,
}

impl HomeMode {
    /// The word on the command line and in configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            HomeMode::Private => "private",
            HomeMode::Host => "host",
        }
    }
}

/// The HOME mode and the harness configuration directories of a launch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessHome {
    /// Where HOME points.
    #[serde(default)]
    pub mode: HomeMode,
    /// `codex`'s configuration directory, when not `$HOME/.codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<PathBuf>,
    /// Claude Code's configuration directory, when not `$HOME/.claude`:
    /// the work directory the harness runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<PathBuf>,
    /// The Claude Code profile the work directory is brought up to date
    /// with before every start. `None`, or the work directory itself, runs
    /// the harness on its directory as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_profile_source: Option<PathBuf>,
    /// Extra arguments of every harness run, from the `agent_args` setting;
    /// they win over the arguments Narsor chooses itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_args: Vec<String>,
}

impl HarnessHome {
    /// The `narsor-runner` arguments that carry it.
    pub fn runner_args(&self) -> Vec<OsString> {
        let mut args = vec![OsString::from("--home"), OsString::from(self.mode.as_str())];
        if let Some(dir) = &self.codex_home {
            args.push("--codex-home".into());
            args.push(dir.into());
        }
        if let Some(dir) = &self.claude_config_dir {
            args.push("--claude-config-dir".into());
            args.push(dir.into());
        }
        if let Some(dir) = &self.claude_profile_source {
            args.push("--claude-profile-source".into());
            args.push(dir.into());
        }
        for arg in &self.agent_args {
            // The `=` form keeps an argument that starts with `-` from being
            // read as a flag of the runner.
            args.push(format!("--agent-arg={arg}").into());
        }
        args
    }

    /// The configuration directories as the variables their harness reads.
    pub fn variables(&self) -> Vec<(String, String)> {
        [
            (CODEX_HOME_ENV, &self.codex_home),
            (CLAUDE_CONFIG_DIR_ENV, &self.claude_config_dir),
        ]
        .into_iter()
        .filter_map(|(name, dir)| {
            dir.as_ref()
                .map(|dir| (name.to_string(), dir.display().to_string()))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_args_name_the_mode_and_only_the_directories_given() {
        let home = HarnessHome {
            mode: HomeMode::Host,
            codex_home: Some(PathBuf::from("/home/me/.codex")),
            claude_config_dir: None,
            claude_profile_source: None,
            agent_args: Vec::new(),
        };
        assert_eq!(
            home.runner_args(),
            ["--home", "host", "--codex-home", "/home/me/.codex"]
                .map(OsString::from)
                .to_vec()
        );
        assert_eq!(
            HarnessHome::default().runner_args(),
            ["--home", "private"].map(OsString::from).to_vec()
        );
    }

    #[test]
    fn the_directories_become_the_variables_their_harness_reads() {
        let home = HarnessHome {
            mode: HomeMode::Private,
            codex_home: Some(PathBuf::from("/c")),
            claude_config_dir: Some(PathBuf::from("/a")),
            claude_profile_source: Some(PathBuf::from("/s")),
            agent_args: Vec::new(),
        };
        assert_eq!(
            home.variables(),
            vec![
                (CODEX_HOME_ENV.to_string(), "/c".to_string()),
                (CLAUDE_CONFIG_DIR_ENV.to_string(), "/a".to_string()),
            ]
        );
        assert!(HarnessHome::default().variables().is_empty());
    }

    #[test]
    fn runner_args_carry_the_agent_args_one_each() {
        let home = HarnessHome {
            agent_args: vec!["--dangerously-skip-permissions".into(), "--effort".into()],
            ..HarnessHome::default()
        };
        assert_eq!(
            home.runner_args(),
            [
                "--home",
                "private",
                "--agent-arg=--dangerously-skip-permissions",
                "--agent-arg=--effort"
            ]
            .map(OsString::from)
            .to_vec()
        );
    }

    #[test]
    fn runner_args_carry_the_profile_source() {
        let home = HarnessHome {
            claude_profile_source: Some(PathBuf::from("/profile")),
            ..HarnessHome::default()
        };
        assert_eq!(
            home.runner_args(),
            ["--home", "private", "--claude-profile-source", "/profile"]
                .map(OsString::from)
                .to_vec()
        );
    }
}
