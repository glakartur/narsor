//! The branches a repository has on its remote, listed with `git ls-remote`.
//!
//! The server has no credentials of a backend, so a private remote answers
//! with an error and the reason, never with an empty list. The call is
//! narrow on purpose: it speaks only `https` to a host beyond this machine
//! (a plain path on this disk is the one other kind it accepts), uses none of
//! the user's own git setup, never prompts, follows no redirect and ends
//! after a bounded time. Nothing it returns or says carries the credentials
//! of a remote URL.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::remote_policy::{check_https_remote, without_credentials};
use crate::workspace::provisioner::{
    check_remote, redact_credentials, restrict_git_access, GitAccess, WorkspaceError,
};

/// How long a listing may take before it is given up.
pub const DEFAULT_LISTING_LIMIT: Duration = Duration::from_secs(20);

/// The most of git's answer that is read: a remote with more refs than this
/// is not a list a branch field can use.
const MAX_ANSWER_BYTES: u64 = 8 * 1024 * 1024;

/// How often a running listing is checked for its end.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Why a list of branches could not be given.
#[derive(Debug, Error)]
pub enum BranchListError {
    /// The remote is not one this server lists, and git was not run.
    #[error("{0}")]
    Refused(String),
    /// Git did not answer within the limit.
    #[error("git ls-remote did not answer within {} s", .limit.as_secs())]
    TimedOut {
        /// The limit that was exceeded.
        limit: Duration,
    },
    /// Git ran and failed, or could not be run. The text has no credentials.
    #[error("could not list the branches: {0}")]
    Failed(String),
}

/// A source of the branch names of a remote.
pub trait BranchLister: Send + Sync {
    /// The names of the branches of `remote`, without the `refs/heads/`
    /// prefix. Tags and other refs are not branches.
    fn list_branches(&self, remote: &str) -> Result<Vec<String>, BranchListError>;
}

/// [`BranchLister`] that runs `git ls-remote --heads`.
#[derive(Debug, Clone)]
pub struct GitBranchLister {
    limit: Duration,
}

impl GitBranchLister {
    /// A lister that gives up after `limit`.
    pub fn new(limit: Duration) -> Self {
        Self { limit }
    }
}

impl Default for GitBranchLister {
    fn default() -> Self {
        Self::new(DEFAULT_LISTING_LIMIT)
    }
}

impl BranchLister for GitBranchLister {
    fn list_branches(&self, remote: &str) -> Result<Vec<String>, BranchListError> {
        check_listable(remote)?;
        let answer = run_with_limit(ls_remote_command(remote), self.limit)?;
        if !answer.success {
            return Err(BranchListError::Failed(failure_reason(&answer, remote)));
        }
        Ok(branch_names(&answer.stdout))
    }
}

/// The `git ls-remote --heads` command for `remote`: hardened as the module
/// says, with the remote after `--` so it is never read as an option.
pub fn ls_remote_command(remote: &str) -> Command {
    let mut command = Command::new("git");
    command
        .args(["ls-remote", "--heads", "--", remote])
        .env("GIT_TERMINAL_PROMPT", "0");
    restrict_git_access(
        &mut command,
        &GitAccess::PinnedOnly {
            home: PathBuf::from("/nonexistent"),
        },
    );
    if is_local_path(remote) {
        command.env("GIT_ALLOW_PROTOCOL", "file");
    }
    command
}

/// Refuses what git must not be asked about: an option, the `ext::`
/// transport, and any network remote that is not `https` to a host beyond
/// this machine.
fn check_listable(remote: &str) -> Result<(), BranchListError> {
    check_remote("", remote).map_err(|error| match error {
        WorkspaceError::UnsafeRemote { remote, reason, .. } => {
            BranchListError::Refused(format!("the remote '{remote}' is refused: {reason}"))
        }
        other => BranchListError::Refused(other.to_string()),
    })?;
    if is_local_path(remote) {
        return Ok(());
    }
    check_https_remote(remote).map_err(BranchListError::Refused)
}

/// Whether git reads `remote` as a path on this disk: no `scheme://`, and no
/// `host:path` form, which git reads as an SSH remote.
fn is_local_path(remote: &str) -> bool {
    let host_form = remote
        .find(':')
        .is_some_and(|colon| remote.find('/').is_none_or(|slash| colon < slash));
    !remote.contains("://") && !host_form
}

struct Answer {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_with_limit(mut command: Command, limit: Duration) -> Result<Answer, BranchListError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| BranchListError::Failed(format!("could not run git: {error}")))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BranchListError::TimedOut { limit });
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BranchListError::Failed(format!(
                    "could not wait for git: {error}"
                )));
            }
        }
    };
    let text = |reader: thread::JoinHandle<Vec<u8>>| {
        String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned()
    };
    Ok(Answer {
        success: status.success(),
        code: status.code(),
        stdout: text(stdout),
        stderr: text(stderr),
    })
}

/// Reads a pipe on its own thread, so a full pipe never stops git.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(MAX_ANSWER_BYTES).read_to_end(&mut bytes);
        }
        bytes
    })
}

fn failure_reason(answer: &Answer, remote: &str) -> String {
    let said = answer.stderr.trim();
    let reason = if said.is_empty() {
        match answer.code {
            Some(code) => format!("git exited with status {code}"),
            None => "git was stopped by a signal".to_string(),
        }
    } else {
        said.to_string()
    };
    // Git repeats the remote as it was written, and that may hold a token.
    redact_credentials(&reason.replace(remote, &without_credentials(remote)))
}

fn branch_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(_, name)| name.strip_prefix("refs/heads/"))
        .map(str::to_string)
        .collect()
}
