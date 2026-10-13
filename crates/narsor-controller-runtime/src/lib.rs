//! Lifecycle helpers shared by the local controller and the standalone
//! `narsor-controller` process.

pub mod backend;
pub mod controller;
pub mod testkit;

pub use controller::{Controller, ControllerConfig, ControllerLink, ControllerStatus, StepOutcome};

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

/// Future returned while enrolling a controller identity.
pub type EnrollmentFuture<'a, Client> =
    Pin<Box<dyn Future<Output = Result<(Client, String), String>> + 'a>>;

/// Restart and shutdown policy for a supervised controller loop.
#[derive(Debug, Clone, Copy)]
pub struct Supervision {
    /// Pause before a controller loop that died is started again.
    pub restart_after: Duration,
    /// Longest time a stopping loop gets to release its lease.
    pub stop_timeout: Duration,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            restart_after: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(10),
        }
    }
}

/// Keeps one controller loop running until `shutdown` resolves.
///
/// A loop that exits or panics is restarted after a pause, letting it
/// reacquire the lease and reconcile from persisted control-plane state.
/// On shutdown it receives a stop signal and a bounded time to release its
/// lease; an unresponsive loop is aborted so its lease can lapse.
pub async fn supervise<F, Fut>(
    mut start: F,
    shutdown: impl Future<Output = ()> + Send,
    policy: Supervision,
) where
    F: FnMut(tokio::sync::oneshot::Receiver<()>) -> Fut + Send,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::pin!(shutdown);
    loop {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let mut run = tokio::spawn(start(stopped));
        tokio::select! {
            _ = &mut shutdown => {
                let _ = stop.send(());
                match tokio::time::timeout(policy.stop_timeout, &mut run).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "controller: loop failed while stopping"),
                    Err(_) => {
                        run.abort();
                        tracing::warn!("controller: loop did not stop in time; its lease will lapse");
                    }
                }
                return;
            }
            outcome = &mut run => {
                match outcome {
                    Err(error) => tracing::error!(%error, "controller: loop died; restarting"),
                    Ok(()) => tracing::warn!("controller: loop ended unexpectedly; restarting"),
                }
                tokio::select! {
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(policy.restart_after) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod supervision_tests {
    use super::{supervise, Supervision};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[tokio::test]
    async fn restarts_a_loop_that_ends_until_shutdown() {
        let starts = Arc::new(AtomicUsize::new(0));
        let loop_starts = Arc::clone(&starts);
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let supervisor = tokio::spawn(supervise(
            move |_stopped| {
                loop_starts.fetch_add(1, Ordering::SeqCst);
                async {}
            },
            async {
                let _ = stop.await;
            },
            Supervision {
                restart_after: Duration::from_millis(1),
                stop_timeout: Duration::from_millis(100),
            },
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while starts.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("loop restarted");
        let _ = shutdown.send(());
        tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .expect("supervisor stopped")
            .expect("supervisor task");
    }

    #[tokio::test]
    async fn aborts_a_loop_that_does_not_stop_in_time() {
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let supervisor = tokio::spawn(supervise(
            |_stopped| std::future::pending::<()>(),
            async {
                let _ = stop.await;
            },
            Supervision {
                restart_after: Duration::from_secs(1),
                stop_timeout: Duration::from_millis(1),
            },
        ));

        let _ = shutdown.send(());
        tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .expect("supervisor stopped")
            .expect("supervisor task");
    }
}

/// Adapts a controller protocol client to credential enrollment.
pub trait EnrollmentClient {
    /// The authenticated client returned to the reconciler.
    type Client;

    /// Whether `credential` is a valid persisted credential for this client.
    fn is_valid_credential(&self, credential: &str) -> bool;

    /// Builds an authenticated client from a persisted credential.
    fn client(&self, control_plane: &str, credential: &str) -> Self::Client;

    /// Redeems an enrollment code and returns the authenticated client and
    /// credential that must be persisted before the client is used.
    fn enroll<'a>(
        &'a self,
        control_plane: &'a str,
        code: &'a str,
    ) -> EnrollmentFuture<'a, Self::Client>;
}

/// Failure while loading or enrolling a controller identity.
#[derive(Debug)]
pub enum EnrollmentError {
    InvalidCredential(PathBuf),
    MissingCode(PathBuf),
    Read { path: PathBuf, source: io::Error },
    Enroll(String),
    Persist { path: PathBuf, source: io::Error },
}

impl EnrollmentError {
    /// The operator-facing explanation.
    pub fn message(&self) -> String {
        match self {
            Self::InvalidCredential(path) => {
                format!("{} does not hold a controller credential", path.display())
            }
            Self::MissingCode(path) => format!(
                "{} does not exist and no enrollment code was given (--enrollment-code)",
                path.display()
            ),
            Self::Read { path, source } => format!("cannot read {}: {source}", path.display()),
            Self::Enroll(message) => format!("enrollment failed: {message}"),
            Self::Persist { path, source } => {
                format!("cannot write {}: {source}", path.display())
            }
        }
    }

    /// Missing or malformed local credentials are invocation errors.
    pub fn is_usage_error(&self) -> bool {
        matches!(self, Self::InvalidCredential(_) | Self::MissingCode(_))
    }
}

/// Loads a controller identity or enrolls and persists it before first use.
pub async fn load_or_enroll_with<A: EnrollmentClient>(
    adapter: &A,
    control_plane: &str,
    credential_file: &Path,
    code: Option<&str>,
) -> Result<A::Client, EnrollmentError> {
    match std::fs::read_to_string(credential_file) {
        Ok(text) => {
            let credential = text.trim();
            if !adapter.is_valid_credential(credential) {
                return Err(EnrollmentError::InvalidCredential(
                    credential_file.to_path_buf(),
                ));
            }
            Ok(adapter.client(control_plane, credential))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let code = code
                .filter(|code| !code.is_empty())
                .ok_or_else(|| EnrollmentError::MissingCode(credential_file.to_path_buf()))?;
            let (client, credential) = adapter
                .enroll(control_plane, code)
                .await
                .map_err(EnrollmentError::Enroll)?;
            persist_credential(credential_file, &credential).map_err(|source| {
                EnrollmentError::Persist {
                    path: credential_file.to_path_buf(),
                    source,
                }
            })?;
            Ok(client)
        }
        Err(source) => Err(EnrollmentError::Read {
            path: credential_file.to_path_buf(),
            source,
        }),
    }
}

fn persist_credential(path: &Path, credential: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(credential.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::{load_or_enroll_with, EnrollmentClient, EnrollmentError};
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeClient;

    impl EnrollmentClient for FakeClient {
        type Client = String;

        fn is_valid_credential(&self, credential: &str) -> bool {
            credential.starts_with("nsk.")
        }

        fn client(&self, control_plane: &str, credential: &str) -> Self::Client {
            format!("{control_plane}:{credential}")
        }

        fn enroll<'a>(
            &'a self,
            control_plane: &'a str,
            code: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(Self::Client, String), String>> + 'a>> {
            Box::pin(async move {
                if code != "one-time" {
                    return Err("invalid enrollment code".to_string());
                }
                Ok((
                    self.client(control_plane, "nsk.new-credential"),
                    "nsk.new-credential".to_string(),
                ))
            })
        }
    }

    fn temp_path() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "narsor-controller-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn enrollment_persists_a_private_credential_before_returning() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path();
        let client = load_or_enroll_with(&FakeClient, "https://control", &path, Some("one-time"))
            .await
            .expect("enrollment");
        assert_eq!(client, "https://control:nsk.new-credential");
        let credential = std::fs::read_to_string(&path).expect("persisted credential");
        assert_eq!(credential, "nsk.new-credential\n");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("credential metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_file(path).expect("remove test credential");
    }

    #[tokio::test]
    async fn existing_credentials_are_validated_and_reused() {
        let path = temp_path();
        std::fs::write(&path, "nsk.existing\n").expect("write credential fixture");
        let client = load_or_enroll_with(&FakeClient, "https://control", &path, None)
            .await
            .expect("reuse credential");
        assert_eq!(client, "https://control:nsk.existing");
        std::fs::remove_file(path).expect("remove test credential");
    }

    #[tokio::test]
    async fn an_absent_credential_needs_an_enrollment_code() {
        let path = temp_path();
        let error = load_or_enroll_with(&FakeClient, "https://control", &path, None)
            .await
            .expect_err("missing code");
        assert!(matches!(error, EnrollmentError::MissingCode(_)));
        assert!(error.is_usage_error());
        assert!(!Path::new(&path).exists());
    }
}
