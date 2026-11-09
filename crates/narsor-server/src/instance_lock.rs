//! One daemon per state directory.
//!
//! The daemon takes an exclusive `flock` on `daemon.instance.lock` and keeps
//! it until it exits. The kernel drops the lock when the process ends, also
//! after `SIGKILL`, so no stale lock can stay behind. The descriptor is
//! opened by `std`, which sets `O_CLOEXEC`: the controller and the runners
//! the daemon starts do not inherit the lock.

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// A lock that can be tried without waiting.
pub trait TryLock {
    /// `Ok(true)` when the lock is now held by this value, `Ok(false)` when
    /// another process holds it.
    fn try_lock(&mut self) -> io::Result<bool>;
}

#[derive(Debug)]
pub struct InstanceLockError(String);

impl std::fmt::Display for InstanceLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InstanceLockError {}

/// Tries `lock` up to `attempts` times, `pause` apart. On success `lock`
/// holds the lock until it is dropped: the caller keeps it alive.
pub async fn acquire<L: TryLock>(
    lock: &mut L,
    attempts: u32,
    pause: Duration,
) -> Result<(), InstanceLockError> {
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(pause).await;
        }
        match lock.try_lock() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                return Err(InstanceLockError(format!(
                    "could not take the daemon instance lock: {error}"
                )))
            }
        }
    }
    Err(InstanceLockError(
        "another narsor daemon holds this state directory; stop it with \
         `narsor-server stop` and start again"
            .to_string(),
    ))
}

/// The real lock: `flock(LOCK_EX | LOCK_NB)` on a file.
pub struct FlockFile {
    path: PathBuf,
    held: Option<File>,
}

impl FlockFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path, held: None }
    }
}

impl TryLock for FlockFile {
    fn try_lock(&mut self) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        if self.held.is_some() {
            return Ok(true);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.path)?;
        // SAFETY: flock takes a live descriptor owned by `file` and no
        // pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            self.held = Some(file);
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        answers: Vec<io::Result<bool>>,
        calls: usize,
    }

    impl TryLock for Fake {
        fn try_lock(&mut self) -> io::Result<bool> {
            let answer = std::mem::replace(&mut self.answers[self.calls], Ok(false));
            self.calls += 1;
            answer
        }
    }

    fn run(answers: Vec<io::Result<bool>>) -> (Result<(), String>, usize) {
        let attempts = answers.len() as u32;
        let mut fake = Fake { answers, calls: 0 };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(acquire(&mut fake, attempts, Duration::ZERO))
            .map_err(|error| error.to_string());
        (outcome, fake.calls)
    }

    #[test]
    fn a_free_lock_is_taken_at_once() {
        assert_eq!(run(vec![Ok(true), Ok(false)]), (Ok(()), 1));
    }

    #[test]
    fn a_lock_freed_by_a_predecessor_is_taken_after_waiting() {
        assert_eq!(run(vec![Ok(false), Ok(false), Ok(true)]), (Ok(()), 3));
    }

    #[test]
    fn a_lock_that_stays_held_ends_in_an_error_after_all_attempts() {
        let (outcome, calls) = run(vec![Ok(false), Ok(false)]);
        assert!(outcome.unwrap_err().contains("another narsor daemon"));
        assert_eq!(calls, 2);
    }

    #[test]
    fn an_io_error_ends_at_once() {
        let (outcome, calls) = run(vec![
            Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            Ok(true),
        ]);
        assert!(outcome.unwrap_err().contains("could not take"));
        assert_eq!(calls, 1);
    }
}
