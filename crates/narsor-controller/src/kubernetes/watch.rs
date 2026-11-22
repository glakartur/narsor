//! Following the managed Pods to wake the controller (R4, KTD5).
//!
//! The follower lists the Pods the adapter manages (label selector
//! `app.kubernetes.io/managed-by=narsor`) for a `resourceVersion`, then
//! watches them from it. Every added, changed or deleted managed Pod wakes
//! the controller; nothing else happens here — what a Pod's state is, the
//! controller's next observation tells, so an event never confirms a loss
//! by itself.
//!
//! * A watch the server ends (its `timeoutSeconds` passed, the connection
//!   closed) is opened again from the last version seen, at once when it
//!   delivered events or lasted [`HEALTHY_ROUND`]; bookmarks keep that
//!   version current on a quiet namespace.
//! * A version too old for the server (410 Gone, as an HTTP status or as an
//!   `ERROR` event) means changes may have gone unseen: the follower lists
//!   again and wakes the controller, which then observes everything.
//! * A failure — the API down, or a Role without `list`/`watch` on pods
//!   (an RBAC from before the watch) — is retried after a pause that
//!   doubles up to [`RETRY_MAX`]; a lasting refusal is logged as a warning
//!   once until the watch works again. Meanwhile the controller keeps its
//!   periodic pace.

use std::time::Duration;

use serde_json::Value;

use narsor_controller_runtime::backend::{BackendError, BackendErrorKind, WakeSignal};

use super::client::{KubeClient, WatchEvent, Watched};
use super::manifest::MANAGED_BY;

/// How long the server keeps one watch open (`timeoutSeconds`).
const WATCH_SECONDS: u64 = 290;

/// What the whole watch exchange may take beyond [`WATCH_SECONDS`] before
/// it counts as cut off.
const WATCH_MARGIN: Duration = Duration::from_secs(30);

/// The first pause after a failed or empty round; it doubles while rounds
/// keep failing, up to [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_millis(100);

/// The longest pause between two rounds.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// A watch the server closes after at least this long ended as it should
/// (its `timeoutSeconds` on a quiet namespace, with no bookmark): it is
/// opened again at once, like one that delivered events. A quicker close
/// is paused after, like a failure.
const HEALTHY_ROUND: Duration = Duration::from_secs(5);

/// Follows the managed Pods and wakes `wake` at each change, until nobody
/// listens to it any more.
pub(super) async fn follow_managed_pods(client: KubeClient, wake: WakeSignal) {
    let follower = Follower {
        client,
        wake: wake.clone(),
    };
    tokio::select! {
        () = wake.deserted() => {}
        () = follower.run() => {}
    }
}

struct Follower {
    client: KubeClient,
    wake: WakeSignal,
}

/// How one watch ended. A watch that fails is opened again from the last
/// version too; should that version be too old by then, the server says so.
enum Ended {
    /// The server closed it: watch again from the last version.
    Closed,
    /// The version is too old: list again.
    Expired,
}

impl Follower {
    async fn run(&self) {
        let mut version: Option<String> = None;
        let mut listed = false;
        let mut retry = RETRY_MIN;
        let mut warned = false;
        loop {
            let from = match version.take() {
                Some(from) => Ok(from),
                None => {
                    let from = self.list().await;
                    if from.is_ok() {
                        if listed {
                            // What changed since the last version seen is
                            // unknown: the controller looks at everything.
                            self.wake.wake();
                        }
                        listed = true;
                    }
                    from
                }
            };
            let round = match from {
                Ok(mut from) => {
                    let started = tokio::time::Instant::now();
                    let round = self.watch(&mut from).await.map(|(ended, heard)| {
                        let lasted =
                            matches!(ended, Ended::Closed) && started.elapsed() >= HEALTHY_ROUND;
                        (ended, heard || lasted)
                    });
                    if !matches!(round, Ok((Ended::Expired, _))) {
                        version = Some(from);
                    }
                    round
                }
                Err(error) => Err(error),
            };
            let heard = match round {
                Ok((_, heard)) => {
                    warned = false;
                    heard
                }
                Err(error) => {
                    report(&error, &mut warned);
                    false
                }
            };
            if heard {
                retry = RETRY_MIN;
                continue;
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /// Lists the managed Pods: the version to watch from.
    async fn list(&self) -> Result<String, BackendError> {
        let path = format!("{}?labelSelector={}", self.pods(), selector());
        let list = self.client.list(&path).await?;
        list.pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BackendError::transient("the Pod list carries no resourceVersion"))
    }

    /// Watches from `version`, moving it along; whether the watch delivered
    /// anything (a stream that ends at once is paused after, like a
    /// failure).
    async fn watch(&self, version: &mut String) -> Result<(Ended, bool), BackendError> {
        let path = format!(
            "{}?watch=true&allowWatchBookmarks=true&timeoutSeconds={WATCH_SECONDS}\
             &labelSelector={}&resourceVersion={}",
            self.pods(),
            selector(),
            encode(version),
        );
        let timeout = Duration::from_secs(WATCH_SECONDS) + WATCH_MARGIN;
        let mut events = match self.client.watch(&path, timeout).await? {
            Watched::Events(events) => events,
            Watched::Expired => return Ok((Ended::Expired, false)),
        };
        let mut heard = false;
        while let Some(event) = events.next_event().await? {
            heard = true;
            match WatchEvent::read(event) {
                WatchEvent::Changed {
                    resource_version,
                    object,
                } => {
                    if is_managed(&object) {
                        self.wake.wake();
                    }
                    if let Some(next) = resource_version {
                        *version = next;
                    }
                }
                WatchEvent::Bookmark(Some(next)) => *version = next,
                WatchEvent::Bookmark(None) => {}
                // Not counted as heard: a server that keeps answering so is
                // asked again only after a pause.
                WatchEvent::Expired => return Ok((Ended::Expired, false)),
                WatchEvent::Failed(message) => return Err(BackendError::transient(message)),
            }
        }
        Ok((Ended::Closed, heard))
    }

    fn pods(&self) -> String {
        self.client.core_path("pods", None)
    }
}

/// Logs a failed round: a lasting refusal (RBAC) once as a warning until the
/// watch works again, anything else at debug level.
fn report(error: &BackendError, warned: &mut bool) {
    if error.kind == BackendErrorKind::Transient {
        tracing::debug!(%error, "kubernetes: cannot follow the Pods; trying again");
    } else if !*warned {
        *warned = true;
        tracing::warn!(
            %error,
            "kubernetes: cannot watch the managed Pods (the controller's Role must grant \
             list and watch on pods); Pod changes are noticed at the periodic observation only"
        );
    }
}

/// Whether the object carries the managed label: the watch is selected by
/// it already, and is checked again here.
fn is_managed(object: &Value) -> bool {
    object
        .pointer("/metadata/labels")
        .and_then(|labels| labels.get(MANAGED_BY.0))
        .and_then(Value::as_str)
        == Some(MANAGED_BY.1)
}

/// The label selector of the managed Pods, query-encoded.
fn selector() -> String {
    encode(&format!("{}={}", MANAGED_BY.0, MANAGED_BY.1))
}

/// Percent-encodes `value` for a URL query.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
