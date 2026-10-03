//! Where an [`super::ApiClient`] gets the bearer token it presents (U8).
//!
//! # Why a source, not a string
//!
//! In trusted mode the token is the daemon's random per-process secret and
//! never changes while that daemon lives. In network mode it is an OIDC
//! access token: short-lived by design, issued by somebody else's tooling
//! (a device-code helper, `oidc-agent`, a CI secret mount) and replaced
//! behind this process's back. A client that captured the token once would
//! fail at the first expiry and stay failed — and an SSE stream ends at
//! exactly that moment, too (`crate::auth::gate::guard_stream`).
//!
//! So the client holds a [`TokenSource`] and re-reads it after a `401`:
//! a `401` means "this credential is not (any longer) good" and is the one
//! answer a fresh credential can fix. A `403` means the credential is fine
//! but its holder is not the owner, or the request did not come through the
//! trusted proxy — re-reading would not change that, so it is never retried.
//!
//! The token never appears in `Debug` output, an error message or a log.

use std::fmt;
use std::path::PathBuf;

use tokio::sync::Mutex;

use super::client::ClientError;

/// Where the token comes from.
#[derive(Clone)]
pub enum TokenSource {
    /// A fixed token: the trusted-mode process token, or `NARSOR_TOKEN`.
    /// Nothing to re-read, so a `401` is final.
    Static(String),
    /// A file holding the token, read on first use and again after every
    /// `401` — what a token helper that refreshes in the background writes.
    File(PathBuf),
    /// A shell command printing the token on stdout, run on first use and
    /// again after every `401` (`oidc-token narsor`, a cloud CLI's
    /// `print-access-token`…).
    Command(String),
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::Static(_) => f.write_str("Static(<redacted>)"),
            TokenSource::File(path) => f.debug_tuple("File").field(path).finish(),
            TokenSource::Command(command) => f.debug_tuple("Command").field(command).finish(),
        }
    }
}

impl TokenSource {
    /// Whether re-reading can ever yield another token.
    pub fn is_refreshable(&self) -> bool {
        !matches!(self, TokenSource::Static(_))
    }

    async fn read(&self) -> Result<String, ClientError> {
        let token = match self {
            TokenSource::Static(token) => token.clone(),
            TokenSource::File(path) => tokio::fs::read_to_string(path).await.map_err(|error| {
                ClientError::Credential(format!(
                    "cannot read the token file {}: {error}",
                    path.display()
                ))
            })?,
            TokenSource::Command(command) => {
                let output = tokio::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(command)
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::inherit())
                    .output()
                    .await
                    .map_err(|error| {
                        ClientError::Credential(format!("cannot run the token command: {error}"))
                    })?;
                if !output.status.success() {
                    return Err(ClientError::Credential(format!(
                        "the token command failed ({})",
                        output.status
                    )));
                }
                String::from_utf8(output.stdout).map_err(|_| {
                    ClientError::Credential("the token command printed non-UTF-8 output".into())
                })?
            }
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(ClientError::Credential("the token source is empty".into()));
        }
        // A header value cannot carry control characters; refusing here keeps
        // a malformed file from turning into a confusing transport error.
        if token.chars().any(|c| c.is_control() || c == ' ') {
            return Err(ClientError::Credential(
                "the token contains whitespace or control characters".into(),
            ));
        }
        Ok(token)
    }
}

/// A [`TokenSource`] and the token last read from it, shared by every clone
/// of one client so a refresh made by one request serves the next.
pub(crate) struct TokenCell {
    source: TokenSource,
    current: Mutex<Option<String>>,
}

impl fmt::Debug for TokenCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenCell")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl TokenCell {
    pub(crate) fn new(source: TokenSource) -> Self {
        let current = match &source {
            TokenSource::Static(token) => Some(token.clone()),
            TokenSource::File(_) | TokenSource::Command(_) => None,
        };
        Self {
            source,
            current: Mutex::new(current),
        }
    }

    /// The token to present now, read from the source on first use.
    pub(crate) async fn current(&self) -> Result<String, ClientError> {
        let mut current = self.current.lock().await;
        if let Some(token) = current.as_ref() {
            return Ok(token.clone());
        }
        let token = self.source.read().await?;
        *current = Some(token.clone());
        Ok(token)
    }

    /// `rejected` drew a `401`: re-reads the source. True when a token other
    /// than `rejected` is now current — only then is a retry worth sending.
    ///
    /// When another request already refreshed past `rejected`, nothing is
    /// read again: that newer token is simply the answer.
    pub(crate) async fn refresh(&self, rejected: &str) -> Result<bool, ClientError> {
        if !self.source.is_refreshable() {
            return Ok(false);
        }
        let mut current = self.current.lock().await;
        if current.as_deref().is_some_and(|token| token != rejected) {
            return Ok(true);
        }
        let token = self.source.read().await?;
        let changed = token != rejected;
        *current = Some(token);
        Ok(changed)
    }
}
